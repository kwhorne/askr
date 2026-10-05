//! What each route costs: requests, PHP time, latency, errors and cache hits per route
//! shape, across every worker — the table behind `askr top` and `GET /api/routes`.
//!
//! A route is the request's method and its path collapsed the way `askr cache-report`
//! collapses it (`/products/1421/reviews` → `/products/*/reviews`), with the host in
//! front when the instance has `[[site]]`s. The table lives in shared memory, mapped by
//! the master before it forks, so the admin plane reads the whole fleet's numbers.
//!
//! It is fixed-size: [`SLOTS`] routes, claimed on first sight. A route that finds no free
//! slot in its probe window is counted under `(other)` rather than evicting one already
//! there — a table that reshuffled under pressure would make every number on it
//! unreliable, and a count that says "this much went elsewhere" is honest.

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, Ordering};

use crate::metrics::BUCKET_BOUNDS_MS;

/// Routes the table can hold, `(other)` included.
pub const SLOTS: usize = 512;
/// Bytes of a route's name kept; longer names are cut at a character boundary.
pub const NAME_MAX: usize = 120;
/// Latency buckets: [`BUCKET_BOUNDS_MS`] plus one for anything slower.
pub const NBUCKETS: usize = BUCKET_BOUNDS_MS.len() + 1;
/// Slots tried before a route is counted under `(other)`.
const PROBES: usize = 32;

const EMPTY: u64 = 0;
const CLAIMING: u64 = 1;
const OTHER: &str = "(other)";

#[repr(C)]
struct Slot {
    /// `EMPTY`, `CLAIMING` while the name is being written, or the route's hash.
    tag: AtomicU64,
    name_len: AtomicU64,
    name: [AtomicU8; NAME_MAX],
    requests: AtomicU64,
    php_us: AtomicU64,
    total_us: AtomicU64,
    errors: AtomicU64,
    bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    buckets: [AtomicU64; NBUCKETS],
}

#[repr(C)]
struct Table {
    slots: [Slot; SLOTS],
}

static TABLE: AtomicPtr<Table> = AtomicPtr::new(ptr::null_mut());

/// Map the table. Call once in the master before forking; zeroed pages are an empty
/// table.
pub fn init() {
    if !TABLE.load(Ordering::SeqCst).is_null() {
        return;
    }
    match map_table() {
        Some(t) => TABLE.store(t, Ordering::SeqCst),
        None => tracing::warn!("routes: mmap failed; per-route accounting disabled"),
    }
}

/// A fresh, empty table in shared memory, `(other)` named.
fn map_table() -> Option<*mut Table> {
    let size = std::mem::size_of::<Table>();
    // SAFETY: an anonymous shared mapping, zero-filled; all-zero is a valid Table.
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
        return None;
    }
    let t = p as *mut Table;
    // Slot 0 is `(other)`, named up front so it is never claimed by a route.
    // SAFETY: just mapped, not yet shared with anyone.
    let other = unsafe { &(*t).slots[0] };
    write_name(other, OTHER);
    other.tag.store(tag_of(OTHER), Ordering::Release);
    Some(t)
}

fn table() -> Option<&'static Table> {
    let p = TABLE.load(Ordering::Acquire);
    // SAFETY: set once by `init` to a mapping that lives for the process.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// The route a request belongs to.
pub fn route_of(method: &str, host: Option<&str>, path: &str) -> String {
    let pattern = crate::oracle::path_pattern(path);
    match host {
        Some(h) => format!("{method} {h}{pattern}"),
        None => format!("{method} {pattern}"),
    }
}

/// PHP time spent on a response, carried in its extensions from `handle` to where the
/// response is counted.
#[derive(Debug, Clone, Copy)]
pub struct PhpTime(pub u64);

/// Count a finished response against its route: what `serve_io` and the HTTP/3 path call
/// for every client request. Requests Askr makes of itself (ESI fragments, background
/// refreshes) never pass through here, so they are not counted twice.
pub fn note_response<B>(route: &str, resp: &hyper::Response<B>, total_us: u64)
where
    B: hyper::body::Body,
{
    let cache_hit = resp
        .headers()
        .get("x-askr-cache")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| match v {
            "HIT" | "STALE" | "STALE-ERROR" => Some(true),
            "MISS" => Some(false),
            _ => None,
        });
    record(
        route,
        Cost {
            status: resp.status().as_u16(),
            bytes: resp.body().size_hint().exact().unwrap_or(0),
            php_us: resp.extensions().get::<PhpTime>().map_or(0, |t| t.0),
            total_us,
            cache_hit,
        },
    );
}

/// What one response cost.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cost {
    pub status: u16,
    pub bytes: u64,
    pub php_us: u64,
    pub total_us: u64,
    /// `Some(true)` for a cache hit (HIT, STALE, coalesced), `Some(false)` for a MISS,
    /// `None` when the response cache had no say.
    pub cache_hit: Option<bool>,
}

