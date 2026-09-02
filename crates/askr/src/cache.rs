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
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const KEY_MAX: usize = 250;
const VAL_SMALL: usize = 4096;
const VAL_LARGE: usize = 64 * 1024;
/// Linear-probe window. A `set` evicts only when all `PROBE` slots from the home
/// slot are occupied by live keys, so a larger window defers eviction to a higher
/// fill factor (fewer premature evictions when the table is, say, 70 % full) at the
/// cost of scanning/locking more slots per op on a collision. 32 balances the two
/// for typical slot counts (512–4096). Size the table generously and this rarely
/// bites either way.
const PROBE: usize = 32;

/// How many times `get` re-reads a slot without its lock before taking the lock.
///
/// A retry means a writer touched the slot mid-copy, which clears in nanoseconds, so a
/// handful is plenty. The point of the bound is the case that never clears: a writer
/// killed mid-update leaves the counter odd for good, and only a lock holder repairs
/// it — so the fallback is the recovery path, not a tuning knob.
const SEQ_ATTEMPTS: usize = 4;

#[repr(C)]
struct Entry<const V: usize> {
    lock: AtomicU32, // 0 = free, else holder pid (see shmlock)
    /// Seqlock counter: even = stable, odd = a writer is inside this slot.
    ///
    /// `get` reads a slot **without taking its lock**, and this is what makes that
    /// safe: it samples the counter, copies, and samples again — a change or an odd
    /// value means a writer overlapped the copy and the read is discarded. So every
    /// mutation of a slot has to pass through [`Writing`], [`Region::mark_state`] or
    /// [`Region::mark_expires`]. A bare `ptr::write` into a slot is the one way to
    /// reintroduce a torn read, and it will not look wrong at the call site.
    ///
    /// It cannot share the `lock` word: shmlock reclaims a slot from a dead holder by
    /// recognising the PID in there, so that field has to keep holding a PID.
    version: AtomicU64,
    state: u32, // 0 = empty, 1 = occupied
    hash: u64,
    expires_at: u64, // unix secs; 0 = never
    written_at: u64, // unix millis at last write (oldest-first eviction)
    key_len: u32,
    val_len: u32,
    key: [u8; KEY_MAX],
    val: [u8; V],
}

