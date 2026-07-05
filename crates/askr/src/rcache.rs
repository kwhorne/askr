//! Shared-memory HTTP **response** cache with instant, app-driven tag
//! invalidation — the Varnish-effect, in the binary, with no external cache.
//!
//! PHP marks a response cacheable with a header (`Askr-Cache: 60, tags=posts`).
//! Askr stores the whole response (status + headers + body) in a fixed-slot
//! table in an anonymous **shared** mmap (created by the master before fork, so
//! every worker sees the same physical table — no IPC). A later matching GET is
//! served straight from Rust, never touching PHP — anonymous traffic runs at
//! static-file speed.
//!
//! The unique bit is **tag invalidation**: `askr_cache_forget_tag('posts')` from
//! anywhere in the app bumps a generation counter in a shared *tag table*, and
//! every stored entry that carries that tag becomes stale instantly across all
//! workers — O(1), no scanning, no coordination.
//!
//! Robustness mirrors the kv cache: fixed-size inline slots (no shared-memory
//! allocator), a per-slot spinlock that can be stolen if a holder dies, and
//! length-clamped reads so a torn write can never cause an out-of-bounds read.

use std::hash::{Hash, Hasher};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const HDR_MAX: usize = 8 * 1024;
const BODY_MAX: usize = 128 * 1024;
const MAX_TAGS: usize = 8;
const PROBE: usize = 8;
const TAG_SLOTS: usize = 4096;

#[repr(C)]
struct Entry {
    lock: AtomicU32,
    state: u32, // 0 = empty, 1 = occupied
    key_hash: u64,
    expires_at: u64, // unix secs; 0 = never
    status: u32,
    ntags: u32,
    tag_hash: [u64; MAX_TAGS],
    tag_gen: [u64; MAX_TAGS], // each tag's generation at store time
    hdr_len: u32,
    body_len: u32,
    hdr: [u8; HDR_MAX],
    body: [u8; BODY_MAX],
}

/// Generation counter per tag. `hash == 0` means an empty slot.
#[repr(C)]
struct TagGen {
    hash: AtomicU64,
    gen: AtomicU64,
}

/// One in-flight (being-computed) key, for request coalescing (#2).
#[repr(C)]
struct Inflight {
    key_hash: AtomicU64, // 0 = free
    deadline: AtomicU64, // unix secs; a stale leader is reclaimed after this
}

const INFLIGHT_SLOTS: usize = 4096;
/// Safety cap: a leader that crashes releases its slot after this many seconds.
const COALESCE_TTL: u64 = 30;

/// Hit/miss/coalesced counters — in shared memory so the master's admin thread
/// sees the totals across all worker processes.
#[repr(C)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
}

static RCACHE_PTR: AtomicPtr<Entry> = AtomicPtr::new(ptr::null_mut());
static RCACHE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static TAGS_PTR: AtomicPtr<TagGen> = AtomicPtr::new(ptr::null_mut());
static INFLIGHT_PTR: AtomicPtr<Inflight> = AtomicPtr::new(ptr::null_mut());
static COUNTERS_PTR: AtomicPtr<Counters> = AtomicPtr::new(ptr::null_mut());

fn counters() -> Option<&'static Counters> {
    let p = COUNTERS_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        None
    } else {
        Some(unsafe { &*p })
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    let v = h.finish();
    if v == 0 {
        1
    } else {
        v
    } // reserve 0 for "empty"
}

