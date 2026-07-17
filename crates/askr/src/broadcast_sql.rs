//! L2 durable pub/sub — the same `publish` / `current_seq` / `read_from` surface
//! and `askr_broadcast()` bridge as the shared-memory broadcast ring
//! (`broadcast.rs`), backed by a SQL Anywhere / SQLite append-only topic table
//! so a publish on one node reaches SSE / Pusher subscribers on **every** node
//! via the replication log — with no Redis pub/sub.
//!
//! Runtime half of epic elyra-2 (elyra-13). It implements the substrate contract
//! `sql-anywhere/docs/contracts/PUBSUB_CONTRACT.md`: an append-only `askr_events`
//! table, publish = `INSERT`, subscribe = tail rows past a cursor. The contract
//! is conformance-tested on the substrate side
//! (`sql-anywhere/sqlanywhere/tests/contract_conformance.rs`).
//!
//! Enabled by setting `ASKR_BROADCAST_DB` to the database path; unset falls back
//! to the L1 shared-memory ring. Compiled only with `--features sql-backend`.
//!
//! Cross-node delivery is the replication log: every `INSERT` is shipped in
//! order to each replica, and the local SSE fan-out task tails its local copy.
//! (A follow-up can replace the 50 ms poll with an update-hook wakeup.)

use std::cell::RefCell;
use std::ffi::{c_char, c_int};

use rusqlite::types::Value;
use rusqlite::{params, Connection};

/// A delivered event: (channel, payload) — matches `broadcast::Delivered`.
pub type Delivered = (Vec<u8>, Vec<u8>);

/// Max events pulled per tail query; the poller continues from the new cursor.
const BATCH: i64 = 256;

thread_local! {
    static CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Path to the L2 pub/sub database, or `None` when the backend is not selected.
pub fn db_path() -> Option<String> {
    std::env::var("ASKR_BROADCAST_DB")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Whether the L2 pub/sub backend is selected for this process.
pub fn enabled() -> bool {
    db_path().is_some()
}

// --- schema + operations (pure over a Connection, so they are unit-testable) --

/// Create the append-only topic table, tail index and subscriber cursors (PUBSUB_CONTRACT §Schema).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS askr_events (
           seq INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL,
           payload BLOB NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()));
         CREATE INDEX IF NOT EXISTS askr_events_chan ON askr_events (channel, seq);
         CREATE TABLE IF NOT EXISTS askr_subscribers (
           name TEXT PRIMARY KEY, cursor INTEGER NOT NULL DEFAULT 0,
           updated_at INTEGER NOT NULL DEFAULT (unixepoch()));",
    )
}

fn value_to_bytes(v: Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b,
        Value::Text(s) => s.into_bytes(),
        Value::Integer(i) => i.to_string().into_bytes(),
        Value::Real(f) => f.to_string().into_bytes(),
        Value::Null => Vec::new(),
    }
}

/// Publish an event (append to the topic). Returns the new seq.
fn do_publish(conn: &Connection, chan: &[u8], payload: &[u8]) -> rusqlite::Result<i64> {
    conn.query_row(
        "INSERT INTO askr_events (channel, payload) VALUES (?1, ?2) RETURNING seq",
        params![String::from_utf8_lossy(chan), payload],
        |r| r.get(0),
    )
}

/// The latest seq — a subscriber starts here to skip history.
fn do_current_seq(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT coalesce(max(seq), 0) FROM askr_events", [], |r| {
        r.get(0)
    })
}

/// Tail events with seq in `(last, …]`, up to `BATCH`. Returns events + new cursor.
fn do_read_from(conn: &Connection, last: i64) -> rusqlite::Result<(Vec<Delivered>, i64)> {
    let mut stmt = conn.prepare(
        "SELECT seq, channel, payload FROM askr_events
         WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let mut new_last = last;
    let mut out = Vec::new();
    let rows = stmt.query_map(params![last, BATCH], |r| {
        let seq: i64 = r.get(0)?;
        let chan: String = r.get(1)?;
        let payload: Value = r.get(2)?;
        Ok((seq, chan.into_bytes(), value_to_bytes(payload)))
    })?;
    for row in rows {
        let (seq, chan, payload) = row?;
        new_last = new_last.max(seq);
        out.push((chan, payload));
    }
    Ok((out, new_last))
}

// --- per-thread connection + public API ---------------------------------------

fn open() -> Connection {
    let path = db_path().expect("ASKR_BROADCAST_DB must be set for the L2 pub/sub");
    let conn = Connection::open(path).expect("open ASKR_BROADCAST_DB");
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;");
    init_schema(&conn).expect("initialize pub/sub schema");
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

pub fn publish(chan: &[u8], payload: &[u8]) -> bool {
    with_conn(|c| do_publish(c, chan, payload)).is_ok()
}

pub fn current_seq() -> u64 {
    with_conn(do_current_seq).unwrap_or(0).max(0) as u64
}

pub fn read_from(last: u64) -> (Vec<Delivered>, u64) {
    match with_conn(|c| do_read_from(c, last as i64)) {
        Ok((events, nl)) => (events, nl.max(0) as u64),
        Err(_) => (Vec::new(), last),
    }
}

// --- PHP bridge (identical shape to broadcast.rs) -----------------------------

extern "C" fn c_broadcast(
    chan: *const c_char,
    clen: usize,
    payload: *const c_char,
    plen: usize,
) -> c_int {
    let chan = unsafe { std::slice::from_raw_parts(chan as *const u8, clen) };
    let payload = unsafe { std::slice::from_raw_parts(payload as *const u8, plen) };
    publish(chan, payload) as c_int
}

/// Register the L2 broadcast callback with the PHP shim for this process.
pub fn register_bridge() {
    if !enabled() {
        return;
    }
    let _ = with_conn(|_| Ok(()));
    unsafe { askr_php::broadcast_bridge::askr_php_set_broadcast_bridge(c_broadcast) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }

    #[test]
    fn publish_is_monotonic_and_current_seq_tracks_it() {
        let c = db();
        assert_eq!(do_current_seq(&c).unwrap(), 0);
        let s1 = do_publish(&c, b"orders", b"o1").unwrap();
        let s2 = do_publish(&c, b"orders", b"o2").unwrap();
        assert!(s2 > s1);
        assert_eq!(do_current_seq(&c).unwrap(), s2);
    }

    #[test]
    fn tail_returns_only_events_after_cursor() {
        let c = db();
        do_publish(&c, b"orders", b"o1").unwrap();
        do_publish(&c, b"chat", b"hi").unwrap();

        // From cursor 0: both events, in seq order, channel + payload preserved.
        let (events, last) = do_read_from(&c, 0).unwrap();
        assert_eq!(
            events,
            vec![
                (b"orders".to_vec(), b"o1".to_vec()),
                (b"chat".to_vec(), b"hi".to_vec()),
            ]
        );

        // Publish more; tailing past `last` yields only the new event.
        do_publish(&c, b"orders", b"o3").unwrap();
        let (newer, _last2) = do_read_from(&c, last).unwrap();
        assert_eq!(newer, vec![(b"orders".to_vec(), b"o3".to_vec())]);
    }

    #[test]
    fn empty_tail_keeps_cursor() {
        let c = db();
        let (events, last) = do_read_from(&c, 0).unwrap();
        assert!(events.is_empty());
        assert_eq!(last, 0);
    }
}
