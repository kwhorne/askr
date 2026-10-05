//! The HTTP front: accept connections, serve static files directly, and hand
//! dynamic requests to the embedded PHP interpreter.
//!
//! tokio/hyper here is the pragmatic A1 I/O layer. The share-nothing endgame
//! swaps this for a per-core io_uring loop behind the same seam:
//! `Php::handle`.
//!
//! Recycling is graceful: after `recycle_after` requests we stop accepting new
//! connections, let the in-flight ones drain, and return — the caller then exits
//! the process and the supervisor respawns a fresh worker. No dropped requests.
//!
//! This file is the request path: configuration, the per-worker runtime, the accept loop
//! and `handle`. What `handle` calls out to lives beside it — [`trust`] (who the client
//! is), [`statics`] (files served without PHP), [`cache_policy`] (the HTTP side of the
//! response cache), [`esi`] and [`sse`].

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Notify};
use tokio_rustls::TlsAcceptor;

use fastwebsockets::upgrade;

use crate::cgi;
use crate::php::{Php, Reply};
use crate::pusher::{self, PusherHub};
use crate::rcache;

mod cache_policy;
mod esi;
mod sse;
mod statics;
mod trust;

use cache_policy::{
    cache_rule_for, cached_response, carries_identity, invalidate_request, maybe_store,
    resolve_cached, response_cache_key, saint_active, saint_mark, spawn_swr_refresh,
    stale_error_fallback, unix_secs,
};
use esi::{esi_expand, esi_requested};
use sse::{sse_response, SseHub};
use statics::{sanitize, serve_static, static_forbidden};
use trust::client_ip;

pub use trust::{client_ip_from, parse_cidr, peer_is_trusted, Cidr};

/// Response body: buffered (Full) or streaming (SSE / files), unified as a box.
pub(crate) type ResBody = BoxBody<Bytes, std::io::Error>;

/// Max simultaneous connections per worker — a backstop against connection
/// exhaustion (slowloris); combined with the handshake/header timeouts, idle
/// connections can't pile up.
const MAX_CONNECTIONS: usize = 8192;

/// How long a coalesced follower waits for the leader before running PHP itself.
const COALESCE_WAIT: Duration = Duration::from_secs(5);

fn full(bytes: Bytes) -> ResBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// Open the access-log sink: a file (append), `-` for stdout, or None to disable.
fn open_access_log(path: Option<&Path>) -> Option<Mutex<Box<dyn std::io::Write + Send>>> {
    let path = path?;
    if path.as_os_str() == "-" {
        return Some(Mutex::new(Box::new(std::io::stdout())));
    }
    // 0640 on creation: the log holds client IPs, paths and user agents, and under a
    // 022 umask a plain create() made it readable by every local user. An existing
    // file keeps whatever mode the operator gave it.
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o640);
    }
    match opts.open(path) {
        Ok(f) => Some(Mutex::new(Box::new(f))),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "access log: open failed; disabled");
            None
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub docroot: PathBuf,
    /// Docroot whose application the queue/scheduler sidecars serve, and therefore whose
    /// shared-memory namespace they use.
    ///
    /// Usually the same as `docroot`. It differs when `[queue] root` (or
    /// `[scheduler] root`) is set, which is what a `[[site]]` instance needs: a sidecar
    /// namespaced to the top-level root cannot pop jobs a site's application pushed,
    /// because `pop` matches the namespaced key. That failure is silent — jobs accepted,
    /// stored, never read — so the knob exists to make it sayable.
    pub sidecar_docroot: PathBuf,
    /// Docroot whose application the scheduler sidecar serves. `[scheduler] root`, else
    /// `[queue] root`, else `[server] root` — separate from `sidecar_docroot` because the
    /// scheduler and the queue workers are distinct processes and may belong to different
    /// applications.
    pub scheduler_docroot: PathBuf,
    pub front_controller: PathBuf, // relative, e.g. index.php
    pub listen: SocketAddr,
    pub https: bool,
    pub worker_script: Option<PathBuf>,
    pub max_requests: usize,
    /// Recycle a worker gracefully once its RSS exceeds this many MB (0 = off).
    /// Leak-aware, predictive recycling: drain before PHP hits `memory_limit`.
    pub max_rss_mb: usize,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_self_signed: bool,
    pub max_body_size: usize,
    /// Directory to record failing (5xx) requests into, for `askr replay` (#5).
    pub record_dir: Option<PathBuf>,
    /// Pusher-compatible WebSocket + trigger endpoints (drop-in Reverb, #6).
    pub pusher: bool,
    /// Pusher app secret — when set, private/presence subscriptions must carry a
    /// valid HMAC auth signature. When unset, they're accepted (dev).
    pub pusher_secret: Option<String>,
    /// Access-log destination: a file path, or `-` for stdout. Off if None.
    pub access_log: Option<PathBuf>,
    /// Traffic-log destination for `askr cache-report`: one JSONL line per request
    /// that ran PHP, including a hash of the response body. Off if None.
    pub traffic_log: Option<PathBuf>,
    /// Harden workers (Linux): seccomp no-exec + (with write paths) Landlock.
    pub sandbox: bool,
    /// Refuse to serve if the sandbox does not fully apply (see `sandbox::shortfall`).
    pub sandbox_required: bool,
    /// Directories the sandbox may write to (enables the Landlock filesystem
    /// restriction; empty = seccomp only).
    pub sandbox_write: Vec<PathBuf>,
    /// Traffic shadowing: mirror sampled safe requests to this upstream URL for
    /// deploy validation (None = off).
    pub shadow_to: Option<String>,
    /// Percent (1..=100) of eligible requests to mirror.
    pub shadow_sample: u8,
    /// Serve HTTP/3 (QUIC) alongside TCP on the TLS port (requires TLS).
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub http3: bool,
    /// Seconds a client may take to complete the TLS handshake (slowloris guard).
    pub tls_handshake_timeout: u64,
    /// Seconds a client may take to send the full request headers (slowloris guard).
    pub header_read_timeout: u64,
    /// Redirect plain HTTP to HTTPS (308).
    pub force_https: bool,
    /// Plain-HTTP address to answer and 308 to HTTPS (see `acme::spawn_front`).
    pub http_redirect: Option<std::net::SocketAddr>,
    /// Declarative host redirects (e.g. `www.x.no` → `https://x.no`).
    pub redirects: Vec<crate::config::RedirectRule>,
    /// Virtual hosts routed by the `Host` header (empty = single-site).
    pub sites: Vec<Site>,
    /// `[server] app_id`: this application's name, when it has one.
    pub app_id: Option<String>,
    /// Query parameters stripped from the response-cache key (trailing `*` globs).
    pub cache_strip_query: Vec<String>,
    /// Cookies that don't defeat response cacheability (trailing `*` globs).
    pub cache_ignore_cookies: Vec<String>,
    /// Split the response-cache key on mobile vs desktop `User-Agent`.
    pub cache_vary_user_agent: bool,
    /// Saint mode: seconds to treat PHP as unhealthy after a 5xx, during which a
    /// request holding a `stale-if-error` entry skips PHP entirely (0 = off).
    pub cache_saint_seconds: u64,
    /// Declarative per-path cache policy (`[[cache.rule]]`), first match wins.
    pub cache_rules: Vec<crate::config::CacheRule>,
    /// Rate-limit rules (`[[ratelimit]]`), first match wins.
    pub ratelimits: Vec<crate::config::RateLimitRule>,
    /// Proxies whose `X-Forwarded-For` may be believed.
    pub trusted_proxies: Vec<Cidr>,
}

