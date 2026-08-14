//! Shared-memory job queue — the last common Redis use (queues) in the binary.
//!
//! A fixed-slot table in an anonymous **shared** mmap (created by the master
//! before fork, so every worker/sidecar sees the same jobs — no IPC). It backs
//! `askr_queue_*` from PHP and a Laravel queue driver.
//!
//! Semantics (what a Laravel queue worker needs):
//! - **push(queue, payload, delay)** — enqueue, optionally available in the
//!   future (delayed jobs).
//! - **pop(queue, visibility)** — reserve the oldest ready job for `visibility`
//!   seconds (so another worker won't take it); returns id + attempts + payload.
//!   A job whose reservation lapsed (worker died) becomes poppable again.
//! - **delete(id)** — ack (job done). **release(id, delay)** — retry later.
//!
//! Robustness mirrors the cache: a per-slot spinlock stolen if a holder dies,
//! and length-clamped reads so a torn write can't cause an out-of-bounds read.

use std::hash::{Hash, Hasher};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const PAYLOAD_MAX: usize = 32 * 1024;

#[repr(C)]
struct Job {
    lock: AtomicU32,
    id: u64, // 0 = free slot
    queue_hash: u64,
    available_at: u64,   // unix ms — poppable when now >= this
    reserved_until: u64, // unix ms — 0 = not reserved; lapsed if now >= this
    created_at: u64,     // unix ms — when push() accepted the job; never changes
    // The queue name, truncated, so a stuck backlog can be *named* rather than just
    // counted. `queue_hash` alone was enough to route jobs and useless to diagnose: an
    // app sending mail to onQueue('mail') while the worker polled 'default' left jobs
    // ageing in the ring with nothing able to say which queue they were on.
    name_len: u32,
    name: [u8; QUEUE_NAME_MAX],
    attempts: u32,
    payload_len: u32,
    payload: [u8; PAYLOAD_MAX],
}

/// Longest queue name kept for reporting. Names are hashed for routing, so a longer name
/// still works — it is only truncated in `askr_queue_stats()` output and warnings.
const QUEUE_NAME_MAX: usize = 48;

#[repr(C)]
struct Ring {
    next_id: AtomicU64,
    _pad: [u64; 7],
    // slots follow, laid out contiguously after the header via the mapping.
}

static QUEUE_PTR: AtomicPtr<Job> = AtomicPtr::new(ptr::null_mut());
static QUEUE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicPtr<Ring> = AtomicPtr::new(ptr::null_mut());

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hash_q(q: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    q.hash(&mut h);
    h.finish()
}

/// Map the queue table with `slots` job slots. Call in the master before fork.
pub fn init(slots: usize) {
    if !QUEUE_PTR.load(Ordering::SeqCst).is_null() {
        return;
    }
    let slots = slots.max(16);
    // A small header (Ring) for the shared id counter + the job slots.
    let header = std::mem::size_of::<Ring>();
    let size = header + slots * std::mem::size_of::<Job>();
    // SAFETY: anonymous shared mapping; zeroed pages are valid (all slots free,
    // next_id = 0).
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
        tracing::warn!("queue: mmap failed; disabled");
        return;
    }
    let jobs = unsafe { (p as *mut u8).add(header) } as *mut Job;
    NEXT_ID.store(p as *mut Ring, Ordering::SeqCst);
    QUEUE_SLOTS.store(slots, Ordering::SeqCst);
    QUEUE_PTR.store(jobs, Ordering::SeqCst);
    tracing::info!(slots, mib = size / 1024 / 1024, "job queue mapped");
}

pub fn enabled() -> bool {
    !QUEUE_PTR.load(Ordering::SeqCst).is_null()
}

fn base() -> Option<(*mut Job, usize)> {
    let p = QUEUE_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        None
    } else {
        Some((p, QUEUE_SLOTS.load(Ordering::SeqCst)))
    }
}

struct Slot(*mut Job);
impl Slot {
    fn lock(e: *mut Job) -> Slot {
        crate::shmlock::acquire(unsafe { &(*e).lock });
        Slot(e)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        crate::shmlock::release(unsafe { &(*self.0).lock });
    }
}

