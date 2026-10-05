//! Verified reloads: before the new code serves the fleet, it answers what the old code
//! was recently asked, and a page that worked before and fails now stops the rollout.
//!
//! The canary gate (`[reload] canary`) judges the new worker on whatever live traffic
//! reaches it inside its window. On most sites that is a handful of requests, often to
//! the same two pages, so the verdict is "inconclusive" and the rollout goes ahead on no
//! evidence; and a route nobody visited in those seconds is never tried at all.
//!
//! With `[reload] verify = true`:
//!
//! 1. While serving, each worker records the last distinct anonymous GETs that ran PHP —
//!    URL, status, and a hash of the body — in a ring in shared memory ([`record`]).
//! 2. When the canary (the first worker restarted by a reload) boots on the new code, it
//!    replays them through its own request handler — no network, no cache, no rate
//!    limit, not counted in the metrics — alongside the live traffic it serves ([`replay`]).
//! 3. A page that answered 2xx/3xx before and 5xx now, or 2xx before and 4xx now, is a
//!    regression. It is tried once more first, so an endpoint that fails now and then is
//!    reported as flaky rather than blamed on the deploy. A changed body is counted, not
//!    judged: a deploy is supposed to change pages.
//! 4. The canary gate waits for the verdict. Regressions abort the rollout as an
//!    unhealthy canary does; a clean replay makes an otherwise inconclusive canary
//!    conclusive. `/api/status` reports it under `verify`.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Distinct URLs the ring remembers. A URL hashes to one slot; a newer one evicts it.
pub const RING: usize = 256;
const HOST_MAX: usize = 96;
const PQ_MAX: usize = 400;
/// Regressions kept for the report.
const REPORT_MAX: usize = 16;
const URL_MAX: usize = 160;

#[repr(C)]
struct Bytes<const N: usize> {
    len: AtomicU64,
    b: [AtomicU8; N],
}

impl<const N: usize> Bytes<N> {
    fn set(&self, s: &[u8]) {
        let n = s.len().min(N);
        for (d, v) in self.b.iter().zip(&s[..n]) {
            d.store(*v, Ordering::Relaxed);
        }
        self.len.store(n as u64, Ordering::Relaxed);
    }
    fn get(&self) -> Vec<u8> {
        let n = (self.len.load(Ordering::Relaxed) as usize).min(N);
        self.b[..n]
            .iter()
            .map(|v| v.load(Ordering::Relaxed))
            .collect()
    }
}

/// One remembered request. `seq` is a seqlock: odd while a writer is in it.
#[repr(C)]
struct Entry {
    seq: AtomicU64,
    url_hash: AtomicU64,
    host: Bytes<HOST_MAX>,
    pq: Bytes<PQ_MAX>,
    status: AtomicU64,
    body_hash: AtomicU64,
    at_ms: AtomicU64,
}

#[repr(C)]
struct Regression {
    url: Bytes<URL_MAX>,
    before: AtomicU64,
    after: AtomicU64,
}

#[repr(C)]
struct Region {
    ring: [Entry; RING],
    /// 0 idle, 1 replaying, 2 done.
    state: AtomicU64,
    started_ms: AtomicU64,
    finished_ms: AtomicU64,
    replayed: AtomicU64,
    identical: AtomicU64,
    changed: AtomicU64,
    flaky: AtomicU64,
    regressed: AtomicU64,
    regressions: [Regression; REPORT_MAX],
}

pub const IDLE: u64 = 0;
pub const REPLAYING: u64 = 1;
pub const DONE: u64 = 2;

static REGION: AtomicPtr<Region> = AtomicPtr::new(ptr::null_mut());
/// `[reload] verify`, set at startup before the fork.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
/// `[reload] verify_requests`: URLs replayed per reload.
pub static MAX_REPLAY: AtomicU64 = AtomicU64::new(200);
/// `[reload] verify_timeout`: seconds the gate waits for the verdict.
pub static TIMEOUT_SECS: AtomicU64 = AtomicU64::new(120);

/// Map the region and switch recording on. Call in the master before forking.
pub fn init(max_replay: u64, timeout_secs: u64) {
    if REGION.load(Ordering::SeqCst).is_null() {
        match map_region() {
            Some(r) => REGION.store(r, Ordering::SeqCst),
            None => {
                tracing::warn!("verify: mmap failed; verified reloads disabled");
                return;
            }
        }
    }
    MAX_REPLAY.store(max_replay.max(1), Ordering::SeqCst);
    TIMEOUT_SECS.store(timeout_secs.max(1), Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
}

fn map_region() -> Option<*mut Region> {
    // SAFETY: anonymous shared mapping, zero-filled; all-zero is an empty Region.
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            std::mem::size_of::<Region>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    };
    (p != libc::MAP_FAILED).then_some(p as *mut Region)
}