/// Count one response against its route.
pub fn record(route: &str, cost: Cost) {
    if let Some(t) = table() {
        record_in(t, route, cost);
    }
}

fn record_in(t: &Table, route: &str, cost: Cost) {
    let s = slot_for(t, route);
    s.requests.fetch_add(1, Ordering::Relaxed);
    s.php_us.fetch_add(cost.php_us, Ordering::Relaxed);
    s.total_us.fetch_add(cost.total_us, Ordering::Relaxed);
    s.bytes.fetch_add(cost.bytes, Ordering::Relaxed);
    if cost.status >= 500 {
        s.errors.fetch_add(1, Ordering::Relaxed);
    }
    match cost.cache_hit {
        Some(true) => {
            s.hits.fetch_add(1, Ordering::Relaxed);
        }
        Some(false) => {
            s.misses.fetch_add(1, Ordering::Relaxed);
        }
        None => {}
    }
    s.buckets[bucket(cost.total_us)].fetch_add(1, Ordering::Relaxed);
}

fn bucket(total_us: u64) -> usize {
    let ms = total_us / 1000;
    BUCKET_BOUNDS_MS
        .iter()
        .position(|&b| ms <= b)
        .unwrap_or(NBUCKETS - 1)
}

/// FNV-1a, kept clear of the two reserved tags.
fn tag_of(route: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in route.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h | 2
}

fn write_name(s: &Slot, route: &str) {
    let mut n = route.len().min(NAME_MAX);
    while !route.is_char_boundary(n) {
        n -= 1;
    }
    for (dst, b) in s.name.iter().zip(&route.as_bytes()[..n]) {
        dst.store(*b, Ordering::Relaxed);
    }
    s.name_len.store(n as u64, Ordering::Relaxed);
}

fn slot_for<'t>(t: &'t Table, route: &str) -> &'t Slot {
    let tag = tag_of(route);
    let start = (tag as usize) % (SLOTS - 1) + 1;
    for i in 0..PROBES {
        let s = &t.slots[(start - 1 + i) % (SLOTS - 1) + 1];
        // Settle this slot before moving on. Moving past a slot another thread is still
        // naming — after losing the race to claim it — is how one route got two slots:
        // the claim was for this very route, and the loser took the next slot too.
        let mut spins = 0u32;
        loop {
            match s.tag.load(Ordering::Acquire) {
                t if t == tag => return s,
                CLAIMING if spins < 1_000_000 => {
                    spins += 1;
                    std::hint::spin_loop();
                }
                EMPTY => {
                    if s.tag
                        .compare_exchange(EMPTY, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        write_name(s, route);
                        s.tag.store(tag, Ordering::Release);
                        return s;
                    }
                    // Lost the claim: look again at what it became.
                }
                // Another route's slot (or a claim that never finished): next one.
                _ => break,
            }
        }
    }
    &t.slots[0]
}

/// One route's totals since the server started.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RouteStats {
    pub route: String,
    pub requests: u64,
    pub php_us: u64,
    pub total_us: u64,
    pub errors: u64,
    pub bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub buckets: Vec<u64>,
}

/// Every route seen, busiest PHP first. Empty when the table is not mapped.
pub fn snapshot() -> Vec<RouteStats> {
    table().map(snapshot_of).unwrap_or_default()
}

fn snapshot_of(t: &Table) -> Vec<RouteStats> {
    let mut out: Vec<RouteStats> = t
        .slots
        .iter()
        .filter(|s| s.tag.load(Ordering::Acquire) > CLAIMING)
        .map(|s| {
            let n = (s.name_len.load(Ordering::Relaxed) as usize).min(NAME_MAX);
            let name: Vec<u8> = s.name[..n]
                .iter()
                .map(|b| b.load(Ordering::Relaxed))
                .collect();
            RouteStats {
                route: String::from_utf8_lossy(&name).into_owned(),
                requests: s.requests.load(Ordering::Relaxed),
                php_us: s.php_us.load(Ordering::Relaxed),
                total_us: s.total_us.load(Ordering::Relaxed),
                errors: s.errors.load(Ordering::Relaxed),
                bytes: s.bytes.load(Ordering::Relaxed),
                hits: s.hits.load(Ordering::Relaxed),
                misses: s.misses.load(Ordering::Relaxed),
                buckets: s
                    .buckets
                    .iter()
                    .map(|b| b.load(Ordering::Relaxed))
                    .collect(),
            }
        })
        .filter(|r| r.requests > 0)
        .collect();
    out.sort_by(|a, b| b.php_us.cmp(&a.php_us).then_with(|| a.route.cmp(&b.route)));
    out
}

