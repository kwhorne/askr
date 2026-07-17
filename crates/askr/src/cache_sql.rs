//! L2 durable cache — the same `get` / `set` / `add` / `delete` / `increment` /
//! `touch` / `flush` / `forget_tag` bridge as the shared-memory cache
//! (`cache.rs`), backed by a SQL Anywhere / SQLite database so entries (and
//! locks and counters) are **durable, transactional and replicated** instead of
//! ephemeral and single-box.
//!
//! Runtime half of epic elyra-2 (elyra-10). It implements, verbatim, the
//! substrate contract in `sql-anywhere/docs/contracts/CACHE_CONTRACT.md` — a
//! table with an expiry column giving TTL get/set, atomic counters, atomic
//! `add` (SETNX / `Cache::lock()`) and tag invalidation — which is
//! conformance-tested on the substrate side
//! (`sql-anywhere/sqlanywhere/tests/contract_conformance.rs`).
//!
//! Enabled by setting `ASKR_CACHE_DB` to the database path; unset falls back to
//! the L1 shared-memory cache. Compiled only with `--features sql-backend`.
//! Each process opens its own WAL connection.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_long};
use std::ptr;

use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};

thread_local! {
    static CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Path to the L2 cache database, or `None` when the backend is not selected.
pub fn db_path() -> Option<String> {
    std::env::var("ASKR_CACHE_DB")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Whether the L2 cache backend is selected for this process.
pub fn enabled() -> bool {
    db_path().is_some()
}

// --- schema + operations (pure over a Connection, so they are unit-testable) --

/// Create the cache table, expiry index, live view and tag map (CACHE_CONTRACT §Schema).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS askr_cache (
           key TEXT PRIMARY KEY, value BLOB NOT NULL, expires_at INTEGER);
         CREATE INDEX IF NOT EXISTS askr_cache_expiry ON askr_cache (expires_at);
         CREATE VIEW IF NOT EXISTS askr_cache_live AS
           SELECT key, value FROM askr_cache
           WHERE expires_at IS NULL OR expires_at > unixepoch();
         CREATE TABLE IF NOT EXISTS askr_cache_tags (
           tag TEXT NOT NULL, key TEXT NOT NULL, PRIMARY KEY (tag, key));
         CREATE INDEX IF NOT EXISTS askr_cache_tags_key ON askr_cache_tags (key);",
    )
}

/// Coerce any stored cell into opaque bytes, so a counter written as an INTEGER
/// (by `increment`) still reads back as the bytes PHP expects (e.g. `b\"8\"`).
fn value_to_bytes(v: Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Blob(b) => Some(b),
        Value::Text(s) => Some(s.into_bytes()),
        Value::Integer(i) => Some(i.to_string().into_bytes()),
        Value::Real(f) => Some(f.to_string().into_bytes()),
    }
}

fn do_get(conn: &Connection, key: &[u8]) -> rusqlite::Result<Option<Vec<u8>>> {
    let v: Option<Value> = conn
        .query_row(
            "SELECT value FROM askr_cache_live WHERE key = ?1",
            params![String::from_utf8_lossy(key)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(value_to_bytes))
}

/// Set with TTL (`ttl` seconds; 0 = forever).
fn do_set(conn: &Connection, key: &[u8], val: &[u8], ttl: u64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO askr_cache (key, value, expires_at)
         VALUES (?1, ?2, CASE WHEN ?3 > 0 THEN unixepoch() + ?3 ELSE NULL END)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
        params![String::from_utf8_lossy(key), val, ttl as i64],
    )?;
    Ok(())
}

/// Atomic add (SETNX): acquire only if absent or expired. Returns whether written.
fn do_add(conn: &Connection, key: &[u8], val: &[u8], ttl: u64) -> rusqlite::Result<bool> {
    let acquired: Option<i64> = conn
        .query_row(
            "INSERT INTO askr_cache (key, value, expires_at)
             VALUES (?1, ?2, CASE WHEN ?3 > 0 THEN unixepoch() + ?3 ELSE NULL END)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at
               WHERE askr_cache.expires_at IS NOT NULL AND askr_cache.expires_at <= unixepoch()
             RETURNING 1",
            params![String::from_utf8_lossy(key), val, ttl as i64],
            |r| r.get(0),
        )
        .optional()?;
    Ok(acquired.is_some())
}

fn do_delete(conn: &Connection, key: &[u8]) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM askr_cache WHERE key = ?1",
        params![String::from_utf8_lossy(key)],
    )?;
    Ok(n > 0)
}

/// Atomic increment/decrement; missing or expired is treated as 0. Returns the new value.
fn do_increment(conn: &Connection, key: &[u8], delta: i64, ttl: u64) -> rusqlite::Result<i64> {
    conn.query_row(
        "INSERT INTO askr_cache (key, value, expires_at)
         VALUES (?1, ?2, CASE WHEN ?3 > 0 THEN unixepoch() + ?3 ELSE NULL END)
         ON CONFLICT(key) DO UPDATE SET value = CAST(
           CASE WHEN expires_at IS NOT NULL AND expires_at <= unixepoch() THEN 0 ELSE value END AS INTEGER) + ?2
         RETURNING CAST(value AS INTEGER)",
        params![String::from_utf8_lossy(key), delta, ttl as i64],
        |r| r.get(0),
    )
}