/// A virtual host: its docroot + front controller, matched by `hosts`.
#[derive(Clone)]
pub struct Site {
    pub hosts: Vec<String>,
    pub docroot: PathBuf,
    pub front_controller: PathBuf,
    /// `[[site]] app_id`, when it has one.
    pub app_id: Option<String>,
}

impl Config {
    /// Name the applications that have an `app_id`, before any process uses their data.
    /// Called once at startup in the process that forks the rest.
    pub fn name_apps(&self) {
        if let Some(id) = &self.app_id {
            crate::ns::name_app(&self.docroot, id);
        }
        for s in &self.sites {
            if let Some(id) = &s.app_id {
                crate::ns::name_app(&s.docroot, id);
            }
        }
    }
}

impl Config {
    /// Resolve the docroot + front controller for a request `Host` — the matching
    /// `[[site]]`, or the default single site when none matches.
    pub fn site_for(&self, host: &str) -> (&std::path::Path, &std::path::Path) {
        for s in &self.sites {
            if s.hosts.iter().any(|h| host_matches(host, h)) {
                return (&s.docroot, &s.front_controller);
            }
        }
        (&self.docroot, &self.front_controller)
    }
}

/// Shared per-worker runtime state for recycling/draining.
pub(crate) struct Runtime {
    config: Arc<Config>,
    php: Php,
    served: AtomicUsize,
    recycle_after: usize,
    shutdown: Notify,
    active: AtomicUsize,
    tls: Option<TlsAcceptor>,
    sse: SseHub,
    pusher: Arc<PusherHub>,
    pusher_enabled: bool,
    access: Option<Mutex<Box<dyn std::io::Write + Send>>>,
    traffic: Option<Mutex<Box<dyn std::io::Write + Send>>>,
    shadow: Option<crate::shadow::Shadow>,
    #[cfg(feature = "observ")]
    observ: Option<crate::observ_sql::TelemetrySink>,
    #[cfg(feature = "otel")]
    otel: Option<crate::otel::Otel>,
}

impl Runtime {
    /// Record one request for the cache oracle (`askr cache-report`).
    ///
    /// Called only for responses that actually ran PHP, which is the point: the log
    /// describes the work still being done, not what the cache already absorbed. The
    /// body hash is what lets the report prove whether a URL is identical for every
    /// visitor \u{2014} the one question a hit-rate estimate can't answer.
    fn record_traffic(&self, sample: crate::oracle::Sample) {
        let Some(w) = &self.traffic else {
            return;
        };
        // One `write` per request, like the access log: `File` is unbuffered, so this
        // is a single syscall and nothing is held across an await.
        if let Ok(mut w) = w.lock() {
            let _ = writeln!(w, "{}", sample.to_line());
        }
    }

