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
    /// The reservation this job is currently out under; 0 when nobody holds it.
    ///
    /// This is what `pop` hands back as the job's id, and what `delete`/`release`
    /// look a job up by. It is what fences a stale ack: once a lease lapses and another
    /// worker pops the job, the slot carries a *new* lease, and the first worker's
    /// token matches nothing — "no such job", not somebody else's job.
    lease: u64,
    created_at: u64, // unix ms — when push() accepted the job; never changes
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
const QUEUE_NAME_MAX: usize = 96;

#[repr(C)]
struct Ring {
    /// [`MAGIC`] once the header below is complete. Written last, so a mapping that
    /// died mid-creation is recognised as garbage rather than trusted.
    magic: AtomicU64,
    /// Bumped whenever `Job`'s layout changes. A mapping from another version is not
    /// migrated — it is unlinked and recreated, and the log says so.
    version: u32,
    slots: u32,
    slot_size: u32,
    payload_max: u32,
    name_max: u32,
    _pad0: u32,
    next_id: AtomicU64,
    /// Global, so a lease is never reused by another slot while a stale holder
    /// could still present it.
    next_lease: AtomicU64,
    _pad: [u64; 2],
    /// Per-queue liveness, so "nothing is draining this lane" is a measurement rather
    /// than an inference. See [`Lane`].
    lanes: [Lane; LANES],
    // slots follow, laid out contiguously after the header via the mapping.
}

/// What a worker has actually done with one queue, as opposed to what is sitting in it.
///
/// The backlog watchdog could only ever say "these jobs are old", which is the same
/// observation for two opposite faults: a lane that is saturated (workers draining it
/// as fast as they can, add more) and a lane that nothing consumes at all (a worker
/// polling the wrong name — fix the name). Job age cannot tell them apart, and the
/// remedies are opposite, so the operator had to guess.
///
/// `pop` stamps `last_polled_ms` every time a worker *asks* this lane for work and
/// `last_drained_ms` only when it actually gets a job. The pair separates the faults:
/// pending jobs and no recent poll means nobody is listening to this name; pending jobs,
/// recent polls and no recent drain means jobs are there but not becoming available.
/// Both are stated as facts in the admin API instead of being left to the reader.
///
/// A lane is claimed on first use and never released — a queue name a worker polled
/// once stays visible with zero pending, which is what makes "a worker polls `mail`,
/// nothing is queued" distinguishable from "nobody has ever polled `mail`".
#[repr(C)]
struct Lane {
    /// `hash_q` of the namespaced queue name; 0 means a free entry.
    hash: AtomicU64,
    /// Unix ms when a worker last asked this lane for a job, empty-handed or not.
    last_polled_ms: AtomicU64,
    /// Unix ms when a worker last actually reserved a job from it; 0 = never.
    last_drained_ms: AtomicU64,
    /// Length of `name`, so the reporting side can name the lane without a job in it.
    name_len: AtomicU32,
    _pad: u32,
    name: [u8; QUEUE_NAME_MAX],
}

/// Distinct queue names tracked for liveness. A deployment with more than this many
/// is well past the point where a per-lane table is the useful view, and the overflow
/// degrades to "no liveness signal for that lane" rather than to wrong numbers.
const LANES: usize = 64;

/// "ASKRQUE1" — the header is complete and this is a queue ring.
const MAGIC: u64 = 0x4153_4b52_5155_4531;
/// The `Job`/`Ring` layout this binary understands.
///
/// 2 added [`Lane`] to the header, which moved where the slots start. A persistent ring
/// written by an older binary is therefore recreated rather than misread — `version` is
/// one of the fields `header_mismatch` checks, so that happens with a log line saying why.
const LAYOUT_VERSION: u32 = 2;

/// Is the ring a named mapping that outlives this process tree?
static PERSISTENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Was the ring re-attached from a previous run rather than created fresh?
pub fn persistent() -> bool {
    PERSISTENT.load(Ordering::Relaxed)
}

/// `[queue] persist`, parked here between config resolution and `init`, which runs
/// later and after the resolved config has been consumed.
static PERSIST_NAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_persist_name(name: Option<String>) {
    if let Ok(mut n) = PERSIST_NAME.lock() {
        *n = name;
    }
}

/// Map the ring the way the configuration asked: named when `[queue] persist` is set,
/// anonymous otherwise.
pub fn init_configured(slots: usize) {
    let name = PERSIST_NAME.lock().ok().and_then(|n| n.clone());
    match name {
        Some(n) => init_persistent(slots, &n),
        None => init(slots),
    }
}

/// Unmap and forget the ring so a test can map another. Not for production: the
/// pointer is read lock-free everywhere, and nothing waits for readers here.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let p = NEXT_ID.swap(ptr::null_mut(), Ordering::SeqCst);
    let slots = QUEUE_SLOTS.swap(0, Ordering::SeqCst);
    QUEUE_PTR.store(ptr::null_mut(), Ordering::SeqCst);
    PERSISTENT.store(false, Ordering::Relaxed);
    if !p.is_null() {
        unsafe { libc::munmap(p as *mut libc::c_void, ring_bytes(slots.max(16))) };
    }
}

