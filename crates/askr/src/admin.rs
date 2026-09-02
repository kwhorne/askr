//! Admin dashboard + API — the built-in "GUI" for maintaining/configuring a
//! running server. It runs in the master process (its own thread + tiny tokio
//! runtime) and exposes:
//!
//!   GET  /            a minimal HTML dashboard (auto-refreshing)
//!   GET  /api/status  supervisor status as JSON
//!   POST /api/reload  trigger a graceful rolling reload
//!
//! Bind it to localhost (default in examples) or reach it over a private
//! network / SSH tunnel. A future desktop control-center (Grove-style) can drive
//! several servers through this same API.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Static server info shown on the dashboard.
#[derive(Clone)]
pub struct Info {
    pub server_listen: SocketAddr,
    pub mode: &'static str,
    pub record_dir: Option<std::path::PathBuf>,
    /// The sandbox as *configured*. What the workers achieved comes from the metrics
    /// region at request time; the status document reports both, side by side.
    pub sandbox: bool,
    pub sandbox_required: bool,
}

/// Start the admin server on its own thread. Never blocks the caller.
pub fn spawn(addr: SocketAddr, info: Info) {
    // Optional bearer token. When set, the mutating endpoint and the info-leaking
    // endpoints require `Authorization: Bearer <token>`.
    let token = std::env::var("ASKR_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    // The admin plane exposes PIDs/RSS/error records and a reload trigger. It has
    // no transport security of its own, so warn loudly if it's reachable off-box.
    if !addr.ip().is_loopback() {
        if token.is_some() {
            tracing::warn!(
                %addr,
                "admin plane bound to a non-loopback address (protected by ASKR_ADMIN_TOKEN)"
            );
        } else {
            tracing::warn!(
                %addr,
                "admin plane bound to a non-loopback address WITHOUT ASKR_ADMIN_TOKEN — \
                 /api/reload is unauthenticated and status/metrics/errors are exposed; \
                 bind to loopback or set ASKR_ADMIN_TOKEN"
            );
        }
    }
    let token = Arc::new(token);
    thread::Builder::new()
        .name("askr-admin".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "admin: runtime");
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(error = %e, %addr, "admin: bind failed");
                        return;
                    }
                };
                tracing::info!(%addr, "admin dashboard listening");
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let io = TokioIo::new(stream);
                    let info = info.clone();
                    let token = token.clone();
                    tokio::task::spawn(async move {
                        let service =
                            service_fn(move |req| handle(req, addr, info.clone(), token.clone()));
                        let _ = http1::Builder::new().serve_connection(io, service).await;
                    });
                }
            });
        })
        .ok();
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    addr: SocketAddr,
    info: Info,
    token: Arc<Option<String>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // When a token is configured, gate the reload trigger and the endpoints that
    // leak operational data. The dashboard shell (`GET /`) stays open — it carries
    // no data itself and its API calls are gated.
    // Deny by default. The previous list of exact paths had no bypass — anything
    // unmatched 404s before reaching data — but it meant a new endpoint was
    // unauthenticated until someone remembered to add it here, and "remember to also
    // edit this list" is not an access-control policy. Now everything under /api/ and
    // /metrics is gated, with the dashboard shell explicitly open: it carries no data of
    // its own and the API calls it makes are gated.
    let protected = !path_is_open(&path);
    if protected {
        // Who is asking, before what they know. Both checks below cost nothing and
        // apply whether or not a token is configured — a token is opt-in, and these
        // two attacks both work fine against a plane that never had one.
        if !host_names_this_listener(&req, addr) {
            return Ok(deny("askr: unexpected Host header for the admin plane"));
        }
        if browser_says_cross_site(&req) {
            return Ok(deny("askr: cross-site request refused"));
        }
        if let Some(tok) = token.as_ref() {
            if !bearer_ok(&req, tok) {
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("WWW-Authenticate", "Bearer")
                    .body(Full::new(Bytes::from("unauthorized")))
                    .unwrap());
            }
        }
    }

    let resp = match (&method, path.as_str()) {
        (&Method::GET, "/") => html(DASHBOARD),
        (&Method::GET, "/healthz") => healthz(),
        (&Method::GET, "/api/status") => json(status_json(&info)),
        (&Method::GET, "/api/metrics") => json(metrics_json()),
        (&Method::GET, "/metrics") => prometheus(),
        (&Method::GET, "/api/errors") => json(errors_json(&info)),
        (&Method::POST, "/api/reload") => {
            crate::supervisor::trigger_reload();
            json(r#"{"ok":true,"action":"reload"}"#.to_string())
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("not found")))
            .unwrap(),
    };
    Ok(resp)
}

