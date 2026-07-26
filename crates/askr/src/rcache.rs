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
/// Linear-probe window (see the note in `cache.rs`). Widened so a response cache at
/// high fill evicts less eagerly; the cost is scanning more slots on a collision.
const PROBE: usize = 16;
const TAG_SLOTS: usize = 4096;
/// Bytes of cache key retained per entry, for URL-targeted invalidation.
const KEY_MAX: usize = 512;

#[repr(C)]
struct Entry {
    lock: AtomicU32,
    state: u32, // 0 = empty, 1 = occupied
    key_hash: u64,
    expires_at: u64,  // fresh deadline (unix secs; 0 = never)
    stale_until: u64, // hard deadline (unix secs; 0 = never). Between expires_at
    // and stale_until the entry is served stale while a background refresh runs.
    error_until: u64, // stale-if-error deadline (unix secs; 0 = off). Past
    // stale_until but within this, the entry is kept as a *fallback* and served
    // only when the origin fails (5xx / handler error).
    status: u32,
    ntags: u32,
    // The full cache key, kept so PURGE/BAN can match entries by URL. Keys longer
    // than KEY_MAX are truncated (pathological; a truncated key simply won't match
    // a purge, so it expires normally).
    key_len: u32,
    key_bytes: [u8; KEY_MAX],
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
    if !RCACHE_PTR.load(Ordering::Relaxed).is_null() {
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
    // Mapped once in the master before forking; pointers are read-only after, so
    // a release-store paired with acquire-loads suffices. Store slots first so any
    // reader that acquire-loads a non-null RCACHE_PTR also sees the slot count.
    RCACHE_SLOTS.store(slots, Ordering::Relaxed);
    TAGS_PTR.store(tp as *mut TagGen, Ordering::Release);
    INFLIGHT_PTR.store(ip as *mut Inflight, Ordering::Release);
    COUNTERS_PTR.store(cp as *mut Counters, Ordering::Release);
    RCACHE_PTR.store(ep as *mut Entry, Ordering::Release);
    tracing::info!(
        slots,
        mib = esize / 1024 / 1024,
        "response cache mapped (tag invalidation)"
    );
}

pub fn enabled() -> bool {
    !RCACHE_PTR.load(Ordering::Acquire).is_null()
}

fn base() -> Option<(*mut Entry, usize)> {
    let p = RCACHE_PTR.load(Ordering::Acquire);
    if p.is_null() {
        None
    } else {
        Some((p, RCACHE_SLOTS.load(Ordering::Relaxed)))
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
        crate::shmlock::acquire(unsafe { &(*e).lock });
        Slot(e)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        crate::shmlock::release(unsafe { &(*self.0).lock });
    }
}

/// A cached response.
pub struct Cached {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// True when past the fresh deadline but within the stale-while-revalidate
    /// window: serve this immediately, but trigger a background refresh.
    pub stale: bool,
    /// True when the entry survives only inside its `stale-if-error` window: it
    /// must NOT be served proactively, only as a fallback when the origin fails.
    pub error_only: bool,
}

/// Glob match with `*` wildcards (any number, anywhere). Iterative and
/// allocation-free — it runs while a slot lock is held, and per request for
/// `[[cache.rule]]` matching.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    // Two-pointer wildcard match with backtracking to the last `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Split a stored key (`METHOD \0 host \0 path?query \0 enc \0 device`) into its
/// method, host and path+query parts.
fn key_parts(key: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let mut it = key.split(|b| *b == 0);
    let method = it.next()?;
    let host = it.next()?;
    let pq = it.next()?;
    Some((method, host, pq))
}

/// Invalidate every variant of one URL — all encodings and device classes, for
/// both `GET` and `HEAD`. Returns how many entries were dropped.
///
/// The stored key is matched by prefix, and only at a component boundary, so
/// purging `/posts/1` never touches `/posts/12`. A query string on the purged URL
/// is honoured: without one, every query variant of that path goes.
pub fn purge_url(host: &str, path: &str, query: Option<&str>) -> usize {
    scan_invalidate(|key| {
        let Some((method, khost, pq)) = key_parts(key) else {
            return false;
        };
        if method != b"GET" && method != b"HEAD" {
            return false;
        }
        if !khost.eq_ignore_ascii_case(host.as_bytes()) {
            return false;
        }
        match query {
            // Exact path+query.
            Some(q) => {
                let want: Vec<u8> = [path.as_bytes(), b"?", q.as_bytes()].concat();
                pq == want.as_slice()
            }
            // Path only: this path with any (or no) query string.
            None => {
                let p = path.as_bytes();
                pq == p || (pq.len() > p.len() && pq.starts_with(p) && pq[p.len()] == b'?')
            }
        }
    })
}

/// Invalidate every entry for `host` whose path matches a `*` glob. Returns how
/// many entries were dropped.
///
/// This is an eager scan at ban time rather than a rule list consulted on every
/// lookup: the hot path stays untouched, and the cost is one pass over the slots.
/// Entries stored *after* the ban are unaffected, which is what you want — they
/// were rendered from current data.
pub fn ban_glob(host: &str, pattern: &str) -> usize {
    scan_invalidate(|key| {
        let Some((method, khost, pq)) = key_parts(key) else {
            return false;
        };
        if method != b"GET" && method != b"HEAD" {
            return false;
        }
        if !khost.eq_ignore_ascii_case(host.as_bytes()) {
            return false;
        }
        // Match the path only — a ban targets URLs, not query permutations.
        let path = match pq.iter().position(|b| *b == b'?') {
            Some(i) => &pq[..i],
            None => pq,
        };
        std::str::from_utf8(path).is_ok_and(|p| glob_match(pattern, p))
    })
}

/// Walk every slot and tombstone the live entries whose key satisfies `want`.
fn scan_invalidate(want: impl Fn(&[u8]) -> bool) -> usize {
    let Some((p, slots)) = base() else {
        return 0;
    };
    let mut n = 0;
    for i in 0..slots {
        let e = unsafe { p.add(i) };
        let _g = Slot::lock(e);
        // SAFETY: fields read under the slot lock; key length clamped before slicing.
        unsafe {
            if ptr::read(ptr::addr_of!((*e).state)) != 1 {
                continue;
            }
            let klen = (ptr::read(ptr::addr_of!((*e).key_len)) as usize).min(KEY_MAX);
            let key = std::slice::from_raw_parts(ptr::addr_of!((*e).key_bytes) as *const u8, klen);
            if want(key) {
                // Tombstone (not empty) so colliding probe chains stay intact.
                ptr::write(ptr::addr_of_mut!((*e).state), 2);
                n += 1;
            }
        }
    }
    n
}

/// Look up a cached response and record a hit/miss. Use [`peek`] for the
/// coalescing poll loop so repeated polls don't inflate the miss counter.
pub fn get(key: &[u8]) -> Option<Cached> {
    // An entry kept alive only by its stale-if-error window is not a hit: the
    // request must run PHP, and only a failure may fall back to it.
    let hit = peek(key).filter(|c| !c.error_only);
    if let Some(c) = counters() {
        if hit.is_some() {
            c.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            c.misses.fetch_add(1, Ordering::Relaxed);
        }
    }
    hit
}

/// Look up an entry to serve as a **failure fallback**: the origin just returned
/// 5xx (or the handler errored), so anything we still hold — stale or inside the
/// `stale-if-error` window — beats an error page. Doesn't touch the counters (the
/// miss was already counted when the request started).
pub fn stale_on_error(key: &[u8]) -> Option<Cached> {
    peek(key)
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
            let st = ptr::read(ptr::addr_of!((*e).state));
            if st == 0 {
                break; // empty slot ends the probe chain
            }
            if st != 1 {
                continue; // tombstone (2) ⇒ skip, but keep probing the chain
            }
            if ptr::read(ptr::addr_of!((*e).key_hash)) != h {
                continue;
            }
            let exp = ptr::read(ptr::addr_of!((*e).expires_at));
            let hard = ptr::read(ptr::addr_of!((*e).stale_until));
            let err_until = ptr::read(ptr::addr_of!((*e).error_until));
            // Retain until the widest deadline: a stale-if-error window can outlive
            // the stale-while-revalidate window (that's the point of it).
            let retain = hard.max(err_until);
            // Past every deadline ⇒ fully expired (a real miss).
            if retain != 0 && retain < now {
                ptr::write(ptr::addr_of_mut!((*e).state), 2); // tombstone, keep chain
                break;
            }
            // Tag invalidation is a *hard* invalidation (content changed): never
            // serve it stale.
            let ntags = (ptr::read(ptr::addr_of!((*e).ntags)) as usize).min(MAX_TAGS);
            let mut tag_invalid = false;
            for t in 0..ntags {
                let th = ptr::read(ptr::addr_of!((*e).tag_hash[t]));
                let tg = ptr::read(ptr::addr_of!((*e).tag_gen[t]));
                if tag_gen(th) != tg {
                    tag_invalid = true;
                    break;
                }
            }
            if tag_invalid {
                ptr::write(ptr::addr_of_mut!((*e).state), 2); // tombstone, keep chain
                break;
            }
            // Past the fresh deadline but within the stale window ⇒ serve stale.
            let swr_stale = exp != 0 && exp < now;
            // Past the stale window too ⇒ only usable as an error fallback.
            let error_only = swr_stale && (hard == 0 || hard < now);
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
                stale: swr_stale && !error_only,
                error_only,
            });
            break;
        }
    }
    hit
}

