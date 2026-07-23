//! Shared-memory cache exposed to PHP.
//!
//! A fixed-slot hash table living in an anonymous **shared** mmap (created by the
//! master before fork, so every worker sees the same physical table — no IPC).
//! It backs `askr_cache_*` from PHP: cache, atomic counters (rate limiting),
//! atomic `add` (locks), and — with the large region — Laravel sessions and
//! rendered fragments, all in the same binary, with no Redis for a single box.
//!
//! **Size classes.** Two regions: a *small* one (4 KB values, many slots — for
//! counters, locks, small entries) and an optional *large* one (64 KB values,
//! fewer slots — for sessions, serialized collections, cached fragments). `set`
//! routes by value size and clears the key from the other region; `get`/`delete`
//! check both. This keeps big values working without wasting 64 KB per counter.
//!
//! Robustness (per region): inline fixed-size slots (no allocator), a per-slot
//! spinlock that can be stolen if a holder dies, and length-clamped reads so a
//! torn write can never cause an out-of-bounds read.

use std::hash::{Hash, Hasher};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const KEY_MAX: usize = 250;
const VAL_SMALL: usize = 4096;
const VAL_LARGE: usize = 64 * 1024;
const PROBE: usize = 16;

#[repr(C)]
struct Entry<const V: usize> {
    lock: AtomicU32, // 0 = free, else holder pid (see shmlock)
    state: u32,      // 0 = empty, 1 = occupied
    hash: u64,
    expires_at: u64, // unix secs; 0 = never
    written_at: u64, // unix millis at last write (oldest-first eviction)
    key_len: u32,
    val_len: u32,
    key: [u8; KEY_MAX],
    val: [u8; V],
}

/// One cache region of `Entry<V>` slots in shared memory.
struct Region<const V: usize> {
    ptr: AtomicPtr<Entry<V>>,
    slots: AtomicUsize,
}