fn deny(msg: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Full::new(Bytes::from(msg)))
        .unwrap()
}

/// Does the `Host` header name this listener?
///
/// This is the defence against DNS rebinding, and it is the only one that works. A
/// page on the attacker's domain re-resolves its own hostname to 127.0.0.1; the
/// browser then considers `http://evil.test:9000/api/status` same-origin and hands
/// the response to the attacker's script. Nothing about the request looks
/// cross-site — no `Origin`, `Sec-Fetch-Site: same-origin` — because as far as the
/// browser is concerned it isn't. What the attacker cannot change is that `Host:`
/// says `evil.test` and this listener is not called that.
///
/// Enforced only for a loopback bind. Bound to a private address the admin plane is
/// legitimately reached by name, and that is also the case rebinding cannot reach.
/// `ASKR_ADMIN_HOSTS` (comma-separated) extends the list for a loopback bind sitting
/// behind a proxy that forwards the original `Host`.
fn host_names_this_listener<B>(req: &Request<B>, addr: SocketAddr) -> bool {
    if !addr.ip().is_loopback() {
        return true;
    }
    // HTTP/1.1 requires Host and this listener is http1-only, so absence is not a
    // client we need to accommodate.
    let Some(host) = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let name = crate::cgi::host_without_port(host)
        .trim_start_matches('[')
        .trim_end_matches(']');
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // Any loopback literal: only loopback can reach a loopback bind anyway.
    if name
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
    {
        return true;
    }
    std::env::var("ASKR_ADMIN_HOSTS").is_ok_and(|allowed| {
        allowed
            .split(',')
            .map(str::trim)
            .any(|a| !a.is_empty() && a.eq_ignore_ascii_case(name))
    })
}

/// Did a browser tell us this request came from another site?
///
/// `POST /api/reload` with no custom headers is a CORS "simple request": no
/// preflight, so CORS never gets a say, and any page anywhere could roll the fleet.
/// Browsers do say where a request came from, in `Sec-Fetch-Site` and `Origin`.
///
/// This rejects what identifies itself as cross-site rather than demanding proof of
/// not being a browser: curl and deploy scripts send neither header and keep working,
/// which is the point — `POST /api/reload` is documented and in use.
fn browser_says_cross_site<B>(req: &Request<B>) -> bool {
    if let Some(site) = req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
    {
        let site = site.trim();
        if !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none") {
            return true;
        }
    }
    if let Some(origin) = req
        .headers()
        .get(hyper::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // "null" (a sandboxed or file:// origin) matches nothing and is refused.
        let origin_host = origin.trim().rsplit("//").next().unwrap_or("");
        if !origin_host.eq_ignore_ascii_case(host) {
            return true;
        }
    }
    false
}