    /// Write one structured (JSON) access-log line, if access logging is on.
    fn log_access(
        &self,
        method: &str,
        path: &str,
        status: u16,
        bytes: u64,
        dur: Duration,
        peer: SocketAddr,
    ) {
        // Ship to the ElyraSQL telemetry sink (non-blocking; independent of the
        // file access log). Off unless built with `--features observ` and
        // configured via ASKR_OBSERV_DSN.
        #[cfg(feature = "observ")]
        if let Some(o) = &self.observ {
            let ts_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            let level = if status >= 500 {
                "error"
            } else if status >= 400 {
                "warn"
            } else {
                "info"
            };
            o.log(crate::observ_sql::LogRow {
                ts_us,
                level,
                method: method.to_string(),
                path: path.to_string(),
                status,
                latency_ms: dur.as_secs_f64() * 1000.0,
                ip: peer.ip().to_string(),
            });
        }
        let Some(w) = &self.access else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!(
            r#"{{"ts":{ts},"ip":"{}","method":"{}","path":"{}","status":{status},"bytes":{bytes},"dur_ms":{:.2}}}"#,
            peer.ip(),
            json_escape(method),
            json_escape(path),
            dur.as_secs_f64() * 1000.0,
        );
        if let Ok(mut w) = w.lock() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
}

/// Minimal JSON string escaping for log fields.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Streaming body for a PHP response, which can end in two very different ways.
///
/// A closed channel means PHP finished normally. An `Err` item means the interpreter died
/// with the stream open — and those must not look the same to the client. The headers are
/// already on the wire by then (status 200, sent on the first flush), so the only honest
/// signal left is to fail the transfer: the client gets a truncated response it can
/// detect, instead of a valid, complete-looking **200 with an empty body**. A blank 200 is
/// the worst possible answer — caches store it, browsers render it, and monitoring calls
/// it healthy.
struct PhpStreamBody {
    rx: mpsc::Receiver<Result<Bytes, ()>>,
}

impl Body for PhpStreamBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        match self.get_mut().rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(Ok(Frame::data(b)))),
            Poll::Ready(Some(Err(()))) => Poll::Ready(Some(Err(std::io::Error::other(
                "php worker died mid-stream",
            )))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Serve on an already-bound listener. Returns when a graceful recycle/shutdown
/// has drained; `recycle_after` = 0 means serve forever. When `tls` is set,
/// every connection is TLS-terminated (ALPN: h2, http/1.1).
pub async fn run(
    listener: TcpListener,
    config: Arc<Config>,
    php: Php,
    recycle_after: usize,
    tls: Option<TlsAcceptor>,
    draining: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let pusher_enabled = config.pusher;
    let access = open_access_log(config.access_log.as_deref());
    let traffic = open_access_log(config.traffic_log.as_deref());
    let shadow = config
        .shadow_to
        .as_ref()
        .map(|url| crate::shadow::Shadow::new(url.clone(), config.shadow_sample));
    let rt = Arc::new(Runtime {
        config,
        php,
        served: AtomicUsize::new(0),
        recycle_after,
        shutdown: Notify::new(),
        active: AtomicUsize::new(0),
        tls,
        sse: SseHub::default(),
        pusher: Arc::new(PusherHub::default()),
        pusher_enabled,
        access,
        traffic,
        shadow,
        #[cfg(feature = "observ")]
        observ: crate::observ_sql::TelemetrySink::from_env(),
        #[cfg(feature = "otel")]
        otel: crate::otel::Otel::from_env(),
    });

    // HTTP/3 (QUIC) alongside the TCP listener, on the same TLS port.
    #[cfg(feature = "http3")]
    if rt.config.http3 {
        match (rt.config.tls_cert.clone(), rt.config.tls_key.clone()) {
            (Some(cert), Some(key)) => {
                match crate::http3::endpoint(&cert, &key, rt.config.listen) {
                    Ok(ep) => {
                        tracing::info!(listen = %rt.config.listen, "HTTP/3 (QUIC) listening");
                        tokio::spawn(crate::http3::serve(ep, rt.clone()));
                    }
                    Err(e) => tracing::error!(error = %e, "HTTP/3 setup failed"),
                }
            }
            _ => tracing::warn!("--http3 requires --tls-cert/--tls-key; HTTP/3 off"),
        }
    }

    // Tail the shared broadcast ring and fan events out to local SSE subscribers
    // and Pusher WebSocket connections (a publish from any process reaches all).
    if crate::broadcast::enabled() {
        let rt2 = rt.clone();
        tokio::spawn(async move {
            let mut last = crate::broadcast::current_seq();
            let mut ticks: u32 = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let (events, nl) = crate::broadcast::read_from(last);
                last = nl;
                for (ch, payload) in events {
                    let channel = String::from_utf8_lossy(&ch);
                    let frame =
                        Bytes::from(format!("data: {}\n\n", String::from_utf8_lossy(&payload)));
                    rt2.sse.deliver(&channel, &frame);
                    if rt2.pusher_enabled {
                        rt2.pusher.deliver(&channel, &payload);
                    }
                }
                ticks += 1;
                if ticks % 300 == 0 {
                    rt2.sse.ping(); // ~15s keep-alive
                    rt2.pusher.prune();
                }
            }
        });
    }

    // SIGTERM triggers a graceful drain (used for shutdown and rolling reload).
    // Through a pipe this worker makes now, not tokio's per-process one, which a worker
    // forked after the master built a runtime shares with it — see `crate::term`.
    let mut sigterm = crate::term::Term::install()?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // A failed accept() used to end the whole worker via `?` — silently,
                // since nothing logged it and the PHP side then reported the tear-down
                // as "fatal/OOM?". Most accept errors are transient and per-connection
                // (the peer vanished mid-handshake, or we're briefly out of file
                // descriptors); killing a worker that is serving other requests is a
                // wildly disproportionate response to one of those.
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        let kind = e.kind();
                        // Out of descriptors: don't spin at full speed retrying, and
                        // don't die — the pressure usually clears as requests finish.
                        let out_of_fds = matches!(kind, std::io::ErrorKind::OutOfMemory)
                            || e.raw_os_error() == Some(libc::EMFILE)
                            || e.raw_os_error() == Some(libc::ENFILE);
                        if out_of_fds {
                            tracing::error!(error = %e, "accept failed: out of file descriptors — raise the open-file limit (LimitNOFILE / ulimit -n)");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        } else {
                            tracing::warn!(error = %e, ?kind, "accept failed; continuing");
                        }
                        continue;
                    }
                };
                // Shed load past the connection cap (dropping closes the socket).
                if rt.active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    tracing::warn!(%peer, "connection cap reached; dropping");
                    drop(stream);
                    continue;
                }
                let rt = rt.clone();
                rt.active.fetch_add(1, Ordering::SeqCst);
                tokio::task::spawn(async move {
                    serve_conn(stream, rt.clone(), peer).await;
                    rt.active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            _ = rt.shutdown.notified() => {
                draining.store(true, Ordering::SeqCst);
                tracing::info!(served = rt.served.load(Ordering::SeqCst), "recycling: draining");
                break;
            }
            _ = sigterm.recv() => {
                draining.store(true, Ordering::SeqCst);
                tracing::info!(served = rt.served.load(Ordering::SeqCst), "SIGTERM: draining");
                break;
            }
        }
    }

    // Drain: let in-flight connections finish (bounded).
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.active.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Handle one connection: optionally TLS-terminate, then serve HTTP/1.1 or
/// HTTP/2 (auto-negotiated) until the connection closes.
async fn serve_conn(stream: tokio::net::TcpStream, rt: Arc<Runtime>, peer: SocketAddr) {
    if let Some(acceptor) = rt.tls.clone() {
        // Bound the handshake so a slow/malicious client can't hold a slot open.
        let handshake_to = Duration::from_secs(rt.config.tls_handshake_timeout);
        match tokio::time::timeout(handshake_to, acceptor.accept(stream)).await {
            Ok(Ok(tls)) => serve_io(TokioIo::new(tls), rt, peer).await,
            Ok(Err(e)) => tracing::debug!(error = %e, "TLS handshake failed"),
            Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
        }
    } else {
        serve_io(TokioIo::new(stream), rt, peer).await;
    }
}