static SMALL: Region<VAL_SMALL> = Region {
    ptr: AtomicPtr::new(ptr::null_mut()),
    slots: AtomicUsize::new(0),
};
static LARGE: Region<VAL_LARGE> = Region {
    ptr: AtomicPtr::new(ptr::null_mut()),
    slots: AtomicUsize::new(0),
};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hash_key(key: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

fn note_eviction() {
    if let Some(m) = crate::metrics::Metrics::get() {
        m.cache_evictions.fetch_add(1, Ordering::Relaxed);
    }
}

/// A value was too large for any cache region and was dropped. Counted so an
/// operator can see (via `/metrics`) that big sessions/fragments aren't caching,
/// instead of it failing silently.
fn note_oversize(len: usize) {
    if let Some(m) = crate::metrics::Metrics::get() {
        m.cache_oversize.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(
        bytes = len,
        limit = VAL_LARGE,
        "cache: value too large, not cached"
    );
}

/// RAII spinlock guard over one slot.
struct Slot<const V: usize>(*mut Entry<V>);
impl<const V: usize> Slot<V> {
    fn lock(e: *mut Entry<V>) -> Slot<V> {
        // SAFETY: `lock` is an AtomicU32 in the shared mapping.
        crate::shmlock::acquire(unsafe { &(*e).lock });
        Slot(e)
    }
}
impl<const V: usize> Drop for Slot<V> {
    fn drop(&mut self) {
        crate::shmlock::release(unsafe { &(*self.0).lock });
    }
}

unsafe fn r_u32(p: *const u32) -> u32 {
    ptr::read(p)
}
unsafe fn r_u64(p: *const u64) -> u64 {
    ptr::read(p)
}

impl<const V: usize> Region<V> {
    fn map(&self, slots: usize) {
        if !self.ptr.load(Ordering::Relaxed).is_null() {
            return;
        }
        let slots = slots.max(16);
        let size = slots * std::mem::size_of::<Entry<V>>();
        // SAFETY: anonymous shared mapping; zeroed pages are a valid table.
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
            tracing::warn!(val = V, "cache: mmap failed; region disabled");
            return;
        }
        // Publish: write slots first, then release-store the pointer so any reader
        // that acquire-loads a non-null pointer also sees the matching slot count.
        // (The region is mapped once in the master before forking; the pointer is
        // read-only thereafter, so Acquire/Release suffices — no need for SeqCst's
        // stronger barrier on the per-op read path, which is costlier on ARM.)
        self.slots.store(slots, Ordering::Relaxed);
        self.ptr.store(p as *mut Entry<V>, Ordering::Release);
        tracing::info!(
            slots,
            mib = size / 1024 / 1024,
            val_max = V,
            "cache region mapped"
        );
    }

    fn base(&self) -> Option<(*mut Entry<V>, usize)> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            Some((p, self.slots.load(Ordering::Relaxed)))
        }
    }

    fn enabled(&self) -> bool {
        !self.ptr.load(Ordering::Acquire).is_null()
    }

    /// Does the occupied slot hold `key`/`hash`?
    unsafe fn matches(e: *mut Entry<V>, key: &[u8], h: u64) -> bool {
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

    unsafe fn read_val(e: *mut Entry<V>) -> Vec<u8> {
        let vlen = (r_u32(ptr::addr_of!((*e).val_len)) as usize).min(V);
        let vp = ptr::addr_of!((*e).val) as *const u8;
        std::slice::from_raw_parts(vp, vlen).to_vec()
    }

    unsafe fn write(e: *mut Entry<V>, key: &[u8], val: &[u8], h: u64, expires: u64) {
        ptr::write(ptr::addr_of_mut!((*e).state), 1);
        ptr::write(ptr::addr_of_mut!((*e).hash), h);
        ptr::write(ptr::addr_of_mut!((*e).expires_at), expires);
        ptr::write(ptr::addr_of_mut!((*e).written_at), now_ms());
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

    unsafe fn expired(e: *mut Entry<V>, now: u64) -> bool {
        let exp = r_u64(ptr::addr_of!((*e).expires_at));
        exp != 0 && exp < now
    }

    fn get(&self, key: &[u8], h: u64) -> Option<Vec<u8>> {
        let (p, slots) = self.base()?;
        let now = now_secs();
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _g = Slot::lock(e);
            unsafe {
                // Only an EMPTY (0) slot ends the probe chain. A TOMBSTONE (2) is
                // skipped (matches() requires state==1), so a live key placed past a
                // deleted colliding key is still found. (Fixes the false-miss where
                // delete used to punch a 0 hole mid-chain.)
                if r_u32(ptr::addr_of!((*e).state)) == 0 {
                    return None;
                }
                if Self::matches(e, key, h) {
                    if Self::expired(e, now) {
                        ptr::write(ptr::addr_of_mut!((*e).state), 2); // tombstone
                        return None;
                    }
                    return Some(Self::read_val(e));
                }
            }
        }
        None
    }

    fn set(&self, key: &[u8], val: &[u8], h: u64, ttl: u64) -> bool {
        let Some((p, slots)) = self.base() else {
            return false;
        };
        if val.len() > V {
            return false;
        }
        let now = now_secs();
        let expires = if ttl > 0 { now + ttl } else { 0 };
        let mut reuse: Option<usize> = None; // first EMPTY or TOMBSTONE slot
        let mut victim = (h as usize) % slots;
        let mut oldest = u64::MAX;
        let mut expired_victim: Option<usize> = None;
        for i in 0..PROBE {
            let idx = (h as usize).wrapping_add(i) % slots;
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe {
                let state = r_u32(ptr::addr_of!((*e).state));
                // Live/expired match ⇒ update in place, atomically (lock held).
                if state == 1 && Self::matches(e, key, h) {
                    Self::write(e, key, val, h, expires);
                    return true;
                }
                if (state == 0 || state == 2) && reuse.is_none() {
                    reuse = Some(idx);
                }
                if state == 0 {
                    break; // chain end: key is absent, write at `reuse` (≤ this idx)
                }
                if state == 1 {
                    if Self::expired(e, now) && expired_victim.is_none() {
                        expired_victim = Some(idx);
                    }
                    let wa = r_u64(ptr::addr_of!((*e).written_at));
                    if wa < oldest {
                        oldest = wa;
                        victim = idx;
                    }
                }
            }
        }
        // Prefer a free slot (empty/tombstone), then an expired entry, then evict
        // the oldest live entry. Only the last case is a real eviction.
        let evicting = reuse.is_none() && expired_victim.is_none();
        let target = reuse.or(expired_victim).unwrap_or(victim);
        let e = unsafe { p.add(target) };
        let _g = Slot::lock(e);
        // Re-validate under the lock: if a racing writer already put *our* key
        // somewhere we'd now clobber, last-writer-wins is still correct; we just
        // write our value. (Closes the evict/target race window.)
        unsafe { Self::write(e, key, val, h, expires) };
        if evicting {
            note_eviction();
        }
        true
    }

    /// Atomic set-if-absent (for locks). Returns true if the key was written.
    fn add(&self, key: &[u8], val: &[u8], h: u64, ttl: u64) -> bool {
        let Some((p, slots)) = self.base() else {
            return false;
        };
        if val.len() > V {
            return false;
        }
        let now = now_secs();
        let expires = if ttl > 0 { now + ttl } else { 0 };
        let mut reuse: Option<usize> = None;
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _g = Slot::lock(e);
            unsafe {
                let state = r_u32(ptr::addr_of!((*e).state));
                if state == 1 && Self::matches(e, key, h) {
                    if !Self::expired(e, now) {
                        return false; // already present and live
                    }
                    Self::write(e, key, val, h, expires); // expired ⇒ acquire in place
                    return true;
                }
                if (state == 0 || state == 2) && reuse.is_none() {
                    reuse = Some((h as usize).wrapping_add(i) % slots);
                }
                if state == 0 {
                    break; // chain end: no live holder ahead, safe to insert
                }
            }
        }
        // Insert at the first free slot, but re-check under the lock so two racing
        // `add`s for the same key can't both succeed (atomic-lock correctness).
        if let Some(idx) = reuse {
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe {
                let state = r_u32(ptr::addr_of!((*e).state));
                if state == 1 && Self::matches(e, key, h) && !Self::expired(e, now) {
                    return false; // lost the race to another acquirer
                }
                if state == 0 || state == 2 || Self::matches(e, key, h) || Self::expired(e, now) {
                    Self::write(e, key, val, h, expires);
                    return true;
                }
            }
        }
        false // probe window full of other live keys
    }

    fn delete(&self, key: &[u8], h: u64) -> bool {
        let Some((p, slots)) = self.base() else {
            return false;
        };
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _g = Slot::lock(e);
            unsafe {
                if r_u32(ptr::addr_of!((*e).state)) == 0 {
                    return false;
                }
                if Self::matches(e, key, h) {
                    // Tombstone (2), not empty (0): preserves the probe chain so a
                    // colliding key stored later in the chain stays reachable.
                    ptr::write(ptr::addr_of_mut!((*e).state), 2);
                    return true;
                }
            }
        }
        false
    }

    /// Refresh the TTL of an existing, live key without touching its value.
    fn touch(&self, key: &[u8], h: u64, ttl: u64) -> bool {
        let Some((p, slots)) = self.base() else {
            return false;
        };
        let now = now_secs();
        let expires = if ttl > 0 { now + ttl } else { 0 };
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _g = Slot::lock(e);
            unsafe {
                if r_u32(ptr::addr_of!((*e).state)) == 0 {
                    return false;
                }
                if Self::matches(e, key, h) {
                    if Self::expired(e, now) {
                        ptr::write(ptr::addr_of_mut!((*e).state), 2); // tombstone
                        return false;
                    }
                    ptr::write(ptr::addr_of_mut!((*e).expires_at), expires);
                    return true;
                }
            }
        }
        false
    }

    fn increment(&self, key: &[u8], h: u64, delta: i64, ttl: u64) -> i64 {
        let Some((p, slots)) = self.base() else {
            return 0;
        };
        let now = now_secs();
        let expires = if ttl > 0 { now + ttl } else { 0 };
        let mut reuse: Option<usize> = None;
        for i in 0..PROBE {
            let idx = (h as usize).wrapping_add(i) % slots;
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe {
                let state = r_u32(ptr::addr_of!((*e).state));
                // Found the counter (skipping tombstones/other keys): bump in place.
                if state == 1 && Self::matches(e, key, h) {
                    let live = !Self::expired(e, now);
                    let cur: i64 = if live {
                        std::str::from_utf8(&Self::read_val(e))
                            .ok()
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    let next = cur + delta;
                    let exp = if live {
                        r_u64(ptr::addr_of!((*e).expires_at))
                    } else {
                        expires
                    };
                    Self::write(e, key, next.to_string().as_bytes(), h, exp);
                    return next;
                }
                if (state == 0 || state == 2) && reuse.is_none() {
                    reuse = Some(idx);
                }
                if state == 0 {
                    break; // chain end: counter is absent, create it at `reuse`
                }
            }
        }
        // Create a fresh counter at the first free slot, re-checking under the lock
        // so a racing increment doesn't get lost.
        if let Some(idx) = reuse {
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe {
                let state = r_u32(ptr::addr_of!((*e).state));
                if state == 1 && Self::matches(e, key, h) && !Self::expired(e, now) {
                    let cur: i64 = std::str::from_utf8(&Self::read_val(e))
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let next = cur + delta;
                    let exp = r_u64(ptr::addr_of!((*e).expires_at));
                    Self::write(e, key, next.to_string().as_bytes(), h, exp);
                    return next;
                }
                Self::write(e, key, delta.to_string().as_bytes(), h, expires);
                return delta;
            }
        }
        delta
    }

    fn flush(&self) {
        let Some((p, slots)) = self.base() else {
            return;
        };
        for idx in 0..slots {
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe { ptr::write(ptr::addr_of_mut!((*e).state), 0) };
        }
    }
}

