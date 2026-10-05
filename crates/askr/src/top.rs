//! `askr top`: what each route costs, live.
//!
//! The admin plane serves the per-route totals ([`crate::routes`]) as `GET /api/routes`.
//! `askr top` polls it and shows the difference between two polls — requests per second,
//! each route's share of the PHP time, its p95 and its cache hit rate — redrawn in place,
//! the way `top` does for processes. `--once` prints the totals since the server started
//! instead, for a script or a ticket.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;

use crate::metrics::BUCKET_BOUNDS_MS;
use crate::routes::{percentile_ms, RouteStats};

/// What `GET /api/routes` answers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoutesDoc {
    pub uptime_secs: u64,
    /// Upper bounds of the latency buckets, in ms; each route's `buckets` has one more,
    /// for anything slower.
    pub bounds_ms: Vec<u64>,
    pub routes: Vec<RouteStats>,
}

/// The document, from the shared table.
pub fn doc() -> RoutesDoc {
    RoutesDoc {
        uptime_secs: crate::supervisor::now_secs().saturating_sub(
            crate::supervisor::START_TIME.load(std::sync::atomic::Ordering::SeqCst),
        ),
        bounds_ms: BUCKET_BOUNDS_MS.to_vec(),
        routes: crate::routes::snapshot(),
    }
}

/// What changed between two snapshots: `now` minus `before`, per route. A route new
/// since `before` counts from zero.
pub fn delta(before: &[RouteStats], now: &[RouteStats]) -> Vec<RouteStats> {
    now.iter()
        .map(|r| match before.iter().find(|b| b.route == r.route) {
            None => r.clone(),
            Some(b) => RouteStats {
                route: r.route.clone(),
                requests: r.requests.saturating_sub(b.requests),
                php_us: r.php_us.saturating_sub(b.php_us),
                total_us: r.total_us.saturating_sub(b.total_us),
                errors: r.errors.saturating_sub(b.errors),
                bytes: r.bytes.saturating_sub(b.bytes),
                hits: r.hits.saturating_sub(b.hits),
                misses: r.misses.saturating_sub(b.misses),
                buckets: r
                    .buckets
                    .iter()
                    .zip(b.buckets.iter().chain(std::iter::repeat(&0)))
                    .map(|(n, o)| n.saturating_sub(*o))
                    .collect(),
            },
        })
        .filter(|r| r.requests > 0)
        .collect()
}

/// How to order the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Sort {
    /// Share of the PHP time — where the CPU goes.
    Cpu,
    /// Requests.
    Requests,
    /// 95th-percentile latency.
    P95,
    /// 5xx responses.
    Errors,
}

/// The table, for `routes` observed over `secs` seconds.
pub fn render(routes: &[RouteStats], secs: f64, sort: Sort, limit: usize) -> String {
    let mut rows: Vec<&RouteStats> = routes.iter().collect();
    let p95 = |r: &RouteStats| match percentile_ms(&r.buckets, 95.0) {
        Some(Some(ms)) => ms,
        Some(None) => u64::MAX,
        None => 0,
    };
    match sort {
        Sort::Cpu => rows.sort_by_key(|r| std::cmp::Reverse(r.php_us)),
        Sort::Requests => rows.sort_by_key(|r| std::cmp::Reverse(r.requests)),
        Sort::P95 => rows.sort_by_key(|r| std::cmp::Reverse(p95(r))),
        Sort::Errors => rows.sort_by_key(|r| std::cmp::Reverse(r.errors)),
    }
    let php_total: u64 = routes.iter().map(|r| r.php_us).sum();
    let req_total: u64 = routes.iter().map(|r| r.requests).sum();
    let secs = secs.max(0.001);
    let mut out = format!(
        "{} routes · {:.1} req/s · PHP {:.2} s/s over {:.0} s\n\n",
        routes.len(),
        req_total as f64 / secs,
        php_total as f64 / 1e6 / secs,
        secs
    );
    let width = rows
        .iter()
        .take(limit)
        .map(|r| r.route.chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 60);
    out.push_str(&format!(
        "{:<width$}  {:>8}  {:>6}  {:>8}  {:>8}  {:>5}  {:>6}\n",
        "ROUTE", "req/s", "CPU", "avg", "p95", "5xx", "cache"
    ));
    for r in rows.iter().take(limit) {
        let name: String = if r.route.chars().count() > width {
            let cut: String = r.route.chars().take(width - 1).collect();
            format!("{cut}…")
        } else {
            r.route.clone()
        };
        let cpu = if php_total == 0 {
            "–".to_string()
        } else {
            format!("{:.0}%", r.php_us as f64 * 100.0 / php_total as f64)
        };
        let avg = ms(r.total_us as f64 / r.requests.max(1) as f64 / 1000.0);
        let p95 = match percentile_ms(&r.buckets, 95.0) {
            Some(Some(b)) => format!("≤{b}ms"),
            Some(None) => format!(">{}ms", BUCKET_BOUNDS_MS[BUCKET_BOUNDS_MS.len() - 1]),
            None => "–".to_string(),
        };
        let cache = if r.hits + r.misses == 0 {
            "–".to_string()
        } else {
            format!("{:.0}%", r.hits as f64 * 100.0 / (r.hits + r.misses) as f64)
        };
        out.push_str(&format!(
            "{:<width$}  {:>8.1}  {:>6}  {:>8}  {:>8}  {:>5}  {:>6}\n",
            name,
            r.requests as f64 / secs,
            cpu,
            avg,
            p95,
            r.errors,
            cache
        ));
    }
    if rows.len() > limit {
        out.push_str(&format!("… and {} more\n", rows.len() - limit));
    }
    out
}

