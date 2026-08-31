//! Rate limiting in the Rust layer, before PHP is woken.
//!
//! Token buckets live in shared memory mapped **before the fork**, so a limit is
//! enforced across the whole worker fleet rather than per process — the thing
//! FPM + nginx can't do without an external store. A blocked request never costs a
//! PHP cycle: it's refused in the same layer that serves cache hits.
//!
//! Buckets are keyed by `rule index + identity` (client IP, a header, or a cookie),
//! so two rules matching the same visitor don't share a counter.

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Linear-probe window before we reuse the least recently used bucket.
const PROBE: usize = 8;
/// Tokens are tracked in thousandths so refill is integer maths.
const MILLI: u64 = 1000;

#[repr(C)]
struct Bucket {
    lock: AtomicU32,
    key_hash: u64,     // 0 = free
    tokens_milli: u64, // available tokens × 1000
    last_ms: u64,      // unix millis of the last refill
}

static BUCKETS: AtomicPtr<Bucket> = AtomicPtr::new(ptr::null_mut());
static SLOTS: AtomicUsize = AtomicUsize::new(0);

struct Slot(*mut Bucket);
impl Slot {
    fn lock(b: *mut Bucket) -> Slot {
        crate::shmlock::acquire(unsafe { &(*b).lock });
        Slot(b)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        crate::shmlock::release(unsafe { &(*self.0).lock });
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hash(key: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    let v = h.finish();
    if v == 0 {
        1
    } else {
        v
    }
}

/// Map the bucket table. Call in the master **before** forking.
pub fn init(slots: usize) {
    if !BUCKETS.load(Ordering::Relaxed).is_null() || slots == 0 {
        return;
    }
    let slots = slots.max(64);
    let bytes = slots * std::mem::size_of::<Bucket>();
    // SAFETY: anonymous shared mapping; zeroed pages are a valid initial state.
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        tracing::warn!("rate limiting: mmap failed; disabled");
        return;
    }
    SLOTS.store(slots, Ordering::Relaxed);
    BUCKETS.store(p as *mut Bucket, Ordering::Release);
    tracing::info!(slots, kib = bytes / 1024, "rate limit buckets mapped");
}

pub fn enabled() -> bool {
    !BUCKETS.load(Ordering::Acquire).is_null()
}

/// What to do with a request.
pub struct Verdict {
    pub allowed: bool,
    /// Tokens left after this request (0 when refused).
    pub remaining: u64,
    /// Seconds until one token is available again (only meaningful when refused).
    pub retry_after: u64,
}

/// Take one token from `key`'s bucket.
///
/// `limit` tokens refill over `window` seconds; `burst` allows that many extra
/// tokens to accumulate for bursty clients.
pub fn check(key: &[u8], limit: u64, window: u64, burst: u64) -> Verdict {
    let allow_all = Verdict {
        allowed: true,
        remaining: limit,
        retry_after: 0,
    };
    let p = BUCKETS.load(Ordering::Acquire);
    if p.is_null() || limit == 0 || window == 0 {
        return allow_all;
    }
    let slots = SLOTS.load(Ordering::Relaxed);
    let capacity = (limit + burst).saturating_mul(MILLI);
    let h = hash(key);
    let now = now_ms();

    // Find this key's bucket, remembering the coldest slot in case we must evict.
    let mut victim: Option<*mut Bucket> = None;
    let mut victim_age = u64::MAX;
    for i in 0..PROBE {
        let b = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let g = Slot::lock(b);
        // SAFETY: fields are plain integers, read under the slot lock.
        unsafe {
            let kh = ptr::read(ptr::addr_of!((*b).key_hash));
            if kh == h {
                return consume(b, now, limit, window, capacity);
            }
            if kh == 0 {
                // Free slot: claim it with a full bucket.
                ptr::write(ptr::addr_of_mut!((*b).key_hash), h);
                ptr::write(ptr::addr_of_mut!((*b).tokens_milli), capacity);
                ptr::write(ptr::addr_of_mut!((*b).last_ms), now);
                return consume(b, now, limit, window, capacity);
            }
            let last = ptr::read(ptr::addr_of!((*b).last_ms));
            if last < victim_age {
                victim_age = last;
                victim = Some(b);
            }
        }
        drop(g);
    }

    // Probe window full: reuse the least recently used bucket. This is deliberately
    // **fail-open** — under table pressure a client may get a fresh allowance rather
    // than being wrongly refused. Refusing legitimate traffic to save memory is the
    // worse failure for a web server.
    let b = victim.unwrap_or_else(|| unsafe { p.add((h as usize) % slots) });
    let _g = Slot::lock(b);
    // SAFETY: as above.
    unsafe {
        ptr::write(ptr::addr_of_mut!((*b).key_hash), h);
        ptr::write(ptr::addr_of_mut!((*b).tokens_milli), capacity);
        ptr::write(ptr::addr_of_mut!((*b).last_ms), now);
    }
    consume(b, now, limit, window, capacity)
}

/// Refill by elapsed time, then take one token. Caller holds the slot lock.
fn consume(b: *mut Bucket, now: u64, limit: u64, window: u64, capacity: u64) -> Verdict {
    // SAFETY: plain integer fields, slot lock held by the caller.
    unsafe {
        let last = ptr::read(ptr::addr_of!((*b).last_ms));
        let mut tokens = ptr::read(ptr::addr_of!((*b).tokens_milli));
        // `limit` tokens per `window` seconds ⇒ limit/window milli-tokens per ms.
        let elapsed = now.saturating_sub(last);
        if elapsed > 0 {
            let refill = elapsed.saturating_mul(limit) / window;
            // `last` used to jump to `now` whether or not the division produced
            // anything, so the sub-token remainder was thrown away on every call. A
            // client arriving faster than one token's worth of milliseconds (possible
            // whenever `limit < window`, e.g. 10 per 60 s ⇒ 6 s per token) refilled
            // zero every time and stayed blocked indefinitely, however long it had
            // actually been waiting. Advancing `last` only by the time the refill
            // accounts for carries the remainder into the next call instead.
            if refill == 0 {
                // Nothing to credit yet — leave `last` alone so the elapsed time
                // keeps accumulating.
            } else if tokens.saturating_add(refill) >= capacity {
                // Full: there is no remainder worth carrying, and letting `last` lag
                // behind a saturated bucket would hand out a burst later.
                tokens = capacity;
                ptr::write(ptr::addr_of_mut!((*b).last_ms), now);
            } else {
                tokens += refill;
                let credited_ms = refill.saturating_mul(window) / limit.max(1);
                ptr::write(
                    ptr::addr_of_mut!((*b).last_ms),
                    last.saturating_add(credited_ms).min(now),
                );
            }
        }
        if tokens >= MILLI {
            tokens -= MILLI;
            ptr::write(ptr::addr_of_mut!((*b).tokens_milli), tokens);
            Verdict {
                allowed: true,
                remaining: tokens / MILLI,
                retry_after: 0,
            }
        } else {
            ptr::write(ptr::addr_of_mut!((*b).tokens_milli), tokens);
            // Counted in the shared metrics region, not a process-local static:
            // `/metrics` is served by the master, which never handles requests.
            if let Some(m) = crate::metrics::Metrics::get() {
                m.ratelimit_blocked.fetch_add(1, Ordering::Relaxed);
            }
            // Milliseconds until one whole token exists again.
            let missing = MILLI - tokens;
            let ms = missing.saturating_mul(window).div_ceil(limit.max(1));
            Verdict {
                allowed: false,
                remaining: 0,
                retry_after: ms.div_ceil(1000).max(1),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These share one process-wide region, so run them one at a time.
    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn burst_is_allowed_then_refused() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(256);
        assert!(enabled());

        // 5 requests per 60s, no burst: the 6th is refused.
        let key = b"t1:1.2.3.4";
        for i in 0..5 {
            let v = check(key, 5, 60, 0);
            assert!(v.allowed, "request {i} should pass");
        }
        let v = check(key, 5, 60, 0);
        assert!(!v.allowed);
        assert_eq!(v.remaining, 0);
        // 5 per 60s ⇒ one token every 12s.
        assert!(
            v.retry_after >= 1 && v.retry_after <= 12,
            "got {}",
            v.retry_after
        );
    }

    #[test]
    fn separate_keys_have_separate_budgets() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(256);
        for _ in 0..3 {
            assert!(check(b"t2:a", 3, 60, 0).allowed);
        }
        assert!(!check(b"t2:a", 3, 60, 0).allowed);
        // A different identity is untouched by the first one's exhaustion.
        assert!(check(b"t2:b", 3, 60, 0).allowed);
    }

    #[test]
    fn burst_adds_headroom() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(256);
        // limit 2 + burst 3 ⇒ 5 immediate requests.
        for i in 0..5 {
            assert!(check(b"t3:x", 2, 60, 3).allowed, "request {i}");
        }
        assert!(!check(b"t3:x", 2, 60, 3).allowed);
    }

    /// The refill used to move `last_ms` to `now` even when the integer division
    /// produced nothing, discarding the sub-token remainder on every call. A client
    /// arriving faster than one token's worth of milliseconds — which `limit < window`
    /// makes ordinary, 10 per 60 s is 6 s per token — then refilled zero forever.
    ///
    /// `consume` takes `now`, so this drives it directly rather than sleeping for the
    /// six seconds a real-time version of this test would need.
    #[test]
    fn the_refill_remainder_is_carried_not_discarded() {
        let (limit, window) = (10u64, 60u64); // 6 s per token
        let capacity = limit * MILLI;
        let mut b = Bucket {
            lock: AtomicU32::new(0),
            key_hash: 1,
            tokens_milli: 0,
            last_ms: 0,
        };

        // Hammer every millisecond. Each call on its own refills nothing
        // (1 × 10 / 60 == 0), so the only way a token ever appears is if the
        // remainder survives between calls.
        let mut allowed_at = None;
        for ms in 1..=7000u64 {
            let v = consume(&mut b as *mut Bucket, ms, limit, window, capacity);
            if v.allowed {
                allowed_at = Some(ms);
                break;
            }
        }
        let ms = allowed_at.expect("a token must eventually accrue under 1 ms polling");
        assert!(
            (5900..=6100).contains(&ms),
            "one token per 6 s; got one at {ms} ms"
        );
    }

    #[test]
    fn tokens_refill_over_time() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(256);
        // 100 per second: exhaust, then a short sleep must restore some budget.
        for _ in 0..100 {
            assert!(check(b"t4:x", 100, 1, 0).allowed);
        }
        assert!(!check(b"t4:x", 100, 1, 0).allowed);
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(
            check(b"t4:x", 100, 1, 0).allowed,
            "≥1 token should have refilled after 120ms"
        );
    }

    #[test]
    fn disabled_limit_allows_everything() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(256);
        for _ in 0..50 {
            assert!(check(b"t5:x", 0, 60, 0).allowed, "limit 0 = no limiting");
        }
    }
}