// --- public API (routes across the two size classes) ----------------------

/// Map the cache regions. Call in the master before forking. `large_slots` = 0
/// disables the large region (only small values are cacheable then).
pub fn init(slots: usize, large_slots: usize) {
    SMALL.map(slots);
    if large_slots > 0 {
        LARGE.map(large_slots);
    }
}

pub fn enabled() -> bool {
    SMALL.enabled() || LARGE.enabled()
}

/// Get a value (checks small then large). None on miss/expired/disabled.
pub fn get(key: &[u8]) -> Option<Vec<u8>> {
    if key.len() > KEY_MAX {
        return None;
    }
    let h = hash_key(key);
    SMALL.get(key, h).or_else(|| LARGE.get(key, h))
}

/// Set a value, routing by size. Clears the key from the other region so a
/// resize (small↔large) can't leave a stale copy. False if too large / disabled.
pub fn set(key: &[u8], val: &[u8], ttl: u64) -> bool {
    if key.len() > KEY_MAX {
        return false;
    }
    let h = hash_key(key);
    if val.len() <= VAL_SMALL {
        LARGE.delete(key, h);
        SMALL.set(key, val, h, ttl)
    } else if val.len() <= VAL_LARGE {
        SMALL.delete(key, h);
        LARGE.set(key, val, h, ttl)
    } else {
        note_oversize(val.len()); // exceeds the largest slot — dropped, not silent
        false
    }
}

