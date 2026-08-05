//! L2 durable queue — the same `push` / `pop` / `delete` / `release` / `size`
//! bridge as the shared-memory queue (`squeue.rs`), but backed by a SQL Anywhere
//! / SQLite database so jobs are **durable, transactional and replicated**
//! across boxes instead of ephemeral and single-box.
//!
//! This is the runtime half of epic elyra-2 (elyra-9). It implements, verbatim,
//! the substrate contract in
//! `sql-anywhere/docs/contracts/QUEUE_CONTRACT.md` — a table plus an atomic
//! `UPDATE … RETURNING` claim giving at-least-once delivery, delayed jobs,
//! priority, a visibility timeout and a dead-letter table. Because the contract
//! is conformance-tested on the substrate side
//! (`sql-anywhere/sqlanywhere/tests/contract_conformance.rs`), this driver
//! builds against a proven spec.
//!
//! Enabled at runtime by setting `ASKR_QUEUE_DB` to the database path (an
//! embedded SQL Anywhere file, an embedded replica, or a `sqld`-managed file);
//! when unset, `queue::register_bridge` falls back to the L1 shared-memory queue.
//! Compiled only with `--features sql-backend`.
//!
//! Each process opens its own connection in WAL mode (safe multi-process access
//! via SQLite file locking), so the pre-fork worker model needs no shared state.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_long};
use std::ptr;

use rusqlite::{params, Connection, OptionalExtension};

/// A reserved job returned by [`pop`] — mirrors `squeue::Reserved`.
pub struct Reserved {
    pub id: u64,
    pub attempts: u32,
    pub payload: Vec<u8>,
}

thread_local! {
    static CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Path to the L2 database, or `None` when the L2 backend is not selected.
pub fn db_path() -> Option<String> {
    std::env::var("ASKR_QUEUE_DB")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Whether the L2 queue backend is selected for this process.
pub fn enabled() -> bool {
    db_path().is_some()
}

// --- schema + operations (pure over a Connection, so they are unit-testable) --

/// Create the queue tables and claim index if absent (QUEUE_CONTRACT §Schema).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS askr_jobs (
           id             INTEGER PRIMARY KEY AUTOINCREMENT,
           queue          TEXT    NOT NULL DEFAULT 'default',
           payload        BLOB    NOT NULL,
           priority       INTEGER NOT NULL DEFAULT 0,
           available_at   INTEGER NOT NULL,
           reserved_until INTEGER,
           attempts       INTEGER NOT NULL DEFAULT 0,
           max_attempts   INTEGER NOT NULL DEFAULT 25,
           created_at     INTEGER NOT NULL DEFAULT (unixepoch()));
         CREATE INDEX IF NOT EXISTS askr_jobs_claim
           ON askr_jobs (queue, reserved_until, priority DESC, available_at, id);
         CREATE TABLE IF NOT EXISTS askr_failed_jobs (
           id        INTEGER PRIMARY KEY AUTOINCREMENT,
           uuid      TEXT, queue TEXT NOT NULL, payload BLOB NOT NULL,
           exception TEXT, attempts INTEGER NOT NULL,
           failed_at INTEGER NOT NULL DEFAULT (unixepoch()));",
    )
}

/// Enqueue a job; `delay` seconds in the future (0 = now). Returns the new id.
fn do_push(conn: &Connection, queue: &[u8], payload: &[u8], delay: u64) -> rusqlite::Result<i64> {
    conn.query_row(
        "INSERT INTO askr_jobs (queue, payload, priority, available_at, max_attempts)
         VALUES (?1, ?2, 0, unixepoch() + ?3, 25) RETURNING id",
        // queue is TEXT in the contract; bind it as text so it compares equal to
        // rows written by any other client (a BLOB never equals a TEXT in SQLite).
        params![String::from_utf8_lossy(queue), payload, delay as i64],
        |r| r.get(0),
    )
}

