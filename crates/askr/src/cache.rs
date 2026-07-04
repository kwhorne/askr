//! Shared-memory cache exposed to PHP.
//!
//! A fixed-slot hash table living in an anonymous **shared** mmap (created by the
//! master before fork, so every worker sees the same physical table — no IPC).
//! It backs `askr_cache_*` from PHP: cache, atomic counters (rate limiting) and
//! locks in the same binary, with no Redis for small/mid deployments.
//!
//! Design choices for robustness:
//! - **Inline, fixed-size** key/value slots — no shared-memory allocator.
//! - **Per-slot spinlock** (an atomic in shared memory) with a bounded spin then
//!   steal, so a crashed holder can't deadlock the whole table forever.
//! - **Length clamping** on every read, so a torn write can never cause an
//!   out-of-bounds read — memory safety holds regardless of races (a race can
//!   only yield a stale/garbage *value*, which a cache tolerates).

use std::hash::{Hash, Hasher};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const KEY_MAX: usize = 250;
const VAL_MAX: usize = 4096;
const PROBE: usize = 16;

#[repr(C)]
struct Entry {
    lock: AtomicU32, // 0 = free, 1 = held
    state: u32,      // 0 = empty, 1 = occupied
    hash: u64,
    expires_at: u64, // unix secs; 0 = never
    key_len: u32,
    val_len: u32,
    key: [u8; KEY_MAX],
    val: [u8; VAL_MAX],
}

static CACHE_PTR: AtomicPtr<Entry> = AtomicPtr::new(ptr::null_mut());
static CACHE_SLOTS: AtomicUsize = AtomicUsize::new(0);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hash_key(key: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// Map the shared cache table with `slots` entries. Call in the master before
/// forking. Idempotent-ish: a second call is ignored.
pub fn init(slots: usize) {
    if !CACHE_PTR.load(Ordering::SeqCst).is_null() {
        return;
    }
    let slots = slots.max(64);
    let size = slots * std::mem::size_of::<Entry>();
    // SAFETY: anonymous shared mapping; zeroed pages are a valid table (all
    // slots empty, all locks free).
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
        tracing::warn!("cache: mmap failed; cache disabled");
        return;
    }
    CACHE_SLOTS.store(slots, Ordering::SeqCst);
    CACHE_PTR.store(p as *mut Entry, Ordering::SeqCst);
    tracing::info!(slots, mib = size / 1024 / 1024, "shared cache mapped");
}

pub fn enabled() -> bool {
    !CACHE_PTR.load(Ordering::SeqCst).is_null()
}

fn base() -> Option<(*mut Entry, usize)> {
    let p = CACHE_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        None
    } else {
        Some((p, CACHE_SLOTS.load(Ordering::SeqCst)))
    }
}

/// RAII spinlock guard over one slot.
struct Slot(*mut Entry);

impl Slot {
    fn lock(e: *mut Entry) -> Slot {
        // SAFETY: `lock` is an AtomicU32 in the shared mapping.
        let lock = unsafe { &(*e).lock };
        for _ in 0..50_000 {
            if lock
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Slot(e);
            }
            std::hint::spin_loop();
        }
        // Holder likely died mid-op; steal the lock to avoid a permanent stall.
        lock.store(1, Ordering::SeqCst);
        Slot(e)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        unsafe { (*self.0).lock.store(0, Ordering::Release) };
    }
}

// Raw field accessors (used only while holding the slot lock).
unsafe fn r_u32(p: *const u32) -> u32 {
    ptr::read(p)
}
unsafe fn r_u64(p: *const u64) -> u64 {
    ptr::read(p)
}

/// Does the occupied slot `e` hold `key`/`hash`?
unsafe fn matches(e: *mut Entry, key: &[u8], h: u64) -> bool {
    if r_u32(ptr::addr_of!((*e).state)) != 1 || r_u64(ptr::addr_of!((*e).hash)) != h {
        return false;
    }
    let klen = r_u32(ptr::addr_of!((*e).key_len)) as usize;
    if klen != key.len() || klen > KEY_MAX {
        return false;
    }
    let kp = ptr::addr_of!((*e).key) as *const u8;
    std::slice::from_raw_parts(kp, klen) == key
}

