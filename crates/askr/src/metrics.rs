//! Shared-memory metrics.
//!
//! The master maps an anonymous **shared** region *before* forking, so every
//! worker process and the master's admin thread see the same physical counters
//! (no IPC, no locks — just atomics on shared pages). This is also the seed of
//! the shared-memory substrate that later backs a cross-process cache and
//! broadcast bus.
//!
//! Because it's in-process, Askr can measure something FPM/proxies can't cleanly
//! see: how much of each request is **PHP** vs **TLS/I/O**.

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

/// Latency histogram bucket upper bounds, in milliseconds (last is overflow).
pub const BUCKET_BOUNDS_MS: [u64; 12] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000];
const NBUCKETS: usize = 13; // one extra "+Inf" overflow bucket

/// Shared counters. `#[repr(C)]` for a stable cross-process layout; all-zero is
/// a valid initial state (anonymous shared pages are zeroed), so we never run a
/// constructor in the mapping.
/// Must be >= `supervisor::MAX_WORKERS`; slots beyond it are simply not attributed.
pub const STAT_SLOTS: usize = 512;

/// One prefork slot's request counters.
#[repr(C)]
#[derive(Default)]
pub struct WorkerStat {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    /// Sum of total request microseconds, for a mean-latency comparison.
    pub us_sum: AtomicU64,
}

impl WorkerStat {
    /// Zero the slot — called before spawning a worker into it, so a canary's
    /// counters describe exactly that worker's life rather than its predecessors'.
    pub fn reset(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.requests.store(0, Relaxed);
        self.errors.store(0, Relaxed);
        self.us_sum.store(0, Relaxed);
    }

    /// `(requests, errors, mean_us)`
    pub fn snapshot(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let r = self.requests.load(Relaxed);
        let e = self.errors.load(Relaxed);
        let us = self.us_sum.load(Relaxed);
        (r, e, us.checked_div(r).unwrap_or(0))
    }
}

#[repr(C)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub bytes_out: AtomicU64,
    pub php_us: AtomicU64,
    pub total_us: AtomicU64,
    /// Index by status class: [0]=1xx [1]=2xx [2]=3xx [3]=4xx [4]=5xx.
    pub status: [AtomicU64; 5],
    pub buckets: [AtomicU64; NBUCKETS],
    pub slowest_us: AtomicU64,
    pub errors: AtomicU64,
    /// Requests currently executing in PHP across all workers — the queueing
    /// signal the CoW autoscaler reads to add/harvest workers.
    pub inflight: AtomicU64,
    /// KV cache entries evicted under pressure (probe window full).
    pub cache_evictions: AtomicU64,
    /// KV cache writes rejected because the value exceeds the largest slot (64 KB).
    pub cache_oversize: AtomicU64,
    /// Responses not cached because they carried more tags than an entry can hold.
    pub cache_tag_overflow: AtomicU64,
    /// Requests refused by a `[[ratelimit]]` rule. In shared memory so the
    /// master's admin thread sees the total across all worker processes.
    pub ratelimit_blocked: AtomicU64,
    /// Per-worker counters, indexed by prefork slot. The canary gate compares the
    /// new worker against the rest of the fleet in the same window, which a
    /// fleet-wide total can't express: without attribution, errors from the *old*
    /// workers count against the canary.
    pub per_worker: [WorkerStat; STAT_SLOTS],
    /// Traffic-shadow outcomes: mirrored requests, matches, mismatches, errors.
    pub shadow_total: AtomicU64,
    pub shadow_match: AtomicU64,
    pub shadow_mismatch: AtomicU64,
    pub shadow_error: AtomicU64,
    /// PID of the elected metrics-rollup writer (0 = none). One process snapshots
    /// the shared metrics to the observability `metrics` table; the rest defer.
    pub metrics_leader: AtomicU32,
    /// What the sandbox *achieved*, counted by the workers that applied it, so the
    /// admin plane can tell a hardened fleet from a partly hardened one instead of
    /// repeating the configuration back. `sandbox_workers` is how many ran
    /// `sandbox::apply`; the other two are how many of those got each half.
    pub sandbox_workers: AtomicU64,
    pub sandbox_seccomp: AtomicU64,
    pub sandbox_landlock: AtomicU64,
    /// The Landlock ABI in force (0 = none / not applied).
    pub sandbox_landlock_abi: AtomicU64,
}