async fn serve_io<I>(io: I, rt: Arc<Runtime>, peer: SocketAddr)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let header_read_to = Duration::from_secs(rt.config.header_read_timeout);
    // Wrap handle so every response — whatever branch produced it — is logged.
    let service = service_fn(move |req: Request<Incoming>| {
        let rt = rt.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            let start = Instant::now();
            let resp = handle(req, rt.clone(), peer).await;
            if let Ok(r) = &resp {
                let bytes = r.body().size_hint().exact().unwrap_or(0);
                rt.log_access(
                    &method,
                    &path,
                    r.status().as_u16(),
                    bytes,
                    start.elapsed(),
                    peer,
                );
            }
            resp
        }
    });
    let mut builder = auto::Builder::new(TokioExecutor::new());
    // Bound how long a client may take to send request headers (slowloris).
    // header_read_timeout needs a timer registered on the builder.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_to);
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!(error = %e, "connection closed");
    }
}

pub(crate) async fn handle<B>(
    mut req: Request<B>,
    rt: Arc<Runtime>,
    peer: SocketAddr,
) -> Result<Response<ResBody>, Infallible>
where
    B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let t_start = Instant::now();
    #[cfg(feature = "otel")]
    let t_start_wall = std::time::SystemTime::now();
    #[cfg(feature = "otel")]
    let mut otel_phases: Vec<crate::otel::Phase> = Vec::new();
    let config = &rt.config;
    let port = config.listen.port();
    let accept_encoding = req
        .headers()
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    // Request Host (lowercased, port stripped) — drives redirects + virtual hosts.
    let authority = crate::cgi::effective_host(req.headers(), req.uri()).unwrap_or_default();
    let host = crate::cgi::host_without_port(&authority).to_ascii_lowercase();

    // Rate limiting: refuse before anything expensive happens — a blocked request
    // never costs a PHP cycle, a cache lookup, or a disk stat.
    if let Some(resp) = ratelimit_check(&req, peer, config) {
        finish(&rt, &resp, t_start, 0);
        return Ok(resp);
    }

    // Cache invalidation over HTTP: PURGE one URL, or BAN a glob of URLs. Handled
    // before the redirect engine so a control-plane call over plain HTTP isn't
    // bounced to HTTPS. Authenticated: ASKR_ADMIN_TOKEN as a bearer token, or —
    // when no token is set — loopback peers only. An open PURGE is a cache-wiping
    // DoS.
    if req.method().as_str() == "PURGE" || req.method().as_str() == "BAN" {
        let resp = invalidate_request(&req, &host, peer, config);
        finish(&rt, &resp, t_start, 0);
        return Ok(resp);
    }

    // Host / scheme redirects (www→apex, http→https) — before any dispatch.
    if config.force_https || !config.redirects.is_empty() {
        if let Some(resp) = redirect_target(&req, &host, config, &rt) {
            finish(&rt, &resp, t_start, 0);
            return Ok(resp);
        }
    }

    // Virtual host: this request's docroot + front controller (a matching
    // `[[site]]`, or the default single site).
    let (docroot, front_controller) = config.site_for(&host);

    // Pusher WebSocket endpoint: /app/{key} (drop-in Reverb, #6).
    if rt.pusher_enabled
        && pusher::is_ws_path(req.uri().path())
        && upgrade::is_upgrade_request(&req)
    {
        return Ok(match upgrade::upgrade(&mut req) {
            Ok((resp, fut)) => {
                tokio::spawn(pusher::serve(
                    fut,
                    rt.pusher.clone(),
                    config.pusher_secret.clone(),
                ));
                let (parts, _) = resp.into_parts();
                Response::from_parts(parts, full(Bytes::new()))
            }
            Err(e) => text(StatusCode::BAD_REQUEST, &format!("askr: ws upgrade: {e}")),
        });
    }

    // Reserved SSE endpoint: GET /askr/events?channel=NAME streams broadcast
    // events (see askr_broadcast() in PHP).
    if req.method() == Method::GET && req.uri().path() == "/askr/events" {
        return Ok(sse_response(req.uri().query(), &rt));
    }

    // try_files: serve an existing static file directly (async stat, no blocking
    // syscall on the async path). Sources and dotfiles are never served as static
    // bytes — they fall through to the front controller (see `static_forbidden`).
    let rel = sanitize(req.uri().path());
    if !rel.as_os_str().is_empty() && !static_forbidden(&rel) {
        let candidate = docroot.join(&rel);
        if let Ok(meta) = tokio::fs::metadata(&candidate).await {
            if meta.is_file() {
                return Ok(serve_static(&candidate, &meta, req.method(), req.headers()).await);
            }
        }
    }

    // --- response cache: read before touching PHP (#1) -----------------
    // Only anonymous GET/HEAD requests are cacheable — a request that carries a
    // session/auth cookie may see user-specific content. Cookies listed in
    // `[cache] ignore_cookies` (analytics like `_ga`) don't count as identity,
    // so a visitor who only has those is still served from the shared cache.
    // A `[[cache.rule]]` can override that policy per path: bypass the cache, or
    // cache despite cookies.
    let rule = cache_rule_for(req.uri().path(), &rt.config.cache_rules);
    let passed = rule.is_some_and(|r| r.is_pass());
    let anonymous = !carries_identity(&req, &rt.config.cache_ignore_cookies);
    let cacheable = rcache::enabled()
        && !passed
        && matches!(*req.method(), Method::GET | Method::HEAD)
        && (anonymous || rule.is_some_and(|r| r.force));
    let cache_key = cacheable.then(|| response_cache_key(&req, &host, &rt.config));
    // Own the rule for the rest of the request — `req` is consumed further down.
    let rule = rule.cloned();

    // Traffic shadow: decide (and sample) now, while `req` is still intact, what
    // to mirror. The mirror itself fires after the real response is built.
    let shadow_probe: Option<(Method, String)> = rt.shadow.as_ref().and_then(|sh| {
        let has_cookie = req.headers().contains_key(hyper::header::COOKIE);
        if crate::shadow::eligible(req.method(), has_cookie) && sh.sampled() {
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "/".to_string());
            Some((req.method().clone(), pq))
        } else {
            None
        }
    });
    // #2 request coalescing: when a cacheable key misses, exactly one request
    // (the leader) runs PHP; the rest wait for it to populate the cache.
    let mut coalesce_leader = false;
    if let Some(key) = &cache_key {
        if let Some((c, varied)) = resolve_cached(key, req.headers(), rcache::get) {
            // Stale-while-revalidate: serve the stale body now, and trigger one
            // background refresh (coalesced) so PHP runs off the request path.
            //
            // Not for a variant. The refresh rebuilds a synthetic request from the
            // stored URL, and that request does not carry the headers that selected
            // this variant — it would refresh the *wrong* one. A varied entry is served
            // stale through its window and refreshed by the next real request instead.
            let state = if c.stale { "STALE" } else { "HIT" };
            #[cfg(feature = "otel")]
            let cache_state = state;
            if c.stale && !varied {
                spawn_swr_refresh(&rt, key, &req, peer);
            }
            // ESI: the cached shell holds the tags — assemble the fragments now, so a
            // page can sit in cache for a day while a cart fragment is per-request.
            let response = if esi_requested(&c.headers) && crate::esi::has_tags(&c.body) {
                let expanded =
                    esi_expand(&rt, &host, docroot, front_controller, peer, c.body).await;
                build_response(
                    askr_php::Response {
                        status: c.status,
                        php_status: c.status as i32,
                        headers: c.headers,
                        body: expanded,
                    },
                    Some(state),
                    &accept_encoding,
                )
            } else {
                cached_response(c)
            };
            #[cfg(feature = "otel")]
            otel_fast(
                &rt,
                req.method(),
                req.uri(),
                req.version(),
                t_start_wall,
                t_start,
                &response,
                cache_state,
            );
            finish(&rt, &response, t_start, 0);
            return Ok(response);
        }
        // Saint mode: PHP failed recently — don't queue more work onto a dying
        // backend when the app told us this page may be served on error.
        if saint_active() {
            if let Some(response) = stale_error_fallback(key, req.headers()) {
                tracing::warn!(
                    path = %req.uri().path(),
                    "saint mode: serving stale-if-error fallback without running PHP"
                );
                #[cfg(feature = "otel")]
                otel_fast(
                    &rt,
                    req.method(),
                    req.uri(),
                    req.version(),
                    t_start_wall,
                    t_start,
                    &response,
                    "STALE-ERROR",
                );
                finish(&rt, &response, t_start, 0);
                return Ok(response);
            }
        }
        match rcache::begin(key) {
            rcache::Lead::Leader => coalesce_leader = true,
            rcache::Lead::Follower => {
                // Wait (fail-open) for the leader to fill the cache. While the
                // leader is still computing, followers only do a cheap atomic
                // `is_inflight` load (no per-slot lock) with backoff — so a
                // 100-way fan-in doesn't melt a core contending on the slot
                // spinlock. `peek` (which locks) runs at most once, after the
                // leader clears inflight (it stores the response *before*
                // clearing, so the cache is populated by then).
                let deadline = Instant::now() + COALESCE_WAIT;
                let mut served = None;
                let mut backoff = Duration::from_millis(1);
                while Instant::now() < deadline {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_millis(16));
                    if !rcache::is_inflight(key) {
                        // Leader finished: read once (a HIT if it was cacheable).
                        // An entry alive only on its stale-if-error window is not
                        // servable here — the leader's response wasn't cacheable.
                        served = resolve_cached(key, req.headers(), rcache::peek)
                            .map(|(c, _)| c)
                            .filter(|c| !c.error_only);
                        break;
                    }
                }
                if let Some(c) = served {
                    rcache::note_coalesced();
                    let response = cached_response(c);
                    #[cfg(feature = "otel")]
                    otel_fast(
                        &rt,
                        req.method(),
                        req.uri(),
                        req.version(),
                        t_start_wall,
                        t_start,
                        &response,
                        "HIT",
                    );
                    finish(&rt, &response, t_start, 0);
                    return Ok(response);
                }
                // fall through: run PHP uncoalesced (leader didn't cache / timed out)
            }
        }
    }

    let script = docroot.join(front_controller);
    let script_name = format!("/{}", front_controller.display());

    #[cfg(feature = "otel")]
    let read_t0 = Instant::now();
    let (parts, body) = req.into_parts();
    let max = config.max_body_size;

    // multipart/form-data → stream files to temp paths (constant memory) and
    // collect fields, instead of buffering the whole body in RAM (#uploads).
    let multipart_boundary = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|ct| ct.starts_with("multipart/form-data"))
        .and_then(|ct| multer::parse_boundary(ct).ok());

    // `_upload_temp_paths` is an RAII guard: it unlinks the streamed temp files
    // when this handler returns *or* when its future is cancelled (client
    // disconnect during PHP execution). Held to end of scope on purpose.
    let (request, _upload_temp_paths) = if let Some(boundary) = multipart_boundary {
        match crate::upload::parse(body.into_data_stream(), &boundary, max).await {
            Ok(parsed) => {
                let mut request = cgi::build_request(
                    &parts,
                    Vec::new(), // body consumed while streaming; PHP uses $_POST/$_FILES
                    &cgi::Context {
                        docroot,
                        script: &script,
                        script_name: &script_name,
                        peer,
                        https: config.https,
                        server_port: port,
                        trusted_proxies: &config.trusted_proxies,
                    },
                );
                request.post_fields = parsed.fields;
                request.files = parsed.files;
                (request, parsed.temp_paths)
            }
            Err(crate::upload::UploadError::TooLarge) => {
                return Ok(text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "askr: upload too large",
                ));
            }
            Err(crate::upload::UploadError::Parse(e)) => {
                return Ok(text(
                    StatusCode::BAD_REQUEST,
                    &format!("askr: bad upload: {e}"),
                ));
            }
        }
    } else {
        // Enforce a maximum request body size (protect against memory
        // exhaustion): reject early on a declared Content-Length, and cap the
        // actual read so a chunked body can't exceed it either.
        if let Some(len) = parts
            .headers
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
        {
            if len > max {
                return Ok(text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "askr: request body too large",
                ));
            }
        }
        let body_bytes = match Limited::new(body, max).collect().await {
            Ok(c) => c.to_bytes().to_vec(),
            Err(_) => {
                return Ok(text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "askr: request body too large",
                ));
            }
        };

        // Pusher HTTP trigger: POST /apps/{id}/events (what Laravel's broadcaster
        // calls server-side). Publish into the ring; the WS tailer fans it out.
        if rt.pusher_enabled && parts.method == Method::POST && pusher::is_trigger(parts.uri.path())
        {
            // The write side of the Pusher surface. Subscriptions to private-/presence-
            // channels are HMAC-checked; this used to publish into those same channels
            // for anyone who could reach the port. With a secret configured the
            // request must carry Pusher's own signature, which Laravel's broadcaster
            // sends on every call. Without one it is accepted, as subscriptions are —
            // and says so, because here "development mode" means anyone can publish.
            match &config.pusher_secret {
                Some(secret) => {
                    if let Err(why) = pusher::verify_trigger(
                        secret,
                        parts.uri.path(),
                        parts.uri.query(),
                        &body_bytes,
                        unix_secs(),
                    ) {
                        tracing::warn!(%peer, reason = %why, "pusher: trigger refused");
                        let response = text(
                            StatusCode::UNAUTHORIZED,
                            &format!("askr: pusher trigger refused: {why}"),
                        );
                        finish(&rt, &response, t_start, 0);
                        return Ok(response);
                    }
                }
                None => pusher::warn_unauthenticated_trigger_once(),
            }
            let out = pusher::trigger(&body_bytes);
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .body(full(Bytes::from(out)))
                .unwrap();
            finish(&rt, &response, t_start, 0);
            return Ok(response);
        }

        let request = cgi::build_request(
            &parts,
            body_bytes,
            &cgi::Context {
                docroot,
                script: &script,
                script_name: &script_name,
                peer,
                https: config.https,
                server_port: port,
                trusted_proxies: &config.trusted_proxies,
            },
        );
        (request, crate::upload::TempFiles::default())
    };

    #[cfg(feature = "otel")]
    otel_phases.push(crate::otel::Phase {
        name: "request.read",
        offset: read_t0.saturating_duration_since(t_start),
        dur: read_t0.elapsed(),
    });

    // Keep a copy of the request iff we may need to record it on a 5xx (#5).
    let record_copy = config.record_dir.as_ref().map(|_| request.clone());

    // Time PHP specifically (vs total) — the in-process split FPM can't see.
    // Track the in-flight (busy) gauge so the CoW autoscaler can size the pool.
    let php_start = Instant::now();
    if let Some(m) = crate::metrics::Metrics::get() {
        m.inflight.fetch_add(1, Ordering::Relaxed);
    }
    let php_result = rt.php.handle(request).await;
    if let Some(m) = crate::metrics::Metrics::get() {
        m.inflight.fetch_sub(1, Ordering::Relaxed);
    }
    let php_us = php_start.elapsed().as_micros() as u64;
    #[cfg(feature = "otel")]
    otel_phases.push(crate::otel::Phase {
        name: "php.execute",
        offset: php_start.saturating_duration_since(t_start),
        dur: std::time::Duration::from_micros(php_us),
    });

    #[allow(unused_mut)]
    let mut response = match php_result {
        // Streaming response: PHP flush()ed mid-request (SSE, large export). Serve
        // the chunks as they arrive; bypass the cache/compression/shadow path.
        Ok(Reply::Stream {
            status,
            headers,
            body,
        }) => stream_response(status, headers, body),
        Ok(Reply::Buffered(resp)) => {
            // Cache store: the app opts in per-response with an `Askr-Cache`
            // header (which we consume, never forwarding it to the client).
            if let Some(key) = &cache_key {
                maybe_store(
                    key,
                    &resp,
                    &accept_encoding,
                    rt.config.cache_vary_user_agent,
                    rule.as_ref(),
                    &crate::ns::for_docroot(docroot),
                    &parts.headers,
                );
            }
            // Fire the shadow mirror off the request path: hash prod's body now,
            // then compare on a background task without touching the client.
            if let (Some((method, pq)), Some(sh)) = (shadow_probe, rt.shadow.as_ref()) {
                let client = sh.clone_client();
                let base = sh.base_url().to_string();
                let (ps, ph) = (resp.status, crate::shadow::hash_body(&resp.body));
                tokio::spawn(async move {
                    crate::shadow::compare_owned(client, base, method, pq, ps, ph).await;
                });
            }
            // `PASS` makes a rule-bypassed path visible in the response, so you can
            // tell "not cacheable" from "a rule said no" with curl.
            // Cache oracle: record what this request cost and what it returned, so
            // `askr cache-report` can tell the operator whether caching it would be
            // both worthwhile and safe. Off unless --traffic-log is set.
            if rt.traffic.is_some() {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                resp.body.hash(&mut h);
                let set_cookie = resp
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("set-cookie"));
                let opted_in = resp
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("askr-cache"));
                rt.record_traffic(crate::oracle::Sample {
                    ts_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    method: parts.method.as_str().to_string(),
                    host: host.clone(),
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().unwrap_or("").to_string(),
                    // "Carried cookies" means cookies that count as identity — the
                    // ones already excused by `ignore_cookies` shouldn't scare anyone.
                    cookie: !anonymous,
                    set_cookie,
                    status: resp.status,
                    bytes: resp.body.len() as u64,
                    php_us,
                    body_hash: h.finish(),
                    opted_in,
                });
            }
            let state = if passed {
                Some("PASS")
            } else {
                rcache::enabled().then_some("MISS")
            };
            #[cfg(feature = "otel")]
            let build_t0 = Instant::now();
            // ESI runs after the store above, so the cache keeps the tags and each
            // client gets its own assembly.
            let resp = if esi_requested(&resp.headers) && crate::esi::has_tags(&resp.body) {
                let mut resp = resp;
                resp.body =
                    esi_expand(&rt, &host, docroot, front_controller, peer, resp.body).await;
                resp
            } else {
                resp
            };
            let built = build_response(resp, state, &accept_encoding);
            #[cfg(feature = "otel")]
            otel_phases.push(crate::otel::Phase {
                name: "response.build",
                offset: build_t0.saturating_duration_since(t_start),
                dur: build_t0.elapsed(),
            });
            built
        }
        Err(e) => {
            tracing::error!(error = %e, "php handling failed");
            if let Some(m) = crate::metrics::Metrics::get() {
                m.note_error();
            }
            text(StatusCode::BAD_GATEWAY, &format!("askr: {e}"))
        }
    };

    // stale-if-error (+ saint mode): the origin failed — a 5xx from PHP or a dead /
    // timed-out worker (502). If the app marked this page usable on failure and we
    // still hold it, serve that instead of shipping the error to the client. One
    // place covers both paths, and the coalescing release below still runs.
    if response.status().as_u16() >= 500 {
        saint_mark(rt.config.cache_saint_seconds);
        if let Some(fallback) = cache_key
            .as_ref()
            .and_then(|k| stale_error_fallback(k, &parts.headers))
        {
            // Record the *real* failure for `askr replay` first — the substituted
            // response won't trip the status check further down.
            if let (Some(dir), Some(req)) = (&config.record_dir, &record_copy) {
                crate::record::record_failure(dir, req, response.status().as_u16());
            }
            tracing::warn!(
                status = response.status().as_u16(),
                path = %parts.uri.path(),
                "origin failed; serving stale-if-error fallback"
            );
            response = fallback;
        }
    }

    // Advertise HTTP/3 so TCP (h1/h2) clients can upgrade to QUIC.
    #[cfg(feature = "http3")]
    if config.http3 {
        if let Ok(v) = hyper::header::HeaderValue::from_str(&format!("h3=\":{}\"; ma=86400", port))
        {
            response.headers_mut().insert(hyper::header::ALT_SVC, v);
        }
    }

    // Release any followers waiting on this key (the cache is now populated, or
    // this response wasn't cacheable and they should run PHP themselves).
    if coalesce_leader {
        if let Some(key) = &cache_key {
            rcache::end(key);
        }
    }

    // Uploaded temp files are cleaned up by the `_upload_temp_paths` RAII guard
    // when this scope ends (or the future is cancelled) — no explicit unlink.

    // Record a failing request so it can be replayed later (#5).
    if response.status().as_u16() >= 500 {
        if let (Some(dir), Some(req)) = (&config.record_dir, &record_copy) {
            crate::record::record_failure(dir, req, response.status().as_u16());
        }
    }

    // OpenTelemetry: export this PHP request as root http.request + child
    // php.execute, with exact wall-clock windows (feature `otel`).
    #[cfg(feature = "otel")]
    if let Some(o) = &rt.otel {
        o.record(crate::otel::RequestSpan {
            method: parts.method.to_string(),
            path: parts.uri.path().to_string(),
            status: response.status().as_u16(),
            start_wall: t_start_wall,
            total: t_start.elapsed(),
            cache: if rcache::enabled() { "MISS" } else { "" },
            bytes: response.body().size_hint().exact().unwrap_or(0),
            proto: proto_str(parts.version),
            query: parts.uri.query().unwrap_or("").to_string(),
            phases: std::mem::take(&mut otel_phases),
        });
    }

    finish(&rt, &response, t_start, php_us);
    Ok(response)
}