/// Atomic set-if-absent (backs `Cache::lock()`). Values are small (owner tokens).
pub fn add(key: &[u8], val: &[u8], ttl: u64) -> bool {
    if key.len() > KEY_MAX || val.len() > VAL_SMALL {
        return false;
    }
    let h = hash_key(key);
    // A key present in either region blocks the add.
    if LARGE.get(key, h).is_some() {
        return false;
    }
    SMALL.add(key, val, h, ttl)
}

/// Delete a key from both regions. True if it existed anywhere.
pub fn delete(key: &[u8]) -> bool {
    let h = hash_key(key);
    let s = SMALL.delete(key, h);
    let l = LARGE.delete(key, h);
    s || l
}

/// Atomically refresh a key's TTL without reading and rewriting its value —
/// closes the get-then-set race a naive cache `touch()` would have (a concurrent
/// writer's value can't be clobbered because the value is never rewritten).
pub fn touch(key: &[u8], ttl: u64) -> bool {
    if key.len() > KEY_MAX {
        return false;
    }
    let h = hash_key(key);
    SMALL.touch(key, h, ttl) || LARGE.touch(key, h, ttl)
}

/// Atomically add `delta` to a numeric key (counters / rate limiting).
pub fn increment(key: &[u8], delta: i64, ttl: u64) -> i64 {
    if key.len() > KEY_MAX {
        return 0;
    }
    let h = hash_key(key);
    SMALL.increment(key, h, delta, ttl)
}

/// Empty both regions.
pub fn flush() {
    SMALL.flush();
    LARGE.flush();
}

// --- PHP bridge -----------------------------------------------------------

use std::ffi::{c_char, c_int, c_long};

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

extern "C" fn c_add(
    key: *const c_char,
    klen: usize,
    val: *const c_char,
    vlen: usize,
    ttl: c_long,
) -> c_int {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    let val = unsafe { std::slice::from_raw_parts(val as *const u8, vlen) };
    add(key, val, ttl.max(0) as u64) as c_int
}

extern "C" fn c_del(key: *const c_char, klen: usize) -> c_int {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    delete(key) as c_int
}

extern "C" fn c_incr(key: *const c_char, klen: usize, delta: c_long, ttl: c_long) -> c_long {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    increment(key, delta, ttl.max(0) as u64)
}