unsafe fn r_u64(p: *const u64) -> u64 {
    ptr::read(p)
}
unsafe fn r_u32(p: *const u32) -> u32 {
    ptr::read(p)
}

/// A reserved job handed to a worker.
pub struct Reserved {
    pub id: u64,
    pub attempts: u32,
    pub payload: Vec<u8>,
}

/// Enqueue a job. `delay` seconds until it becomes available. Returns the job
/// id, or 0 if the queue is full/disabled/too large.
pub fn push(queue: &[u8], payload: &[u8], delay: u64) -> u64 {
    let Some((p, slots)) = base() else {
        return 0;
    };
    if payload.len() > PAYLOAD_MAX {
        return 0;
    }
    let ring = NEXT_ID.load(Ordering::SeqCst);
    let id = unsafe { (*ring).next_id.fetch_add(1, Ordering::SeqCst) } + 1;
    let qh = hash_q(queue);
    let created_at = now_ms();
    let available_at = created_at + delay * 1000;
    // Start the probe at a spot derived from the id, so concurrent pushes spread.
    let start = (id as usize) % slots;
    for i in 0..slots {
        let e = unsafe { p.add((start + i) % slots) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) == 0 {
                ptr::write(ptr::addr_of_mut!((*e).id), id);
                ptr::write(ptr::addr_of_mut!((*e).queue_hash), qh);
                ptr::write(ptr::addr_of_mut!((*e).available_at), available_at);
                ptr::write(ptr::addr_of_mut!((*e).created_at), created_at);
                let n = queue.len().min(QUEUE_NAME_MAX);
                ptr::write(ptr::addr_of_mut!((*e).name_len), n as u32);
                ptr::copy_nonoverlapping(
                    queue.as_ptr(),
                    ptr::addr_of_mut!((*e).name) as *mut u8,
                    n,
                );
                ptr::write(ptr::addr_of_mut!((*e).reserved_until), 0);
                ptr::write(ptr::addr_of_mut!((*e).attempts), 0);
                ptr::write(ptr::addr_of_mut!((*e).payload_len), payload.len() as u32);
                ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    ptr::addr_of_mut!((*e).payload) as *mut u8,
                    payload.len(),
                );
                return id;
            }
        }
    }
    0 // full
}

/// Reserve the oldest ready job for `queue` (available and not live-reserved) for
/// `visibility` seconds. Increments its attempt count. None if nothing ready.
pub fn pop(queue: &[u8], visibility: u64) -> Option<Reserved> {
    let (p, slots) = base()?;
    let qh = hash_q(queue);
    let now = now_ms();
    // First pass: find the best candidate (smallest available_at) without holding
    // a lock across the whole scan.
    let mut best: Option<(usize, u64, u64)> = None; // (idx, available_at, id)
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            let id = r_u64(ptr::addr_of!((*e).id));
            if id == 0 || r_u64(ptr::addr_of!((*e).queue_hash)) != qh {
                continue;
            }
            let avail = r_u64(ptr::addr_of!((*e).available_at));
            let reserved = r_u64(ptr::addr_of!((*e).reserved_until));
            let ready = avail <= now && (reserved == 0 || reserved <= now);
            if ready {
                let better = match best {
                    None => true,
                    Some((_, ba, bid)) => (avail, id) < (ba, bid),
                };
                if better {
                    best = Some((idx, avail, id));
                }
            }
        }
    }
    let (idx, _, want_id) = best?;
    // Second pass: reserve the chosen slot, re-checking it's still that job and
    // still ready (another worker may have taken it).
    let e = unsafe { p.add(idx) };
    let _g = Slot::lock(e);
    unsafe {
        let id = r_u64(ptr::addr_of!((*e).id));
        if id != want_id {
            return None; // taken/changed since the scan; caller can retry
        }
        let reserved = r_u64(ptr::addr_of!((*e).reserved_until));
        if r_u64(ptr::addr_of!((*e).available_at)) > now || (reserved != 0 && reserved > now) {
            return None;
        }
        let attempts = r_u32(ptr::addr_of!((*e).attempts)) + 1;
        ptr::write(ptr::addr_of_mut!((*e).attempts), attempts);
        ptr::write(
            ptr::addr_of_mut!((*e).reserved_until),
            now + visibility * 1000,
        );
        let plen = (r_u32(ptr::addr_of!((*e).payload_len)) as usize).min(PAYLOAD_MAX);
        let payload =
            std::slice::from_raw_parts(ptr::addr_of!((*e).payload) as *const u8, plen).to_vec();
        Some(Reserved {
            id,
            attempts,
            payload,
        })
    }
}