/// Store a response. `tags` are opaque byte strings. Returns false if too large
/// or the cache is disabled.
#[allow(clippy::too_many_arguments)]
pub fn store(
    key: &[u8],
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    ttl: u64,
    swr: u64,
    // `stale-if-error` grace, in seconds past the fresh deadline (0 = off).
    sie: u64,
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
    let now = now_secs();
    // expires_at = fresh deadline; stale_until = fresh + swr (hard deadline).
    // ttl == 0 means forever (both 0). swr == 0 means no stale window.
    let expires = if ttl > 0 { now + ttl } else { 0 };
    let stale_until = if ttl > 0 && swr > 0 {
        expires + swr
    } else {
        expires
    };
    // The error window is measured from the fresh deadline too, so
    // `stale-if-error` is independent of (and usually far longer than) `swr`.
    let error_until = if ttl > 0 && sie > 0 { expires + sie } else { 0 };

    // Snapshot each tag's current generation, so a forget_tag that raced ahead
    // of this store still invalidates us.
    let ntags = tags.len().min(MAX_TAGS);
    let mut th = [0u64; MAX_TAGS];
    let mut tg = [0u64; MAX_TAGS];
    for (i, tag) in tags.iter().take(MAX_TAGS).enumerate() {
        th[i] = hash_bytes(tag);
        tg[i] = tag_gen(th[i]);
    }

    let mut reuse = None; // first tombstone (free to reuse without evicting)
    let mut target = None; // first live slot (eviction victim of last resort)
    for i in 0..PROBE {
        let e = unsafe { p.add((h as usize).wrapping_add(i) % slots) };
        let _g = Slot::lock(e);
        let state = unsafe { ptr::read(ptr::addr_of!((*e).state)) };
        let same = unsafe { ptr::read(ptr::addr_of!((*e).key_hash)) } == h;
        if state == 0 || same {
            unsafe {
                write_entry(
                    e,
                    h,
                    status,
                    &blob,
                    body,
                    expires,
                    stale_until,
                    error_until,
                    ntags,
                    &th,
                    &tg,
                    key,
                )
            };
            return true;
        }
        if state == 2 {
            if reuse.is_none() {
                reuse = Some(e);
            }
        } else if target.is_none() {
            target = Some(e);
        }
    }
    // Window full: reuse a tombstone if present, else evict the first live slot.
    let e = reuse
        .or(target)
        .unwrap_or_else(|| unsafe { p.add((h as usize) % slots) });
    let _g = Slot::lock(e);
    unsafe {
        write_entry(
            e,
            h,
            status,
            &blob,
            body,
            expires,
            stale_until,
            error_until,
            ntags,
            &th,
            &tg,
            key,
        )
    };
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
    stale_until: u64,
    error_until: u64,
    ntags: usize,
    th: &[u64; MAX_TAGS],
    tg: &[u64; MAX_TAGS],
    key: &[u8],
) {
    ptr::write(ptr::addr_of_mut!((*e).state), 1);
    ptr::write(ptr::addr_of_mut!((*e).key_hash), h);
    ptr::write(ptr::addr_of_mut!((*e).expires_at), expires);
    ptr::write(ptr::addr_of_mut!((*e).stale_until), stale_until);
    ptr::write(ptr::addr_of_mut!((*e).error_until), error_until);
    ptr::write(ptr::addr_of_mut!((*e).status), status as u32);
    ptr::write(ptr::addr_of_mut!((*e).ntags), ntags as u32);
    for i in 0..MAX_TAGS {
        ptr::write(ptr::addr_of_mut!((*e).tag_hash[i]), th[i]);
        ptr::write(ptr::addr_of_mut!((*e).tag_gen[i]), tg[i]);
    }
    let klen = key.len().min(KEY_MAX);
    ptr::write(ptr::addr_of_mut!((*e).key_len), klen as u32);
    ptr::copy_nonoverlapping(
        key.as_ptr(),
        ptr::addr_of_mut!((*e).key_bytes) as *mut u8,
        klen,
    );
    ptr::write(ptr::addr_of_mut!((*e).hdr_len), blob.len() as u32);
    ptr::write(ptr::addr_of_mut!((*e).body_len), body.len() as u32);
    ptr::copy_nonoverlapping(
        blob.as_ptr(),
        ptr::addr_of_mut!((*e).hdr) as *mut u8,
        blob.len(),
    );
    ptr::copy_nonoverlapping(
        body.as_ptr(),
        ptr::addr_of_mut!((*e).body) as *mut u8,
        body.len(),
    );
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
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

// --- persistence across restarts ----------------------------------------
//
// The region is a flat array of `Entry` with everything stored inline (no
// pointers), so it can be written to disk verbatim and mapped back on boot. That
// makes a restart cost nothing instead of paying a cold-cache stampede — without
// the synthetic warm-up requests a log-driven warmer would need.

/// File header. Any mismatch makes the dump unusable, on purpose: a layout change
/// must invalidate old files rather than reinterpret their bytes.
#[repr(C)]
struct DumpHeader {
    magic: [u8; 8],
    version: u32,
    entry_size: u32,
    slots: u64,
    tag_slots: u64,
    /// Identifies the deployed application; a mismatch drops the cache.
    app_stamp: u64,
    saved_at: u64,
}

const DUMP_MAGIC: [u8; 8] = *b"ASKRRC01";
const DUMP_VERSION: u32 = 1;

/// Write the response cache and its tag generations to `path`.
///
/// Call only when the workers are gone: a dump taken mid-flight could capture a
/// slot lock in the held state, and `restore` would then hand a deadlock to the
/// next boot. (`restore` zeroes the locks anyway — belt and braces.)
pub fn dump(path: &std::path::Path, app_stamp: u64) -> std::io::Result<usize> {
    use std::io::Write;
    let Some((p, slots)) = base() else {
        return Ok(0);
    };
    let tp = TAGS_PTR.load(Ordering::Acquire);
    if tp.is_null() {
        return Ok(0);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write to a temp file and rename, so a crash mid-write can't leave a torn
    // dump that the next boot would try to read.
    let tmp = path.with_extension("tmp");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);

    let hdr = DumpHeader {
        magic: DUMP_MAGIC,
        version: DUMP_VERSION,
        entry_size: std::mem::size_of::<Entry>() as u32,
        slots: slots as u64,
        tag_slots: TAG_SLOTS as u64,
        app_stamp,
        saved_at: now_secs(),
    };
    // SAFETY: plain old data, written as bytes and validated on read.
    let hdr_bytes = unsafe {
        std::slice::from_raw_parts(
            &hdr as *const DumpHeader as *const u8,
            std::mem::size_of::<DumpHeader>(),
        )
    };
    f.write_all(hdr_bytes)?;

    // SAFETY: the region is quiescent (no workers left) and sized `slots`.
    let entries =
        unsafe { std::slice::from_raw_parts(p as *const u8, slots * std::mem::size_of::<Entry>()) };
    f.write_all(entries)?;
    // SAFETY: tag table, fixed size.
    let tags = unsafe {
        std::slice::from_raw_parts(tp as *const u8, TAG_SLOTS * std::mem::size_of::<TagGen>())
    };
    f.write_all(tags)?;
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, path)?;

    // Count what was actually worth saving, for the log line.
    let now = now_secs();
    let mut live = 0usize;
    for i in 0..slots {
        // SAFETY: quiescent region.
        unsafe {
            let e = p.add(i);
            if ptr::read(ptr::addr_of!((*e).state)) == 1 {
                let hard = ptr::read(ptr::addr_of!((*e).stale_until));
                let err = ptr::read(ptr::addr_of!((*e).error_until));
                let retain = hard.max(err);
                if retain == 0 || retain >= now {
                    live += 1;
                }
            }
        }
    }
    Ok(live)
}