/// The latency at percentile `p` (0–100) as the upper bound of the bucket it falls in,
/// in milliseconds; `None` for the overflow bucket ("slower than the last bound") or no
/// requests.
pub fn percentile_ms(buckets: &[u64], p: f64) -> Option<Option<u64>> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let want = ((total as f64) * p / 100.0).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, n) in buckets.iter().enumerate() {
        seen += n;
        if seen >= want {
            return Some(BUCKET_BOUNDS_MS.get(i).copied());
        }
    }
    Some(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table of the test's own, so tests running at once cannot crowd each other.
    fn own() -> &'static Table {
        // SAFETY: freshly mapped and leaked for the life of the test process.
        unsafe { &*map_table().expect("mmap") }
    }

    #[test]
    fn routes_collapse_to_shapes_and_keep_their_own_slot() {
        let t = own();
        assert_eq!(
            route_of("GET", None, "/products/1421/reviews"),
            "GET /products/*/reviews"
        );
        assert_eq!(
            route_of("POST", Some("shop.test"), "/login"),
            "POST shop.test/login"
        );

        let r = "GET /products/*";
        for ms in [1u64, 3, 3, 300] {
            record_in(
                t,
                r,
                Cost {
                    status: 200,
                    bytes: 10,
                    php_us: ms * 1000,
                    total_us: ms * 1000,
                    cache_hit: Some(false),
                },
            );
        }
        record_in(
            t,
            r,
            Cost {
                status: 502,
                total_us: 10,
                cache_hit: Some(true),
                ..Cost::default()
            },
        );
        record_in(
            t,
            "GET /cheap",
            Cost {
                status: 200,
                php_us: 5,
                ..Cost::default()
            },
        );
        let snap = snapshot_of(t);
        assert_eq!(snap[0].route, r, "busiest PHP first: {snap:?}");
        let s = &snap[0];
        assert_eq!(s.requests, 5);
        assert_eq!(s.php_us, 307_000);
        assert_eq!((s.errors, s.hits, s.misses, s.bytes), (1, 1, 4, 40));
        assert_eq!(s.buckets.iter().sum::<u64>(), 5);
        assert_eq!(snap.iter().filter(|s| s.route == r).count(), 1, "one slot");
        assert!(
            !snap.iter().any(|s| s.route == OTHER),
            "an unused (other) is not listed"
        );
    }

    #[test]
    fn a_full_table_counts_the_rest_as_other() {
        let t = own();
        // Far more distinct routes than slots: every one is counted somewhere, and the
        // ones that found no slot are under (other) rather than lost or evicting.
        let n = SLOTS * 3;
        for i in 0..n {
            record_in(
                t,
                &format!("GET /flood-{i}"),
                Cost {
                    status: 200,
                    ..Cost::default()
                },
            );
        }
        let snap = snapshot_of(t);
        let named: u64 = snap
            .iter()
            .filter(|s| s.route != OTHER)
            .map(|s| s.requests)
            .sum();
        let other = snap
            .iter()
            .find(|s| s.route == OTHER)
            .map_or(0, |s| s.requests);
        assert!(other > 0, "some routes had nowhere to go");
        assert_eq!(
            named + other,
            n as u64,
            "every request counted exactly once"
        );
        assert_eq!(snap.len(), SLOTS, "every slot in use, none twice");
    }

    #[test]
    fn concurrent_first_sightings_share_one_slot() {
        let t = own();
        std::thread::scope(|sc| {
            for _ in 0..8 {
                sc.spawn(|| {
                    for _ in 0..1000 {
                        record_in(
                            t,
                            "GET /race",
                            Cost {
                                status: 200,
                                ..Cost::default()
                            },
                        );
                    }
                });
            }
        });
        let hits: Vec<_> = snapshot_of(t)
            .into_iter()
            .filter(|s| s.route == "GET /race")
            .collect();
        assert_eq!(hits.len(), 1, "one slot, however many claimed it at once");
        assert_eq!(hits[0].requests, 8000);
    }

    #[test]
    fn percentiles_come_from_the_buckets() {
        let mut b = vec![0u64; NBUCKETS];
        assert_eq!(percentile_ms(&b, 95.0), None);
        b[0] = 90; // ≤ 1 ms
        b[6] = 10; // ≤ 100 ms
        assert_eq!(percentile_ms(&b, 50.0), Some(Some(1)));
        assert_eq!(percentile_ms(&b, 95.0), Some(Some(100)));
        b[NBUCKETS - 1] = 1000;
        assert_eq!(
            percentile_ms(&b, 95.0),
            Some(None),
            "slower than the last bound"
        );
    }

    #[test]
    fn a_long_name_is_cut_at_a_character_boundary() {
        let t = own();
        let long = format!("GET /{}", "ø".repeat(NAME_MAX));
        record_in(t, &long, Cost::default());
        let s = &snapshot_of(t)[0];
        assert!(
            long.starts_with(&s.route) && s.route.len() <= NAME_MAX,
            "{}",
            s.route
        );
    }
}
