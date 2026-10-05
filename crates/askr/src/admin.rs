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
    /// Whether that listener speaks TLS, for `askr why`'s probe.
    pub server_tls: bool,
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
        // Startup refuses a non-loopback bind without a token (main.rs), so reaching
        // here off-box means a token is set. Say so — the bind is still a choice worth
        // seeing in the log.
        tracing::warn!(
            %addr,
            token = token.is_some(),
            "admin plane bound to a non-loopback address; protected by ASKR_ADMIN_TOKEN"
        );
    } else if token.is_none() {
        tracing::info!(
            %addr,
            "admin plane on loopback without ASKR_ADMIN_TOKEN: open to local processes, \
             which is the documented model; set a token to require one anyway"
        );
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
        (&Method::GET, "/api/why") => why(&req, &info).await,
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

/// `askr why`: send the request through this server and report its decisions.
async fn why(req: &Request<hyper::body::Incoming>, info: &Info) -> Response<Full<Bytes>> {
    let error = |status: StatusCode, msg: String| {
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(to_json(
                &serde_json::json!({ "error": msg }),
            ))))
            .unwrap()
    };
    let q = match crate::explain::Question::from_query(req.uri().query().unwrap_or("")) {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    match crate::explain::probe(info.server_listen, info.server_tls, &q).await {
        Ok(report) => json(to_json(&report)),
        Err(e) => error(StatusCode::BAD_GATEWAY, e),
    }
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
    let workers: Vec<WorkerDoc> = s
        .pids
        .iter()
        .map(|&pid| WorkerDoc {
            pid,
            rss_kb: crate::metrics::rss_kb(pid).unwrap_or(0),
        })
        .collect();
    // Per-queue, because the aggregate hides the failure that matters. "queue_ready: 1"
    // is true whether the job is on a queue a worker polls or one nobody listens to, and
    // that ambiguity is what let a site's password-reset mail stop without anyone
    // noticing. The name comes from the ring, so it is what the app actually dispatched to.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Liveness per lane, so a reader can tell a lane nothing polls from one that is
    // merely busy without knowing Askr's thresholds. `null` means never, which is not
    // the same as "a long time ago" and must not render as a duration.
    let lanes = crate::queue::lanes();
    let ago = |ms: u64| (ms != 0).then(|| now_ms.saturating_sub(ms) / 1000);
    let app_id = |a: &Option<crate::ns::App>| a.map(|a| a.to_string());
    let occupied = crate::queue::by_queue_with_app();
    let queues = occupied
        .iter()
        .map(|(app, name, c)| {
            // Matched on the namespaced identity, not the display name. A lane polled by
            // another application's worker cannot yield these jobs, and reporting its
            // poll time here is what made an unreachable queue look attended.
            let lane = lanes.iter().find(|l| &l.name == name && &l.app == app);
            QueueDoc {
                queue: name.clone(),
                app: app_id(app),
                pending: c.pending,
                delayed: c.delayed,
                reserved: c.reserved,
                oldest_pending_secs: if c.oldest_pending_created_ms > 0 {
                    now_ms.saturating_sub(c.oldest_pending_created_ms) / 1000
                } else {
                    0
                },
                last_polled_secs: lane.and_then(|l| ago(l.last_polled_ms)),
                last_drained_secs: lane.and_then(|l| ago(l.last_drained_ms)),
            }
        })
        .collect();
    // Lanes a worker has polled but which hold nothing right now. Healthy, and worth
    // reporting: it is the evidence that a worker is attached to that name at all, which
    // is what makes an unattended lane elsewhere diagnosable rather than ambiguous.
    let queues_idle = lanes
        .iter()
        .filter(|l| !occupied.iter().any(|(a, n, _)| n == &l.name && a == &l.app))
        .map(|l| IdleLaneDoc {
            queue: l.name.clone(),
            app: app_id(&l.app),
            last_polled_secs: ago(l.last_polled_ms),
            last_drained_secs: ago(l.last_drained_ms),
        })
        .collect();
    // What is actually wrong, named, with the numbers that justify it.
    //
    // This is the field the Félagi outage needed and did not have: Askr held every
    // number, computed the fault correctly every ten seconds for three days, and only
    // ever wrote it to its own stderr. A product cannot render a log line it never sees,
    // and it should not have to reimplement the thresholds to rediscover a conclusion
    // Askr had already reached. Empty array means nothing is wrong.
    let warnings = crate::queue::warnings_from(now_ms, &occupied, &lanes)
        .into_iter()
        .map(|w| WarningDoc {
            kind: w.fault.kind(),
            queue: w.queue,
            app: app_id(&w.app),
            polled_by: match &w.fault {
                crate::queue::LaneFault::WrongApplication { polled_by } => {
                    polled_by.iter().map(|a| a.to_string()).collect()
                }
                _ => Vec::new(),
            },
            pending: w.pending,
            oldest_pending_secs: w.oldest_pending_secs,
            last_polled_secs: w.last_polled_secs,
            last_drained_secs: w.last_drained_secs,
            detail: match w.fault {
                crate::queue::LaneFault::WrongApplication { .. } => {
                    "these jobs were pushed by one application and the only workers \
                     polling this queue name belong to another, so no worker can ever \
                     see them — set [queue] root (and [scheduler] root) to the docroot \
                     of the application that dispatches them. Adding workers cannot \
                     help"
                }
                crate::queue::LaneFault::Unattended => {
                    "no worker is asking this queue for jobs — check the queue name a \
                     worker polls (ASKR_QUEUE) against the one the app dispatches to"
                }
                crate::queue::LaneFault::NotDraining => {
                    "workers poll this queue and the backlog is still growing — raise \
                     the queue worker count, or check what is failing and releasing \
                     jobs back"
                }
            },
        })
        .collect();
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
        SandboxDoc {
            configured: info.sandbox,
            required: info.sandbox_required,
            workers: w,
            seccomp: sc,
            landlock: ll,
            landlock_abi: abi,
        }
    };
    to_json(&StatusDoc {
        version: env!("CARGO_PKG_VERSION"),
        listen: info.server_listen.to_string(),
        mode: info.mode,
        uptime_secs: s.uptime_secs,
        workers_configured: s.workers_configured,
        workers_alive: s.workers_alive,
        respawns: s.respawns,
        rss_kb_total: workers.iter().map(|w| w.rss_kb).sum(),
        queue_workers: s.queue_workers,
        queue_ready: s.queue_ready,
        queue_total: s.queue_total,
        queue_oldest_secs: s.queue_oldest_secs,
        queues,
        queues_idle,
        warnings,
        rollout: s.rollout,
        sandbox,
        pids: s.pids,
        workers,
    })
}