extern "C" fn c_touch(key: *const c_char, klen: usize, ttl: c_long) -> c_int {
    let key = unsafe { std::slice::from_raw_parts(key as *const u8, klen) };
    touch(key, ttl.max(0) as u64) as c_int
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
/// when either the kv cache or the response cache is enabled.
pub fn register_bridge() {
    // L2 (durable, replicated) cache backend takes over when ASKR_CACHE_DB is set
    // and this build includes the `sql-backend` feature (elyra-10).
    #[cfg(feature = "sql-backend")]
    if crate::cache_sql::enabled() {
        crate::cache_sql::register_bridge();
        return;
    }
    if !enabled() && !crate::rcache::enabled() {
        return;
    }
    // SAFETY: one-time registration; the trampolines are 'static fns.
    unsafe {
        askr_php::cache_bridge::askr_php_set_cache_bridge(
            c_get,
            c_set,
            c_add,
            c_del,
            c_incr,
            c_flush,
            c_forget_tag,
            c_touch,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_classes_and_add() {
        init(256, 64);
        assert!(enabled());

        // small value → small region
        assert!(set(b"name", b"askr", 0));
        assert_eq!(get(b"name").as_deref(), Some(&b"askr"[..]));

        // large value (> 4 KB) → large region, and readable
        let big = vec![b'x'; 20_000];
        assert!(set(b"session:abc", &big, 60));
        assert_eq!(get(b"session:abc").as_deref(), Some(&big[..]));

        // resizing a key across regions leaves no stale copy
        assert!(set(b"session:abc", b"small now", 60));
        assert_eq!(get(b"session:abc").as_deref(), Some(&b"small now"[..]));

        // atomic add: first wins, second fails while it lives
        assert!(add(b"lock:x", b"owner1", 60));
        assert!(!add(b"lock:x", b"owner2", 60));
        assert!(delete(b"lock:x"));
        assert!(add(b"lock:x", b"owner3", 60));

        // counters
        assert_eq!(increment(b"hits", 1, 60), 1);
        assert_eq!(increment(b"hits", 5, 60), 6);

        // atomic touch: refreshes TTL of an existing key, leaves value intact;
        // false for a missing key.
        assert!(set(b"tk", b"tv", 60));
        assert!(touch(b"tk", 120));
        assert_eq!(get(b"tk").as_deref(), Some(&b"tv"[..]));
        assert!(!touch(b"missing", 60));

        // too large for any region
        assert!(!set(b"huge", &vec![0u8; VAL_LARGE + 1], 0));

        flush();
        assert_eq!(get(b"name"), None);
        assert_eq!(get(b"session:abc"), None);

        // Regression (same test to avoid racing the shared global cache with a
        // parallel test): deleting a key must not punch a hole that hides a
        // colliding key stored later in the same probe chain (tombstone deletion).
        use std::collections::HashMap;
        let slots = 256usize;
        // Find two small-value keys that share a starting slot (collide).
        let mut buckets: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
        let (mut k1, mut k2) = (Vec::new(), Vec::new());
        for n in 0..50_000u32 {
            let k = format!("collide-{n}").into_bytes();
            let s = hash_key(&k) as usize % slots;
            let v = buckets.entry(s).or_default();
            v.push(k);
            if v.len() == 2 {
                k1 = v[0].clone();
                k2 = v[1].clone();
                break;
            }
        }
        assert!(!k1.is_empty() && !k2.is_empty(), "no colliding pair found");

        // A occupies the start slot, B lands later in the same chain.
        assert!(set(&k1, b"A", 60));
        assert!(set(&k2, b"B", 60));
        assert_eq!(get(&k1).as_deref(), Some(&b"A"[..]));
        assert_eq!(get(&k2).as_deref(), Some(&b"B"[..]));

        // Delete A. Before the tombstone fix this created a `state==0` hole that
        // ended B's probe chain early ⇒ a false miss.
        assert!(delete(&k1));
        assert_eq!(get(&k1), None);
        assert_eq!(
            get(&k2).as_deref(),
            Some(&b"B"[..]),
            "colliding key hidden by a deleted neighbour"
        );

        // Atomic-lock correctness: with A tombstoned and B still live in the chain,
        // add(B) must fail (B is held) — the old hole let it falsely re-acquire.
        assert!(!add(&k2, b"B2", 60), "add falsely re-acquired a live lock");

        // A tombstone must be reusable, so add(A) succeeds again.
        assert!(add(&k1, b"A2", 60));
        assert_eq!(get(&k1).as_deref(), Some(&b"A2"[..]));
        // …and B is still intact after reuse.
        assert_eq!(get(&k2).as_deref(), Some(&b"B"[..]));
        flush();
    }
}