/// Delete (ack) a reserved job. True if it existed.
pub fn delete(id: u64) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) == id {
                ptr::write(ptr::addr_of_mut!((*e).id), 0);
                return true;
            }
        }
    }
    false
}

/// Release a reserved job back to the queue, available again after `delay`
/// seconds (retry). True if it existed.
pub fn release(id: u64, delay: u64) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    let avail = now_ms() + delay * 1000;
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) == id {
                ptr::write(ptr::addr_of_mut!((*e).available_at), avail);
                ptr::write(ptr::addr_of_mut!((*e).reserved_until), 0);
                return true;
            }
        }
    }
    false
}

/// Number of ready (available, not live-reserved) jobs on `queue`.
/// Backlog stats across *all* queues, for autoscaling and metrics:
/// `(ready, total, oldest_ready_ms)`.
///
/// - `ready`   — occupied jobs available now and not live-reserved (waiting for a
///   worker); the signal the master autoscales queue workers on.
/// - `total`   — all occupied jobs (incl. delayed and reserved).
/// - `oldest_ready_ms` — age of the oldest ready job (queue latency).
///
/// Lock-free approximate scan (aligned u64 reads): a slightly stale count is fine
/// for a heuristic gauge, and it keeps the master off the per-slot spinlocks.
pub fn stats() -> (usize, usize, u64) {
    let Some((p, slots)) = base() else {
        return (0, 0, 0);
    };
    let now = now_ms();
    let (mut ready, mut total, mut oldest) = (0usize, 0usize, 0u64);
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) == 0 {
                continue;
            }
            total += 1;
            let avail = r_u64(ptr::addr_of!((*e).available_at));
            let reserved = r_u64(ptr::addr_of!((*e).reserved_until));
            if avail <= now && (reserved == 0 || reserved <= now) {
                ready += 1;
                oldest = oldest.max(now.saturating_sub(avail));
            }
        }
    }
    (ready, total, oldest)
}

/// Counts for one queue, from a single pass over the slot table.
///
/// Laravel 13's `Queue` contract wants these separately, and until 1.4.10 the driver had
/// to report zeros because only [`size`] was reachable from PHP — so `queue:monitor` saw
/// no delayed backlog at all. Reporting invented numbers to a dashboard would have been
/// worse, but reporting none was still lying by omission.
///
/// The three buckets are disjoint and sum to the number of occupied slots for the queue,
/// which is the invariant worth testing: a job is reserved, or waiting for its delay, or
/// available now.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Delay elapsed, no live reservation — a worker can take it now.
    pub pending: u64,
    /// Waiting for `available_at`.
    pub delayed: u64,
    /// Held by a worker whose visibility window hasn't lapsed.
    pub reserved: u64,
    /// `created_at` of the oldest pending job, unix ms; 0 when there are none.
    pub oldest_pending_created_ms: u64,
}

/// Walk the table once and bucket every occupied slot for `queue`.
pub fn counts(queue: &[u8]) -> Counts {
    let Some((p, slots)) = base() else {
        return Counts::default();
    };
    let qh = hash_q(queue);
    let now = now_ms();
    let mut c = Counts::default();
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            let id = r_u64(ptr::addr_of!((*e).id));
            if id == 0 || r_u64(ptr::addr_of!((*e).queue_hash)) != qh {
                continue;
            }
            let avail = r_u64(ptr::addr_of!((*e).available_at));
            let reserved = r_u64(ptr::addr_of!((*e).reserved_until));
            // Order matters: a reserved job may also be past its availability time, and
            // counting it in both buckets would break the sum invariant.
            if reserved > now {
                c.reserved += 1;
            } else if avail > now {
                c.delayed += 1;
            } else {
                c.pending += 1;
                let created = r_u64(ptr::addr_of!((*e).created_at));
                if c.oldest_pending_created_ms == 0 || created < c.oldest_pending_created_ms {
                    c.oldest_pending_created_ms = created;
                }
            }
        }
    }
    c
}