/// Escape a Prometheus label value: backslash, double quote and newline, and nothing
/// else (the exposition format defines only those three).
///
/// An app is free to name a queue `say "hi"`, and a scrape that cannot be parsed loses
/// every series in the response, not just this one.
fn label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
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
    let (hits, misses, coalesced) = crate::rcache::stats();
    to_json(&MetricsDoc {
        requests: req,
        errors: m.errors.load(Relaxed),
        bytes_out: m.bytes_out.load(Relaxed),
        avg_total_ms: two_dp(avg_total_ms),
        avg_php_ms: two_dp(avg_php_ms),
        php_pct,
        io_pct: 100 - php_pct,
        slowest_ms: two_dp(m.slowest_us.load(Relaxed) as f64 / 1000.0),
        cache: CacheDoc {
            hits,
            misses,
            coalesced,
            hit_pct: hits
                .saturating_mul(100)
                .checked_div(hits + misses)
                .unwrap_or(0),
        },
        status: StatusCountsDoc {
            s1: st[0],
            s2: st[1],
            s3: st[2],
            s4: st[3],
            s5: st[4],
        },
        histogram: HistogramDoc {
            bounds_ms: crate::metrics::BUCKET_BOUNDS_MS.to_vec(),
            counts: m.bucket_counts().to_vec(),
        },
    })
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
    let (enabled, errors) = match &info.record_dir {
        Some(dir) => (
            true,
            crate::record::list(dir)
                .into_iter()
                .take(20)
                .map(|(id, status)| ErrorDoc { id, status })
                .collect(),
        ),
        None => (false, Vec::new()),
    };
    to_json(&ErrorsDoc { enabled, errors })
}

// --- JSON documents -----------------------------------------------------------------------
//
// The admin API's documents, as types. They used to be nine hand-written `format!`
// templates with their own escaping, and five string fields — `version`, `listen`, `mode`,
// `rollout`, `errors[].id` — went in with none at all, safe only because today's values
// happen to be fixed words. Every change to a document was an edit to a raw string with
// `{{` doubling and hand-placed commas. Field order is declaration order, so each document
// reads exactly as it did.

#[derive(serde::Serialize)]
struct StatusDoc {
    version: &'static str,
    listen: String,
    mode: &'static str,
    uptime_secs: u64,
    workers_configured: usize,
    workers_alive: usize,
    respawns: usize,
    rss_kb_total: u64,
    queue_workers: usize,
    queue_ready: usize,
    queue_total: usize,
    queue_oldest_secs: u64,
    queues: Vec<QueueDoc>,
    queues_idle: Vec<IdleLaneDoc>,
    warnings: Vec<WarningDoc>,
    rollout: &'static str,
    sandbox: SandboxDoc,
    workers: Vec<WorkerDoc>,
    pids: Vec<i32>,
}