fn mmap_shared(bytes: usize) -> *mut libc::c_void {
    // SAFETY: anonymous shared mapping; zeroed pages are a valid initial state.
    unsafe {
        libc::mmap(
            ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    }
}

/// Map the response cache (`slots` entries) and the tag table. Call in the
/// master before forking. Idempotent-ish.
pub fn init(slots: usize) {
    if !RCACHE_PTR.load(Ordering::SeqCst).is_null() {
        return;
    }
    let slots = slots.max(16);
    let esize = slots * std::mem::size_of::<Entry>();
    let tsize = TAG_SLOTS * std::mem::size_of::<TagGen>();
    let isize_ = INFLIGHT_SLOTS * std::mem::size_of::<Inflight>();
    let ep = mmap_shared(esize);
    let tp = mmap_shared(tsize);
    let ip = mmap_shared(isize_);
    let cp = mmap_shared(std::mem::size_of::<Counters>());
    if ep == libc::MAP_FAILED
        || tp == libc::MAP_FAILED
        || ip == libc::MAP_FAILED
        || cp == libc::MAP_FAILED
    {
        tracing::warn!("response cache: mmap failed; disabled");
        return;
    }
    RCACHE_SLOTS.store(slots, Ordering::SeqCst);
    TAGS_PTR.store(tp as *mut TagGen, Ordering::SeqCst);
    INFLIGHT_PTR.store(ip as *mut Inflight, Ordering::SeqCst);
    COUNTERS_PTR.store(cp as *mut Counters, Ordering::SeqCst);
    RCACHE_PTR.store(ep as *mut Entry, Ordering::SeqCst);
    tracing::info!(
        slots,
        mib = esize / 1024 / 1024,
        "response cache mapped (tag invalidation)"
    );
}

pub fn enabled() -> bool {
    !RCACHE_PTR.load(Ordering::SeqCst).is_null()
}

fn base() -> Option<(*mut Entry, usize)> {
    let p = RCACHE_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        None
    } else {
        Some((p, RCACHE_SLOTS.load(Ordering::SeqCst)))
    }
}

/// Hit / miss / coalesced counters for the admin dashboard.
pub fn stats() -> (u64, u64, u64) {
    match counters() {
        Some(c) => (
            c.hits.load(Ordering::Relaxed),
            c.misses.load(Ordering::Relaxed),
            c.coalesced.load(Ordering::Relaxed),
        ),
        None => (0, 0, 0),
    }
}

// --- request coalescing (singleflight, #2) --------------------------------

/// The outcome of claiming a key for computation.
pub enum Lead {
    /// This caller should run PHP and (if cacheable) populate the cache.
    Leader,
    /// Another caller is already computing this key; wait for the result.
    Follower,
}

/// Claim a key for computation. All-but-one concurrent callers for the same key
/// become `Follower`s. Fail-open: on a hash collision or disabled cache we
/// return `Leader`, so at worst coalescing just doesn't apply.
pub fn begin(key: &[u8]) -> Lead {
    let p = INFLIGHT_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        return Lead::Leader;
    }
    let h = hash_bytes(key);
    let now = now_secs();
    // Single primary slot per key hash, so the herd converges on one leader.
    let s = unsafe { &*p.add((h as usize) % INFLIGHT_SLOTS) };
    let kh = s.key_hash.load(Ordering::Acquire);
    if kh == h && s.deadline.load(Ordering::Acquire) > now {
        return Lead::Follower;
    }
    if kh == 0 || s.deadline.load(Ordering::Acquire) <= now {
        if s.key_hash
            .compare_exchange(kh, h, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            s.deadline.store(now + COALESCE_TTL, Ordering::Release);
            return Lead::Leader;
        }
        // Lost the race — someone else claimed it.
        if s.key_hash.load(Ordering::Acquire) == h {
            return Lead::Follower;
        }
    }
    Lead::Leader // slot busy with a different key (collision) → don't coalesce
}

/// Is a key still being computed by a leader?
pub fn is_inflight(key: &[u8]) -> bool {
    let p = INFLIGHT_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        return false;
    }
    let h = hash_bytes(key);
    let s = unsafe { &*p.add((h as usize) % INFLIGHT_SLOTS) };
    s.key_hash.load(Ordering::Acquire) == h && s.deadline.load(Ordering::Acquire) > now_secs()
}

/// Release a key a leader finished computing, waking any followers.
pub fn end(key: &[u8]) {
    let p = INFLIGHT_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        return;
    }
    let h = hash_bytes(key);
    let s = unsafe { &*p.add((h as usize) % INFLIGHT_SLOTS) };
    if s.key_hash.load(Ordering::Acquire) == h {
        s.deadline.store(0, Ordering::Release);
        s.key_hash.store(0, Ordering::Release);
    }
}

/// Count a request that was served by waiting on a coalesced leader.
pub fn note_coalesced() {
    if let Some(c) = counters() {
        c.coalesced.fetch_add(1, Ordering::Relaxed);
    }
}

// --- tag generations ------------------------------------------------------

/// Current generation for a tag hash (0 if the tag has never been forgotten).
fn tag_gen(h: u64) -> u64 {
    let p = TAGS_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        return 0;
    }
    let start = (h as usize) % TAG_SLOTS;
    for i in 0..PROBE {
        // SAFETY: TagGen atomics live in the shared mapping.
        let t = unsafe { &*p.add((start + i) % TAG_SLOTS) };
        let hv = t.hash.load(Ordering::Acquire);
        if hv == h {
            return t.gen.load(Ordering::Acquire);
        }
        if hv == 0 {
            return 0;
        }
    }
    0
}