#[cfg(test)]
pub(crate) fn unlink_for_tests(name: &str) {
    if let Ok(c) = std::ffi::CString::new(shm_name(name)) {
        unsafe { libc::shm_unlink(c.as_ptr()) };
    }
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

/// Per-queue liveness, as the reporting side sees it.
pub struct LaneStats {
    pub name: String,
    /// Unix ms a worker last asked this lane for work; 0 = never.
    pub last_polled_ms: u64,
    /// Unix ms a worker last took a job from it; 0 = never.
    pub last_drained_ms: u64,
}

/// Find this queue's lane, claiming a free entry on first use.
///
/// Open addressing, linear probing from the hash. The claim is a compare-exchange, so
/// two workers racing on a queue name nobody has polled before settle on one entry
/// instead of each taking their own and halving the table.
///
/// Returns `None` only when every entry is taken by some *other* queue: the signal for
/// that lane is then simply absent, which the reporting side renders as "unknown" rather
/// than as "not drained". An absent signal must never read as a fault.
///
/// # Safety
/// `ring` points at a live, writable ring mapping.
unsafe fn lane_for(ring: *mut Ring, qh: u64, name: &[u8]) -> Option<*const Lane> {
    // 0 marks a free entry, so a hash that lands on 0 has to become something else.
    let qh = if qh == 0 { 1 } else { qh };
    let start = (qh % LANES as u64) as usize;
    for probe in 0..LANES {
        let i = (start + probe) % LANES;
        let l = ptr::addr_of_mut!((*ring).lanes[i]);
        let cur = (*l).hash.load(Ordering::Acquire);
        if cur == qh {
            return Some(l);
        }
        if cur == 0 {
            // Claim with the hash FIRST, then write the name.
            //
            // Writing the name first looks friendlier — a reader would never see a
            // claimed lane without one — but it is wrong: two workers on *different*
            // queues probing the same free entry both write their name, and whichever
            // then wins the compare-exchange publishes its hash over the loser's name.
            // The lane would report the wrong queue, which is worse than reporting none.
            //
            // So the hash is the claim, and `name_len` is published with a release store
            // after the bytes. A reader that sees a claimed lane with `name_len == 0` is
            // looking at the gap between the two and skips it; the lane shows up on the
            // next read, microseconds later.
            match (*l)
                .hash
                .compare_exchange(0, qh, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    let n = name.len().min(QUEUE_NAME_MAX);
                    ptr::copy_nonoverlapping(
                        name.as_ptr(),
                        ptr::addr_of_mut!((*l).name) as *mut u8,
                        n,
                    );
                    (*l).name_len.store(n as u32, Ordering::Release);
                    return Some(l);
                }
                // Lost the race. Same queue means the winner claimed it for us too;
                // a different queue means keep probing rather than steal their entry.
                Err(won) if won == qh => return Some(l),
                Err(_) => continue,
            }
        }
    }
    None
}

/// Every claimed lane as `(hash, stored name)` — the raw pairing, for asserting that a
/// lane's name belongs to its hash. Test-only: nothing in production needs the hash.
#[cfg(test)]
pub(crate) fn lane_pairs_for_tests() -> Vec<(u64, Vec<u8>)> {
    let ring = NEXT_ID.load(Ordering::SeqCst);
    if ring.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..LANES {
        unsafe {
            let l = ptr::addr_of!((*ring).lanes[i]);
            let h = (*l).hash.load(Ordering::Acquire);
            let n = ((*l).name_len.load(Ordering::Acquire) as usize).min(QUEUE_NAME_MAX);
            if h == 0 || n == 0 {
                continue;
            }
            let stored = std::slice::from_raw_parts(ptr::addr_of!((*l).name) as *const u8, n);
            out.push((h, stored.to_vec()));
        }
    }
    out
}

/// Note that a worker asked `queue` for work — whether or not it got any.
///
/// This is the signal that separates "nobody is listening to this name" from "this lane
/// is just busy", which job age alone cannot do. Called on every poll, so it stays a
/// single relaxed store on an already-mapped page.
fn mark_polled(qh: u64, name: &[u8], now: u64) {
    let ring = NEXT_ID.load(Ordering::SeqCst);
    if ring.is_null() {
        return;
    }
    unsafe {
        if let Some(l) = lane_for(ring, qh, name) {
            (*l).last_polled_ms.store(now, Ordering::Relaxed);
        }
    }
}

/// Note that a worker actually reserved a job from `queue`.
fn mark_drained(qh: u64, name: &[u8], now: u64) {
    let ring = NEXT_ID.load(Ordering::SeqCst);
    if ring.is_null() {
        return;
    }
    unsafe {
        if let Some(l) = lane_for(ring, qh, name) {
            (*l).last_drained_ms.store(now, Ordering::Relaxed);
        }
    }
}