/// Load a dump written by [`dump`]. Returns how many live entries were restored.
///
/// Silently returns 0 when the file is missing, from another build, or describes a
/// different layout — a cache is an optimisation, and refusing to start over a
/// stale file would be the wrong trade.
pub fn restore(path: &std::path::Path, app_stamp: u64) -> usize {
    use std::io::Read;
    let Some((p, slots)) = base() else {
        return 0;
    };
    let tp = TAGS_PTR.load(Ordering::Acquire);
    if tp.is_null() {
        return 0;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return 0;
    };
    let mut hdr_buf = vec![0u8; std::mem::size_of::<DumpHeader>()];
    if f.read_exact(&mut hdr_buf).is_err() {
        return 0;
    }
    // SAFETY: reading POD out of a byte buffer of exactly its size.
    let hdr: DumpHeader = unsafe { ptr::read_unaligned(hdr_buf.as_ptr() as *const DumpHeader) };
    if hdr.magic != DUMP_MAGIC
        || hdr.version != DUMP_VERSION
        || hdr.entry_size != std::mem::size_of::<Entry>() as u32
        || hdr.slots != slots as u64
        || hdr.tag_slots != TAG_SLOTS as u64
    {
        tracing::info!("response cache dump ignored (different build or cache size)");
        return 0;
    }
    if hdr.app_stamp != app_stamp {
        tracing::info!("response cache dump ignored (application changed since it was written)");
        return 0;
    }

    let esize = slots * std::mem::size_of::<Entry>();
    let tsize = TAG_SLOTS * std::mem::size_of::<TagGen>();
    let mut buf = vec![0u8; esize + tsize];
    if f.read_exact(&mut buf).is_err() {
        tracing::warn!("response cache dump is truncated; ignoring");
        return 0;
    }
    // SAFETY: mapped before any fork, so nothing else is touching the region yet;
    // sizes were validated against the header above.
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), p as *mut u8, esize);
        ptr::copy_nonoverlapping(buf.as_ptr().add(esize), tp as *mut u8, tsize);
    }

    // Free every slot lock and drop anything already past its deadline, so a boot
    // never inherits a held lock or serves content that expired while we were down.
    let now = now_secs();
    let mut live = 0usize;
    for i in 0..slots {
        // SAFETY: pre-fork, exclusive access.
        unsafe {
            let e = p.add(i);
            (*e).lock.store(0, Ordering::Relaxed);
            if ptr::read(ptr::addr_of!((*e).state)) != 1 {
                continue;
            }
            let hard = ptr::read(ptr::addr_of!((*e).stale_until));
            let err = ptr::read(ptr::addr_of!((*e).error_until));
            let retain = hard.max(err);
            if retain != 0 && retain < now {
                ptr::write(ptr::addr_of_mut!((*e).state), 0);
            } else {
                live += 1;
            }
        }
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests share one process-wide region (and both `init` it), so run them
    /// one at a time — parallel probing/eviction across them is a flake source.
    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn store_get_and_tag_invalidation() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(64);
        assert!(enabled());

        let hdrs = vec![("Content-Type".to_string(), "text/html".to_string())];
        assert!(store(
            b"GET|/posts",
            200,
            &hdrs,
            b"<h1>posts</h1>",
            60,
            0,
            0,
            &[b"posts".to_vec()]
        ));

        let hit = get(b"GET|/posts").expect("hit");
        assert_eq!(hit.status, 200);
        assert_eq!(hit.body, b"<h1>posts</h1>");
        assert!(!hit.stale);
        assert_eq!(hit.headers[0], ("Content-Type".into(), "text/html".into()));

        // Forgetting the tag invalidates the entry instantly.
        forget_tag(b"posts");
        assert!(get(b"GET|/posts").is_none());

        // A fresh store after invalidation is servable again.
        assert!(store(
            b"GET|/posts",
            200,
            &hdrs,
            b"v2",
            60,
            0,
            0,
            &[b"posts".to_vec()]
        ));
        assert_eq!(get(b"GET|/posts").unwrap().body, b"v2");

        // An untagged entry is unaffected by tag bumps.
        assert!(store(b"GET|/about", 200, &hdrs, b"about", 0, 0, 0, &[]));
        forget_tag(b"posts");
        assert_eq!(get(b"GET|/about").unwrap().body, b"about");

        // Stale-while-revalidate: 1s fresh, +5s stale window. Fresh immediately;
        // past the fresh deadline it is served STALE (not a miss) until the hard
        // deadline.
        assert!(store(b"GET|/swr", 200, &hdrs, b"swrbody", 1, 10, 0, &[]));
        assert!(!get(b"GET|/swr").unwrap().stale);
        // Sleep past the 1s fresh deadline (with margin for the second boundary),
        // staying inside the 10s stale window.
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let s = get(b"GET|/swr").expect("served stale");
        assert!(s.stale);
        assert_eq!(s.body, b"swrbody");
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("/category/tech/*", "/category/tech/rust"));
        assert!(glob_match("/category/tech/*", "/category/tech/"));
        assert!(!glob_match("/category/tech/*", "/category/food/x"));
        assert!(glob_match("*", "/anything"));
        assert!(glob_match("/a/*/c", "/a/b/c"));
        assert!(glob_match("/a/*/c", "/a/b/b/c"));
        assert!(!glob_match("/a/*/c", "/a/b/d"));
        assert!(glob_match("/post?", "/posts"));
        assert!(glob_match("/exact", "/exact"));
        assert!(!glob_match("/exact", "/exactly"));
    }

    #[test]
    fn purge_and_ban_invalidate_by_url() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(256);
        flush();
        let h = vec![("Content-Type".to_string(), "text/html".to_string())];
        // Two encoding variants of one URL, plus neighbours that must survive.
        let k = |pq: &str, enc: &str| format!("GET\0site.no\0{pq}\0{enc}\0").into_bytes();
        assert!(store(&k("/posts/1", "id"), 200, &h, b"a", 60, 0, 0, &[]));
        assert!(store(&k("/posts/1", "gzip"), 200, &h, b"b", 60, 0, 0, &[]));
        assert!(store(
            &k("/posts/1?page=2", "id"),
            200,
            &h,
            b"c",
            60,
            0,
            0,
            &[]
        ));
        assert!(store(&k("/posts/12", "id"), 200, &h, b"d", 60, 0, 0, &[]));
        assert!(store(&k("/about", "id"), 200, &h, b"e", 60, 0, 0, &[]));

        // PURGE /posts/1 drops both encodings *and* the query variant, but must not
        // touch /posts/12 (prefix boundary) or /about.
        assert_eq!(purge_url("site.no", "/posts/1", None), 3);
        assert!(get(&k("/posts/1", "id")).is_none());
        assert!(get(&k("/posts/1", "gzip")).is_none());
        assert!(get(&k("/posts/1?page=2", "id")).is_none());
        assert!(
            get(&k("/posts/12", "id")).is_some(),
            "/posts/12 must survive"
        );
        assert!(get(&k("/about", "id")).is_some());

        // Host scoping: another vhost's identical URL is untouched.
        let other = b"GET\0other.no\0/about\0id\0".to_vec();
        assert!(store(&other, 200, &h, b"x", 60, 0, 0, &[]));
        assert_eq!(purge_url("site.no", "/about", None), 1);
        assert!(get(&other).is_some(), "other host must survive");

        // BAN by glob.
        assert!(store(
            &k("/cat/tech/rust", "id"),
            200,
            &h,
            b"1",
            60,
            0,
            0,
            &[]
        ));
        assert!(store(
            &k("/cat/tech/go", "id"),
            200,
            &h,
            b"2",
            60,
            0,
            0,
            &[]
        ));
        assert!(store(
            &k("/cat/food/pizza", "id"),
            200,
            &h,
            b"3",
            60,
            0,
            0,
            &[]
        ));
        assert_eq!(ban_glob("site.no", "/cat/tech/*"), 2);
        assert!(get(&k("/cat/tech/rust", "id")).is_none());
        assert!(get(&k("/cat/tech/go", "id")).is_none());
        assert!(get(&k("/cat/food/pizza", "id")).is_some());
    }

    #[test]
    fn stale_if_error_is_a_fallback_not_a_hit() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(64);
        let hdrs = vec![("Content-Type".to_string(), "text/html".to_string())];

        // 1s fresh, no swr window, but a long stale-if-error grace.
        assert!(store(b"GET|/sie", 200, &hdrs, b"siebody", 1, 0, 3600, &[]));
        // Fresh: a normal hit, and not an error-only entry.
        let fresh = get(b"GET|/sie").expect("fresh hit");
        assert!(!fresh.stale && !fresh.error_only);

        std::thread::sleep(std::time::Duration::from_millis(2100));

        // Past the fresh deadline with no swr window, `get` must NOT serve it:
        // the request has to run PHP.
        assert!(
            get(b"GET|/sie").is_none(),
            "error-only entry must not be a hit"
        );
        // But it is still held, and usable as a failure fallback.
        let fb = stale_on_error(b"GET|/sie").expect("fallback available");
        assert!(fb.error_only);
        assert_eq!(fb.body, b"siebody");

        // Without stale-if-error, an expired entry is simply gone.
        assert!(store(b"GET|/plain", 200, &hdrs, b"x", 1, 0, 0, &[]));
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert!(get(b"GET|/plain").is_none());
        assert!(stale_on_error(b"GET|/plain").is_none());
    }
}
