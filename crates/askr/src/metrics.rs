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
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// Latency histogram bucket upper bounds, in milliseconds (last is overflow).
pub const BUCKET_BOUNDS_MS: [u64; 12] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000];
const NBUCKETS: usize = 13; // one extra "+Inf" overflow bucket

/// Shared counters. `#[repr(C)]` for a stable cross-process layout; all-zero is
/// a valid initial state (anonymous shared pages are zeroed), so we never run a
/// constructor in the mapping.
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