#[derive(serde::Serialize)]
struct QueueDoc {
    queue: String,
    app: Option<String>,
    pending: u64,
    delayed: u64,
    reserved: u64,
    oldest_pending_secs: u64,
    /// `null` means never — not "a long time ago".
    last_polled_secs: Option<u64>,
    last_drained_secs: Option<u64>,
}

#[derive(serde::Serialize)]
struct IdleLaneDoc {
    queue: String,
    app: Option<String>,
    last_polled_secs: Option<u64>,
    last_drained_secs: Option<u64>,
}

#[derive(serde::Serialize)]
struct WarningDoc {
    /// Stable; switch on this. `detail` is prose and is not.
    kind: &'static str,
    queue: String,
    app: Option<String>,
    polled_by: Vec<String>,
    pending: u64,
    oldest_pending_secs: u64,
    last_polled_secs: Option<u64>,
    last_drained_secs: Option<u64>,
    detail: &'static str,
}

#[derive(serde::Serialize)]
struct SandboxDoc {
    configured: bool,
    required: bool,
    workers: u64,
    seccomp: u64,
    landlock: u64,
    landlock_abi: u64,
}

#[derive(serde::Serialize)]
struct WorkerDoc {
    pid: i32,
    rss_kb: u64,
}

#[derive(serde::Serialize)]
struct MetricsDoc {
    requests: u64,
    errors: u64,
    bytes_out: u64,
    avg_total_ms: f64,
    avg_php_ms: f64,
    php_pct: u64,
    io_pct: u64,
    slowest_ms: f64,
    cache: CacheDoc,
    status: StatusCountsDoc,
    histogram: HistogramDoc,
}

#[derive(serde::Serialize)]
struct CacheDoc {
    hits: u64,
    misses: u64,
    coalesced: u64,
    hit_pct: u64,
}

#[derive(serde::Serialize)]
struct StatusCountsDoc {
    #[serde(rename = "1xx")]
    s1: u64,
    #[serde(rename = "2xx")]
    s2: u64,
    #[serde(rename = "3xx")]
    s3: u64,
    #[serde(rename = "4xx")]
    s4: u64,
    #[serde(rename = "5xx")]
    s5: u64,
}

#[derive(serde::Serialize)]
struct HistogramDoc {
    bounds_ms: Vec<u64>,
    counts: Vec<u64>,
}

#[derive(serde::Serialize)]
struct ErrorsDoc {
    enabled: bool,
    errors: Vec<ErrorDoc>,
}

#[derive(serde::Serialize)]
struct ErrorDoc {
    id: String,
    status: u16,
}

/// Serialise an admin document. These types cannot fail to serialise — no maps with
/// non-string keys, and serde_json writes a non-finite float as `null` — so the fallback
/// is unreachable, and an empty object is a safer answer than a panic in the admin thread.
fn to_json<T: serde::Serialize>(doc: &T) -> String {
    serde_json::to_string(doc).unwrap_or_else(|_| "{}".to_string())
}