/// Map a hyper protocol version to an OTel `network.protocol.version` value.
#[cfg(feature = "otel")]
fn proto_str(v: hyper::Version) -> &'static str {
    match v {
        hyper::Version::HTTP_3 => "3",
        hyper::Version::HTTP_2 => "2",
        hyper::Version::HTTP_10 => "1.0",
        _ => "1.1",
    }
}

/// Emit a phase-less root span for a fast return path (cache HIT/STALE, a
/// coalesced follower) so cached requests are visible in the trace view too —
/// not just the misses that reach PHP.
#[cfg(feature = "otel")]
#[allow(clippy::too_many_arguments)]
fn otel_fast(
    rt: &Runtime,
    method: &Method,
    uri: &hyper::Uri,
    version: hyper::Version,
    start_wall: std::time::SystemTime,
    t_start: Instant,
    resp: &Response<ResBody>,
    cache: &'static str,
) {
    if let Some(o) = &rt.otel {
        o.record(crate::otel::RequestSpan {
            method: method.to_string(),
            path: uri.path().to_string(),
            status: resp.status().as_u16(),
            start_wall,
            total: t_start.elapsed(),
            cache,
            bytes: resp.body().size_hint().exact().unwrap_or(0),
            proto: proto_str(version),
            query: uri.query().unwrap_or("").to_string(),
            phases: Vec::new(),
        });
    }
}