fn region() -> Option<&'static Region> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let p = REGION.load(Ordering::Acquire);
    // SAFETY: set once by `init` to a mapping that lives for the process.
    (!p.is_null()).then(|| unsafe { &*p })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fnv(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in *p {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The hash a body is compared by.
pub fn body_hash(body: &[u8]) -> u64 {
    fnv(&[body])
}

/// The body hash of a response that ran PHP, carried in its extensions so a replay
/// compares the same bytes the recording hashed (before compression and ESI assembly).
#[derive(Debug, Clone, Copy)]
pub struct BodyHash(pub u64);

tokio::task_local! {
    static REPLAY: ();
}

/// Is this request a replay? Replays skip the cache, the rate limiter, recording and the
/// metrics — they are the server asking itself, not traffic.
pub fn replaying() -> bool {
    REPLAY.try_with(|_| ()).is_ok()
}

/// Remember a request the old code answered. Called for anonymous GETs that ran PHP.
pub fn record(host: &str, path_and_query: &str, status: u16, body_hash: u64) {
    let Some(r) = region() else { return };
    if replaying() {
        return;
    }
    record_in(r, host, path_and_query, status, body_hash, now_ms());
}

fn record_in(r: &Region, host: &str, pq: &str, status: u16, body_hash: u64, at: u64) {
    if host.len() > HOST_MAX || pq.len() > PQ_MAX {
        return; // a URL that would not fit is not replayed half-written
    }
    let h = fnv(&[host.as_bytes(), pq.as_bytes()]);
    let e = &r.ring[(h as usize) % RING];
    let seq = e.seq.load(Ordering::Acquire);
    // Another writer is in this slot: skip, the next request to this URL records it.
    if seq % 2 == 1
        || e.seq
            .compare_exchange(seq, seq + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    e.url_hash.store(h, Ordering::Relaxed);
    e.host.set(host.as_bytes());
    e.pq.set(pq.as_bytes());
    e.status.store(status as u64, Ordering::Relaxed);
    e.body_hash.store(body_hash, Ordering::Relaxed);
    e.at_ms.store(at, Ordering::Relaxed);
    e.seq.store(seq + 2, Ordering::Release);
}

/// A remembered request, read consistently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub host: String,
    pub path_and_query: String,
    pub status: u16,
    pub body_hash: u64,
    pub at_ms: u64,
}

fn recorded_in(r: &Region, limit: usize) -> Vec<Recorded> {
    let mut out = Vec::new();
    for e in &r.ring {
        for _ in 0..4 {
            let s1 = e.seq.load(Ordering::Acquire);
            if s1 == 0 || s1 % 2 == 1 {
                break;
            }
            let rec = Recorded {
                host: String::from_utf8_lossy(&e.host.get()).into_owned(),
                path_and_query: String::from_utf8_lossy(&e.pq.get()).into_owned(),
                status: e.status.load(Ordering::Relaxed) as u16,
                body_hash: e.body_hash.load(Ordering::Relaxed),
                at_ms: e.at_ms.load(Ordering::Relaxed),
            };
            if e.seq.load(Ordering::Acquire) == s1 {
                out.push(rec);
                break;
            }
        }
    }
    // The most recent first: the replay budget goes to what visitors asked lately.
    out.sort_by_key(|r| std::cmp::Reverse(r.at_ms));
    out.truncate(limit);
    out
}

/// What a replayed response says about the deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Identical,
    /// Same kind of answer, different body — expected of a deploy, not judged.
    Changed,
    /// It worked before and does not now.
    Regressed,
}

/// Compare what the old code answered with what the new code answers.
pub fn judge(before: u16, before_hash: u64, after: u16, after_hash: Option<u64>) -> Outcome {
    let ok_before = (200..400).contains(&before);
    let broken_now = after >= 500
        || ((200..300).contains(&before) && (400..500).contains(&after) && after != 429);
    if ok_before && broken_now {
        return Outcome::Regressed;
    }
    // `is_none_or` reads better and is stable only from 1.82; the MSRV is 1.80.
    let same_body = match after_hash {
        None => true,
        Some(h) => h == before_hash,
    };
    if before == after && same_body {
        Outcome::Identical
    } else {
        Outcome::Changed
    }
}

/// Reset the verdict for a new reload. Atomics only: called from the reload signal path.
pub fn reset() {
    let p = REGION.load(Ordering::Acquire);
    if p.is_null() || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: set once by `init`.
    let r = unsafe { &*p };
    r.state.store(IDLE, Ordering::SeqCst);
    for c in [
        &r.replayed,
        &r.identical,
        &r.changed,
        &r.flaky,
        &r.regressed,
    ] {
        c.store(0, Ordering::SeqCst);
    }
}

/// Should this worker replay? Only the canary of a reload in progress, with verify on.
pub fn should_replay() -> bool {
    region().is_some()
        && crate::supervisor::MY_SLOT.load(Ordering::Relaxed) == 0
        && crate::supervisor::CANARY_ACTIVE.load(Ordering::SeqCst)
}

/// Replay what the old code answered against this worker's new code, and leave the
/// verdict for the canary gate. Runs alongside live traffic; never blocks serving.
pub async fn replay(rt: Arc<crate::server::Runtime>) {
    let Some(r) = region() else { return };
    r.state.store(REPLAYING, Ordering::SeqCst);
    r.started_ms.store(now_ms(), Ordering::SeqCst);
    let todo = recorded_in(r, MAX_REPLAY.load(Ordering::SeqCst) as usize);
    tracing::info!(
        urls = todo.len(),
        "verify: replaying recent requests against the new code"
    );
    let mut kept = 0usize;
    for rec in &todo {
        let (after, hash) = ask(&rt, rec).await;
        let mut outcome = judge(rec.status, rec.body_hash, after, hash);
        let mut after = after;
        if outcome == Outcome::Regressed {
            // Once more: an endpoint that fails now and then is not the deploy's doing.
            let (again, hash2) = ask(&rt, rec).await;
            if judge(rec.status, rec.body_hash, again, hash2) != Outcome::Regressed {
                r.flaky.fetch_add(1, Ordering::SeqCst);
                outcome = Outcome::Changed;
            } else {
                after = again;
            }
        }
        r.replayed.fetch_add(1, Ordering::SeqCst);
        match outcome {
            Outcome::Identical => r.identical.fetch_add(1, Ordering::SeqCst),
            Outcome::Changed => r.changed.fetch_add(1, Ordering::SeqCst),
            Outcome::Regressed => {
                if kept < REPORT_MAX {
                    let g = &r.regressions[kept];
                    g.url
                        .set(format!("{}{}", rec.host, rec.path_and_query).as_bytes());
                    g.before.store(rec.status as u64, Ordering::SeqCst);
                    g.after.store(after as u64, Ordering::SeqCst);
                    kept += 1;
                }
                tracing::error!(
                    url = %format!("{}{}", rec.host, rec.path_and_query),
                    before = rec.status,
                    after,
                    "verify: this answered {} before the deploy and {} now", rec.status, after
                );
                r.regressed.fetch_add(1, Ordering::SeqCst)
            }
        };
    }
    r.finished_ms.store(now_ms(), Ordering::SeqCst);
    r.state.store(DONE, Ordering::SeqCst);
    tracing::info!(
        replayed = r.replayed.load(Ordering::SeqCst),
        regressed = r.regressed.load(Ordering::SeqCst),
        changed = r.changed.load(Ordering::SeqCst),
        flaky = r.flaky.load(Ordering::SeqCst),
        "verify: replay finished"
    );
}

async fn ask(rt: &Arc<crate::server::Runtime>, rec: &Recorded) -> (u16, Option<u64>) {
    let req = match hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(&rec.path_and_query)
        .header(hyper::header::HOST, &rec.host)
        .body(http_body_util::Empty::<bytes::Bytes>::new())
    {
        Ok(r) => r,
        Err(_) => return (0, None),
    };
    let peer: std::net::SocketAddr = ([127, 0, 0, 1], 0).into();
    let resp = REPLAY
        .scope((), crate::server::handle(req, rt.clone(), peer))
        .await;
    match resp {
        Ok(resp) => (
            resp.status().as_u16(),
            resp.extensions().get::<BodyHash>().map(|h| h.0),
        ),
        Err(_) => (502, None),
    }
}

/// The gate's reading of the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Gate {
    /// Still replaying (or the canary has not started yet), and there is time left.
    Wait,
    /// Nothing regressed; `replayed` pages were tried.
    Pass {
        replayed: u64,
    },
    Fail {
        reason: String,
    },
}