/// Every lane a worker has polled since the ring was created.
///
/// Includes lanes with nothing queued, which is the point: a lane that is polled and
/// empty is healthy, and a lane with jobs that nobody has ever polled is the fault the
/// aggregate numbers hid. Names are returned with the application namespace stripped,
/// matching [`by_queue`].
pub fn lanes() -> Vec<LaneStats> {
    let ring = NEXT_ID.load(Ordering::SeqCst);
    if ring.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..LANES {
        unsafe {
            let l = ptr::addr_of!((*ring).lanes[i]);
            if (*l).hash.load(Ordering::Acquire) == 0 {
                continue;
            }
            let n = ((*l).name_len.load(Ordering::Acquire) as usize).min(QUEUE_NAME_MAX);
            let stored = std::slice::from_raw_parts(ptr::addr_of!((*l).name) as *const u8, n);
            // Claimed, name not yet published — see `lane_for`. Skipping is right: a
            // lane reported with an empty name is indistinguishable from a queue
            // literally named "", and it becomes readable on the very next pass.
            if n == 0 {
                continue;
            }
            // A lane claimed by another application in a shared ring is not ours to report.
            if !crate::ns::owns(stored) {
                continue;
            }
            out.push(LaneStats {
                name: String::from_utf8_lossy(crate::ns::strip(stored)).into_owned(),
                last_polled_ms: (*l).last_polled_ms.load(Ordering::Relaxed),
                last_drained_ms: (*l).last_drained_ms.load(Ordering::Relaxed),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Map the queue table with `slots` job slots. Call in the master before fork.
/// Map an anonymous ring: shared across the process tree, gone on restart.
pub fn init(slots: usize) {
    if !QUEUE_PTR.load(Ordering::SeqCst).is_null() {
        return;
    }
    let slots = slots.max(16);
    let size = ring_bytes(slots);
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
    // SAFETY: fresh zeroed mapping of `size` bytes.
    unsafe { write_header(p as *mut Ring, slots) };
    publish(p, slots);
    tracing::info!(slots, mib = size / 1024 / 1024, "job queue mapped");
}

/// Map a **named** ring that outlives this process tree, so pending jobs survive a
/// restart — `askr upgrade` included.
///
/// The known issue this closes said real persistence meant a named mapping with a
/// `{magic, version, geometry}` header, and that adding the header alone would freeze
/// a layout whose migration had not been designed. The migration design is: there is
/// none. A mapping whose header does not match this binary — different version,
/// different slot count, different slot size — is unlinked and recreated, and the log
/// names what did not match. Jobs in a mismatched ring are lost exactly as they were
/// lost on every restart before; the difference is that a matching ring keeps them.
///
/// Opt-in, because the mapping lives in `/dev/shm`, and a container's default
/// `/dev/shm` is 64 MiB — smaller than a ring of a few thousand 33 KB slots. Where it
/// cannot be created, this falls back to the anonymous ring and says so; the queue
/// keeps working either way.
///
/// `name` is the operator's; the object is `/askr.q.<hash>` so it fits every
/// platform's limit on POSIX shm names.
pub fn init_persistent(slots: usize, name: &str) {
    if !QUEUE_PTR.load(Ordering::SeqCst).is_null() {
        return;
    }
    let slots = slots.max(16);
    let size = ring_bytes(slots);
    let object = shm_name(name);
    match map_named(&object, size, slots, true) {
        Ok((p, survived)) => {
            PERSISTENT.store(true, Ordering::Relaxed);
            publish(p, slots);
            if survived > 0 {
                tracing::info!(
                    slots,
                    mib = size / 1024 / 1024,
                    object,
                    jobs = survived,
                    "job queue re-attached: jobs survived the restart"
                );
            } else {
                tracing::info!(
                    slots,
                    mib = size / 1024 / 1024,
                    object,
                    "job queue created (persistent)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                object,
                "queue: could not map a persistent ring — falling back to an anonymous \
                 one. Jobs will NOT survive a restart. In a container, raise --shm-size."
            );
            init(slots);
        }
    }
}

fn ring_bytes(slots: usize) -> usize {
    std::mem::size_of::<Ring>() + slots * std::mem::size_of::<Job>()
}

/// `/askr.q.` plus sixteen hex digits: 24 bytes, under macOS's 31-byte limit.
fn shm_name(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    format!("/askr.q.{:016x}", h.finish())
}

fn publish(p: *mut libc::c_void, slots: usize) {
    let jobs = unsafe { (p as *mut u8).add(std::mem::size_of::<Ring>()) } as *mut Job;
    NEXT_ID.store(p as *mut Ring, Ordering::SeqCst);
    QUEUE_SLOTS.store(slots, Ordering::SeqCst);
    QUEUE_PTR.store(jobs, Ordering::SeqCst);
}

/// Fill in a fresh ring's header. `magic` goes last, after a fence, so a creator that
/// dies between the geometry fields and the marker leaves a ring nobody trusts.
///
/// # Safety
/// `ring` points at a writable mapping of at least `ring_bytes(slots)` zeroed bytes.
unsafe fn write_header(ring: *mut Ring, slots: usize) {
    ptr::write(ptr::addr_of_mut!((*ring).version), LAYOUT_VERSION);
    ptr::write(ptr::addr_of_mut!((*ring).slots), slots as u32);
    ptr::write(
        ptr::addr_of_mut!((*ring).slot_size),
        std::mem::size_of::<Job>() as u32,
    );
    ptr::write(ptr::addr_of_mut!((*ring).payload_max), PAYLOAD_MAX as u32);
    ptr::write(ptr::addr_of_mut!((*ring).name_max), QUEUE_NAME_MAX as u32);
    std::sync::atomic::fence(Ordering::Release);
    (*ring).magic.store(MAGIC, Ordering::Release);
}

/// What an existing header would have to say to be ours.
///
/// # Safety
/// `ring` points at a readable mapping at least `size_of::<Ring>()` long.
unsafe fn header_mismatch(ring: *const Ring, slots: usize) -> Option<String> {
    let magic = (*ring).magic.load(Ordering::Acquire);
    if magic != MAGIC {
        return Some(format!(
            "magic {magic:#x} (expected {MAGIC:#x}) — not a ring, or one that died mid-creation"
        ));
    }
    let checks = [
        (
            "version",
            ptr::read(ptr::addr_of!((*ring).version)),
            LAYOUT_VERSION,
        ),
        (
            "slots",
            ptr::read(ptr::addr_of!((*ring).slots)),
            slots as u32,
        ),
        (
            "slot_size",
            ptr::read(ptr::addr_of!((*ring).slot_size)),
            std::mem::size_of::<Job>() as u32,
        ),
        (
            "payload_max",
            ptr::read(ptr::addr_of!((*ring).payload_max)),
            PAYLOAD_MAX as u32,
        ),
        (
            "name_max",
            ptr::read(ptr::addr_of!((*ring).name_max)),
            QUEUE_NAME_MAX as u32,
        ),
    ];
    checks
        .iter()
        .find(|(_, got, want)| got != want)
        .map(|(what, got, want)| format!("{what} {got} (expected {want})"))
}

/// Open or create the named object, validate or (re)create its header, and map it.
/// Returns the mapping and how many jobs were found in it. `retry` allows one
/// unlink-and-recreate when an existing object does not match.
fn map_named(
    object: &str,
    size: usize,
    slots: usize,
    retry: bool,
) -> Result<(*mut libc::c_void, usize), String> {
    let cname = std::ffi::CString::new(object).map_err(|e| e.to_string())?;
    // O_EXCL to learn whether *we* created the object, rather than measuring its size:
    // a POSIX shm object's `fstat` size is reliable on Linux but not on macOS (it can
    // report zero after ftruncate, and ftruncate may run only once), so "did it already
    // exist" is the portable question. EEXIST → open it plainly and read the header.
    // SAFETY: plain libc calls with a valid NUL-terminated name.
    let mut fresh = true;
    let mut fd = unsafe {
        libc::shm_open(
            cname.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            fresh = false;
            fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0o600) };
        }
        if fd < 0 {
            return Err(format!("shm_open: {err}"));
        }
    }
    if fresh {
        // A newly created object has size 0; size it once, now.
        if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
            }
            return Err(format!("ftruncate to {size} bytes: {e}"));
        }
    }
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    unsafe { libc::close(fd) }; // the mapping keeps the object alive
    if p == libc::MAP_FAILED {
        // A pre-existing object too small to map at `size` fails here; recreate it.
        if !fresh {
            return recreate(
                object,
                size,
                slots,
                retry,
                "existing object too small".into(),
            );
        }
        return Err(format!("mmap: {}", std::io::Error::last_os_error()));
    }
    let ring = p as *mut Ring;
    if fresh {
        unsafe { write_header(ring, slots) };
        return Ok((p, 0));
    }
    if let Some(why) = unsafe { header_mismatch(ring, slots) } {
        unsafe { libc::munmap(p, size) };
        return recreate(object, size, slots, retry, why);
    }
    // Ours. Count what survived, for the log.
    let jobs = unsafe { (p as *mut u8).add(std::mem::size_of::<Ring>()) } as *mut Job;
    let survived = (0..slots)
        .filter(|&i| unsafe { r_u64(ptr::addr_of!((*jobs.add(i)).id)) } != 0)
        .count();
    Ok((p, survived))
}

fn recreate(
    object: &str,
    size: usize,
    slots: usize,
    retry: bool,
    why: String,
) -> Result<(*mut libc::c_void, usize), String> {
    if !retry {
        return Err(format!(
            "existing ring does not match and recreation failed: {why}"
        ));
    }
    tracing::warn!(
        object,
        mismatch = %why,
        "queue: an existing persistent ring does not match this binary; recreating it. \
         Any jobs it held are lost — as they were on every restart before persistence."
    );
    let cname = std::ffi::CString::new(object).map_err(|e| e.to_string())?;
    unsafe { libc::shm_unlink(cname.as_ptr()) };
    map_named(object, size, slots, false)
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
    /// The **lease**, not the job's id — the thing to pass back to `delete`/`release`.
    ///
    /// A worker that pops a job holds it under this token. If its lease lapses and
    /// another worker pops the same job, that worker gets a fresh token and this one
    /// stops matching anything. Opaque to the application: the Laravel driver stores
    /// it and hands it back, which is all it ever did with the id.
    pub id: u64,
    pub attempts: u32,
    pub payload: Vec<u8>,
}

/// Enqueue a job. `delay` seconds until it becomes available. Returns the job
/// id, or 0 if the queue is full/disabled/too large.
/// Warned once about pushing into a queue that was never mapped.
static NO_RING_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn push(queue: &[u8], payload: &[u8], delay: u64) -> u64 {
    let queue = &*crate::ns::key(queue);
    let Some((p, slots)) = base() else {
        // No ring: the server was started without queue slots. Returning 0 is all the PHP
        // API can express, and Laravel does not check it — so a dropped job is invisible
        // from the application side. Say it here instead of losing mail in silence.
        // Once per process: this is per push, and a busy app would drown the log.
        if !NO_RING_WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::error!(
                queue = %String::from_utf8_lossy(crate::ns::strip(queue)),
                "queue push DISCARDED — no shared-memory ring is mapped. Start the server \
                 with --queue-slots (or [queue] slots) or jobs pushed from PHP go nowhere: \
                 no exception, no retry, no mail."
            );
        }
        return 0;
    };
    if payload.len() > PAYLOAD_MAX {
        return 0;
    }
    let ring = NEXT_ID.load(Ordering::SeqCst);
    let id = unsafe { (*ring).next_id.fetch_add(1, Ordering::SeqCst) } + 1;
    let qh = hash_q(queue);
    let created_at = now_ms();
    // Saturating throughout: `delay` and `visibility` are PHP integers, and
    // `visibility * 1000` with a large value overflowed — panicking in debug and
    // wrapping to a small `reserved_until` in release, which made a job somebody
    // was running immediately poppable again.
    let available_at = created_at.saturating_add(delay.saturating_mul(1000));
    // Start the probe at a spot derived from the id, so concurrent pushes spread.
    let start = (id as usize) % slots;
    for i in 0..slots {
        let e = unsafe { p.add((start + i) % slots) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) == 0 {
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
                ptr::write(ptr::addr_of_mut!((*e).lease), 0);
                ptr::write(ptr::addr_of_mut!((*e).attempts), 0);
                ptr::write(ptr::addr_of_mut!((*e).payload_len), payload.len() as u32);
                ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    ptr::addr_of_mut!((*e).payload) as *mut u8,
                    payload.len(),
                );
                // `id` last, as the commit marker — the same discipline
                // `broadcast::publish` already uses with its `seq`.
                //
                // It used to be written first. `id != 0` is what makes a slot
                // occupied, so a process that died anywhere in the writes below it
                // left a slot claimed by a job that does not exist: `pop` could hand
                // out the previous occupant's payload under the new id, and nothing
                // ever frees it — one permanently lost slot per crash, until the ring
                // fills up with them. Written last, a crash mid-push leaves `id == 0`
                // and the slot simply still free.
                //
                // The fence is for store order in the compiled code, not for a
                // concurrent reader: every reader takes this slot's lock, and the lock
                // itself is reclaimed from a dead holder by shmlock.
                std::sync::atomic::fence(Ordering::Release);
                ptr::write(ptr::addr_of_mut!((*e).id), id);
                return id;
            }
        }
    }
    // Full. Dropping is the right behaviour — a queue that silently evicted an older
    // job to make room would be worse — but it must not be a *silent* drop. The
    // no-ring branch above already says exactly why: returning 0 is all the PHP API
    // can express and Laravel does not check it, so from the application side a lost
    // job looks like a job that ran. That reasoning applies here word for word, and
    // this path had no log at all.
    //
    // Throttled rather than once-per-process: a full ring is a condition that recurs
    // and then clears, and an operator needs to see it each time it comes back, not
    // only the first time since boot.
    let now = now_ms() / 1000;
    let last = FULL_WARNED_AT.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= FULL_WARN_EVERY_SECS
        && FULL_WARNED_AT
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    {
        tracing::error!(
            queue = %String::from_utf8_lossy(crate::ns::strip(queue)),
            slots,
            "queue push DISCARDED — every slot is occupied. The job is gone: no \
             exception, no retry. Raise --queue-slots (or [queue] slots), or find out \
             why the backlog is not draining."
        );
    }
    0
}