unsafe fn read_val(e: *mut Entry) -> Vec<u8> {
    let vlen = (r_u32(ptr::addr_of!((*e).val_len)) as usize).min(VAL_MAX);
    let vp = ptr::addr_of!((*e).val) as *const u8;
    std::slice::from_raw_parts(vp, vlen).to_vec()
}

unsafe fn write_entry(e: *mut Entry, key: &[u8], val: &[u8], h: u64, expires: u64) {
    ptr::write(ptr::addr_of_mut!((*e).state), 1);
    ptr::write(ptr::addr_of_mut!((*e).hash), h);
    ptr::write(ptr::addr_of_mut!((*e).expires_at), expires);
    ptr::write(ptr::addr_of_mut!((*e).key_len), key.len() as u32);
    ptr::write(ptr::addr_of_mut!((*e).val_len), val.len() as u32);
    ptr::copy_nonoverlapping(
        key.as_ptr(),
        ptr::addr_of_mut!((*e).key) as *mut u8,
        key.len(),
    );
    ptr::copy_nonoverlapping(
        val.as_ptr(),
        ptr::addr_of_mut!((*e).val) as *mut u8,
        val.len(),
    );
}

unsafe fn expired(e: *mut Entry, now: u64) -> bool {
    let exp = r_u64(ptr::addr_of!((*e).expires_at));
    exp != 0 && exp < now
}

/// Get a value. Returns None on miss/expired/disabled.
pub fn get(key: &[u8]) -> Option<Vec<u8>> {
    let (p, slots) = base()?;
    if key.len() > KEY_MAX {
        return None;
    }
    let h = hash_key(key);
    let now = now_secs();
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        unsafe {
            let state = r_u32(ptr::addr_of!((*e).state));
            if state == 0 {
                return None; // empty slot ends the probe chain
            }
            if matches(e, key, h) {
                if expired(e, now) {
                    ptr::write(ptr::addr_of_mut!((*e).state), 0);
                    return None;
                }
                return Some(read_val(e));
            }
        }
    }
    None
}

/// Set a value with an optional TTL (seconds; 0 = never). Returns false if the
/// key/value is too large or the cache is disabled.
pub fn set(key: &[u8], val: &[u8], ttl: u64) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    if key.len() > KEY_MAX || val.len() > VAL_MAX {
        return false;
    }
    let h = hash_key(key);
    let expires = if ttl > 0 { now_secs() + ttl } else { 0 };
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        unsafe {
            let state = r_u32(ptr::addr_of!((*e).state));
            if state == 0 || matches(e, key, h) {
                write_entry(e, key, val, h, expires);
                return true;
            }
        }
    }
    // Probe window full: evict the primary slot.
    let e = unsafe { p.add((h as usize) % slots) };
    let _g = Slot::lock(e);
    unsafe { write_entry(e, key, val, h, expires) };
    true
}

/// Delete a key. Returns true if it existed.
pub fn delete(key: &[u8]) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    let h = hash_key(key);
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u32(ptr::addr_of!((*e).state)) == 0 {
                return false;
            }
            if matches(e, key, h) {
                ptr::write(ptr::addr_of_mut!((*e).state), 0);
                return true;
            }
        }
    }
    false
}

/// Atomically add `delta` to a numeric key (for rate limiting / counters).
/// Returns the new value.
pub fn increment(key: &[u8], delta: i64, ttl: u64) -> i64 {
    let Some((p, slots)) = base() else {
        return 0;
    };
    if key.len() > KEY_MAX {
        return 0;
    }
    let h = hash_key(key);
    let now = now_secs();
    let expires = if ttl > 0 { now + ttl } else { 0 };
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        unsafe {
            let state = r_u32(ptr::addr_of!((*e).state));
            let is_match = state == 1 && matches(e, key, h) && !expired(e, now);
            if state == 0 || is_match {
                let cur: i64 = if is_match {
                    std::str::from_utf8(&read_val(e))
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0)
                } else {
                    0
                };
                let next = cur + delta;
                let s = next.to_string();
                // preserve existing expiry on a matching key, else use ttl
                let exp = if is_match {
                    r_u64(ptr::addr_of!((*e).expires_at))
                } else {
                    expires
                };
                write_entry(e, key, s.as_bytes(), h, exp);
                return next;
            }
        }
    }
    delta
}