/// Two decimal places, as the dashboard has always shown these.
///
/// Parsed back from the same `{:.2}` formatting the hand-built document used, so the
/// value is exactly the one it wrote rather than a rounding that agrees most of the
/// time. serde would otherwise print every digit of the average.
fn two_dp(x: f64) -> f64 {
    format!("{x:.2}").parse().unwrap_or(0.0)
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

    // Per queue, labelled. The aggregates above cannot answer "which lane", and that is
    // the only question worth alerting on: a fleet-wide `askr_queue_oldest_seconds` is
    // equally high whether one abandoned lane is ageing or every lane is busy.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let occupied = crate::queue::by_queue_with_app();
    let lanes = crate::queue::lanes();
    if !occupied.is_empty() || !lanes.is_empty() {
        let _ = write!(
            s,
            "# HELP askr_queue_pending_jobs Jobs ready and unclaimed, by queue.\n# TYPE askr_queue_pending_jobs gauge\n\
             # HELP askr_queue_oldest_pending_seconds Age of the oldest ready job, by queue.\n# TYPE askr_queue_oldest_pending_seconds gauge\n\
             # HELP askr_queue_seconds_since_poll Seconds since a worker last asked this queue for work.\n# TYPE askr_queue_seconds_since_poll gauge\n\
             # HELP askr_queue_seconds_since_drain Seconds since a worker last took a job from this queue.\n# TYPE askr_queue_seconds_since_drain gauge\n\
             # HELP askr_queue_unattended 1 when jobs are waiting and nothing is polling this queue.\n# TYPE askr_queue_unattended gauge\n\
             # HELP askr_queue_unreachable 1 when jobs are waiting under one application and only another application's workers poll that queue name.\n# TYPE askr_queue_unreachable gauge\n"
        );
        for (app, name, c) in &occupied {
            let q = label_value(name);
            let a = label_value(app.as_ref().map_or("", |a| a.as_str()));
            let age = if c.oldest_pending_created_ms > 0 {
                now_ms.saturating_sub(c.oldest_pending_created_ms) / 1000
            } else {
                0
            };
            let _ = write!(
                s,
                "askr_queue_pending_jobs{{queue=\"{q}\",app=\"{a}\"}} {}\naskr_queue_oldest_pending_seconds{{queue=\"{q}\",app=\"{a}\"}} {age}\n",
                c.pending
            );
        }
        // A lane never polled has no age to report. Emitting 0 would read as "polled just
        // now" — the opposite of the truth — so the series is simply absent, and an alert
        // uses `absent()` or the unattended gauge instead.
        for l in &lanes {
            let q = label_value(&l.name);
            let a = label_value(l.app.as_ref().map_or("", |a| a.as_str()));
            if l.last_polled_ms > 0 {
                let _ = writeln!(
                    s,
                    "askr_queue_seconds_since_poll{{queue=\"{q}\",app=\"{a}\"}} {}",
                    now_ms.saturating_sub(l.last_polled_ms) / 1000
                );
            }
            if l.last_drained_ms > 0 {
                let _ = writeln!(
                    s,
                    "askr_queue_seconds_since_drain{{queue=\"{q}\",app=\"{a}\"}} {}",
                    now_ms.saturating_sub(l.last_drained_ms) / 1000
                );
            }
        }
        let warned = crate::queue::warnings_from(now_ms, &occupied, &lanes);
        for (app, name, _) in &occupied {
            let q = label_value(name);
            let a = label_value(app.as_ref().map_or("", |a| a.as_str()));
            let fault = warned
                .iter()
                .find(|w| &w.queue == name && &w.app == app)
                .map(|w| &w.fault);
            let unattended = matches!(fault, Some(crate::queue::LaneFault::Unattended));
            // Separate from `unattended` on purpose: "nobody is listening" and "somebody
            // is listening, under the wrong application" have different fixes, and an
            // alert that conflates them sends the operator to the queue name when the
            // name is already right.
            let unreachable = matches!(
                fault,
                Some(crate::queue::LaneFault::WrongApplication { .. })
            );
            let _ = write!(
                s,
                "askr_queue_unattended{{queue=\"{q}\",app=\"{a}\"}} {}\naskr_queue_unreachable{{queue=\"{q}\",app=\"{a}\"}} {}\n",
                u8::from(unattended),
                u8::from(unreachable)
            );
        }
    }

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

    /// Queue names come from the application, and a name with a quote, a backslash or a
    /// control character in it must come back out of the status document unchanged.
    ///
    /// Through the real document rather than an escaping helper: the helper was correct,
    /// and five other string fields went into the same hand-built document with no
    /// escaping at all. What matters is what a dashboard parses.
    #[test]
    fn a_hostile_queue_name_survives_the_status_document() {
        let _g = crate::ns::tests::GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::squeue::init(64);
        crate::ns::clear();
        let hostile = "say \"hi\" \\ tab\there\nline\u{7}";
        assert!(crate::squeue::push(hostile.as_bytes(), b"{}", 0) > 0);
        let info = Info {
            server_listen: "127.0.0.1:8000".parse().unwrap(),
            server_tls: false,
            mode: "worker",
            record_dir: None,
            sandbox: false,
            sandbox_required: false,
        };
        let doc: serde_json::Value =
            serde_json::from_str(&status_json(&info)).expect("the status document must parse");
        let names: Vec<&str> = doc["queues"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|q| q["queue"].as_str())
            .collect();
        assert!(
            names.contains(&hostile),
            "the name round-trips exactly: {names:?}"
        );
        while let Some(r) = crate::squeue::pop(hostile.as_bytes(), 30) {
            crate::squeue::delete(r.id);
        }
    }
    use super::*;

    /// Every admin JSON endpoint must emit a document that parses.
    ///
    /// These used to be hand-built with `format!`, where the risk was that someone would
    /// one day interpolate a string that isn't machine-generated and quietly emit broken
    /// JSON to every dashboard and scraper. They are serialised from types now, which
    /// removes that risk by construction; this stays as the guard that says so.
    #[test]
    fn admin_json_endpoints_emit_valid_json() {
        let info = Info {
            server_listen: "127.0.0.1:8000".parse().unwrap(),
            server_tls: false,
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