static METRICS_PTR: AtomicPtr<Metrics> = AtomicPtr::new(ptr::null_mut());

impl Metrics {
    /// Map the shared region and register it globally. Call once in the master
    /// **before** forking so children inherit the same physical pages.
    pub fn init() {
        let size = std::mem::size_of::<Metrics>();
        // SAFETY: anonymous shared mapping; zeroed pages are a valid Metrics.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            tracing::warn!("metrics: mmap failed; metrics disabled");
            return;
        }
        METRICS_PTR.store(p as *mut Metrics, Ordering::SeqCst);
    }

    /// The shared metrics, if mapped.
    pub fn get() -> Option<&'static Metrics> {
        let p = METRICS_PTR.load(Ordering::SeqCst);
        if p.is_null() {
            None
        } else {
            // SAFETY: set once by init() to a valid, process-shared mapping.
            Some(unsafe { &*p })
        }
    }

    /// Record one finished request.
    pub fn record(&self, status: u16, bytes: u64, php_us: u64, total_us: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes, Ordering::Relaxed);
        self.php_us.fetch_add(php_us, Ordering::Relaxed);
        self.total_us.fetch_add(total_us, Ordering::Relaxed);

        let class = ((status / 100).clamp(1, 5) - 1) as usize;
        self.status[class].fetch_add(1, Ordering::Relaxed);

        let ms = total_us / 1000;
        let mut b = NBUCKETS - 1;
        for (i, &bound) in BUCKET_BOUNDS_MS.iter().enumerate() {
            if ms <= bound {
                b = i;
                break;
            }
        }
        self.buckets[b].fetch_add(1, Ordering::Relaxed);
        self.slowest_us.fetch_max(total_us, Ordering::Relaxed);
    }

    pub fn note_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the histogram buckets.
    pub fn bucket_counts(&self) -> [u64; NBUCKETS] {
        let mut out = [0u64; NBUCKETS];
        for (i, b) in self.buckets.iter().enumerate() {
            out[i] = b.load(Ordering::Relaxed);
        }
        out
    }
}

/// Resident set size of a process in KB (via `ps`, portable Linux/macOS).
pub fn rss_kb(pid: i32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_stat_snapshots_and_resets() {
        let st = WorkerStat::default();
        assert_eq!(st.snapshot(), (0, 0, 0), "a fresh slot has no history");

        st.requests.store(4, Ordering::Relaxed);
        st.errors.store(1, Ordering::Relaxed);
        st.us_sum.store(8_000, Ordering::Relaxed);
        assert_eq!(st.snapshot(), (4, 1, 2_000), "mean is us_sum / requests");

        // Reset is what makes a canary's counters describe only its own life, not
        // the worker it replaced in that slot.
        st.reset();
        assert_eq!(st.snapshot(), (0, 0, 0));
    }

    #[test]
    fn mean_latency_of_zero_requests_does_not_divide_by_zero() {
        let st = WorkerStat::default();
        st.us_sum.store(500, Ordering::Relaxed);
        assert_eq!(st.snapshot(), (0, 0, 0));
    }

    #[test]
    fn record_classifies_status_codes() {
        Metrics::init();
        let m = Metrics::get().expect("metrics region");
        let before: Vec<u64> = m.status.iter().map(|s| s.load(Ordering::Relaxed)).collect();

        m.record(200, 10, 1, 2);
        m.record(301, 10, 1, 2);
        m.record(404, 10, 1, 2);
        m.record(500, 10, 1, 2);
        m.record(503, 10, 1, 2);

        let after: Vec<u64> = m.status.iter().map(|s| s.load(Ordering::Relaxed)).collect();
        assert_eq!(after[1] - before[1], 1, "2xx");
        assert_eq!(after[2] - before[2], 1, "3xx");
        assert_eq!(after[3] - before[3], 1, "4xx");
        assert_eq!(after[4] - before[4], 2, "5xx");
    }

    #[test]
    fn rss_of_this_process_is_readable() {
        // `tune` and the RSS recycler both depend on this working on the host.
        let kb = rss_kb(std::process::id() as i32);
        assert!(kb.unwrap_or(0) > 0, "should be able to read our own RSS");
    }
}