/// Atomically claim the next ready job for `visibility` seconds (the contract's
/// `UPDATE … RETURNING` claim). Returns `None` when the queue is empty.
fn do_pop(conn: &Connection, queue: &[u8], visibility: u64) -> rusqlite::Result<Option<Reserved>> {
    // prepare_cached: the claim runs on every worker poll — cache the compiled
    // statement on the (persistent, per-process) connection instead of recompiling.
    conn.prepare_cached(
        "UPDATE askr_jobs
         SET reserved_until = unixepoch() + ?2, attempts = attempts + 1
         WHERE id = (
           SELECT id FROM askr_jobs
           WHERE queue = ?1
             AND available_at <= unixepoch()
             AND (reserved_until IS NULL OR reserved_until <= unixepoch())
           ORDER BY priority DESC, available_at, id
           LIMIT 1)
         RETURNING id, payload, attempts",
    )?
    .query_row(
        params![String::from_utf8_lossy(queue), visibility as i64],
        |r| {
            Ok(Reserved {
                id: r.get::<_, i64>(0)? as u64,
                payload: r.get::<_, Vec<u8>>(1)?,
                attempts: r.get::<_, i64>(2)? as u32,
            })
        },
    )
    .optional()
}

/// Ack a job (delete on success). Returns whether a row was removed.
fn do_delete(conn: &Connection, id: u64) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM askr_jobs WHERE id = ?1", params![id as i64])?;
    Ok(n > 0)
}

/// Release (nack): re-arm the job `delay` seconds in the future. `attempts` is
/// left unchanged (it was incremented at claim, matching Laravel).
fn do_release(conn: &Connection, id: u64, delay: u64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE askr_jobs
         SET reserved_until = NULL, available_at = unixepoch() + ?2
         WHERE id = ?1",
        params![id as i64, delay as i64],
    )?;
    Ok(n > 0)
}

/// Ready backlog for a queue (claimable now) — the `size()` the PHP API expects.
fn do_size(conn: &Connection, queue: &[u8]) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM askr_jobs
         WHERE queue = ?1 AND available_at <= unixepoch()
           AND (reserved_until IS NULL OR reserved_until <= unixepoch())",
        params![String::from_utf8_lossy(queue)],
        |r| r.get(0),
    )
}