/// Every queue that currently holds a job, with its counts — for the backlog watchdog
/// and per-queue reporting.
///
/// Grouped by the stored name rather than the hash, because the point is to be able to
/// say *which* queue is stuck. Aggregate numbers were what made today's failure hard to
/// see: "1 job ready" was true, and told nobody that it was on `mail` while the only
/// worker polled `default`.
pub fn by_queue() -> Vec<(String, Counts)> {
    let Some((p, slots)) = base() else {
        return Vec::new();
    };
    let now = now_ms();
    let mut out: std::collections::HashMap<String, Counts> = std::collections::HashMap::new();
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) == 0 {
                continue;
            }
            let n = (ptr::read(ptr::addr_of!((*e).name_len)) as usize).min(QUEUE_NAME_MAX);
            let name = String::from_utf8_lossy(std::slice::from_raw_parts(
                ptr::addr_of!((*e).name) as *const u8,
                n,
            ))
            .into_owned();
            let avail = r_u64(ptr::addr_of!((*e).available_at));
            let reserved = r_u64(ptr::addr_of!((*e).reserved_until));
            let created = r_u64(ptr::addr_of!((*e).created_at));
            let c = out.entry(name).or_default();
            // Same bucket order as `counts()`, so the two can never disagree.
            if reserved > now {
                c.reserved += 1;
            } else if avail > now {
                c.delayed += 1;
            } else {
                c.pending += 1;
                if c.oldest_pending_created_ms == 0 || created < c.oldest_pending_created_ms {
                    c.oldest_pending_created_ms = created;
                }
            }
        }
    }
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_by(|a, b| b.1.pending.cmp(&a.1.pending).then_with(|| a.0.cmp(&b.0)));
    v
}

pub fn size(queue: &[u8]) -> u64 {
    let Some((p, slots)) = base() else {
        return 0;
    };
    let qh = hash_q(queue);
    let now = now_ms();
    let mut n = 0;
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            let id = r_u64(ptr::addr_of!((*e).id));
            if id == 0 || r_u64(ptr::addr_of!((*e).queue_hash)) != qh {
                continue;
            }
            let avail = r_u64(ptr::addr_of!((*e).available_at));
            let reserved = r_u64(ptr::addr_of!((*e).reserved_until));
            if avail <= now && (reserved == 0 || reserved <= now) {
                n += 1;
            }
        }
    }
    n
}

// --- PHP bridge -----------------------------------------------------------

use std::ffi::{c_char, c_int, c_long};

extern "C" fn c_push(
    q: *const c_char,
    qlen: usize,
    payload: *const c_char,
    plen: usize,
    delay: c_long,
) -> c_long {
    let q = unsafe { crate::ffi::bytes(q, qlen) };
    let payload = unsafe { crate::ffi::bytes(payload, plen) };
    push(q, payload, delay.max(0) as u64) as c_long
}

#[allow(clippy::too_many_arguments)]
extern "C" fn c_pop(
    q: *const c_char,
    qlen: usize,
    visibility: c_long,
    out_id: *mut c_long,
    out_attempts: *mut c_int,
    out_payload: *mut *mut c_char,
    out_len: *mut usize,
) -> c_int {
    let q = unsafe { crate::ffi::bytes(q, qlen) };
    match pop(q, visibility.max(0) as u64) {
        Some(r) => {
            let buf = unsafe { libc::malloc(r.payload.len().max(1)) } as *mut u8;
            if buf.is_null() {
                return 0;
            }
            unsafe {
                ptr::copy_nonoverlapping(r.payload.as_ptr(), buf, r.payload.len());
                *out_id = r.id as c_long;
                *out_attempts = r.attempts as c_int;
                *out_payload = buf as *mut c_char;
                *out_len = r.payload.len();
            }
            1
        }
        None => 0,
    }
}