/// Bump a tag's generation — every stored entry carrying it becomes stale at
/// once, across all workers.
pub fn forget_tag(tag: &[u8]) {
    let p = TAGS_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        return;
    }
    let h = hash_bytes(tag);
    let start = (h as usize) % TAG_SLOTS;
    for i in 0..PROBE {
        // SAFETY: shared mapping.
        let t = unsafe { &*p.add((start + i) % TAG_SLOTS) };
        let hv = t.hash.load(Ordering::Acquire);
        if hv == h {
            t.gen.fetch_add(1, Ordering::AcqRel);
            return;
        }
        if hv == 0
            && t.hash
                .compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            t.gen.store(1, Ordering::Release);
            return;
        }
    }
    // Probe window full: reuse the primary slot (worst case: a false stale).
    let t = unsafe { &*p.add(start) };
    t.hash.store(h, Ordering::Release);
    t.gen.fetch_add(1, Ordering::AcqRel);
}

// --- slot lock (mirrors cache.rs) -----------------------------------------

struct Slot(*mut Entry);
impl Slot {
    fn lock(e: *mut Entry) -> Slot {
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
        lock.store(1, Ordering::SeqCst); // steal from a dead holder
        Slot(e)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        unsafe { (*self.0).lock.store(0, Ordering::Release) };
    }
}

/// A cached response.
pub struct Cached {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Look up a cached response and record a hit/miss. Use [`peek`] for the
/// coalescing poll loop so repeated polls don't inflate the miss counter.
pub fn get(key: &[u8]) -> Option<Cached> {
    let hit = peek(key);
    if let Some(c) = counters() {
        if hit.is_some() {
            c.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            c.misses.fetch_add(1, Ordering::Relaxed);
        }
    }
    hit
}

/// Look up a cached response without touching the hit/miss counters.
pub fn peek(key: &[u8]) -> Option<Cached> {
    let (p, slots) = base()?;
    let h = hash_bytes(key);
    let now = now_secs();
    let mut hit = None;
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        // SAFETY: fields read under the slot lock; lengths clamped before slicing.
        unsafe {
            if ptr::read(ptr::addr_of!((*e).state)) == 0 {
                break; // empty slot ends the probe chain
            }
            if ptr::read(ptr::addr_of!((*e).key_hash)) != h {
                continue;
            }
            let exp = ptr::read(ptr::addr_of!((*e).expires_at));
            if exp != 0 && exp < now {
                ptr::write(ptr::addr_of_mut!((*e).state), 0);
                break;
            }
            // Tag validity: any tag whose generation advanced ⇒ stale.
            let ntags = (ptr::read(ptr::addr_of!((*e).ntags)) as usize).min(MAX_TAGS);
            let mut stale = false;
            for t in 0..ntags {
                let th = ptr::read(ptr::addr_of!((*e).tag_hash[t]));
                let tg = ptr::read(ptr::addr_of!((*e).tag_gen[t]));
                if tag_gen(th) != tg {
                    stale = true;
                    break;
                }
            }
            if stale {
                ptr::write(ptr::addr_of_mut!((*e).state), 0);
                break;
            }
            let status = ptr::read(ptr::addr_of!((*e).status)) as u16;
            let hlen = (ptr::read(ptr::addr_of!((*e).hdr_len)) as usize).min(HDR_MAX);
            let blen = (ptr::read(ptr::addr_of!((*e).body_len)) as usize).min(BODY_MAX);
            let hdr =
                std::slice::from_raw_parts(ptr::addr_of!((*e).hdr) as *const u8, hlen).to_vec();
            let body =
                std::slice::from_raw_parts(ptr::addr_of!((*e).body) as *const u8, blen).to_vec();
            hit = Some(Cached {
                status,
                headers: parse_hdr_blob(&hdr),
                body,
            });
            break;
        }
    }
    hit
}