/// Constant-time check of an `Authorization: Bearer <token>` header.
fn bearer_ok(req: &Request<hyper::body::Incoming>, token: &str) -> bool {
    let Some(h) = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let (a, b) = (h.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn status_json(info: &Info) -> String {
    let s = crate::supervisor::status();
    let mut rss_total = 0u64;
    let workers = s
        .pids
        .iter()
        .map(|&p| {
            let rss = crate::metrics::rss_kb(p).unwrap_or(0);
            rss_total += rss;
            format!(r#"{{"pid":{p},"rss_kb":{rss}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    let pids = s
        .pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // Per-queue, because the aggregate hides the failure that matters. "queue_ready: 1"
    // is true whether the job is on a queue a worker polls or one nobody listens to, and
    // that ambiguity is what let a site's password-reset mail stop without anyone
    // noticing. The name comes from the ring, so it is what the app actually dispatched to.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let queues = crate::queue::by_queue()
        .into_iter()
        .map(|(name, c)| {
            let age = if c.oldest_pending_created_ms > 0 {
                now_ms.saturating_sub(c.oldest_pending_created_ms) / 1000
            } else {
                0
            };
            format!(
                r#"{{"queue":{name},"pending":{p},"delayed":{d},"reserved":{r},"oldest_pending_secs":{age}}}"#,
                name = json_string(&name),
                p = c.pending,
                d = c.delayed,
                r = c.reserved,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    // Intent beside achievement. `configured`/`required` are what the operator asked
    // for; `workers`/`seccomp`/`landlock` are counted by the workers that applied it.
    // A fleet where `workers` exceeds `seccomp` or `landlock` is serving partly
    // unhardened, and that used to be invisible from here.
    let sandbox = {
        use std::sync::atomic::Ordering::Relaxed;
        let (w, sc, ll, abi) = match crate::metrics::Metrics::get() {
            Some(m) => (
                m.sandbox_workers.load(Relaxed),
                m.sandbox_seccomp.load(Relaxed),
                m.sandbox_landlock.load(Relaxed),
                m.sandbox_landlock_abi.load(Relaxed),
            ),
            None => (0, 0, 0, 0),
        };
        format!(
            r#"{{"configured":{c},"required":{r},"workers":{w},"seccomp":{sc},"landlock":{ll},"landlock_abi":{abi}}}"#,
            c = info.sandbox,
            r = info.sandbox_required,
        )
    };
    format!(
        r#"{{"version":"{ver}","listen":"{listen}","mode":"{mode}","uptime_secs":{up},"workers_configured":{wc},"workers_alive":{wa},"respawns":{rs},"rss_kb_total":{rss},"queue_workers":{qw},"queue_ready":{qr},"queue_total":{qt},"queue_oldest_secs":{qo},"queues":[{queues}],"rollout":"{ro}","sandbox":{sandbox},"workers":[{workers}],"pids":[{pids}]}}"#,
        ver = env!("CARGO_PKG_VERSION"),
        listen = info.server_listen,
        mode = info.mode,
        up = s.uptime_secs,
        wc = s.workers_configured,
        wa = s.workers_alive,
        rs = s.respawns,
        rss = rss_total,
        qw = s.queue_workers,
        qr = s.queue_ready,
        qt = s.queue_total,
        qo = s.queue_oldest_secs,
        ro = s.rollout,
    )
}

/// Quote a string for JSON.
///
/// Queue names come from the application, so they are the one field here that is not
/// machine-generated — everything else in this document is a number or a fixed word. An
/// app is free to name a queue `say "hi"`, and a hand-built document has to survive it.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn metrics_json() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(m) = crate::metrics::Metrics::get() else {
        return "{}".to_string();
    };
    let req = m.requests.load(Relaxed);
    let php = m.php_us.load(Relaxed);
    let total = m.total_us.load(Relaxed);
    let (avg_total_ms, avg_php_ms) = if req > 0 {
        (
            total as f64 / req as f64 / 1000.0,
            php as f64 / req as f64 / 1000.0,
        )
    } else {
        (0.0, 0.0)
    };
    let php_pct = php.saturating_mul(100).checked_div(total).unwrap_or(0);
    let st: Vec<u64> = (0..5).map(|i| m.status[i].load(Relaxed)).collect();
    let buckets = m.bucket_counts();
    let bounds = crate::metrics::BUCKET_BOUNDS_MS;
    let bounds_s = bounds
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let counts_s = buckets
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let (chits, cmisses, ccoalesced) = crate::rcache::stats();
    let ctotal = chits + cmisses;
    let chit_pct = chits.saturating_mul(100).checked_div(ctotal).unwrap_or(0);
    format!(
        r#"{{"requests":{req},"errors":{err},"bytes_out":{bytes},"avg_total_ms":{att:.2},"avg_php_ms":{aph:.2},"php_pct":{php_pct},"io_pct":{io_pct},"slowest_ms":{slow:.2},"cache":{{"hits":{chits},"misses":{cmisses},"coalesced":{ccoalesced},"hit_pct":{chit_pct}}},"status":{{"1xx":{s1},"2xx":{s2},"3xx":{s3},"4xx":{s4},"5xx":{s5}}},"histogram":{{"bounds_ms":[{bounds_s}],"counts":[{counts_s}]}}}}"#,
        req = req,
        err = m.errors.load(Relaxed),
        bytes = m.bytes_out.load(Relaxed),
        att = avg_total_ms,
        aph = avg_php_ms,
        php_pct = php_pct,
        io_pct = 100 - php_pct,
        slow = m.slowest_us.load(Relaxed) as f64 / 1000.0,
        s1 = st[0],
        s2 = st[1],
        s3 = st[2],
        s4 = st[3],
        s5 = st[4],
    )
}

/// Paths served without a bearer token when `ASKR_ADMIN_TOKEN` is set.
///
/// Deny by default: everything not named here is gated, so an endpoint added later is
/// protected without anyone having to remember to protect it. The dashboard shell carries
/// no data of its own (its API calls are gated), and `/healthz` answers liveness only.
fn path_is_open(path: &str) -> bool {
    matches!(path, "/" | "/favicon.ico" | "/healthz")
}

/// Liveness for orchestrators: 200 when at least one worker can serve, else 503.
///
/// Deliberately unauthenticated and deliberately empty. The container healthcheck used
/// to poll `/api/status`, which returns PIDs and memory figures and is therefore gated by
/// `ASKR_ADMIN_TOKEN` — so switching that token on made Docker, Kubernetes and Swarm
/// declare a perfectly healthy container unhealthy and restart it. A probe that needs a
/// credential is a probe that will eventually be wrong.
///
/// It answers with liveness only. Two words leak nothing, and anything richer would
/// recreate the reason `/api/status` needs protecting.
fn healthz() -> Response<Full<Bytes>> {
    // `workers_configured` is only set by a supervisor, so zero means single-process
    // mode: there is no worker table, and this thread answering *is* the liveness
    // signal. Reading `workers_alive` unconditionally would report 503 on a perfectly
    // healthy single-process server — which is the same class of false alarm this
    // endpoint exists to remove.
    let s = crate::supervisor::status();
    let ok = s.workers_configured == 0 || s.workers_alive > 0;
    let (code, body) = if ok {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no workers")
    };
    Response::builder()
        .status(code)
        .header(hyper::header::CONTENT_TYPE, "text/plain")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn errors_json(info: &Info) -> String {
    let Some(dir) = &info.record_dir else {
        return r#"{"enabled":false,"errors":[]}"#.to_string();
    };
    let items = crate::record::list(dir)
        .into_iter()
        .take(20)
        .map(|(id, status)| format!(r#"{{"id":"{id}","status":{status}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"enabled":true,"errors":[{items}]}}"#)
}

fn push_counter(s: &mut String, name: &str, help: &str, val: &str) {
    use std::fmt::Write;
    let _ = write!(
        s,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n"
    );
}

/// Prometheus text-format exposition of the shared metrics (`GET /metrics`).
fn prometheus() -> Response<Full<Bytes>> {
    use std::fmt::Write;
    use std::sync::atomic::Ordering::Relaxed;
    let mut s = String::new();
    let Some(m) = crate::metrics::Metrics::get() else {
        return text_plain(s);
    };

    push_counter(
        &mut s,
        "askr_requests_total",
        "Total HTTP requests served.",
        &m.requests.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_errors_total",
        "Requests that failed at the server layer.",
        &m.errors.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_bytes_out_total",
        "Response bytes sent.",
        &m.bytes_out.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_php_seconds_total",
        "Cumulative time spent in PHP.",
        &format!("{:.6}", m.php_us.load(Relaxed) as f64 / 1e6),
    );
    push_counter(
        &mut s,
        "askr_request_seconds_total",
        "Cumulative total request time.",
        &format!("{:.6}", m.total_us.load(Relaxed) as f64 / 1e6),
    );
    push_counter(
        &mut s,
        "askr_cache_evictions_total",
        "KV cache entries evicted under pressure.",
        &m.cache_evictions.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_cache_oversize_total",
        "KV cache writes dropped for exceeding the largest slot (64 KB).",
        &m.cache_oversize.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_cache_tag_overflow_total",
        "Responses not cached because they carried more tags than an entry holds.",
        &m.cache_tag_overflow.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_ratelimit_blocked_total",
        "Requests refused by a [[ratelimit]] rule before reaching PHP.",
        &m.ratelimit_blocked.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_shadow_total",
        "Requests mirrored to the shadow upstream.",
        &m.shadow_total.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_shadow_match_total",
        "Shadow responses matching production (status + body).",
        &m.shadow_match.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_shadow_mismatch_total",
        "Shadow responses diverging from production.",
        &m.shadow_mismatch.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_shadow_error_total",
        "Shadow upstream unreachable / read errors.",
        &m.shadow_error.load(Relaxed).to_string(),
    );

    // Response status classes.
    let _ = write!(
        s,
        "# HELP askr_responses_total Responses by status class.\n# TYPE askr_responses_total counter\n"
    );
    for (i, class) in ["1xx", "2xx", "3xx", "4xx", "5xx"].iter().enumerate() {
        let _ = writeln!(
            s,
            "askr_responses_total{{class=\"{class}\"}} {}",
            m.status[i].load(Relaxed)
        );
    }

    // Response cache.
    let (hits, misses, coalesced) = crate::rcache::stats();
    push_counter(
        &mut s,
        "askr_cache_hits_total",
        "Response cache hits.",
        &hits.to_string(),
    );
    push_counter(
        &mut s,
        "askr_cache_misses_total",
        "Response cache misses.",
        &misses.to_string(),
    );
    push_counter(
        &mut s,
        "askr_cache_coalesced_total",
        "Requests served by coalescing onto a leader.",
        &coalesced.to_string(),
    );

    // Gauges.
    let _ = write!(
        s,
        "# HELP askr_inflight Requests currently executing in PHP.\n# TYPE askr_inflight gauge\naskr_inflight {}\n",
        m.inflight.load(Relaxed)
    );
    let st = crate::supervisor::status();
    let _ = write!(
        s,
        "# HELP askr_workers_alive Live worker processes.\n# TYPE askr_workers_alive gauge\naskr_workers_alive {}\n",
        st.workers_alive
    );
    // Queue backlog + autoscaled worker count (0 when the job queue is off).
    let _ = write!(
        s,
        "# HELP askr_queue_workers Queue-worker processes (autoscaled).\n# TYPE askr_queue_workers gauge\naskr_queue_workers {}\n\
         # HELP askr_queue_ready Ready jobs waiting for a worker.\n# TYPE askr_queue_ready gauge\naskr_queue_ready {}\n\
         # HELP askr_queue_total Occupied job slots (incl. delayed/reserved).\n# TYPE askr_queue_total gauge\naskr_queue_total {}\n\
         # HELP askr_queue_oldest_seconds Age of the oldest ready job.\n# TYPE askr_queue_oldest_seconds gauge\naskr_queue_oldest_seconds {}\n",
        st.queue_workers, st.queue_ready, st.queue_total, st.queue_oldest_secs
    );

    // Latency histogram (cumulative buckets, seconds).
    let buckets = m.bucket_counts();
    let bounds = crate::metrics::BUCKET_BOUNDS_MS;
    let _ = write!(
        s,
        "# HELP askr_request_duration_seconds Request latency.\n# TYPE askr_request_duration_seconds histogram\n"
    );
    let mut cum = 0u64;
    for (i, &bound) in bounds.iter().enumerate() {
        cum += buckets[i];
        let _ = writeln!(
            s,
            "askr_request_duration_seconds_bucket{{le=\"{:.3}\"}} {cum}",
            bound as f64 / 1000.0
        );
    }
    cum += buckets[bounds.len()]; // overflow bucket
    let _ = write!(
        s,
        "askr_request_duration_seconds_bucket{{le=\"+Inf\"}} {cum}\naskr_request_duration_seconds_count {cum}\naskr_request_duration_seconds_sum {:.6}\n",
        m.total_us.load(Relaxed) as f64 / 1e6
    );

    text_plain(s)
}

fn text_plain(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            hyper::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn json(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn html(body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap()
}

const DASHBOARD: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Askr admin</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root { color-scheme: light dark; }
  body { font: 15px/1.5 system-ui, sans-serif; max-width: 720px; margin: 3rem auto; padding: 0 1rem; }
  h1 { font-size: 1.4rem; } h1 small { color: #888; font-weight: 400; font-size: .7em; }
  table { border-collapse: collapse; width: 100%; margin: 1rem 0; }
  td { padding: .4rem .6rem; border-bottom: 1px solid #8883; }
  td:first-child { color: #888; width: 40%; }
  .pill { display:inline-block; padding:.1rem .5rem; border-radius:1rem; background:#3a7afe22; color:#3a7afe; }
  button { font: inherit; padding: .5rem 1rem; border: 0; border-radius: .4rem; background: #3a7afe; color: #fff; cursor: pointer; }
  button:active { transform: translateY(1px); }
  #msg { margin-left: 1rem; color: #2a2; }
</style></head>
<body>
  <h1>🌳 Askr <small id="ver"></small></h1>
  <table>
    <tr><td>Listening</td><td id="listen">—</td></tr>
    <tr><td>Mode</td><td id="mode">—</td></tr>
    <tr><td>Uptime</td><td id="uptime">—</td></tr>
    <tr><td>Workers</td><td id="workers">—</td></tr>
    <tr><td>Respawns</td><td id="respawns">—</td></tr>
    <tr><td>Memory (RSS)</td><td id="rss">—</td></tr>
    <tr><td>Worker PIDs</td><td id="pids">—</td></tr>
  </table>

  <h2 style="font-size:1.1rem;margin-top:2rem">Traffic</h2>
  <table>
    <tr><td>Throughput</td><td id="rps">—</td></tr>
    <tr><td>Requests</td><td id="requests">—</td></tr>
    <tr><td>Avg latency</td><td id="avglat">—</td></tr>
    <tr><td>PHP vs I/O</td><td id="split">—</td></tr>
    <tr><td>Response cache</td><td id="cache">—</td></tr>
    <tr><td>Slowest</td><td id="slowest">—</td></tr>
    <tr><td>Status</td><td id="status">—</td></tr>
    <tr><td>Latency</td><td id="hist" style="font:12px/1.4 ui-monospace,monospace">—</td></tr>
  </table>

  <h2 style="font-size:1.1rem;margin-top:2rem">Recorded failures <small style="color:#888;font-weight:400">— <code>askr replay &lt;id&gt;.json</code></small></h2>
  <div id="errors" style="font:13px/1.6 ui-monospace,monospace;color:#888">—</div>

  <button onclick="reload()">Graceful reload</button>
  <span id="msg"></span>
<script>
let last = null;
function bar(pct){ pct=Math.max(0,Math.min(100,pct)); return '<span style="display:inline-block;height:.8em;width:'+pct+'%;background:#3a7afe;border-radius:2px"></span>'; }
async function refresh() {
  try {
    const s = await (await fetch('/api/status')).json();
    ver.textContent = 'v' + s.version;
    listen.textContent = s.listen;
    mode.innerHTML = '<span class="pill">' + s.mode + '</span>';
    const h = Math.floor(s.uptime_secs/3600), mn = Math.floor(s.uptime_secs%3600/60), sec = s.uptime_secs%60;
    uptime.textContent = h + 'h ' + mn + 'm ' + sec + 's';
    workers.textContent = s.workers_alive + ' / ' + s.workers_configured + ' alive';
    respawns.textContent = s.respawns;
    rss.textContent = (s.rss_kb_total/1024).toFixed(0) + ' MB' +
      (s.workers && s.workers.length ? '  (' + s.workers.map(w => (w.rss_kb/1024).toFixed(0)).join(', ') + ' MB)' : '');
    pids.textContent = s.pids.join(', ');

    const m = await (await fetch('/api/metrics')).json();
    const now = performance.now();
    if (last && m.requests >= last.requests) {
      const dr = m.requests - last.requests, dt = (now - last.t) / 1000;
      rps.textContent = dt > 0 ? (dr/dt).toFixed(0) + ' req/s' : '—';
    }
    last = { requests: m.requests, t: now };
    requests.textContent = m.requests + (m.errors ? '  (' + m.errors + ' errors)' : '');
    avglat.textContent = (m.avg_total_ms||0).toFixed(1) + ' ms';
    split.innerHTML = 'PHP ' + m.php_pct + '%  ' + bar(m.php_pct) + '  I/O ' + m.io_pct + '%';
    const c = m.cache || {hits:0,misses:0,coalesced:0,hit_pct:0};
    cache.textContent = (c.hits+c.misses) ? (c.hit_pct + '% hit  (' + c.hits + ' hits, ' + c.misses + ' misses, ' + (c.coalesced||0) + ' coalesced)') : 'no lookups';
    slowest.textContent = (m.slowest_ms||0).toFixed(1) + ' ms';
    const st = m.status || {};
    status.textContent = ['2xx','3xx','4xx','5xx'].map(k => k+':'+(st[k]||0)).join('  ');
    const b = m.histogram || {bounds_ms:[],counts:[]};
    const max = Math.max(1, ...b.counts);
    const labels = b.bounds_ms.map(x => '≤'+x+'ms').concat(['>'+b.bounds_ms[b.bounds_ms.length-1]+'ms']);
    hist.innerHTML = b.counts.map((c,i) =>
      labels[i].padStart(8) + ' ' + '█'.repeat(Math.round(c/max*24)) + ' ' + c
    ).filter((_,i)=> b.counts[i]>0).join('<br>') || '(no traffic yet)';

    const er = await (await fetch('/api/errors')).json();
    if (!er.enabled) { errors.textContent = 'disabled (start with --record-errors <dir>)'; }
    else if (!er.errors.length) { errors.textContent = 'none recorded 🎉'; }
    else { errors.innerHTML = er.errors.map(e => 'HTTP ' + e.status + '  ' + e.id + '.json').join('<br>'); }
  } catch (e) {}
}
async function reload() {
  msg.textContent = 'reloading…';
  await fetch('/api/reload', { method: 'POST' });
  msg.textContent = 'rolling reload triggered';
  setTimeout(() => { msg.textContent = ''; refresh(); }, 2000);
}
refresh(); setInterval(refresh, 2000);
</script>
</body></html>"#;

#[cfg(test)]
mod tests {

    /// Queue names come from the application, and they are the only field in the status
    /// document that isn't machine-generated. A name with a quote in it would otherwise
    /// produce a document that parses as something else — or not at all.
    /// DNS rebinding is the attack this stops, and the reason it needs stopping at
    /// `Host` rather than at `Origin`: after the rebind the browser believes the
    /// request is same-origin and says so, or says nothing at all.
    #[test]
    fn a_rebound_host_is_refused_on_a_loopback_bind() {
        let local: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let req = |host: &str| {
            hyper::Request::builder()
                .uri("/api/status")
                .header("host", host)
                .body(())
                .unwrap()
        };

        assert!(host_names_this_listener(&req("127.0.0.1:9000"), local));
        assert!(host_names_this_listener(&req("localhost:9000"), local));
        assert!(host_names_this_listener(&req("localhost"), local));
        assert!(host_names_this_listener(&req("[::1]:9000"), local));

        assert!(
            !host_names_this_listener(&req("evil.test:9000"), local),
            "an attacker's hostname resolved to 127.0.0.1 is the whole attack"
        );
        assert!(!host_names_this_listener(&req("192.168.1.10:9000"), local));

        // No Host at all: HTTP/1.1 requires one and this listener is http1-only.
        let bare = hyper::Request::builder()
            .uri("/api/status")
            .body(())
            .unwrap();
        assert!(!host_names_this_listener(&bare, local));

        // Bound off-box the plane is reached by name on purpose — and that is also
        // the case rebinding cannot reach.
        let public: SocketAddr = "10.0.0.5:9000".parse().unwrap();
        assert!(host_names_this_listener(&req("admin.internal"), public));
    }

    /// `POST /api/reload` is a CORS "simple request", so no preflight ever ran and
    /// any page could roll the fleet. curl sends neither header and must keep
    /// working: this refuses what says it is cross-site, it does not demand proof of
    /// not being a browser.
    #[test]
    fn a_cross_site_request_is_refused_and_a_script_is_not() {
        let plain = hyper::Request::builder()
            .uri("/api/reload")
            .body(())
            .unwrap();
        assert!(
            !browser_says_cross_site(&plain),
            "curl and deploy scripts send neither header"
        );

        let from_a_page = hyper::Request::builder()
            .uri("/api/reload")
            .header("host", "127.0.0.1:9000")
            .header("sec-fetch-site", "cross-site")
            .body(())
            .unwrap();
        assert!(browser_says_cross_site(&from_a_page));

        let mismatched_origin = hyper::Request::builder()
            .uri("/api/reload")
            .header("host", "127.0.0.1:9000")
            .header("origin", "https://evil.test")
            .body(())
            .unwrap();
        assert!(browser_says_cross_site(&mismatched_origin));

        let own_dashboard = hyper::Request::builder()
            .uri("/api/reload")
            .header("host", "127.0.0.1:9000")
            .header("sec-fetch-site", "same-origin")
            .header("origin", "http://127.0.0.1:9000")
            .body(())
            .unwrap();
        assert!(!browser_says_cross_site(&own_dashboard));
    }

    #[test]
    fn queue_names_are_escaped_in_json() {
        assert_eq!(json_string("mail"), "\"mail\"");
        assert_eq!(json_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(json_string("bell\u{7}"), "\"bell\\u0007\"");
    }
    use super::*;

    /// These responses are built with `format!`, not a serializer. Nothing interpolated
    /// into them today is attacker- or app-controlled — record ids are
    /// `"{secs}-{pid}-{seq}"`, the rest are numbers, a socket address and compile-time
    /// constants — so there is no injection vector to fix. What there *is* is the risk
    /// that someone later interpolates a string that isn't machine-generated and quietly
    /// emits broken JSON to every dashboard and scraper. This test is the guard for that:
    /// it fails the moment the output stops parsing.
    #[test]
    fn admin_json_endpoints_emit_valid_json() {
        let info = Info {
            server_listen: "127.0.0.1:8000".parse().unwrap(),
            mode: "per-request",
            record_dir: None,
            sandbox: true,
            sandbox_required: false,
        };
        for (name, body) in [
            ("status", status_json(&info)),
            ("metrics", metrics_json()),
            ("errors", errors_json(&info)),
        ] {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&body);
            assert!(parsed.is_ok(), "{name} is not valid JSON: {body}");
            assert!(
                parsed.unwrap().is_object(),
                "{name} is not an object: {body}"
            );
        }
    }

    /// The healthcheck must not need a credential, and must not become a data endpoint.
    #[test]
    fn healthz_is_terse_and_open() {
        let r = healthz();
        assert_eq!(r.status(), StatusCode::OK);
        // Single-process mode (no supervisor) counts as live; see `healthz`.
        assert!(path_is_open("/healthz"));
        assert!(!path_is_open("/api/status"), "status must stay gated");
        assert!(!path_is_open("/metrics"), "metrics must stay gated");
        assert!(
            !path_is_open("/api/anything-added-later"),
            "deny by default"
        );
    }
}