extern "C" fn c_delete(id: c_long) -> c_int {
    delete(id.max(0) as u64) as c_int
}

extern "C" fn c_release(id: c_long, delay: c_long) -> c_int {
    release(id.max(0) as u64, delay.max(0) as u64) as c_int
}

extern "C" fn c_size(q: *const c_char, qlen: usize) -> c_long {
    let q = unsafe { crate::ffi::bytes(q, qlen) };
    size(q) as c_long
}

extern "C" fn c_counts(
    q: *const c_char,
    qlen: usize,
    out_pending: *mut c_long,
    out_delayed: *mut c_long,
    out_reserved: *mut c_long,
    out_oldest_ms: *mut c_long,
) {
    let q = unsafe { crate::ffi::bytes(q, qlen) };
    let c = counts(q);
    unsafe {
        *out_pending = c.pending as c_long;
        *out_delayed = c.delayed as c_long;
        *out_reserved = c.reserved as c_long;
        *out_oldest_ms = c.oldest_pending_created_ms as c_long;
    }
}

/// Register the queue callbacks with the PHP shim for this process.
pub fn register_bridge() {
    if !enabled() {
        return;
    }
    // SAFETY: one-time registration; trampolines are 'static fns.
    unsafe {
        askr_php::queue_bridge::askr_php_set_queue_bridge(
            c_push, c_pop, c_delete, c_release, c_size, c_counts,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests share the process-wide job ring — `init()` maps it once and every push
    // lands in the same table — so they have to be serialized. Unique queue names keep the
    // *counts* apart, but not the slot table itself, and `by_queue()` walks all of it.
    //
    // `cache.rs` has had this guard for the same reason; squeue did not, and the tests
    // passed for weeks on scheduling luck. A Dependabot bump of `cc` and `clap` — crates
    // that touch nothing at runtime — was enough to change compile output, reshuffle test
    // timing, and fail one run in ten. The dependency bump was not the bug; it was the
    // thing that finally showed it.
    //
    // `into_inner` ignores poisoning so one failing test doesn't cascade into the rest.
    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn counts_bucket_every_job_exactly_once() {
        let _g = guard();
        init(128);
        assert!(enabled());
        let q = b"counts-test";

        assert_eq!(counts(q), Counts::default(), "empty queue reports nothing");

        // Two available now, one delayed a minute.
        push(q, b"a", 0);
        push(q, b"b", 0);
        push(q, b"c", 60);

        let c = counts(q);
        assert_eq!(c.pending, 2);
        assert_eq!(c.delayed, 1);
        assert_eq!(c.reserved, 0);
        assert_eq!(
            c.pending + c.delayed + c.reserved,
            3,
            "the buckets must sum to the occupied slots — a job counted twice or not at \
             all is what makes a queue dashboard lie"
        );
        assert!(
            c.oldest_pending_created_ms > 0,
            "a pending job has a creation time"
        );
        assert_eq!(
            size(q),
            c.pending,
            "size() and pending must agree, or every existing queue:monitor threshold \
             silently changes meaning"
        );

        // Popping reserves: the job moves from pending to reserved, sum unchanged.
        let job = pop(q, 90).expect("a job is available");
        let c = counts(q);
        assert_eq!(c.pending, 1);
        assert_eq!(c.reserved, 1);
        assert_eq!(c.delayed, 1);
        assert_eq!(c.pending + c.delayed + c.reserved, 3);

        // Deleting removes it from every bucket.
        assert!(delete(job.id));
        let c = counts(q);
        assert_eq!(c.pending + c.delayed + c.reserved, 2);

        // Another queue's jobs must not leak into these counts.
        push(b"other-queue", b"x", 0);
        assert_eq!(counts(q).pending, 1, "counts are per queue");

        // The oldest pending job's creation time is the *earliest*, not the latest.
        let c = counts(q);
        let first = c.oldest_pending_created_ms;
        std::thread::sleep(std::time::Duration::from_millis(5));
        push(q, b"newer", 0);
        assert_eq!(
            counts(q).oldest_pending_created_ms,
            first,
            "a newer job must not move the oldest-pending timestamp forward"
        );
    }

    /// The watchdog can only name a stuck queue if the name is in the ring. Before
    /// 1.4.11 only the hash was, which routed jobs perfectly and diagnosed nothing.
    #[test]
    fn by_queue_names_each_backlog_separately() {
        let _g = guard();
        init(256);
        // Unique names: the ring is a process-global, so a test that used "default" would
        // see (and be seen by) every other test in this module. A shared fixture that
        // passes alone and fails in a suite is worse than no fixture.
        let (mail, deflt) = (b"bq-mail".as_slice(), b"bq-default".as_slice());
        push(mail, b"reset link", 0);
        push(mail, b"invitation", 0);
        push(deflt, b"something", 0);
        push(mail, b"later", 3600);

        let by: std::collections::HashMap<String, Counts> = by_queue().into_iter().collect();
        let m = by
            .get("bq-mail")
            .expect("the bq-mail queue is reported by name");
        assert_eq!(m.pending, 2, "two available on bq-mail");
        assert_eq!(m.delayed, 1, "one still waiting on bq-mail");
        assert_eq!(by.get("bq-default").map(|c| c.pending), Some(1));
        assert!(
            m.oldest_pending_created_ms > 0,
            "an age is what turns a count into a warning"
        );

        // This is the shape of the failure the watchdog exists for: jobs on one queue,
        // the worker draining another. Draining `default` must leave `mail` untouched.
        let job = pop(deflt, 90).expect("a default job");
        assert!(delete(job.id));
        let by: std::collections::HashMap<String, Counts> = by_queue().into_iter().collect();
        assert!(!by.contains_key("bq-default"), "bq-default drained");
        assert_eq!(
            by.get("bq-mail").map(|c| c.pending),
            Some(2),
            "bq-mail is still stuck — exactly the state that used to be invisible"
        );

        // A name longer than the stored field must not corrupt the neighbouring fields.
        let long = format!("bq-{}", "q".repeat(QUEUE_NAME_MAX + 20)).into_bytes();
        push(&long, b"x", 0);
        let by: std::collections::HashMap<String, Counts> = by_queue().into_iter().collect();
        let truncated = String::from_utf8_lossy(&long[..QUEUE_NAME_MAX]).into_owned();
        assert_eq!(
            by.get(&truncated).map(|c| c.pending),
            Some(1),
            "a long name is truncated for reporting, not dropped"
        );
        assert_eq!(
            size(&long),
            1,
            "routing still uses the full name, so the job is findable by its real name"
        );
    }

    #[test]
    fn push_pop_delay_reserve_release() {
        let _g = guard();
        init(128);
        assert!(enabled());

        // FIFO-ish by availability.
        let a = push(b"default", b"job-a", 0);
        let b = push(b"default", b"job-b", 0);
        assert!(a > 0 && b > 0 && a != b);
        assert_eq!(size(b"default"), 2);

        // pop reserves the oldest; a second pop gets the next, not the reserved.
        let r1 = pop(b"default", 60).expect("first");
        assert_eq!(r1.payload, b"job-a");
        assert_eq!(r1.attempts, 1);
        let r2 = pop(b"default", 60).expect("second");
        assert_eq!(r2.payload, b"job-b");

        // nothing else ready (both reserved).
        assert!(pop(b"default", 60).is_none());

        // delete (ack) one, release (retry) the other.
        assert!(delete(r1.id));
        assert!(release(r2.id, 0));
        let r3 = pop(b"default", 60).expect("released job comes back");
        assert_eq!(r3.id, r2.id);
        assert_eq!(r3.attempts, 2); // attempt count carried across the retry
        assert!(delete(r3.id));

        // delayed job isn't popped early.
        let d = push(b"default", b"later", 3600);
        assert!(d > 0);
        assert!(pop(b"default", 60).is_none());
        assert_eq!(size(b"default"), 0); // not counted as ready

        // queue isolation.
        assert!(push(b"emails", b"mail-1", 0) > 0);
        assert_eq!(pop(b"default", 60).map(|r| r.payload), None);
        assert_eq!(pop(b"emails", 60).unwrap().payload, b"mail-1");
    }
}