/// Store a response. `tags` are opaque byte strings. Returns false if too large
/// or the cache is disabled.
pub fn store(
    key: &[u8],
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    ttl: u64,
    tags: &[Vec<u8>],
) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    let blob = hdr_blob(headers);
    if blob.len() > HDR_MAX || body.len() > BODY_MAX {
        return false;
    }
    let h = hash_bytes(key);
    let expires = if ttl > 0 { now_secs() + ttl } else { 0 };

    // Snapshot each tag's current generation, so a forget_tag that raced ahead
    // of this store still invalidates us.
    let ntags = tags.len().min(MAX_TAGS);
    let mut th = [0u64; MAX_TAGS];
    let mut tg = [0u64; MAX_TAGS];
    for (i, tag) in tags.iter().take(MAX_TAGS).enumerate() {
        th[i] = hash_bytes(tag);
        tg[i] = tag_gen(th[i]);
    }

    let mut target = None;
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        let state = unsafe { ptr::read(ptr::addr_of!((*e).state)) };
        let same = unsafe { ptr::read(ptr::addr_of!((*e).key_hash)) } == h;
        if state == 0 || same {
            unsafe { write_entry(e, h, status, &blob, body, expires, ntags, &th, &tg) };
            return true;
        }
        if target.is_none() {
            target = Some(e);
        }
    }
    // Probe window full: evict the primary slot.
    let e = target.unwrap_or_else(|| unsafe { p.add((h as usize) % slots) });
    let _g = Slot::lock(e);
    unsafe { write_entry(e, h, status, &blob, body, expires, ntags, &th, &tg) };
    true
}

#[allow(clippy::too_many_arguments)]
unsafe fn write_entry(
    e: *mut Entry,
    h: u64,
    status: u16,
    blob: &[u8],
    body: &[u8],
    expires: u64,
    ntags: usize,
    th: &[u64; MAX_TAGS],
    tg: &[u64; MAX_TAGS],
) {
    ptr::write(ptr::addr_of_mut!((*e).state), 1);
    ptr::write(ptr::addr_of_mut!((*e).key_hash), h);
    ptr::write(ptr::addr_of_mut!((*e).expires_at), expires);
    ptr::write(ptr::addr_of_mut!((*e).status), status as u32);
    ptr::write(ptr::addr_of_mut!((*e).ntags), ntags as u32);
    for i in 0..MAX_TAGS {
        ptr::write(ptr::addr_of_mut!((*e).tag_hash[i]), th[i]);
        ptr::write(ptr::addr_of_mut!((*e).tag_gen[i]), tg[i]);
    }
    ptr::write(ptr::addr_of_mut!((*e).hdr_len), blob.len() as u32);
    ptr::write(ptr::addr_of_mut!((*e).body_len), body.len() as u32);
    ptr::copy_nonoverlapping(blob.as_ptr(), ptr::addr_of_mut!((*e).hdr) as *mut u8, blob.len());
    ptr::copy_nonoverlapping(body.as_ptr(), ptr::addr_of_mut!((*e).body) as *mut u8, body.len());
}

/// Empty the cache (keeps tag generations).
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

fn hdr_blob(headers: &[(String, String)]) -> Vec<u8> {
    let mut s = String::new();
    for (k, v) in headers {
        s.push_str(k);
        s.push_str(": ");
        s.push_str(v);
        s.push_str("\r\n");
    }
    s.into_bytes()
}

fn parse_hdr_blob(raw: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_get_and_tag_invalidation() {
        init(64);
        assert!(enabled());

        let hdrs = vec![("Content-Type".to_string(), "text/html".to_string())];
        assert!(store(b"GET|/posts", 200, &hdrs, b"<h1>posts</h1>", 60, &[b"posts".to_vec()]));

        let hit = get(b"GET|/posts").expect("hit");
        assert_eq!(hit.status, 200);
        assert_eq!(hit.body, b"<h1>posts</h1>");
        assert_eq!(hit.headers[0], ("Content-Type".into(), "text/html".into()));

        // Forgetting the tag invalidates the entry instantly.
        forget_tag(b"posts");
        assert!(get(b"GET|/posts").is_none());

        // A fresh store after invalidation is servable again.
        assert!(store(b"GET|/posts", 200, &hdrs, b"v2", 60, &[b"posts".to_vec()]));
        assert_eq!(get(b"GET|/posts").unwrap().body, b"v2");

        // An untagged entry is unaffected by tag bumps.
        assert!(store(b"GET|/about", 200, &hdrs, b"about", 0, &[]));
        forget_tag(b"posts");
        assert_eq!(get(b"GET|/about").unwrap().body, b"about");
    }
}