/// Refresh TTL on a live key without reading/writing the value. Returns found.
fn do_touch(conn: &Connection, key: &[u8], ttl: u64) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE askr_cache
         SET expires_at = CASE WHEN ?2 > 0 THEN unixepoch() + ?2 ELSE NULL END
         WHERE key = ?1 AND (expires_at IS NULL OR expires_at > unixepoch())",
        params![String::from_utf8_lossy(key), ttl as i64],
    )?;
    Ok(n > 0)
}

fn do_flush(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("DELETE FROM askr_cache; DELETE FROM askr_cache_tags;")
}

/// Invalidate every key carrying `tag` (CACHE_CONTRACT §Tags).
fn do_forget_tag(conn: &Connection, tag: &[u8]) -> rusqlite::Result<()> {
    let tag = String::from_utf8_lossy(tag);
    conn.execute(
        "DELETE FROM askr_cache WHERE key IN (SELECT key FROM askr_cache_tags WHERE tag = ?1)",
        params![tag],
    )?;
    conn.execute("DELETE FROM askr_cache_tags WHERE tag = ?1", params![tag])?;
    Ok(())
}

// --- per-thread connection + public API used by the C bridge ------------------

fn open() -> Connection {
    let path = db_path().expect("ASKR_CACHE_DB must be set for the L2 cache");
    let conn = Connection::open(path).expect("open ASKR_CACHE_DB");
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;");
    init_schema(&conn).expect("initialize cache schema");
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

pub fn get(key: &[u8]) -> Option<Vec<u8>> {
    with_conn(|c| do_get(c, key)).unwrap_or(None)
}
pub fn set(key: &[u8], val: &[u8], ttl: u64) -> bool {
    with_conn(|c| do_set(c, key, val, ttl)).is_ok()
}
pub fn add(key: &[u8], val: &[u8], ttl: u64) -> bool {
    with_conn(|c| do_add(c, key, val, ttl)).unwrap_or(false)
}
pub fn delete(key: &[u8]) -> bool {
    with_conn(|c| do_delete(c, key)).unwrap_or(false)
}
pub fn increment(key: &[u8], delta: i64, ttl: u64) -> i64 {
    with_conn(|c| do_increment(c, key, delta, ttl)).unwrap_or(0)
}
pub fn touch(key: &[u8], ttl: u64) -> bool {
    with_conn(|c| do_touch(c, key, ttl)).unwrap_or(false)
}
pub fn flush() {
    let _ = with_conn(do_flush);
}
pub fn forget_tag(tag: &[u8]) {
    let _ = with_conn(|c| do_forget_tag(c, tag));
}

// --- PHP bridge (identical shape to cache.rs) ---------------------------------

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
}

extern "C" fn c_forget_tag(tag: *const c_char, tlen: usize) {
    let tag = unsafe { std::slice::from_raw_parts(tag as *const u8, tlen) };
    forget_tag(tag);
}

/// Register the L2 cache callbacks with the PHP shim for this process.
pub fn register_bridge() {
    if !enabled() {
        return;
    }
    let _ = with_conn(|_| Ok(()));
    // SAFETY: one-time registration; trampolines are 'static fns.
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

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }

    #[test]
    fn set_get_expiry() {
        let c = db();
        do_set(&c, b"k", b"v", 3600).unwrap();
        assert_eq!(do_get(&c, b"k").unwrap().as_deref(), Some(b"v".as_slice()));
        // Force it expired -> lazy miss via the live view.
        c.execute(
            "UPDATE askr_cache SET expires_at = unixepoch() - 1 WHERE key='k'",
            [],
        )
        .unwrap();
        assert_eq!(do_get(&c, b"k").unwrap(), None);
    }

    #[test]
    fn increment_then_get_reads_back_as_bytes() {
        let c = db();
        assert_eq!(do_increment(&c, b"ctr", 5, 0).unwrap(), 5);
        assert_eq!(do_increment(&c, b"ctr", 3, 0).unwrap(), 8);
        // The counter is stored as INTEGER but must read back as the bytes "8".
        assert_eq!(
            do_get(&c, b"ctr").unwrap().as_deref(),
            Some(b"8".as_slice())
        );
    }

    #[test]
    fn add_is_setnx_with_expired_steal() {
        let c = db();
        assert!(do_add(&c, b"lock", b"A", 30).unwrap(), "fresh acquired");
        assert!(!do_add(&c, b"lock", b"B", 30).unwrap(), "held not acquired");
        c.execute(
            "UPDATE askr_cache SET expires_at = unixepoch() - 1 WHERE key='lock'",
            [],
        )
        .unwrap();
        assert!(do_add(&c, b"lock", b"B", 30).unwrap(), "expired stolen");
        assert_eq!(
            do_get(&c, b"lock").unwrap().as_deref(),
            Some(b"B".as_slice())
        );
    }

    #[test]
    fn touch_delete_and_tag_invalidation() {
        let c = db();
        do_set(&c, b"a", b"1", 0).unwrap();
        assert!(do_touch(&c, b"a", 60).unwrap());
        assert!(!do_touch(&c, b"missing", 60).unwrap());
        assert!(do_delete(&c, b"a").unwrap());

        do_set(&c, b"p:1", b"x", 0).unwrap();
        do_set(&c, b"p:2", b"y", 0).unwrap();
        c.execute(
            "INSERT INTO askr_cache_tags (tag, key) VALUES ('posts','p:1'),('posts','p:2')",
            [],
        )
        .unwrap();
        do_forget_tag(&c, b"posts").unwrap();
        assert_eq!(do_get(&c, b"p:1").unwrap(), None);
        assert_eq!(do_get(&c, b"p:2").unwrap(), None);

        do_flush(&c).unwrap();
    }
}