fn ms(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}ms")
    } else {
        format!("{v:.1}ms")
    }
}

async fn fetch(admin: SocketAddr) -> anyhow::Result<RoutesDoc> {
    let tcp = tokio::net::TcpStream::connect(admin).await.map_err(|e| {
        anyhow::anyhow!("connecting to the admin plane at {admin}: {e} — is [admin] listen set?")
    })?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/api/routes")
        .header(hyper::header::HOST, admin.to_string());
    if let Some(t) = std::env::var("ASKR_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = sender
        .send_request(req.body(Full::new(Bytes::new()))?)
        .await?;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    anyhow::ensure!(
        status.is_success(),
        "the admin plane answered {status}: {}",
        String::from_utf8_lossy(&body)
    );
    Ok(serde_json::from_slice(&body)?)
}

/// `askr top`.
pub fn run(
    admin: &str,
    interval: Duration,
    once: bool,
    json: bool,
    sort: Sort,
    limit: usize,
) -> anyhow::Result<()> {
    let admin: SocketAddr = admin
        .parse()
        .map_err(|_| anyhow::anyhow!("--admin {admin:?} is not an address like 127.0.0.1:9000"))?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let first = fetch(admin).await?;
        if once {
            if json {
                println!("{}", serde_json::to_string(&first)?);
            } else {
                print!(
                    "{}",
                    render(&first.routes, first.uptime_secs as f64, sort, limit)
                );
            }
            return Ok(());
        }
        let mut before = first;
        let mut at = Instant::now();
        loop {
            tokio::time::sleep(interval).await;
            let now = fetch(admin).await?;
            let secs = at.elapsed().as_secs_f64();
            let changed = delta(&before.routes, &now.routes);
            if json {
                println!("{}", serde_json::to_string(&changed)?);
            } else {
                // Home and clear, then the table: redrawn in place, like top.
                print!(
                    "\x1b[H\x1b[2Jaskr top — {admin} — every {:.0}s, sorted by {} (Ctrl-C to quit)\n\n{}",
                    interval.as_secs_f64(),
                    format!("{sort:?}").to_lowercase(),
                    render(&changed, secs, sort, limit)
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            before = now;
            at = Instant::now();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: &str, requests: u64, php_ms: u64, buckets: &[(usize, u64)]) -> RouteStats {
        let mut b = vec![0u64; crate::routes::NBUCKETS];
        for (i, n) in buckets {
            b[*i] = *n;
        }
        RouteStats {
            route: name.into(),
            requests,
            php_us: php_ms * 1000,
            total_us: php_ms * 1000,
            buckets: b,
            ..RouteStats::default()
        }
    }

    #[test]
    fn a_delta_is_what_happened_between_two_polls() {
        let before = vec![
            route("GET /a", 10, 100, &[(0, 10)]),
            route("GET /gone", 5, 5, &[]),
        ];
        let now = vec![
            route("GET /a", 15, 160, &[(0, 12), (3, 3)]),
            route("GET /new", 2, 40, &[(5, 2)]),
            route("GET /idle", 0, 0, &[]),
        ];
        let d = delta(&before, &now);
        assert_eq!(d.len(), 2, "only routes with requests in the window: {d:?}");
        let a = d.iter().find(|r| r.route == "GET /a").unwrap();
        assert_eq!((a.requests, a.php_us), (5, 60_000));
        assert_eq!(a.buckets[0], 2);
        assert_eq!(a.buckets[3], 3);
        assert_eq!(
            d.iter().find(|r| r.route == "GET /new").unwrap().requests,
            2
        );
    }

    #[test]
    fn the_table_puts_the_expensive_route_first_and_says_why() {
        let routes = vec![
            route("GET /cheap", 100, 50, &[(0, 100)]),
            route("GET /api/search", 10, 950, &[(7, 10)]),
        ];
        let t = render(&routes, 10.0, Sort::Cpu, 10);
        let lines: Vec<&str> = t.lines().collect();
        assert!(
            lines[0].starts_with("2 routes · 11.0 req/s · PHP 0.10 s/s"),
            "{t}"
        );
        assert!(lines[2].starts_with("ROUTE"), "{t}");
        assert!(lines[3].starts_with("GET /api/search"), "{t}");
        assert!(
            lines[3].contains("95%") && lines[3].contains("≤250ms"),
            "{t}"
        );
        let by_req = render(&routes, 10.0, Sort::Requests, 1);
        assert!(
            by_req.contains("GET /cheap") && by_req.contains("… and 1 more"),
            "{by_req}"
        );
    }
}