/// What the canary gate should do, `elapsed_secs` into the reload.
pub fn gate(elapsed_secs: u64) -> Gate {
    let Some(r) = region() else {
        return Gate::Pass { replayed: 0 };
    };
    if r.state.load(Ordering::SeqCst) != DONE {
        return if elapsed_secs < TIMEOUT_SECS.load(Ordering::SeqCst) {
            Gate::Wait
        } else {
            Gate::Fail {
                reason: format!(
                    "the replay did not finish within {} s (reload.verify_timeout)",
                    TIMEOUT_SECS.load(Ordering::SeqCst)
                ),
            }
        };
    }
    let regressed = r.regressed.load(Ordering::SeqCst);
    if regressed > 0 {
        let first: Vec<String> = r
            .regressions
            .iter()
            .take((regressed as usize).min(3))
            .map(|g| {
                format!(
                    "{} {}→{}",
                    String::from_utf8_lossy(&g.url.get()),
                    g.before.load(Ordering::SeqCst),
                    g.after.load(Ordering::SeqCst)
                )
            })
            .collect();
        return Gate::Fail {
            reason: format!(
                "{regressed} page(s) that worked before the deploy fail now: {}",
                first.join(", ")
            ),
        };
    }
    Gate::Pass {
        replayed: r.replayed.load(Ordering::SeqCst),
    }
}