/// Per-queue counts for Laravel 13's `Queue` contract — the L2 twin of
/// [`crate::squeue::counts`].
///
/// The buckets are evaluated in the same order as the shared-memory backend (reserved,
/// then delayed, then available), because a reserved job can also be past its
/// availability time and counting it twice would break the sum invariant. Note the unit:
/// this table stores seconds, while the FFI contract is milliseconds.
fn do_counts(conn: &Connection, queue: &[u8]) -> rusqlite::Result<(i64, i64, i64, i64)> {
    conn.query_row(
        "SELECT
           count(*) FILTER (WHERE (reserved_until IS NULL OR reserved_until <= unixepoch())
                AND available_at <= unixepoch())                              AS pending,
           count(*) FILTER (WHERE (reserved_until IS NULL OR reserved_until <= unixepoch())
                AND available_at > unixepoch())                               AS delayed,
           count(*) FILTER (WHERE reserved_until IS NOT NULL
                AND reserved_until > unixepoch())                             AS reserved,
           coalesce(min(created_at) FILTER (WHERE
                (reserved_until IS NULL OR reserved_until <= unixepoch())
                AND available_at <= unixepoch()), 0) * 1000                   AS oldest_ms
         FROM askr_jobs WHERE queue = ?1",
        params![String::from_utf8_lossy(queue)],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
}

/// Backlog across all queues: (ready, total, oldest_ms) — the same shape as
/// `squeue::stats()`, feeding the backlog-driven worker autoscaler (elyra-8).
fn do_stats(conn: &Connection) -> rusqlite::Result<(usize, usize, u64)> {
    conn.query_row(
        "SELECT
           count(*) FILTER (WHERE available_at <= unixepoch()
                AND (reserved_until IS NULL OR reserved_until <= unixepoch())) AS ready,
           count(*) AS total,
           coalesce(unixepoch() - min(available_at) FILTER (WHERE available_at <= unixepoch()
                AND (reserved_until IS NULL OR reserved_until <= unixepoch())), 0) AS oldest_secs
         FROM askr_jobs",
        [],
        |r| {
            let ready: i64 = r.get(0)?;
            let total: i64 = r.get(1)?;
            let oldest_secs: i64 = r.get(2)?;
            Ok((
                ready as usize,
                total as usize,
                (oldest_secs.max(0) as u64) * 1000,
            ))
        },
    )
}

// --- per-thread connection + public API used by the C bridge ------------------

fn open() -> Connection {
    let path = db_path().expect("ASKR_QUEUE_DB must be set for the L2 queue");
    let conn = Connection::open(path).expect("open ASKR_QUEUE_DB");
    // WAL + a busy timeout make concurrent worker processes safe.
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;");
    init_schema(&conn).expect("initialize queue schema");
    conn
}

fn with_conn<R>(f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> rusqlite::Result<R> {
    CONN.with(|c| {
        if c.borrow().is_none() {
            *c.borrow_mut() = Some(open());
        }
        f(c.borrow().as_ref().unwrap())
    })
}

pub fn push(queue: &[u8], payload: &[u8], delay: u64) -> u64 {
    with_conn(|c| do_push(c, queue, payload, delay))
        .map(|id| id as u64)
        .unwrap_or(0)
}

pub fn pop(queue: &[u8], visibility: u64) -> Option<Reserved> {
    with_conn(|c| do_pop(c, queue, visibility)).unwrap_or(None)
}

pub fn delete(id: u64) -> bool {
    with_conn(|c| do_delete(c, id)).unwrap_or(false)
}

pub fn release(id: u64, delay: u64) -> bool {
    with_conn(|c| do_release(c, id, delay)).unwrap_or(false)
}

pub fn size(queue: &[u8]) -> u64 {
    with_conn(|c| do_size(c, queue)).unwrap_or(0) as u64
}

/// Per-queue counts; zeros when the backend is unreachable, matching `size()`.
pub fn counts(queue: &[u8]) -> crate::squeue::Counts {
    let (pending, delayed, reserved, oldest) =
        with_conn(|c| do_counts(c, queue)).unwrap_or((0, 0, 0, 0));
    crate::squeue::Counts {
        pending: pending.max(0) as u64,
        delayed: delayed.max(0) as u64,
        reserved: reserved.max(0) as u64,
        oldest_pending_created_ms: oldest.max(0) as u64,
    }
}

/// Backlog across all queues for the worker autoscaler: (ready, total, oldest_ms).
pub fn stats() -> (usize, usize, u64) {
    with_conn(do_stats).unwrap_or((0, 0, 0))
}

// --- PHP bridge (identical shape to squeue.rs) --------------------------------

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

/// Register the L2 queue callbacks with the PHP shim for this process, opening
/// (and migrating) the database connection for this thread.
pub fn register_bridge() {
    if !enabled() {
        return;
    }
    // Open eagerly so a bad path fails at boot, not on the first job.
    let _ = with_conn(|_| Ok(()));
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

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }

    /// The same invariant as the shared-memory backend: every job lands in exactly one
    /// bucket, and the buckets sum to the jobs on the queue. The two backends are an
    /// independent SQL query and a pointer walk, so this is where they're held to the
    /// same meaning — the unit included, since this table stores seconds while the FFI
    /// contract is milliseconds.
    #[test]
    fn counts_bucket_every_job_exactly_once() {
        let c = db();
        do_push(&c, b"q", b"a", 0).unwrap();
        do_push(&c, b"q", b"b", 0).unwrap();
        do_push(&c, b"q", b"c", 3600).unwrap();
        // Another queue must not leak into the counts.
        do_push(&c, b"other", b"x", 0).unwrap();

        let (pending, delayed, reserved, oldest) = do_counts(&c, b"q").unwrap();
        assert_eq!((pending, delayed, reserved), (2, 1, 0));
        assert_eq!(pending, do_size(&c, b"q").unwrap(), "size() == pending");
        assert!(
            oldest > 1_600_000_000_000,
            "milliseconds, not seconds: {oldest} — a factor-of-1000 unit mismatch would \
             make every queue-age reading wrong"
        );

        // Claiming moves a job from pending to reserved; the sum is unchanged.
        let r = do_pop(&c, b"q", 90).unwrap().expect("a ready job");
        let (pending, delayed, reserved, _) = do_counts(&c, b"q").unwrap();
        assert_eq!((pending, delayed, reserved), (1, 1, 1));
        assert_eq!(pending + delayed + reserved, 3);

        assert!(do_delete(&c, r.id).unwrap());
        let (pending, delayed, reserved, _) = do_counts(&c, b"q").unwrap();
        assert_eq!(pending + delayed + reserved, 2);

        // An empty queue reports nothing rather than failing.
        let (p, d, rv, o) = do_counts(&c, b"nothing-here").unwrap();
        assert_eq!((p, d, rv, o), (0, 0, 0, 0));
    }

    #[test]
    fn push_pop_ack_are_contract_shaped() {
        let c = db();
        let id = do_push(&c, b"default", b"email A", 0).unwrap();
        assert!(id > 0);

        let r = do_pop(&c, b"default", 30).unwrap().expect("a ready job");
        assert_eq!(r.id as i64, id);
        assert_eq!(r.payload, b"email A");
        assert_eq!(r.attempts, 1, "attempts incremented at claim");

        // Reserved -> not claimable again immediately.
        assert!(do_pop(&c, b"default", 30).unwrap().is_none());

        // Ack removes it.
        assert!(do_delete(&c, r.id).unwrap());
        assert!(!do_delete(&c, r.id).unwrap());
    }

    #[test]
    fn priority_then_fifo() {
        let c = db();
        let _lo = do_push(&c, b"q", b"lo", 0).unwrap();
        // higher priority inserted after, must still be claimed first (payload
        // bound as a BLOB, matching how the driver always writes it).
        c.execute(
            "INSERT INTO askr_jobs (queue, payload, priority, available_at) \
             VALUES ('q', ?1, 10, unixepoch())",
            params![b"hi".as_slice()],
        )
        .unwrap();
        let first = do_pop(&c, b"q", 30).unwrap().unwrap();
        assert_eq!(first.payload, b"hi", "highest priority first");
    }

    #[test]
    fn at_least_once_redelivery_after_lapse() {
        let c = db();
        let id = do_push(&c, b"q", b"job", 0).unwrap();
        let first = do_pop(&c, b"q", 30).unwrap().unwrap();
        assert_eq!(first.attempts, 1);
        // Worker "dies": force the reservation to lapse.
        c.execute(
            "UPDATE askr_jobs SET reserved_until = unixepoch() - 1 WHERE id = ?1",
            params![id as i64],
        )
        .unwrap();
        let second = do_pop(&c, b"q", 30).unwrap().unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.attempts, 2, "redelivery consumes another attempt");
    }

    #[test]
    fn stats_reports_ready_total_oldest() {
        let c = db();
        do_push(&c, b"q", b"a", 0).unwrap();
        do_push(&c, b"q", b"b", 0).unwrap();
        do_push(&c, b"q", b"later", 3600).unwrap(); // delayed -> not ready
        let (ready, total, _oldest_ms) = do_stats(&c).unwrap();
        assert_eq!(total, 3, "all jobs counted");
        assert_eq!(ready, 2, "delayed job is not ready");
        // Reserve one; it should drop out of the ready count.
        do_pop(&c, b"q", 30).unwrap().unwrap();
        let (ready2, total2, _) = do_stats(&c).unwrap();
        assert_eq!(total2, 3);
        assert_eq!(ready2, 1, "reserved job is not ready");
    }

    #[test]
    fn delayed_job_not_ready_and_release_rearms() {
        let c = db();
        // Delayed one hour: not claimable now, and not counted in ready size.
        do_push(&c, b"q", b"later", 3600).unwrap();
        assert!(do_pop(&c, b"q", 30).unwrap().is_none());
        assert_eq!(do_size(&c, b"q").unwrap(), 0);

        let id = do_push(&c, b"q", b"now", 0).unwrap();
        assert_eq!(do_size(&c, b"q").unwrap(), 1);
        let r = do_pop(&c, b"q", 30).unwrap().unwrap();
        assert_eq!(do_size(&c, b"q").unwrap(), 0, "reserved job is not ready");
        // Release with no backoff re-arms it immediately.
        assert!(do_release(&c, r.id, 0).unwrap());
        assert_eq!(do_size(&c, b"q").unwrap(), 1);
        let again = do_pop(&c, b"q", 30).unwrap().unwrap();
        assert_eq!(again.id as i64, id);
        assert_eq!(again.attempts, 2);
    }
}