/// Empty the whole table.
pub fn flush() {
    let Some((p, slots)) = base() else {
        return;
    };
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe { ptr::write(ptr::addr_of_mut!((*e).state), 0) };
    }
}

// --- PHP bridge -----------------------------------------------------------

use std::ffi::{c_char, c_int, c_long};

/// # Safety: called by the shim with a valid key pointer/length.
extern "C" fn c_get(
    key: *const c_char,
    klen: usize,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> c_int {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    match get(key) {
        Some(v) => {
            let p = unsafe { libc::malloc(v.len().max(1)) } as *mut u8;
            if p.is_null() {
                return 0;
            }
            unsafe {
                ptr::copy_nonoverlapping(v.as_ptr(), p, v.len());
                *out = p as *mut c_char;
                *out_len = v.len();
            }
            1
        }
        None => 0,
    }
}

extern "C" fn c_set(
    key: *const c_char,
    klen: usize,
    val: *const c_char,
    vlen: usize,
    ttl: c_long,
) -> c_int {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    let val = unsafe { std::slice::from_raw_parts(val as *const u8, vlen) };
    set(key, val, ttl.max(0) as u64) as c_int
}

extern "C" fn c_del(key: *const c_char, klen: usize) -> c_int {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    delete(key) as c_int
}

extern "C" fn c_incr(key: *const c_char, klen: usize, delta: c_long, ttl: c_long) -> c_long {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    increment(key, delta, ttl.max(0) as u64)
}

extern "C" fn c_flush() {
    flush();
    crate::rcache::flush(); // askr_cache_flush() clears both caches
}

/// Invalidate every cached response carrying `tag` (response cache, #1).
extern "C" fn c_forget_tag(tag: *const c_char, tlen: usize) {
    let tag = unsafe { std::slice::from_raw_parts(tag as *const u8, tlen) };
    crate::rcache::forget_tag(tag);
}

/// Register the cache callbacks with the PHP shim for this process. Registered
/// when either the kv cache or the response cache is enabled (the response cache
/// needs `askr_cache_forget_tag`); disabled halves return misses.
pub fn register_bridge() {
    if !enabled() && !crate::rcache::enabled() {
        return;
    }
    // SAFETY: one-time registration; the trampolines are 'static fns.
    unsafe {
        askr_php::cache_bridge::askr_php_set_cache_bridge(
            c_get,
            c_set,
            c_del,
            c_incr,
            c_flush,
            c_forget_tag,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ops() {
        init(256);
        assert!(enabled());

        assert_eq!(get(b"missing"), None);

        assert!(set(b"name", b"askr", 0));
        assert_eq!(get(b"name").as_deref(), Some(&b"askr"[..]));

        assert!(delete(b"name"));
        assert_eq!(get(b"name"), None);

        // increment (rate-limit counter)
        assert_eq!(increment(b"hits", 1, 60), 1);
        assert_eq!(increment(b"hits", 1, 60), 2);
        assert_eq!(increment(b"hits", 5, 60), 7);

        // TTL: expires in the past-ish via 0 then manual check is hard here;
        // just confirm a fresh short TTL value is readable.
        assert!(set(b"tmp", b"x", 60));
        assert_eq!(get(b"tmp").as_deref(), Some(&b"x"[..]));

        // oversized value is rejected
        let big = vec![0u8; VAL_MAX + 1];
        assert!(!set(b"big", &big, 0));

        flush();
        assert_eq!(get(b"hits"), None);
    }
}