/// The last verification, for `/api/status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub state: &'static str,
    pub recorded: usize,
    pub replayed: u64,
    pub identical: u64,
    pub changed: u64,
    pub flaky: u64,
    pub regressed: u64,
    pub regressions: Vec<RegressionDoc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RegressionDoc {
    pub url: String,
    pub before: u16,
    pub after: u16,
}

pub fn report() -> Option<Report> {
    let r = region()?;
    let regressed = r.regressed.load(Ordering::SeqCst);
    Some(Report {
        state: match r.state.load(Ordering::SeqCst) {
            REPLAYING => "replaying",
            DONE => "done",
            _ => "idle",
        },
        recorded: recorded_in(r, RING).len(),
        replayed: r.replayed.load(Ordering::SeqCst),
        identical: r.identical.load(Ordering::SeqCst),
        changed: r.changed.load(Ordering::SeqCst),
        flaky: r.flaky.load(Ordering::SeqCst),
        regressed,
        regressions: r
            .regressions
            .iter()
            .take((regressed as usize).min(REPORT_MAX))
            .map(|g| RegressionDoc {
                url: String::from_utf8_lossy(&g.url.get()).into_owned(),
                before: g.before.load(Ordering::SeqCst) as u16,
                after: g.after.load(Ordering::SeqCst) as u16,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> &'static Region {
        // SAFETY: freshly mapped and leaked for the test process.
        unsafe { &*map_region().expect("mmap") }
    }

    #[test]
    fn what_counts_as_a_regression() {
        use Outcome::*;
        assert_eq!(judge(200, 1, 200, Some(1)), Identical);
        assert_eq!(
            judge(200, 1, 200, Some(2)),
            Changed,
            "a deploy changes pages"
        );
        assert_eq!(judge(200, 1, 200, None), Identical, "no body to compare");
        assert_eq!(judge(200, 1, 500, None), Regressed);
        assert_eq!(judge(302, 1, 502, None), Regressed);
        assert_eq!(
            judge(200, 1, 404, None),
            Regressed,
            "a page that existed is gone"
        );
        assert_eq!(
            judge(200, 1, 429, None),
            Changed,
            "a rate limit is not the code"
        );
        assert_eq!(
            judge(301, 1, 404, None),
            Changed,
            "a redirect's target is not judged"
        );
        assert_eq!(
            judge(404, 1, 500, None),
            Changed,
            "it did not work before either"
        );
        assert_eq!(judge(500, 1, 500, Some(1)), Identical);
    }

    #[test]
    fn the_ring_keeps_one_entry_per_url_newest_first() {
        let r = own();
        record_in(r, "shop.test", "/a", 200, 11, 1_000);
        record_in(r, "shop.test", "/b?x=1", 200, 22, 2_000);
        record_in(r, "shop.test", "/a", 500, 33, 3_000);
        let got = recorded_in(r, 10);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].path_and_query, "/a", "newest first");
        assert_eq!(
            (got[0].status, got[0].body_hash),
            (500, 33),
            "the latest answer"
        );
        assert_eq!(got[1].host, "shop.test");
        assert_eq!(recorded_in(r, 1).len(), 1, "the budget is respected");
        // A URL that would not fit is not kept half-written.
        record_in(
            r,
            "shop.test",
            &format!("/{}", "x".repeat(PQ_MAX)),
            200,
            0,
            4_000,
        );
        assert_eq!(recorded_in(r, 10).len(), 2);
    }

    #[test]
    fn concurrent_writers_never_leave_a_torn_entry() {
        let r = own();
        std::thread::scope(|s| {
            for t in 0..8u64 {
                s.spawn(move || {
                    for i in 0..2000u64 {
                        // Every entry's hash encodes its own URL, so a torn read would
                        // pair one URL with another's hash.
                        let pq = format!("/p/{}", i % 40);
                        record_in(r, "h", &pq, 200, body_hash(pq.as_bytes()), t * 10_000 + i);
                    }
                });
            }
            for _ in 0..200 {
                for e in recorded_in(r, RING) {
                    assert_eq!(e.body_hash, body_hash(e.path_and_query.as_bytes()), "{e:?}");
                }
            }
        });
    }
}