/// Match a Host against a redirect `from` pattern: exact, or `*.suffix`.
fn host_matches(host: &str, pattern: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host == suffix || host.ends_with(&format!(".{suffix}"))
    } else {
        host.eq_ignore_ascii_case(pattern)
    }
}

/// A bare redirect response (status + `Location`), no body.
fn redirect_to(location: String, status: u16) -> Response<ResBody> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::PERMANENT_REDIRECT))
        .header(hyper::header::LOCATION, location)
        .header(hyper::header::CONTENT_LENGTH, "0")
        .body(full(Bytes::new()))
        .unwrap()
}

/// Apply `force_https` then the host redirect rules. Returns a redirect (preserving
/// path + query) if one matches, else `None` (request proceeds normally).
fn redirect_target<B>(
    req: &Request<B>,
    host: &str,
    config: &Config,
    rt: &Runtime,
) -> Option<Response<ResBody>> {
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");

    if config.force_https && !host.is_empty() {
        let secure = rt.tls.is_some()
            || config.https
            || req
                .headers()
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("https"))
                .unwrap_or(false);
        if !secure {
            return Some(redirect_to(format!("https://{host}{pq}"), 308));
        }
    }

    for rule in &config.redirects {
        if host_matches(host, &rule.from) {
            let to = rule.to.trim_end_matches('/');
            return Some(redirect_to(format!("{to}{pq}"), rule.status));
        }
    }
    None
}