/// Unix seconds of the last "ring full" report, so a busy app cannot drown the log.
static FULL_WARNED_AT: AtomicU64 = AtomicU64::new(0);
const FULL_WARN_EVERY_SECS: u64 = 30;

/// Reserve the oldest ready job for `queue` (available and not live-reserved) for
/// `visibility` seconds. Increments its attempt count. None if nothing ready.
pub fn pop(queue: &[u8], visibility: u64) -> Option<Reserved> {
    let queue = &*crate::ns::key(queue);
    let (p, slots) = base()?;
    let qh = hash_q(queue);
    let now = now_ms();
    // Before the scan, and regardless of what it finds. An empty-handed poll is the
    // observation that matters most: it is the proof that a worker *is* listening to
    // this name, which is what tells a backlog on some other lane apart from a backlog
    // on this one.
    mark_polled(qh, queue, now);
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
            now.saturating_add(visibility.saturating_mul(1000)),
        );
        // A fresh lease for this reservation. Any token handed out for an earlier
        // reservation of this slot — a worker whose lease lapsed — is now stale.
        let ring = NEXT_ID.load(Ordering::SeqCst);
        let lease = (*ring).next_lease.fetch_add(1, Ordering::SeqCst) + 1;
        ptr::write(ptr::addr_of_mut!((*e).lease), lease);
        let _ = id; // the job id stays the slot's occupancy marker; the caller gets the lease
        let plen = (r_u32(ptr::addr_of!((*e).payload_len)) as usize).min(PAYLOAD_MAX);
        let payload =
            std::slice::from_raw_parts(ptr::addr_of!((*e).payload) as *const u8, plen).to_vec();
        // The reservation is committed, so this lane has demonstrably drained.
        mark_drained(qh, queue, now);
        Some(Reserved {
            id: lease,
            attempts,
            payload,
        })
    }
}