/// Outcome of reading one slot without holding its lock.
enum Peek {
    /// Empty slot: the probe chain ends here.
    ChainEnd,
    /// Occupied by another key, or a tombstone — keep probing.
    Other,
    /// Our key, but past its expiry.
    Expired,
    /// Our key, and the value was copied while nothing was writing it.
    Hit(Vec<u8>),
    /// A writer overlapped the read. Retry, or fall back to the lock.
    Torn,
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

// Volatile, because every one of these reads a field another process may be writing.
// Under a slot lock that cannot happen and the volatile costs nothing measurable; in
// `Region::peek`, which reads without the lock, it is what stops the compiler from
// assuming the memory is unchanging and hoisting a read outside the two counter
// samples that validate it.
/// Marks a slot as being written for as long as the guard lives, so a lock-free
/// reader retries instead of believing a half-updated slot. The caller must already
/// hold the slot lock — this orders the writes, it does not exclude other writers.
///
/// Forcing the counter odd rather than incrementing it is deliberate: a writer killed
/// mid-update leaves it odd forever, and an increment would then make it *even* during
/// the next write — a reader could sample a stable-looking counter in the middle of a
/// copy. Forcing odd on entry and even-and-greater on exit repairs that slot instead.
struct Writing<const V: usize>(*mut Entry<V>, u64);

impl<const V: usize> Writing<V> {
    unsafe fn begin(e: *mut Entry<V>) -> Writing<V> {
        let odd = (*e).version.load(Ordering::Relaxed) | 1;
        (*e).version.store(odd, Ordering::SeqCst);
        // Nothing written below may be hoisted above the marker.
        std::sync::atomic::fence(Ordering::SeqCst);
        Writing(e, odd + 1)
    }
}

impl<const V: usize> Drop for Writing<V> {
    fn drop(&mut self) {
        // Unwinding out of a half-finished write must not advertise the slot as
        // settled. Leaving the counter odd sends readers to the lock — where they read
        // the fields directly — and the next writer puts it back in phase. Without
        // this, `ffi::guard` catching a panic mid-`write` would hand lock-free readers
        // a stable-looking counter over a half-updated slot.
        if std::thread::panicking() {
            return;
        }
        // Release: every write above is visible before the slot reads stable again.
        unsafe { (*self.0).version.store(self.1, Ordering::Release) };
    }
}

unsafe fn r_u32(p: *const u32) -> u32 {
    ptr::read_volatile(p)
}
unsafe fn r_u64(p: *const u64) -> u64 {
    ptr::read_volatile(p)
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
        let _w = Writing::begin(e);
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

    /// Change a slot's `state` (1 = occupied, 2 = tombstone, 0 = empty) under the
    /// seqlock marker. Caller holds the slot lock.
    unsafe fn mark_state(e: *mut Entry<V>, state: u32) {
        let _w = Writing::begin(e);
        ptr::write(ptr::addr_of_mut!((*e).state), state);
    }

    /// Refresh a slot's expiry under the seqlock marker. Caller holds the slot lock.
    unsafe fn mark_expires(e: *mut Entry<V>, expires: u64) {
        let _w = Writing::begin(e);
        ptr::write(ptr::addr_of_mut!((*e).expires_at), expires);
    }

    unsafe fn expired(e: *mut Entry<V>, now: u64) -> bool {
        let exp = r_u64(ptr::addr_of!((*e).expires_at));
        exp != 0 && exp < now
    }

    /// Read one slot **without taking its lock**.
    ///
    /// Copy first, believe second: the seqlock counter is sampled before and after,
    /// and an odd first sample or a changed second one means a writer overlapped the
    /// copy, so the bytes are discarded rather than returned.
    ///
    /// A torn length cannot read out of bounds — `key_len` is clamped to `KEY_MAX` and
    /// `val_len` to `V`, the arrays they index — so the worst a lost race can produce
    /// is garbage that is then thrown away. Reading bytes another process may be
    /// writing is a data race in the strict sense; the counter is what makes the
    /// *result* sound, and the volatile reads plus the two fences are what stop the
    /// compiler from reordering the copy outside the window that validates it.
    unsafe fn peek(e: *mut Entry<V>, key: &[u8], h: u64, now: u64) -> Peek {
        let v1 = (*e).version.load(Ordering::Acquire);
        if v1 & 1 != 0 {
            return Peek::Torn; // a writer is inside the slot
        }

        let state = r_u32(ptr::addr_of!((*e).state));
        let hash = r_u64(ptr::addr_of!((*e).hash));
        let klen = r_u32(ptr::addr_of!((*e).key_len)) as usize;
        let expires = r_u64(ptr::addr_of!((*e).expires_at));

        let ours = state == 1
            && hash == h
            && klen == key.len()
            && klen <= KEY_MAX
            && std::slice::from_raw_parts(ptr::addr_of!((*e).key) as *const u8, klen) == key;

        // A slot holding some other key needs no value copy — but the identity read
        // above is still unvalidated, so it cannot be acted on until the counter says
        // the read was clean.
        if !ours {
            std::sync::atomic::fence(Ordering::Acquire);
            if (*e).version.load(Ordering::Relaxed) != v1 {
                return Peek::Torn;
            }
            // Only an EMPTY (0) slot ends the probe chain. A TOMBSTONE (2) is skipped,
            // so a live key stored past a deleted colliding key stays reachable.
            return if state == 0 {
                Peek::ChainEnd
            } else {
                Peek::Other
            };
        }

        let vlen = (r_u32(ptr::addr_of!((*e).val_len)) as usize).min(V);
        // Uninitialised, then filled — `vec![0u8; vlen]` zeroes the buffer first, and
        // the benchmark caught that: a 4 KB value read by one thread came out slower
        // than the locked path it replaced, purely from the extra pass over the page.
        let mut val: Vec<u8> = Vec::with_capacity(vlen);
        ptr::copy_nonoverlapping(ptr::addr_of!((*e).val) as *const u8, val.as_mut_ptr(), vlen);
        val.set_len(vlen);

        std::sync::atomic::fence(Ordering::Acquire);
        if (*e).version.load(Ordering::Relaxed) != v1 {
            return Peek::Torn;
        }
        if expires != 0 && expires < now {
            Peek::Expired
        } else {
            Peek::Hit(val)
        }
    }

    fn get(&self, key: &[u8], h: u64) -> Option<Vec<u8>> {
        let (p, slots) = self.base()?;
        let now = now_secs();
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };

            let mut peek = Peek::Torn;
            for _ in 0..SEQ_ATTEMPTS {
                peek = unsafe { Self::peek(e, key, h, now) };
                if !matches!(peek, Peek::Torn) {
                    break;
                }
                std::hint::spin_loop();
            }
            if matches!(peek, Peek::Torn) {
                let _g = Slot::lock(e);
                // Under the lock no writer can be active, so the fields are read
                // plainly rather than through the counter — which a writer killed
                // mid-update may have left odd, and which `peek` would then keep
                // reporting as torn for the life of the region.
                unsafe {
                    if r_u32(ptr::addr_of!((*e).state)) == 0 {
                        return None;
                    }
                    peek = if Self::matches(e, key, h) {
                        if Self::expired(e, now) {
                            Peek::Expired
                        } else {
                            Peek::Hit(Self::read_val(e))
                        }
                    } else {
                        Peek::Other
                    };
                }
            }

            match peek {
                Peek::ChainEnd => return None,
                Peek::Other => continue,
                Peek::Hit(v) => return Some(v),
                Peek::Expired => {
                    // Reclaiming needs the lock and the write marker. Expiry is rare,
                    // so it stays off the fast path — and it is re-checked, because the
                    // slot may have been rewritten since the lock-free read saw it.
                    let _g = Slot::lock(e);
                    unsafe {
                        if Self::matches(e, key, h) && Self::expired(e, now) {
                            Self::mark_state(e, 2);
                        }
                    }
                    return None;
                }
                Peek::Torn => unreachable!("the lock fallback cannot return Torn"),
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
        {
            let e = unsafe { p.add(target) };
            let _g = Slot::lock(e);
            unsafe { Self::write(e, key, val, h, expires) };
        }
        // Converge on one slot per key.
        //
        // The probe above holds one slot lock at a time, so two concurrent `set`s of
        // the same key can choose *different* targets: one finds a slot empty that the
        // other has since filled, or the two disagree about which live entry is oldest
        // because `written_at` moved underneath them. The key then exists in two slots
        // — and `delete` stopped at the first match, so tombstoning one left the other
        // live and the next lookup found it. For a session key that is a logged-out
        // user logged back in.
        //
        // The comment that used to sit here claimed this was re-validated under the
        // lock. It wasn't: the write was unconditional. So instead of pretending one
        // slot is authoritative, this sweeps the chain afterwards and tombstones any
        // other copy. One ascending pass, one lock at a time, in the same order as the
        // probe — so it cannot deadlock against a concurrent probe.
        Self::tombstone_others(p, slots, key, h, target);
        if evicting {
            note_eviction();
        }
        true
    }

    /// Tombstone every live copy of `key` in the probe chain except the one at `keep`.
    fn tombstone_others(p: *mut Entry<V>, slots: usize, key: &[u8], h: u64, keep: usize) {
        for i in 0..PROBE {
            let idx = (h as usize).wrapping_add(i) % slots;
            if idx == keep {
                continue;
            }
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe {
                if r_u32(ptr::addr_of!((*e).state)) == 0 {
                    return; // chain end: nothing further can hold the key
                }
                if Self::matches(e, key, h) {
                    Self::mark_state(e, 2);
                }
            }
        }
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
        // Every match in the chain, not just the first. Racing `set`s can leave the
        // key in two slots (see `set`), and returning at the first tombstone left the
        // second copy live — a deleted session came back on the next lookup. Deleting
        // has to mean deleted, so this cannot stop early even though the common case
        // has exactly one match.
        let mut found = false;
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _g = Slot::lock(e);
            unsafe {
                if r_u32(ptr::addr_of!((*e).state)) == 0 {
                    break; // chain end
                }
                if Self::matches(e, key, h) {
                    // Tombstone (2), not empty (0): preserves the probe chain so a
                    // colliding key stored later in the chain stays reachable.
                    Self::mark_state(e, 2);
                    found = true;
                }
            }
        }
        found
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
                        Self::mark_state(e, 2); // tombstone
                        return false;
                    }
                    Self::mark_expires(e, expires);
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
                    // Saturating, not wrapping: `delta` comes from PHP. In a release build
                    // `i64::MAX + 1` wrapped to a negative counter, and a rate limit keyed
                    // on it reopened.
                    let next = cur.saturating_add(delta);
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
                    // Saturating, not wrapping: `delta` comes from PHP. In a release build
                    // `i64::MAX + 1` wrapped to a negative counter, and a rate limit keyed
                    // on it reopened.
                    let next = cur.saturating_add(delta);
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
            unsafe { Self::mark_state(e, 0) };
        }
    }

    /// Tombstone every live entry whose key starts with `prefix`. Tombstones, not
    /// empties, for the same reason `delete` uses them: other applications' keys
    /// further along a probe chain must stay reachable.
    fn flush_prefix(&self, prefix: &[u8]) {
        let Some((p, slots)) = self.base() else {
            return;
        };
        for idx in 0..slots {
            let e = unsafe { p.add(idx) };
            let _g = Slot::lock(e);
            unsafe {
                if r_u32(ptr::addr_of!((*e).state)) != 1 {
                    continue;
                }
                let klen = (r_u32(ptr::addr_of!((*e).key_len)) as usize).min(KEY_MAX);
                let k = std::slice::from_raw_parts(ptr::addr_of!((*e).key) as *const u8, klen);
                if k.starts_with(prefix) {
                    Self::mark_state(e, 2);
                }
            }
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
///
/// Every function in this layer applies the current [`crate::ns`] namespace to the
/// key first. The regions below it see only the prefixed bytes, and two applications
/// in one instance see only their own keys.
pub fn get(key: &[u8]) -> Option<Vec<u8>> {
    let key = crate::ns::key(key);
    if key.len() > KEY_MAX {
        return None;
    }
    let h = hash_key(&key);
    SMALL.get(&key, h).or_else(|| LARGE.get(&key, h))
}

/// Set a value, routing by size. Clears the key from the other region so a
/// resize (small↔large) can't leave a stale copy. False if too large / disabled.
pub fn set(key: &[u8], val: &[u8], ttl: u64) -> bool {
    let key = crate::ns::key(key);
    if key.len() > KEY_MAX {
        return false;
    }
    let h = hash_key(&key);
    if val.len() <= VAL_SMALL {
        LARGE.delete(&key, h);
        SMALL.set(&key, val, h, ttl)
    } else if val.len() <= VAL_LARGE {
        SMALL.delete(&key, h);
        LARGE.set(&key, val, h, ttl)
    } else {
        note_oversize(val.len()); // exceeds the largest slot — dropped, not silent
        false
    }
}

/// Atomic set-if-absent (backs `Cache::lock()`). Values are small (owner tokens).
pub fn add(key: &[u8], val: &[u8], ttl: u64) -> bool {
    let key = crate::ns::key(key);
    if key.len() > KEY_MAX || val.len() > VAL_SMALL {
        return false;
    }
    let h = hash_key(&key);
    // A key present in either region blocks the add.
    if LARGE.get(&key, h).is_some() {
        return false;
    }
    SMALL.add(&key, val, h, ttl)
}

/// Delete a key from both regions. True if it existed anywhere.
pub fn delete(key: &[u8]) -> bool {
    let key = crate::ns::key(key);
    let h = hash_key(&key);
    let s = SMALL.delete(&key, h);
    let l = LARGE.delete(&key, h);
    s || l
}

/// Atomically refresh a key's TTL without reading and rewriting its value —
/// closes the get-then-set race a naive cache `touch()` would have (a concurrent
/// writer's value can't be clobbered because the value is never rewritten).
pub fn touch(key: &[u8], ttl: u64) -> bool {
    let key = crate::ns::key(key);
    if key.len() > KEY_MAX {
        return false;
    }
    let h = hash_key(&key);
    SMALL.touch(&key, h, ttl) || LARGE.touch(&key, h, ttl)
}

/// Atomically add `delta` to a numeric key (counters / rate limiting).
pub fn increment(key: &[u8], delta: i64, ttl: u64) -> i64 {
    let key = crate::ns::key(key);
    if key.len() > KEY_MAX {
        return 0;
    }
    let h = hash_key(&key);
    SMALL.increment(&key, h, delta, ttl)
}

/// Empty the current namespace — or, with none set, both regions entirely.
///
/// `askr_cache_flush()` used to zero the whole table. In an instance hosting several
/// applications that let any one of them log every other's users out. It now sweeps
/// the slots whose key carries this application's prefix and leaves the rest.
pub fn flush() {
    let prefix = crate::ns::prefix();
    if prefix.is_empty() {
        SMALL.flush();
        LARGE.flush();
    } else {
        SMALL.flush_prefix(&prefix);
        LARGE.flush_prefix(&prefix);
    }
}

// --- PHP bridge -----------------------------------------------------------

use std::ffi::{c_char, c_int, c_long};

extern "C" fn c_get(
    key: *const c_char,
    klen: usize,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> c_int {
    crate::ffi::guard("cache::get", 0, || {
        let key = unsafe { crate::ffi::bytes(key, klen) };
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
    })
}

extern "C" fn c_set(
    key: *const c_char,
    klen: usize,
    val: *const c_char,
    vlen: usize,
    ttl: c_long,
) -> c_int {
    crate::ffi::guard("cache::set", 0, || {
        let key = unsafe { crate::ffi::bytes(key, klen) };
        let val = unsafe { crate::ffi::bytes(val, vlen) };
        set(key, val, ttl.max(0) as u64) as c_int
    })
}

extern "C" fn c_add(
    key: *const c_char,
    klen: usize,
    val: *const c_char,
    vlen: usize,
    ttl: c_long,
) -> c_int {
    crate::ffi::guard("cache::add", 0, || {
        let key = unsafe { crate::ffi::bytes(key, klen) };
        let val = unsafe { crate::ffi::bytes(val, vlen) };
        add(key, val, ttl.max(0) as u64) as c_int
    })
}

extern "C" fn c_del(key: *const c_char, klen: usize) -> c_int {
    crate::ffi::guard("cache::delete", 0, || {
        let key = unsafe { crate::ffi::bytes(key, klen) };
        delete(key) as c_int
    })
}

extern "C" fn c_incr(key: *const c_char, klen: usize, delta: c_long, ttl: c_long) -> c_long {
    crate::ffi::guard("cache::increment", 0, || {
        let key = unsafe { crate::ffi::bytes(key, klen) };
        increment(key, delta, ttl.max(0) as u64)
    })
}

extern "C" fn c_touch(key: *const c_char, klen: usize, ttl: c_long) -> c_int {
    crate::ffi::guard("cache::touch", 0, || {
        let key = unsafe { crate::ffi::bytes(key, klen) };
        touch(key, ttl.max(0) as u64) as c_int
    })
}

extern "C" fn c_flush() {
    crate::ffi::guard("cache::flush", (), || {
        flush();
        crate::rcache::flush(); // askr_cache_flush() clears both caches
    })
}

/// Invalidate every cached response carrying `tag` (response cache, #1).
extern "C" fn c_forget_tag(tag: *const c_char, tlen: usize) {
    crate::ffi::guard("cache::forget_tag", (), || {
        let tag = unsafe { crate::ffi::bytes(tag, tlen) };
        // Tags are stored namespaced (see server::maybe_store), so an application can
        // only invalidate pages its own responses tagged.
        crate::rcache::forget_tag(&crate::ns::key(tag));
    })
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

    // The cache tests share the process-wide SMALL/LARGE regions (they init/flush
    // the same statics), so serialize them — parallel execution would interfere.
    // `into_inner` ignores poisoning so one failing test doesn't cascade.
    // The cache, the queue and the process-global namespace are one piece of shared
    // state, so their tests serialise on one lock — `ns::tests::GUARD`. Two locks
    // looked like isolation and were not: a queue test setting a namespace in one
    // thread re-keyed a cache stress test's increments in another, and 33 of 16 000
    // went "missing".
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        crate::ns::tests::GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// A lock-free reader must never observe a half-written value.
    ///
    /// The writer alternates between two values that are distinguishable in every
    /// byte *and* in length, so any mixture — a byte of the other value, or a length
    /// that belongs to neither — is a torn read. Without the seqlock counter this is
    /// exactly what `get` would hand back once it stopped taking the slot lock.
    #[test]
    fn a_lockfree_read_never_sees_a_half_written_value() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::time::{Duration, Instant};

        let _g = guard();
        init(1024, 64);
        let key = b"seq:torn";
        let a = vec![b'A'; 512];
        let b = vec![b'B'; 1024];
        assert!(set(key, &a, 0));

        let stop = AtomicBool::new(false);
        let torn = AtomicU64::new(0);
        let reads = AtomicU64::new(0);
        let start = Instant::now();

        std::thread::scope(|sc| {
            sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    set(key, &a, 0);
                    set(key, &b, 0);
                }
            });
            for _ in 0..4 {
                sc.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(v) = get(key) {
                            reads.fetch_add(1, Ordering::Relaxed);
                            let consistent = match v.len() {
                                512 => v.iter().all(|&c| c == b'A'),
                                1024 => v.iter().all(|&c| c == b'B'),
                                _ => false,
                            };
                            if !consistent {
                                torn.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
            while start.elapsed() < Duration::from_millis(250) {
                std::thread::yield_now();
            }
            stop.store(true, Ordering::Relaxed);
        });

        let n = reads.load(Ordering::Relaxed);
        assert!(
            n > 10_000,
            "the run must actually exercise the path (got {n})"
        );
        assert_eq!(torn.load(Ordering::Relaxed), 0, "of {n} reads");
    }

    /// A writer killed mid-update leaves the counter odd for good. Reads of that slot
    /// must fall back to the lock rather than spin or miss, and the next write must
    /// repair the counter instead of leaving it permanently out of phase — which is
    /// why `Writing::begin` forces the value odd rather than incrementing it.
    #[test]
    fn a_slot_left_mid_write_is_readable_and_then_repaired() {
        let _g = guard();
        init(256, 64);
        let key = b"seq:abandoned";
        assert!(set(key, b"payload", 0));

        let h = hash_key(key);
        let (p, slots) = SMALL.base().expect("region is mapped");
        let mut found = None;
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _lock = Slot::lock(e);
            if unsafe { Region::<VAL_SMALL>::matches(e, key, h) } {
                found = Some(e);
                break;
            }
        }
        let e = found.expect("the key just set must be in its chain");

        // Simulate the corpse: counter left odd, value intact.
        let before = unsafe { (*e).version.load(Ordering::SeqCst) };
        assert_eq!(before & 1, 0, "a settled slot reads even");
        unsafe { (*e).version.store(before | 1, Ordering::SeqCst) };

        assert_eq!(
            get(key).as_deref(),
            Some(&b"payload"[..]),
            "an odd counter must send the read to the lock, not lose the value"
        );

        // The next write puts the slot back in phase.
        assert!(set(key, b"replaced", 0));
        let after = unsafe { (*e).version.load(Ordering::SeqCst) };
        assert_eq!(
            after & 1,
            0,
            "a completed write must leave the counter even"
        );
        assert!(after > before, "and move it forward");
        assert_eq!(get(key).as_deref(), Some(&b"replaced"[..]));
    }

    /// The old locked read path, kept for the benchmark below so the two can be
    /// compared back to back in one process instead of across two builds.
    fn get_taking_the_lock(key: &[u8]) -> Option<Vec<u8>> {
        let h = hash_key(key);
        let (p, slots) = SMALL.base()?;
        let now = now_secs();
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _g = Slot::lock(e);
            unsafe {
                if r_u32(ptr::addr_of!((*e).state)) == 0 {
                    return None;
                }
                if Region::<VAL_SMALL>::matches(e, key, h) {
                    if Region::<VAL_SMALL>::expired(e, now) {
                        return None;
                    }
                    return Some(Region::<VAL_SMALL>::read_val(e));
                }
            }
        }
        None
    }

    /// How read throughput on one hot key scales with concurrent readers, lock-free
    /// against locked, measured in the same process.
    ///
    /// A measurement rather than an assertion — timings are not something to fail a
    /// build on. Run it on demand:
    ///
    /// ```text
    /// cargo test --release -p askr --bins cache_read_scaling -- --ignored --nocapture
    /// ```
    ///
    /// Threads stand in for workers: `shmlock` does not care whether the contenders
    /// are threads or processes, and the mapping is `MAP_SHARED` either way, so the
    /// serialisation on one slot's lock is the same mechanism.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn cache_read_scaling() {
        use std::sync::atomic::AtomicU64;
        use std::sync::Barrier;
        use std::time::{Duration, Instant};

        /// Ops/sec for `threads` readers, timed only while every thread is running.
        ///
        /// The barrier is the point: without it the timer starts before the last
        /// thread is spawned, so late threads run shorter than the divisor assumes and
        /// high thread counts read as a collapse that is really spawn skew.
        fn measure(threads: usize, run: Duration, read: &(dyn Fn() + Sync)) -> f64 {
            let ops = AtomicU64::new(0);
            let ready = Barrier::new(threads + 1);
            let go = Barrier::new(threads + 1);
            let mut elapsed = Duration::ZERO;
            std::thread::scope(|sc| {
                for _ in 0..threads {
                    sc.spawn(|| {
                        ready.wait();
                        go.wait();
                        let start = Instant::now();
                        let mut n = 0u64;
                        while start.elapsed() < run {
                            for _ in 0..64 {
                                read();
                            }
                            n += 64;
                        }
                        ops.fetch_add(n, Ordering::Relaxed);
                    });
                }
                ready.wait();
                let t0 = Instant::now();
                go.wait();
                // Threads stop on their own clocks; this is the wall time they shared.
                std::thread::sleep(run);
                elapsed = t0.elapsed();
            });
            ops.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64()
        }

        let _g = guard();
        init(4096, 64);
        let key = b"hot:session";
        const RUN: Duration = Duration::from_millis(400);

        for size in [16usize, 512, 4096] {
            let val = vec![b'x'; size];
            assert!(set(key, &val, 0));
            assert_eq!(get(key).as_deref(), Some(&val[..]));
            assert_eq!(get_taking_the_lock(key).as_deref(), Some(&val[..]));

            println!("\none hot key, {size} byte value:");
            println!("  threads        locked      lock-free      speedup");
            for threads in [1usize, 2, 4, 8, 12] {
                let locked = measure(threads, RUN, &|| {
                    assert!(get_taking_the_lock(key).is_some());
                });
                let free = measure(threads, RUN, &|| {
                    assert!(get(key).is_some());
                });
                println!(
                    "  {threads:>7}  {locked:>12.0}  {free:>13.0}  {:>10.1}x",
                    free / locked
                );
            }
        }
        println!();
    }

    /// Two applications in one instance used to share one key space: either could
    /// read the other's sessions by key, and `askr_cache_flush()` from one logged the
    /// other's users out. The namespace is set per request from the docroot; here it is
    /// set by hand and the two views must not overlap.
    #[test]
    fn two_namespaces_do_not_see_each_other_and_flush_is_scoped() {
        let _g = guard();
        init(256, 64);
        flush();

        crate::ns::set("aaaaaaaaaaaaaaaa");
        assert!(set(b"sess:1", b"alice", 0));
        assert!(set(b"shared-name", b"from-a", 0));
        crate::ns::set("bbbbbbbbbbbbbbbb");
        assert!(set(b"shared-name", b"from-b", 0));

        assert_eq!(get(b"sess:1"), None, "B cannot read A's session");
        assert_eq!(get(b"shared-name").as_deref(), Some(&b"from-b"[..]));
        assert!(!delete(b"sess:1"), "nor delete it");

        // B flushes: only B goes.
        flush();
        assert_eq!(get(b"shared-name"), None);
        crate::ns::set("aaaaaaaaaaaaaaaa");
        assert_eq!(
            get(b"sess:1").as_deref(),
            Some(&b"alice"[..]),
            "A survives B's flush"
        );
        assert_eq!(get(b"shared-name").as_deref(), Some(&b"from-a"[..]));

        // No namespace: the raw table, and flush() means everything.
        crate::ns::set("");
        assert_eq!(
            get(b"sess:1"),
            None,
            "the raw view has no un-prefixed sess:1"
        );
        flush();
        crate::ns::set("aaaaaaaaaaaaaaaa");
        assert_eq!(get(b"sess:1"), None);
        crate::ns::set("");
    }

    /// `delta` is a PHP integer. `cur + delta` wrapped in release builds, so two
    /// increments by i64::MAX turned a counter negative — and a limit keyed on that
    /// counter was open again.
    #[test]
    fn a_counter_saturates_instead_of_wrapping() {
        let _g = guard();
        init(256, 64);
        crate::ns::set("");
        delete(b"ctr:sat");
        assert_eq!(increment(b"ctr:sat", i64::MAX, 0), i64::MAX);
        assert_eq!(increment(b"ctr:sat", i64::MAX, 0), i64::MAX, "stays pinned");
        assert_eq!(increment(b"ctr:sat", 1, 0), i64::MAX);
        assert_eq!(
            increment(b"ctr:sat", -1, 0),
            i64::MAX - 1,
            "and still moves down"
        );
        delete(b"ctr:sat");
    }

    /// A key can end up in two slots: the probe in `set` holds one slot lock at a
    /// time, so two concurrent `set`s of the same key can choose different targets
    /// (one sees a slot empty that the other has filled, or they disagree about which
    /// live entry is oldest because `written_at` moved). `delete` stopped at the first
    /// match, so the second copy stayed live and the next lookup found it — for a
    /// session key, a logged-out user logged back in.
    ///
    /// The race is not reproducible on demand, so the duplicate is planted directly
    /// and the *consequence* is what gets asserted.
    #[test]
    fn a_deleted_key_cannot_come_back_from_a_duplicate_slot() {
        let _g = guard();
        init(256, 64);

        let key = b"sess:duplicated";
        assert!(set(key, b"first", 0));
        let h = hash_key(key);
        let (p, slots) = SMALL.base().expect("region is mapped after init");

        // Where did `set` put it?
        let mut at = None;
        for i in 0..PROBE {
            let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
            let _lock = Slot::lock(e);
            if unsafe { Region::<VAL_SMALL>::matches(e, key, h) } {
                at = Some(i);
                break;
            }
        }
        let i = at.expect("the key just set must be somewhere in its own chain");

        // Plant a second live copy one step further along the same chain — exactly
        // what the losing side of the race leaves behind.
        let dup = unsafe { p.add((h as usize).wrapping_add(i + 1) % slots) };
        {
            let _lock = Slot::lock(dup);
            unsafe { Region::<VAL_SMALL>::write(dup, key, b"second", h, 0) };
        }

        assert!(delete(key), "delete must report that it removed something");
        assert_eq!(
            get(key),
            None,
            "both copies must be gone — a deleted session that reappears is the bug"
        );
    }

    #[test]
    fn size_classes_and_add() {
        let _g = guard();
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

    // Stress: hammer the shared-memory table from many threads to shake out
    // torn writes / probe-chain / tombstone races (the per-slot spinlock must keep
    // read-modify-write atomic). Threads share the same global regions.
    #[test]
    fn concurrent_stress_no_corruption() {
        let _g = guard();
        init(1024, 64);
        flush();

        // 1) Atomic increment under contention: N threads × M bumps of one counter
        //    must total exactly N*M (the slot lock serialises read-modify-write).
        const T: i64 = 8;
        const M: i64 = 2000;
        std::thread::scope(|s| {
            for _ in 0..T {
                s.spawn(|| {
                    for _ in 0..M {
                        increment(b"ctr", 1, 60);
                    }
                });
            }
        });
        assert_eq!(
            get(b"ctr").as_deref(),
            Some(format!("{}", T * M).as_bytes()),
            "concurrent increments lost an update"
        );

        // 2) set/delete/get churn on a small (colliding) keyspace: liveness (no
        //    deadlock/panic) and a pinned key survives unrelated churn.
        assert!(set(b"pin", b"PIN", 0));
        std::thread::scope(|s| {
            for t in 0..T {
                s.spawn(move || {
                    for i in 0..M {
                        let k = format!("s{}", i % 32);
                        match (t + i) % 3 {
                            0 => {
                                set(k.as_bytes(), b"v", 60);
                            }
                            1 => {
                                let _ = get(k.as_bytes());
                            }
                            _ => {
                                delete(k.as_bytes());
                            }
                        }
                    }
                });
            }
        });
        assert_eq!(
            get(b"pin").as_deref(),
            Some(&b"PIN"[..]),
            "a pinned key was corrupted by unrelated churn"
        );
        flush();
    }
}