/// Record metrics and advance the recycle counter for a finished request.
fn finish(rt: &Runtime, response: &Response<ResBody>, t_start: Instant, php_us: u64) {
    if let Some(m) = crate::metrics::Metrics::get() {
        let total_us = t_start.elapsed().as_micros() as u64;
        let bytes = response.body().size_hint().exact().unwrap_or(0);
        let status = response.status().as_u16();
        m.record(status, bytes, php_us, total_us);
        // Per-worker attribution for the canary gate: which worker served this,
        // and did it fail? A fleet-wide total can't answer that.
        if let Some(st) = m
            .per_worker
            .get(crate::supervisor::MY_SLOT.load(Ordering::Relaxed))
        {
            st.requests.fetch_add(1, Ordering::Relaxed);
            st.us_sum.fetch_add(total_us, Ordering::Relaxed);
            if status >= 500 {
                st.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if rt.recycle_after > 0 {
        let n = rt.served.fetch_add(1, Ordering::SeqCst) + 1;
        if n == rt.recycle_after {
            rt.shutdown.notify_one();
        }
    }
}

/// Apply `[[ratelimit]]` rules. `Some(response)` means the request is refused.
///
/// Reserved `/askr/*` endpoints are exempt: a limit that silently killed SSE or
/// the Pusher WebSocket would be a nasty surprise.
fn ratelimit_check<B>(
    req: &Request<B>,
    peer: SocketAddr,
    config: &Config,
) -> Option<Response<ResBody>> {
    if config.ratelimits.is_empty() || !crate::ratelimit::enabled() {
        return None;
    }
    let path = req.uri().path();
    if path.starts_with("/askr/") {
        return None;
    }
    let (idx, rule) = config
        .ratelimits
        .iter()
        .enumerate()
        .find(|(_, r)| rcache::glob_match(&r.path, path))?;

    // Identity: client IP, a header value, or a cookie value. A request that can't
    // produce the configured identity isn't limited — the rule simply doesn't apply.
    let identity: String = if rule.by == "ip" {
        client_ip(req, peer, &config.trusted_proxies).to_string()
    } else if let Some(name) = rule.by.strip_prefix("header:") {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    } else if let Some(name) = rule.by.strip_prefix("cookie:") {
        cookie_value(req, name).unwrap_or_default()
    } else {
        String::new()
    };
    if identity.is_empty() {
        return None;
    }

    // Key on the rule index too, so two rules matching one visitor don't share a
    // bucket.
    let key = format!("{idx}\0{identity}");
    let v = crate::ratelimit::check(key.as_bytes(), rule.limit, rule.window, rule.burst);
    if v.allowed {
        return None;
    }
    tracing::debug!(path, identity, limit = rule.limit, "rate limit exceeded");
    let body = "429 Too Many Requests\n";
    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(hyper::header::RETRY_AFTER, v.retry_after.to_string())
        .header("X-RateLimit-Limit", rule.limit.to_string())
        .header("X-RateLimit-Remaining", v.remaining.to_string())
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from(body)))
        .ok()
}

/// First value of `name` from the request's `Cookie` header(s).
fn cookie_value<B>(req: &Request<B>, name: &str) -> Option<String> {
    req.headers()
        .get_all(hyper::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
}

/// Build a chunked, streaming response from a PHP `flush()`-driven body channel.
/// Streaming bypasses the response cache and compression (the full body isn't known
/// up front) and is framed chunked (no `Content-Length`).
fn stream_response(
    status: u16,
    headers: Vec<(String, String)>,
    body: mpsc::Receiver<Result<Bytes, ()>>,
) -> Response<ResBody> {
    let mut builder =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
    for (name, value) in &headers {
        if name.eq_ignore_ascii_case("Content-Length")
            || name.eq_ignore_ascii_case("Transfer-Encoding")
            || name.eq_ignore_ascii_case("Askr-Cache")
            || name.eq_ignore_ascii_case("Askr-ESI")
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(PhpStreamBody { rx: body }.boxed())
        .unwrap_or_else(|_| {
            text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "askr: bad stream response",
            )
        })
}

/// Finish a response body, compressing it (br/gzip) when the client accepts it
/// and the content type is worth compressing.
fn finish_body(
    builder: hyper::http::response::Builder,
    body: Vec<u8>,
    content_type: &str,
    accept_encoding: &str,
) -> Response<ResBody> {
    let builder = match crate::compress::maybe(&body, content_type, accept_encoding) {
        Some((enc, compressed)) => {
            return builder
                .header(hyper::header::CONTENT_ENCODING, enc.header())
                .header(hyper::header::VARY, "Accept-Encoding")
                .body(full(Bytes::from(compressed)))
                .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"));
        }
        None => builder,
    };
    builder
        .body(full(Bytes::from(body)))
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"))
}

fn build_response(
    resp: askr_php::Response,
    cache_state: Option<&str>,
    accept_encoding: &str,
) -> Response<ResBody> {
    let mut builder =
        Response::builder().status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK));

    let mut content_type = String::new();
    for (name, value) in &resp.headers {
        // Strip framing headers (hyper recomputes them) and the internal
        // `Askr-Cache` control header (never leaks to the client).
        if name.eq_ignore_ascii_case("Content-Length")
            || name.eq_ignore_ascii_case("Transfer-Encoding")
            || name.eq_ignore_ascii_case("Askr-Cache")
            || name.eq_ignore_ascii_case("Askr-ESI")
        {
            continue;
        }
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value.clone();
        }
        builder = builder.header(name, value);
    }
    if let Some(state) = cache_state {
        builder = builder.header("X-Askr-Cache", state);
    }

    finish_body(builder, resp.body, &content_type, accept_encoding)
}

fn text(status: StatusCode, msg: &str) -> Response<ResBody> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from(msg.to_owned())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The access log holds client IPs, paths and user agents. A plain `create()`
    /// under a 022 umask handed it to every local user.
    #[cfg(unix)]
    #[test]
    fn the_access_log_is_created_group_readable_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("askr-access-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let log = open_access_log(Some(&p));
        assert!(log.is_some(), "the log must open");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "no read for other");
        drop(log);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn build_response_maps_status_and_headers() {
        let resp = askr_php::Response {
            status: 201,
            headers: vec![
                ("X-Test".into(), "yes".into()),
                ("Content-Length".into(), "5".into()), // must be dropped
            ],
            body: b"hello".to_vec(),
            php_status: 0,
        };
        let out = build_response(resp, None, "");
        assert_eq!(out.status(), StatusCode::CREATED);
        assert_eq!(out.headers().get("X-Test").unwrap(), "yes");
        // hyper computes framing; our explicit Content-Length is stripped.
        assert!(out.headers().get(hyper::header::CONTENT_LENGTH).is_none());
    }
}