/// Delete (ack) a reserved job by its lease. True if the lease is current.
///
/// Looking the job up by *lease* rather than by id is the whole fence. `pop` used to
/// leave the id alone across reservations, so once a lease lapsed and a second worker
/// took the job, the first worker's `delete(id)` acked the job the second was still
/// running — and its `release(id)` made the job poppable a third time. Now a lapsed
/// worker's token names a reservation that no longer exists.
pub fn delete(lease: u64) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    if lease == 0 {
        return false;
    }
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) != 0 && r_u64(ptr::addr_of!((*e).lease)) == lease {
                // Leases are global; the queue name carries the namespace. An
                // application that presents another's token gets "no such job".
                if !slot_in_namespace(e) {
                    return false;
                }
                ptr::write(ptr::addr_of_mut!((*e).id), 0);
                return true;
            }
        }
    }
    false
}

/// Does the job in this slot belong to the current namespace? Caller holds the lock.
unsafe fn slot_in_namespace(e: *mut Job) -> bool {
    let n = (r_u32(ptr::addr_of!((*e).name_len)) as usize).min(QUEUE_NAME_MAX);
    let name = std::slice::from_raw_parts(ptr::addr_of!((*e).name) as *const u8, n);
    crate::ns::owns(name)
}

/// Release a reserved job back to the queue, available again after `delay`
/// seconds (retry). True if it existed.
pub fn release(lease: u64, delay: u64) -> bool {
    let Some((p, slots)) = base() else {
        return false;
    };
    if lease == 0 {
        return false;
    }
    let avail = now_ms().saturating_add(delay.saturating_mul(1000));
    for idx in 0..slots {
        let e = unsafe { p.add(idx) };
        let _g = Slot::lock(e);
        unsafe {
            if r_u64(ptr::addr_of!((*e).id)) != 0 && r_u64(ptr::addr_of!((*e).lease)) == lease {
                if !slot_in_namespace(e) {
                    return false;
                }
                ptr::write(ptr::addr_of_mut!((*e).available_at), avail);
                ptr::write(ptr::addr_of_mut!((*e).reserved_until), 0);
                // The reservation is over; a later pop issues a new lease.
                ptr::write(ptr::addr_of_mut!((*e).lease), 0);
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
    let queue = &*crate::ns::key(queue);
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
            let stored = std::slice::from_raw_parts(ptr::addr_of!((*e).name) as *const u8, n);
            let name = String::from_utf8_lossy(crate::ns::strip(stored)).into_owned();
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
    let queue = &*crate::ns::key(queue);
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
    crate::ffi::guard("squeue::push", 0, || {
        let q = unsafe { crate::ffi::bytes(q, qlen) };
        let payload = unsafe { crate::ffi::bytes(payload, plen) };
        push(q, payload, delay.max(0) as u64) as c_long
    })
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
    crate::ffi::guard("squeue::pop", 0, || {
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
    })
}

extern "C" fn c_delete(id: c_long) -> c_int {
    crate::ffi::guard("squeue::delete", 0, || delete(id.max(0) as u64) as c_int)
}

extern "C" fn c_release(id: c_long, delay: c_long) -> c_int {
    crate::ffi::guard("squeue::release", 0, || {
        release(id.max(0) as u64, delay.max(0) as u64) as c_int
    })
}

extern "C" fn c_size(q: *const c_char, qlen: usize) -> c_long {
    crate::ffi::guard("squeue::size", 0, || {
        let q = unsafe { crate::ffi::bytes(q, qlen) };
        size(q) as c_long
    })
}

extern "C" fn c_counts(
    q: *const c_char,
    qlen: usize,
    out_pending: *mut c_long,
    out_delayed: *mut c_long,
    out_reserved: *mut c_long,
    out_oldest_ms: *mut c_long,
) {
    crate::ffi::guard("squeue::counts", (), || {
        let q = unsafe { crate::ffi::bytes(q, qlen) };
        let c = counts(q);
        unsafe {
            *out_pending = c.pending as c_long;
            *out_delayed = c.delayed as c_long;
            *out_reserved = c.reserved as c_long;
            *out_oldest_ms = c.oldest_pending_created_ms as c_long;
        }
    })
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
    // Shared with the cache tests — see cache::tests::guard for why one lock.
    use crate::ns::tests::GUARD as TEST_GUARD;
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A lane records that a worker *asked*, separately from whether it got anything.
    ///
    /// This is the whole point of the table: job age cannot distinguish a lane nothing
    /// polls from a lane that is merely busy, and those have opposite remedies. The
    /// Félagi outage was the first kind, diagnosed correctly in the log for three days
    /// and invisible everywhere a person would look.
    #[test]
    fn a_poll_is_recorded_separately_from_a_drain() {
        let _g = guard();
        init(128);
        let q = b"lane-liveness";

        // An empty-handed poll still proves a worker is listening to this name.
        assert!(pop(q, 60).is_none(), "nothing queued yet");
        let l = lanes()
            .into_iter()
            .find(|l| l.name == "lane-liveness")
            .expect("polling a queue claims its lane");
        assert!(l.last_polled_ms > 0, "the poll is recorded");
        assert_eq!(
            l.last_drained_ms, 0,
            "nothing was taken, so the lane has never drained"
        );

        // A successful reservation is what marks it drained.
        push(q, b"job", 0);
        assert!(pop(q, 60).is_some(), "the job is there to take");
        let l = lanes()
            .into_iter()
            .find(|l| l.name == "lane-liveness")
            .expect("lane still present");
        assert!(l.last_drained_ms > 0, "the drain is recorded");
    }

    /// Every claimed lane's name must belong to that lane's hash.
    ///
    /// This is the invariant the claim ordering exists to hold. The first version of
    /// `lane_for` wrote the name and *then* compare-exchanged the hash, so two workers on
    /// different queues probing the same free entry both wrote their name and the CAS
    /// winner published its hash over the loser's — a lane reporting the wrong queue,
    /// which is worse than one reporting none, because it sends the operator to a queue
    /// that is working fine.
    ///
    /// **Honest about its reach:** run against the old ordering, this failed 0 times in
    /// 20 — the window between the name write and the CAS is a few instructions wide, and
    /// 24 threads doing one poll each do not reliably land in it. So this guards the
    /// invariant against a future reordering; it is not a reproducer for the race, and
    /// nothing here should be read as proof the old ordering was safe. It wasn't: the
    /// interleaving is reachable, just rare, and two queue *processes* starting together
    /// on different lanes is exactly when it would happen.
    #[test]
    fn concurrent_lane_claims_keep_names_with_their_own_queue() {
        let _g = guard();
        init(256);
        // Enough distinct names to collide on probe positions, well under LANES.
        let names: Vec<String> = (0..24).map(|i| format!("race-lane-{i:02}")).collect();
        std::thread::scope(|sc| {
            for n in &names {
                sc.spawn(move || {
                    // An empty-handed poll is all it takes to claim a lane.
                    let _ = pop(n.as_bytes(), 60);
                });
            }
        });

        // The direct invariant: re-hash what each lane says its name is, and it must be
        // the hash the lane was claimed under.
        for (h, stored) in lane_pairs_for_tests() {
            assert_eq!(
                h,
                hash_q(&stored),
                "lane claimed under {h} carries the name {:?}, which hashes to {} — a \
                 name landed on another queue's entry",
                String::from_utf8_lossy(&stored),
                hash_q(&stored)
            );
        }

        // And every queue that was polled is present exactly once.
        let got: Vec<String> = lanes()
            .into_iter()
            .map(|l| l.name)
            .filter(|n| n.starts_with("race-lane-"))
            .collect();
        let mut uniq = got.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            got.len(),
            "a name appeared on more than one lane: {got:?}"
        );
        for n in &names {
            assert!(got.contains(n), "lane {n} was lost in the race: {got:?}");
        }
    }

    /// A lane a worker polled stays visible after it empties.
    ///
    /// `by_queue` reports queues that hold jobs, so a healthy drained lane vanishes from
    /// it entirely. That is exactly the lane worth seeing: "a worker polls `mail` and
    /// there is nothing queued" is the observation that makes an unattended lane
    /// elsewhere diagnosable rather than ambiguous.
    #[test]
    fn a_drained_lane_is_still_reported() {
        let _g = guard();
        init(128);
        let q = b"lane-emptied";
        push(q, b"job", 0);
        let r = pop(q, 60).expect("job");
        assert!(delete(r.id), "ack it, so the queue is empty");

        assert!(
            !by_queue().iter().any(|(n, _)| n == "lane-emptied"),
            "an empty queue holds no jobs, so by_queue drops it"
        );
        assert!(
            lanes().iter().any(|l| l.name == "lane-emptied"),
            "but the lane is still known, with its liveness intact"
        );
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
    /// `visibility * 1000` overflowed for a large PHP integer: a panic in debug, and
    /// in release a wrapped, tiny `reserved_until` — the job a worker was running became
    /// poppable again at once. Saturation pins it to "never", which is what an absurd
    /// visibility means.
    #[test]
    fn absurd_delays_and_visibilities_saturate_instead_of_wrapping() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(64);
        crate::ns::set("");

        let never = push(b"sat", b"delayed forever", u64::MAX);
        assert!(never > 0, "push must not panic");
        assert!(
            pop(b"sat", 30).is_none(),
            "a saturated delay is not yet available"
        );
        // It can never be popped, so it can never be acked; it stays in the ring.

        assert!(push(b"sat", b"held", 0) > 0);
        let got = pop(b"sat", u64::MAX).expect("a saturated visibility still reserves");
        assert_eq!(got.payload, b"held");
        assert!(
            pop(b"sat", 30).is_none(),
            "and nobody else can pop it meanwhile"
        );
        assert!(delete(got.id));
    }

    /// The known issue this retires. A worker whose lease lapsed could still ack —
    /// or worse, release — the job a second worker had since taken, because both
    /// held the same id. Each reservation now has its own lease, and only the current
    /// one is honoured.
    #[test]
    fn a_stale_lease_cannot_ack_or_release_a_job_someone_else_now_holds() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(64);
        crate::ns::set("");

        assert!(push(b"lease", b"once-only", 0) > 0);
        // Visibility 0: the lease lapses immediately, standing in for a slow worker.
        let first = pop(b"lease", 0).expect("first worker takes it");
        let second = pop(b"lease", 60).expect("lease lapsed; second worker takes it");
        assert_ne!(first.id, second.id, "a new reservation is a new lease");
        assert_eq!(second.attempts, 2);

        // The slow first worker wakes up.
        assert!(
            !delete(first.id),
            "a stale lease must not ack the second worker's job"
        );
        assert!(!release(first.id, 0), "nor put it back for a third run");
        assert!(
            pop(b"lease", 60).is_none(),
            "the job is still held by the second worker"
        );

        // The holder acks with the lease it was actually given.
        assert!(delete(second.id));
        assert!(pop(b"lease", 60).is_none(), "and it is gone");
        assert!(!delete(second.id), "a lease is single-use");
    }

    /// The known issue this closes: the ring was an anonymous mapping and a restart
    /// emptied it. A named ring is re-attached with its jobs — when its header says it
    /// is ours — and recreated, loudly, when it does not.
    #[test]
    fn a_persistent_ring_survives_a_remap_and_a_mismatch_is_recreated() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        crate::ns::set("");
        let name = format!("askr-test-{}", std::process::id());
        unlink_for_tests(&name);

        // First life: create, push, "die".
        reset_for_tests();
        init_persistent(32, &name);
        assert!(persistent(), "a fresh named ring is persistent");
        assert!(push(b"durable", b"survive me", 0) > 0);
        assert_eq!(size(b"durable"), 1);
        reset_for_tests();

        // Second life, same geometry: the job is still there.
        init_persistent(32, &name);
        assert!(persistent());
        assert_eq!(size(b"durable"), 1, "the job survived the remap");
        let got = pop(b"durable", 30).expect("and can be popped");
        assert_eq!(got.payload, b"survive me");
        assert!(delete(got.id));
        reset_for_tests();

        // Third life, different slot count: the header no longer matches, so the ring
        // is recreated rather than misread — and the geometry the new header records is
        // the new one.
        init_persistent(64, &name);
        assert!(persistent());
        assert_eq!(size(b"durable"), 0, "a recreated ring is empty");
        assert!(push(b"durable", b"new life", 0) > 0);
        reset_for_tests();
        init_persistent(64, &name);
        assert_eq!(
            size(b"durable"),
            1,
            "and it persists again under the new geometry"
        );

        // Leave the process on an anonymous ring for the other tests, and clean up.
        reset_for_tests();
        unlink_for_tests(&name);
        init(64);
        assert!(!persistent());
    }

    /// A persistent ring written by an older Askr is recreated, not misread.
    ///
    /// 1.6.0 added the lane table to the ring header, which moved where the job slots
    /// start. Reading a 1.5.x ring with 1.6.0's offsets would hand back garbage as jobs —
    /// so `LAYOUT_VERSION` went to 2 and `header_mismatch` checks it. The existing
    /// persistence test covers a *geometry* mismatch; this covers the one an upgrade
    /// actually produces, because `docs/UPGRADING.md` promises this behaviour by name and
    /// a promise about an upgrade path deserves a test of that path.
    #[test]
    fn a_ring_from_an_older_layout_version_is_recreated_empty() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        crate::ns::set("");
        let name = format!("askr-oldver-{}", std::process::id());
        unlink_for_tests(&name);

        reset_for_tests();
        init_persistent(32, &name);
        assert!(push(b"durable", b"from the old version", 0) > 0);
        assert_eq!(size(b"durable"), 1);

        // Stamp the header back to the previous layout version, exactly as a ring left
        // behind by 1.5.x would read.
        unsafe {
            let ring = NEXT_ID.load(Ordering::SeqCst);
            assert!(!ring.is_null());
            ptr::write(ptr::addr_of_mut!((*ring).version), LAYOUT_VERSION - 1);
        }
        reset_for_tests();

        init_persistent(32, &name);
        assert!(persistent());
        assert_eq!(
            size(b"durable"),
            0,
            "a ring from an older layout must be recreated empty, not read with the \
             wrong slot offset"
        );
        // And the recreated ring is usable, at the current version.
        assert!(push(b"durable", b"new life", 0) > 0);
        reset_for_tests();
        init_persistent(32, &name);
        assert_eq!(size(b"durable"), 1, "and it persists again");

        reset_for_tests();
        unlink_for_tests(&name);
        init(64);
        assert!(!persistent());
    }

    /// Two applications, one ring. A could pop B's jobs — and run B's job classes
    /// inside A's codebase — or acknowledge them by guessing an id. Queue names carry
    /// the namespace now, and an ack from the wrong application is "no such job".
    #[test]
    fn a_job_is_invisible_and_unackable_from_another_namespace() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        init(64);

        crate::ns::set("aaaaaaaaaaaaaaaa");
        let id = push(b"default", b"job-for-a", 0);
        assert!(id > 0);

        crate::ns::set("bbbbbbbbbbbbbbbb");
        assert!(pop(b"default", 30).is_none(), "B does not see A's queue");
        assert_eq!(size(b"default"), 0);
        assert!(!delete(id), "B cannot ack A's job by id");
        assert!(!release(id, 0), "nor release it");

        crate::ns::set("aaaaaaaaaaaaaaaa");
        let got = pop(b"default", 30).expect("A still has its job");
        assert_eq!(got.payload, b"job-for-a");
        assert!(delete(got.id), "and can ack it with the lease it was given");
        // Reporting shows the application's own name, not the prefixed one.
        crate::ns::set("");
    }

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
        assert_eq!(r3.payload, b"job-b");
        assert_ne!(r3.id, r2.id, "a new reservation is a new lease");
        assert!(!delete(r2.id), "and the old lease no longer acks it");
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
