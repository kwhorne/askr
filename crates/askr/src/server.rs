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

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::AsyncRead;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Notify};
use tokio_rustls::TlsAcceptor;

use fastwebsockets::upgrade;

use crate::cgi;
use crate::php::{Php, Reply};
use crate::pusher::{self, PusherHub};
use crate::rcache;

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

/// Per-worker registry of live SSE subscribers. A background task tails the
/// shared broadcast ring and pushes matching events to these.
#[derive(Default)]
struct SseHub {
    // Sharded by channel so delivering one event only touches that channel's
    // subscribers — O(subs-on-channel), not O(all-subs) — which matters when a box
    // fans out to thousands of SSE clients across many channels.
    channels: Mutex<std::collections::HashMap<String, Vec<mpsc::Sender<Bytes>>>>,
}

impl SseHub {
    fn subscribe(&self, channel: String) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(128);
        let _ = tx.try_send(Bytes::from_static(b": connected\n\n"));
        self.channels
            .lock()
            .unwrap()
            .entry(channel)
            .or_default()
            .push(tx);
        rx
    }

    fn deliver(&self, channel: &str, data: &Bytes) {
        let mut map = self.channels.lock().unwrap();
        if let Some(subs) = map.get_mut(channel) {
            // Non-blocking: if a subscriber's 128-message buffer is full (a client
            // that can't keep up), try_send fails and we drop it — intentional
            // back-pressure, a slow client is disconnected rather than stalling the
            // fan-out. Prune the channel entry once it's empty.
            subs.retain(|tx| tx.try_send(data.clone()).is_ok());
            if subs.is_empty() {
                map.remove(channel);
            }
        }
    }

    fn ping(&self) {
        let msg = Bytes::from_static(b": ping\n\n");
        // Keep-alive sweep (~15 s): prune dead subscribers and empty channels.
        self.channels.lock().unwrap().retain(|_, subs| {
            subs.retain(|tx| tx.try_send(msg.clone()).is_ok());
            !subs.is_empty()
        });
    }
}

/// Streaming body for an SSE connection: yields frames as events arrive.
struct SseBody {
    rx: mpsc::Receiver<Bytes>,
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

impl Body for SseBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        match self.get_mut().rx.poll_recv(cx) {
            Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
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
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

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
        if let Some(c) = rcache::get(key) {
            // Stale-while-revalidate: serve the stale body now, and trigger one
            // background refresh (coalesced) so PHP runs off the request path.
            let state = if c.stale { "STALE" } else { "HIT" };
            #[cfg(feature = "otel")]
            let cache_state = state;
            if c.stale {
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
            if let Some(response) = stale_error_fallback(key) {
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
                        served = rcache::peek(key).filter(|c| !c.error_only);
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
                    docroot,
                    &script,
                    &script_name,
                    peer,
                    config.https,
                    port,
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
            docroot,
            &script,
            &script_name,
            peer,
            config.https,
            port,
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
        if let Some(fallback) = cache_key.as_ref().and_then(|k| stale_error_fallback(k)) {
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

/// Unix seconds until which the local PHP backend is treated as unhealthy
/// ("saint mode"). Per worker process: each one notices failures on its own, and
/// no shared-memory write is needed on the failure path.
static SAINT_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Note that PHP just failed: hold the backend in saint mode for `secs` so the
/// next requests prefer a stale fallback over hammering a dying app (0 = off).
fn saint_mark(secs: u64) {
    if secs > 0 {
        SAINT_UNTIL.store(unix_secs() + secs, std::sync::atomic::Ordering::Relaxed);
    }
}

fn saint_active() -> bool {
    SAINT_UNTIL.load(std::sync::atomic::Ordering::Relaxed) > unix_secs()
}

/// A trusted-proxy entry: a network address plus prefix length.
pub type Cidr = (std::net::IpAddr, u8);

/// Parse `10.0.0.0/8`, `192.168.1.5` or `::1` into a network + prefix length.
/// A bare address becomes a full-length prefix (a single host).
pub fn parse_cidr(s: &str) -> Option<Cidr> {
    let s = s.trim();
    let (addr, bits) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (s, None),
    };
    let ip: std::net::IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits = bits.unwrap_or(max);
    (bits <= max).then_some((ip, bits))
}

/// Is `ip` inside the network `(net, bits)`?
fn cidr_contains((net, bits): &Cidr, ip: &std::net::IpAddr) -> bool {
    fn masked(bytes: &[u8], bits: u8, out: &mut [u8]) {
        let bits = bits as usize;
        for (i, b) in bytes.iter().enumerate() {
            let keep = bits.saturating_sub(i * 8).min(8);
            out[i] = if keep == 0 {
                0
            } else {
                b & (0xFFu16 << (8 - keep)) as u8
            };
        }
    }
    match (net, ip) {
        (std::net::IpAddr::V4(n), std::net::IpAddr::V4(a)) => {
            let (mut x, mut y) = ([0u8; 4], [0u8; 4]);
            masked(&n.octets(), *bits, &mut x);
            masked(&a.octets(), *bits, &mut y);
            x == y
        }
        (std::net::IpAddr::V6(n), std::net::IpAddr::V6(a)) => {
            let (mut x, mut y) = ([0u8; 16], [0u8; 16]);
            masked(&n.octets(), *bits, &mut x);
            masked(&a.octets(), *bits, &mut y);
            x == y
        }
        _ => false,
    }
}

/// The client's address, honouring `X-Forwarded-For` **only** through trusted
/// proxies.
///
/// Walks the forwarded chain right-to-left and returns the first address that
/// isn't itself a trusted proxy — the standard approach. With no trusted proxies
/// configured the header is ignored entirely, because believing it would let any
/// client rotate a fake address and walk straight past a rate limit.
fn client_ip<B>(req: &Request<B>, peer: SocketAddr, trusted: &[Cidr]) -> std::net::IpAddr {
    let peer_ip = peer.ip();
    if trusted.is_empty() || !trusted.iter().any(|c| cidr_contains(c, &peer_ip)) {
        return peer_ip;
    }
    let chain: Vec<&str> = req
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for hop in chain.iter().rev() {
        // An entry may carry a port (`1.2.3.4:5678`); tolerate both forms.
        let parsed = hop
            .parse::<std::net::IpAddr>()
            .ok()
            .or_else(|| hop.parse::<SocketAddr>().ok().map(|s| s.ip()));
        if let Some(ip) = parsed {
            if !trusted.iter().any(|c| cidr_contains(c, &ip)) {
                return ip;
            }
        }
    }
    peer_ip
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

/// Does this request identify a particular client?
///
/// A session cookie is the obvious case, and was the only one checked. A bearer token
/// is the other: an API request with `Authorization: Bearer …` and no cookies read as
/// anonymous, so with a `[[cache.rule]]` TTL on `/api/*` — "cache policy for apps you
/// can't edit" — one user's response was cached and handed to the next. Varnish passes
/// `Authorization` by default for exactly this reason; so does this now.
/// `Proxy-Authorization` is included because a client that sends it is authenticating
/// to *something*, and the safe reading of that is "not shared".
///
/// Cookies listed in `[cache] ignore_cookies` (analytics) do not count as identity.
fn carries_identity<B>(req: &Request<B>, ignore_cookies: &[String]) -> bool {
    let h = req.headers();
    if h.contains_key(hyper::header::AUTHORIZATION)
        || h.contains_key(hyper::header::PROXY_AUTHORIZATION)
    {
        return true;
    }
    h.get_all(hyper::header::COOKIE).iter().any(|v| {
        !v.to_str()
            .is_ok_and(|c| cookies_ignorable(c, ignore_cookies))
    })
}

/// The first `[[cache.rule]]` whose glob matches this path, if any.
///
/// Rules are the operator's cache policy, applied without touching the app: bypass a
/// path entirely (`action = "pass"`), give a path a TTL the app never asked for, or
/// cache it despite cookies. Evaluated per request, so matching is glob-based and
/// allocation-free.
fn cache_rule_for<'a>(
    path: &str,
    rules: &'a [crate::config::CacheRule],
) -> Option<&'a crate::config::CacheRule> {
    rules.iter().find(|r| rcache::glob_match(&r.path, path))
}

/// How many passes of ESI expansion to run — i.e. how deeply fragments may nest.
const ESI_MAX_PASSES: usize = 3;
/// Total fragment fetches allowed per request, to bound a page (or a loop) that
/// asks for hundreds of includes.
const ESI_MAX_INCLUDES: usize = 32;

/// Did the app opt this response into ESI processing (`Askr-ESI: on`)?
fn esi_requested(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("askr-esi")
            && (v.trim().eq_ignore_ascii_case("on") || v.trim() == "1")
    })
}

/// Expand `<esi:include>` tags, fetching each fragment from the response cache or,
/// on a miss, from PHP.
///
/// Runs up to [`ESI_MAX_PASSES`] passes so a fragment may itself contain includes;
/// a pass that substitutes nothing ends the loop. Iterating instead of recursing
/// keeps this a plain `async fn` (no boxed futures on the response path).
async fn esi_expand(
    rt: &Arc<Runtime>,
    host: &str,
    docroot: &Path,
    front_controller: &Path,
    peer: SocketAddr,
    body: Vec<u8>,
) -> Vec<u8> {
    let mut body = body;
    let mut budget = ESI_MAX_INCLUDES;
    for _ in 0..ESI_MAX_PASSES {
        if !crate::esi::has_tags(&body) {
            break;
        }
        let plan = crate::esi::plan(&body);
        if !plan
            .iter()
            .any(|s| matches!(s, crate::esi::Segment::Include(_)))
        {
            break; // only pass-through tags left
        }
        let mut out = Vec::with_capacity(body.len());
        for seg in plan {
            match seg {
                crate::esi::Segment::Literal(a, b) => out.extend_from_slice(&body[a..b]),
                crate::esi::Segment::Include(src) => {
                    if budget == 0 {
                        tracing::warn!(
                            limit = ESI_MAX_INCLUDES,
                            "esi: include budget exhausted, leaving fragment empty"
                        );
                        continue;
                    }
                    budget -= 1;
                    match esi_fragment(rt, host, docroot, front_controller, peer, &src).await {
                        Some(bytes) => out.extend_from_slice(&bytes),
                        // A broken fragment must not take the page down with it.
                        None => tracing::warn!(src = %src, "esi: fragment failed, left empty"),
                    }
                }
            }
        }
        body = out;
    }
    body
}

/// Fetch one ESI fragment: its own cache entry first, then PHP.
///
/// The fragment is an ordinary request through the front controller, so it carries
/// its own `Askr-Cache` header — that's what gives every hole in the page an
/// independent TTL, tag set and invalidation.
async fn esi_fragment(
    rt: &Arc<Runtime>,
    host: &str,
    docroot: &Path,
    front_controller: &Path,
    peer: SocketAddr,
    src: &str,
) -> Option<Vec<u8>> {
    if !crate::esi::safe_src(src) {
        tracing::warn!(src = %src, "esi: refusing non-same-origin fragment src");
        return None;
    }
    let uri: hyper::Uri = src.parse().ok()?;
    // A synthetic anonymous GET: no cookies, no Accept-Encoding (fragments are
    // stored uncompressed, since they're spliced into a larger body).
    let req = hyper::Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(hyper::header::HOST, host)
        .body(())
        .ok()?;

    // A fragment is a request, and it counts as one. Fetched directly, fragments
    // went round `ratelimit_check`: a page with 32 includes cost one token and ran PHP
    // 33 times. Checked in the same place the top level checks — before the cache —
    // so a `[[ratelimit]]` rule on `/_esi/*` means what it says. A refused fragment is
    // left empty like any other fragment that failed.
    if ratelimit_check(&req, peer, &rt.config).is_some() {
        tracing::debug!(src = %src, "esi: fragment refused by rate limit, left empty");
        return None;
    }

    let key = rcache::enabled().then(|| response_cache_key(&req, host, &rt.config));
    if let Some(k) = &key {
        if let Some(c) = rcache::get(k) {
            return Some(c.body);
        }
    }

    let config = &rt.config;
    let script = docroot.join(front_controller);
    let script_name = format!("/{}", front_controller.display());
    let (parts, _) = req.into_parts();
    let request = cgi::build_request(
        &parts,
        Vec::new(),
        docroot,
        &script,
        &script_name,
        peer,
        config.https,
        config.listen.port(),
    );
    match rt.php.handle(request).await {
        Ok(Reply::Buffered(resp)) if resp.status == 200 => {
            if let Some(k) = &key {
                maybe_store(
                    k,
                    &resp,
                    "",
                    config.cache_vary_user_agent,
                    cache_rule_for(src.split('?').next().unwrap_or(src), &config.cache_rules),
                    &crate::ns::for_docroot(docroot),
                );
            }
            Some(resp.body)
        }
        Ok(Reply::Buffered(resp)) => {
            tracing::warn!(src = %src, status = resp.status, "esi: fragment returned non-200");
            None
        }
        // A fragment can't stream: it has to be spliced into a larger body.
        Ok(Reply::Stream { .. }) => {
            tracing::warn!(src = %src, "esi: fragment tried to stream; not supported");
            None
        }
        Err(e) => {
            tracing::warn!(src = %src, error = %e, "esi: fragment failed");
            None
        }
    }
}

/// Constant-time bearer check against `ASKR_ADMIN_TOKEN`.
fn control_token_ok<B>(req: &Request<B>) -> Option<bool> {
    let token = std::env::var("ASKR_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())?;
    let given = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let (a, b) = (given.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return Some(false);
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    Some(diff == 0)
}

/// Handle a `PURGE` or `BAN` request against the response cache.
///
/// - `PURGE /posts/123` drops every cached variant of that URL (all encodings and
///   device classes, `GET` and `HEAD`). With a query string it purges that exact
///   URL; without one, every query variant of the path.
/// - `BAN` with `X-Ban-Url: /category/tech/*` drops every cached URL matching the
///   glob. Patterns are globs (`*`, `?`), not regexes.
///
/// Both are scoped to the requesting `Host`, so one virtual host can't wipe
/// another's cache.
fn invalidate_request<B>(
    req: &Request<B>,
    host: &str,
    peer: SocketAddr,
    cfg: &Config,
) -> Response<ResBody> {
    // Auth: a configured token must match; with no token, only loopback may call.
    //
    // "Loopback means a local operator" holds only for a server that is itself the
    // front door. Behind nginx or Caddy on 127.0.0.1 every request arrives from
    // loopback, so the fallback authenticated the whole internet — and `BAN /*`
    // empties the cache. `trusted_proxies` is the operator saying in writing that
    // loopback is where the proxy sits, so once it is set the fallback stops
    // applying and a token is required.
    match control_token_ok(req) {
        Some(true) => {}
        Some(false) => return text(StatusCode::FORBIDDEN, "askr: bad or missing bearer token"),
        None if peer.ip().is_loopback() && cfg.trusted_proxies.is_empty() => {}
        None if peer.ip().is_loopback() => {
            return text(
                StatusCode::FORBIDDEN,
                "askr: trusted_proxies is set, so a loopback peer is the proxy and not                  necessarily a local operator — set ASKR_ADMIN_TOKEN to allow PURGE/BAN",
            )
        }
        None => {
            return text(
                StatusCode::FORBIDDEN,
                "askr: set ASKR_ADMIN_TOKEN to allow PURGE/BAN from a non-loopback address",
            )
        }
    }
    if !rcache::enabled() {
        return text(StatusCode::CONFLICT, "askr: response cache is disabled");
    }
    if host.is_empty() {
        return text(
            StatusCode::BAD_REQUEST,
            "askr: PURGE/BAN needs a Host header",
        );
    }

    if req.method().as_str() == "PURGE" {
        let n = rcache::purge_url(host, req.uri().path(), req.uri().query());
        tracing::info!(host, path = req.uri().path(), purged = n, "cache PURGE");
        return json_response(&format!("{{\"purged\":{n}}}"));
    }

    // BAN
    let Some(pattern) = req
        .headers()
        .get("x-ban-url")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return text(
            StatusCode::BAD_REQUEST,
            "askr: BAN needs an X-Ban-Url header, e.g. X-Ban-Url: /category/tech/*",
        );
    };
    // Fail loudly on regex-looking input rather than silently matching nothing.
    if pattern.starts_with('^') || pattern.contains(".*") || pattern.ends_with('$') {
        return text(
            StatusCode::BAD_REQUEST,
            "askr: X-Ban-Url is a glob, not a regex — use /category/tech/* instead of ^/category/tech/.*",
        );
    }
    let n = rcache::ban_glob(host, pattern);
    tracing::info!(host, pattern, banned = n, "cache BAN");
    json_response(&format!("{{\"banned\":{n}}}"))
}

fn json_response(body: &str) -> Response<ResBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(full(Bytes::from(body.to_owned())))
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"))
}

/// Serve a held entry as a **failure fallback** (`stale-if-error`): the origin
/// returned 5xx or the handler errored, and stale content beats an error page.
fn stale_error_fallback(key: &[u8]) -> Option<Response<ResBody>> {
    let c = rcache::stale_on_error(key)?;
    let mut resp = cached_response(c);
    resp.headers_mut().insert(
        hyper::header::HeaderName::from_static("x-askr-cache"),
        hyper::header::HeaderValue::from_static("STALE-ERROR"),
    );
    Some(resp)
}

/// Match a cookie/parameter name against a pattern with an optional trailing
/// `*` wildcard (`utm_*`). Case-insensitive.
fn name_matches(pat: &str, name: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => {
            let (n, p) = (name.as_bytes(), prefix.as_bytes());
            n.len() >= p.len() && n[..p.len()].eq_ignore_ascii_case(p)
        }
        None => name.eq_ignore_ascii_case(pat),
    }
}

/// True when every cookie in a `Cookie` header is on the ignore list, i.e. the
/// request carries no identity and may still be served from the shared cache.
/// An empty ignore list means "any cookie defeats caching" (the default).
fn cookies_ignorable(header: &str, ignore: &[String]) -> bool {
    if ignore.is_empty() {
        return false;
    }
    header.split(';').all(|c| {
        let name = c.split('=').next().unwrap_or("").trim();
        name.is_empty() || ignore.iter().any(|p| name_matches(p, name))
    })
}

/// Normalise a query string for the cache key: drop stripped parameters, then
/// sort what's left so `?a=1&b=2` and `?b=2&a=1` share one entry.
///
/// Sorting is skipped when a name repeats: PHP builds arrays from repeated names
/// (`a[]=1&a[]=2`), where order is meaningful and reordering would collide two
/// requests that render differently.
fn normalize_query(query: &str, strip: &[String]) -> String {
    let mut pairs: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let name = p.split('=').next().unwrap_or(p);
            !strip.iter().any(|s| name_matches(s, name))
        })
        .collect();
    let mut names: Vec<&str> = pairs
        .iter()
        .map(|p| p.split('=').next().unwrap_or(p))
        .collect();
    names.sort_unstable();
    if names.windows(2).all(|w| w[0] != w[1]) {
        pairs.sort_unstable();
    }
    pairs.join("&")
}

/// Coarse device class for the cache key when `vary_user_agent` is on.
fn ua_class(ua: &str) -> &'static str {
    const MOBILE: [&str; 5] = ["Mobi", "Android", "iPhone", "iPad", "iPod"];
    if MOBILE.iter().any(|m| ua.contains(m)) {
        "m"
    } else {
        "d"
    }
}

/// Cache key: `METHOD \0 host \0 path?normalised-query \0 encoding \0 device \0 scheme`.
///
/// Scheme is last on purpose. `rcache::key_parts` reads the first three fields, so
/// appending keeps `PURGE`/`BAN` working *and* scheme-agnostic: invalidating a URL
/// drops both variants, which is what anyone purging a URL means.
///
/// `host` is the already-normalised (lowercased, port-stripped) value also used for
/// virtual-host routing — so `example.com` and `example.com:443` share one entry,
/// and `PURGE`/`BAN` can match keys by the same host they were routed with.
fn response_cache_key<B>(req: &Request<B>, host: &str, cfg: &Config) -> Vec<u8> {
    // Normalised path+query: tracking parameters are dropped from the *key* only
    // — PHP still receives the full, untouched query string.
    let path = req.uri().path();
    let query = normalize_query(req.uri().query().unwrap_or(""), &cfg.cache_strip_query);
    let pq = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    // Vary the key on the negotiated content-encoding so each encoding caches its
    // own *already-compressed* bytes. Without this, one uncompressed entry is
    // shared by all clients and every HIT recompresses the same body.
    let enc = req
        .headers()
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::compress::negotiate)
        .map(|e| e.header())
        .unwrap_or("id");
    let device = if cfg.cache_vary_user_agent {
        req.headers()
            .get(hyper::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(ua_class)
            .unwrap_or("d")
    } else {
        ""
    };
    // Without the scheme in the key, one entry was shared by http and https. With
    // `force_https` off — where an http request is not bounced before it reaches the
    // cache — a page holding absolute URLs (url(), asset(), a canonical tag) could be
    // rendered once over http and then served to https clients with http links baked
    // in, or the reverse. Determined the same way the redirect engine determines it,
    // so the two cannot disagree about what scheme a request arrived on.
    let scheme = request_scheme(req, cfg.https);
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        req.method().as_str(),
        host.to_ascii_lowercase(),
        pq,
        enc,
        device,
        scheme
    )
    .into_bytes()
    // (host is already lowercase here)
}

/// Store a 200 response if the app opted in via an `Askr-Cache` header.
/// `Set-Cookie` is stripped so a cached page can't pin one client's session
/// onto every anonymous visitor.
fn maybe_store(
    key: &[u8],
    resp: &askr_php::Response,
    accept_encoding: &str,
    vary_ua: bool,
    rule: Option<&crate::config::CacheRule>,
    namespace: &str,
) {
    if resp.status != 200 {
        return;
    }
    let app_dir = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("askr-cache"))
        .map(|(_, v)| v.as_str())
        .and_then(parse_cache_directive);
    // A rule's TTL is the operator's explicit policy, so it wins over the app's
    // header — but the app's tags are kept, so a rule-cached page can still be
    // invalidated with `askr_cache_forget_tag()`.
    let (ttl, swr, sie, tags) = match (rule.and_then(|r| r.ttl), app_dir) {
        (Some(ttl), app) => {
            let r = rule.expect("ttl came from the rule");
            let tags = app.map(|(_, _, _, t)| t).unwrap_or_default();
            (ttl, r.swr, r.stale_if_error, tags)
        }
        (None, Some(d)) => d,
        // Neither the app nor a rule asked for caching.
        (None, None) => return,
    };
    // The key varies on encoding, and on device class when `vary_user_agent` is on.
    // It cannot represent anything else, and the app's own `Vary` was dropped by
    // `storable_header` rather than honoured — so a localised Laravel app answering
    // `Vary: Accept-Language` had the first visitor's language cached and served to
    // everyone. Refusing to store those responses costs hit rate on exactly the
    // responses that were being served wrong.
    if let Some(v) = header_value(&resp.headers, "vary") {
        if !vary_is_covered_by_key(v, vary_ua) {
            return;
        }
    }
    // A body the app compressed itself. `storable_header` drops Content-Encoding,
    // and `compress::maybe` hands an already-compressed body back unchanged because
    // re-compressing it comes out larger — so the entry held gzip bytes with nothing
    // saying so, and every hit sent binary to the browser.
    if let Some(e) = header_value(&resp.headers, "content-encoding") {
        let e = e.trim();
        if !e.is_empty() && !e.eq_ignore_ascii_case("identity") {
            return;
        }
    }
    let mut stored: Vec<(String, String)> = resp
        .headers
        .iter()
        .filter(|(k, _)| storable_header(k))
        .cloned()
        .collect();
    // Compress *once*, at store time, and cache the finished bytes. Every HIT on
    // this (encoding-keyed) entry then serves them verbatim — no re-compression.
    let content_type = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    // An ESI page is cached with its tags intact and assembled on the way out, so it
    // must be stored *uncompressed* — otherwise the fragments couldn't be spliced in
    // without inflating it on every hit. Compression happens after assembly instead.
    let accept_encoding = if esi_requested(&resp.headers) {
        ""
    } else {
        accept_encoding
    };
    let mut vary: Vec<&str> = Vec::new();
    let body = match crate::compress::maybe(&resp.body, content_type, accept_encoding) {
        Some((enc, compressed)) => {
            stored.push((
                hyper::header::CONTENT_ENCODING.to_string(),
                enc.header().to_string(),
            ));
            vary.push("Accept-Encoding");
            compressed
        }
        None => resp.body.clone(),
    };
    // The key splits on device class, so tell shared caches downstream too —
    // otherwise a proxy could hand mobile HTML to a desktop client.
    if vary_ua {
        vary.push("User-Agent");
    }
    if !vary.is_empty() {
        stored.push((hyper::header::VARY.to_string(), vary.join(", ")));
    }
    // Tags are the application's own names (`posts`, `user:7`), so two applications
    // in one instance would collide on them and `askr_cache_forget_tag('posts')` from
    // one would invalidate the other's pages. Stored under the application's
    // namespace, to match what `c_forget_tag` looks up.
    let tags: Vec<Vec<u8>> = tags
        .into_iter()
        .map(|t| {
            let mut k = Vec::with_capacity(namespace.len() + 1 + t.len());
            if !namespace.is_empty() {
                k.extend_from_slice(namespace.as_bytes());
                k.push(crate::ns::SEP);
            }
            k.extend_from_slice(&t);
            k
        })
        .collect();
    rcache::store(key, resp.status, &stored, &body, ttl, swr, sie, &tags);
}

/// Which scheme a request arrived on. Determined the same way the redirect engine
/// determines it, so the cache key and the http→https redirect cannot disagree.
fn request_scheme<B>(req: &Request<B>, https: bool) -> &'static str {
    let secure = https
        || req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    if secure {
        "https"
    } else {
        "http"
    }
}

/// Can the cache key represent every dimension this `Vary` names?
///
/// The key varies on negotiated encoding, and on device class when
/// `vary_user_agent` is on. Anything else — `Accept-Language`, a custom header, `*` —
/// it cannot express, so the entry would be served to clients it was not rendered
/// for.
fn vary_is_covered_by_key(vary: &str, vary_ua: bool) -> bool {
    vary.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .all(|t| {
            t.eq_ignore_ascii_case("accept-encoding")
                || (vary_ua && t.eq_ignore_ascii_case("user-agent"))
        })
}

/// First value of `name` in a PHP response's header list, case-insensitively.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn storable_header(name: &str) -> bool {
    // Askr owns Content-Encoding/Vary for cached entries (set at store time), and
    // hyper recomputes framing headers — so don't persist any of these.
    !(name.eq_ignore_ascii_case("set-cookie")
        || name.eq_ignore_ascii_case("askr-cache")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("content-encoding")
        || name.eq_ignore_ascii_case("vary"))
}

/// Seconds from a `name=` parameter inside a cache directive (0 when absent).
fn directive_secs(v: &str, name: &str) -> u64 {
    v.find(name)
        .and_then(|i| v[i + name.len()..].split([',', ';', ' ']).next())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Parse a directive like `60, swr=600, stale-if-error=86400, tags=posts,homepage`
/// → `(ttl=60, swr=600, sie=86400, [posts, homepage])`.
///
/// - `swr` (stale-while-revalidate): window after the fresh TTL during which the
///   stale entry is served *proactively* while a background refresh runs.
/// - `stale-if-error` (alias `sie`): window after the fresh TTL during which the
///   entry is kept as a **failure fallback** only — served when the origin returns
///   5xx or the handler errors, never proactively. Usually far longer than `swr`.
fn parse_cache_directive(v: &str) -> Option<(u64, u64, u64, Vec<Vec<u8>>)> {
    let (head, tagstr) = match v.find("tags=") {
        Some(i) => (&v[..i], &v[i + 5..]),
        None => (v, ""),
    };
    let ttl = head
        .split([',', ';', ' '])
        .find_map(|t| t.trim().parse::<u64>().ok())?;
    let swr = directive_secs(v, "swr=");
    let sie = directive_secs(v, "stale-if-error=").max(directive_secs(v, "sie="));
    let tags = tagstr
        .split([',', ';', ' '])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
        .collect();
    Some((ttl, swr, sie, tags))
}

/// Trigger a single background refresh of a stale cache entry so a page in its
/// stale-while-revalidate window is recomputed *off* the request path. Coalesced
/// through the inflight table: at most one refresh per key runs at a time.
fn spawn_swr_refresh<B>(rt: &Arc<Runtime>, key: &[u8], req: &Request<B>, peer: SocketAddr) {
    if !matches!(rcache::begin(key), rcache::Lead::Leader) {
        return; // a refresh (or a live leader) already owns this key
    }
    let rt = rt.clone();
    let key = key.to_vec();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let header = |name: hyper::header::HeaderName| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    // Same HTTP/2 trap as everywhere else: over h2/h3 there is no Host header, and a
    // refresh that rebuilt the request without the host would render the wrong site's
    // page into this key.
    let host = crate::cgi::effective_host(req.headers(), req.uri()).unwrap_or_default();
    let accept_encoding = header(hyper::header::ACCEPT_ENCODING);
    // With `vary_user_agent`, the key splits on device class — so the refresh has
    // to render as the same class, or desktop HTML lands under the mobile key.
    let user_agent = if rt.config.cache_vary_user_agent {
        header(hyper::header::USER_AGENT)
    } else {
        String::new()
    };
    tokio::spawn(async move {
        refresh_entry(
            rt,
            key,
            method,
            uri,
            host,
            accept_encoding,
            user_agent,
            peer,
        )
        .await;
    });
}

/// Re-run the front controller for `key` and re-store the fresh response. Only
/// anonymous, cacheable GET/HEADs reach here, so the request is fully determined
/// by method + host + path + Accept-Encoding (+ User-Agent when the key varies on
/// device class) — no body, no cookies.
#[allow(clippy::too_many_arguments)]
async fn refresh_entry(
    rt: Arc<Runtime>,
    key: Vec<u8>,
    method: Method,
    uri: hyper::Uri,
    host: String,
    accept_encoding: String,
    user_agent: String,
    peer: SocketAddr,
) {
    let config = &rt.config;
    let port = config.listen.port();
    let (docroot, front_controller) = config.site_for(&host);
    let script = docroot.join(front_controller);
    let script_name = format!("/{}", front_controller.display());

    // Keep the path: the builder consumes `uri`, and the store below needs it to
    // re-evaluate `[[cache.rule]]` for this URL.
    let uri_path = uri.path().to_string();
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    builder = builder.header(hyper::header::HOST, host);
    if !accept_encoding.is_empty() {
        builder = builder.header(hyper::header::ACCEPT_ENCODING, &accept_encoding);
    }
    if !user_agent.is_empty() {
        builder = builder.header(hyper::header::USER_AGENT, &user_agent);
    }
    let Ok(built) = builder.body(()) else {
        rcache::end(&key);
        return;
    };
    let (parts, _) = built.into_parts();
    let request = cgi::build_request(
        &parts,
        Vec::new(),
        docroot,
        &script,
        &script_name,
        peer,
        config.https,
        port,
    );
    // Only a buffered response is cacheable; a streaming one is skipped.
    if let Ok(Reply::Buffered(resp)) = rt.php.handle(request).await {
        maybe_store(
            &key,
            &resp,
            &accept_encoding,
            rt.config.cache_vary_user_agent,
            cache_rule_for(&uri_path, &rt.config.cache_rules),
            &crate::ns::for_docroot(docroot),
        );
    }
    rcache::end(&key);
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

/// Build a hyper response from a cached entry. The body is already in its final
/// form (compressed at store time, per the encoding baked into the cache key), so
/// this serves the stored bytes and headers verbatim — no per-HIT compression.
fn cached_response(c: rcache::Cached) -> Response<ResBody> {
    let mut builder =
        Response::builder().status(StatusCode::from_u16(c.status).unwrap_or(StatusCode::OK));
    for (name, value) in &c.headers {
        builder = builder.header(name, value);
    }
    let state = if c.error_only {
        "STALE-ERROR"
    } else if c.stale {
        "STALE"
    } else {
        "HIT"
    };
    builder = builder.header("X-Askr-Cache", state);
    builder
        .body(full(Bytes::from(c.body)))
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

/// Subscribe to a channel and stream Server-Sent Events.
fn sse_response(query: Option<&str>, rt: &Runtime) -> Response<ResBody> {
    let channel = query
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("channel=").map(|c| c.to_string()))
        })
        .unwrap_or_else(|| "default".to_string());

    // pusher.rs HMAC-verifies `private-`/`presence-` subscriptions before adding
    // them to a socket. This bridge has no socket id and no signature to check one
    // against, so it cannot honour that rule — and until it can, it must not be the
    // way round it: GET /askr/events?channel=private-orders would otherwise hand any
    // caller on the internet everything broadcast on a channel the WebSocket path
    // guards. Same case-sensitive prefixes as pusher.rs, so the two agree on which
    // names are privileged.
    if channel.starts_with("private-") || channel.starts_with("presence-") {
        return text(
            StatusCode::FORBIDDEN,
            "askr: private- and presence- channels are only available over the \
             WebSocket path, which authenticates the subscription",
        );
    }
    // The publish side refuses a channel name over CHAN_MAX; the subscribe side kept
    // whatever it was given, as a HashMap key held for the life of the connection. A
    // name nothing can ever publish to is only memory.
    if channel.len() > crate::broadcast::CHAN_MAX {
        return text(StatusCode::BAD_REQUEST, "askr: channel name too long");
    }

    let rx = rt.sse.subscribe(channel);
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(SseBody { rx }.boxed())
        .unwrap()
}

/// A streaming file body — reads the file in 64 KB chunks so a large file never
/// buffers the whole thing in RAM (and reports an exact size so hyper sets
/// Content-Length and suppresses the body for HEAD).
struct FileBody {
    file: tokio::fs::File,
    remaining: u64,
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }
        let want = this.remaining.min(64 * 1024) as usize;
        let mut buf = vec![0u8; want];
        let mut rb = tokio::io::ReadBuf::new(&mut buf);
        match Pin::new(&mut this.file).poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                let n = rb.filled().len();
                if n == 0 {
                    this.remaining = 0;
                    return Poll::Ready(None);
                }
                this.remaining -= n as u64;
                buf.truncate(n);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(buf)))))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// Serve a static file: streamed, with ETag + Cache-Control, conditional GET
/// (304) and single-range (206) support.
async fn serve_static(
    path: &Path,
    meta: &std::fs::Metadata,
    method: &Method,
    headers: &hyper::HeaderMap,
) -> Response<ResBody> {
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("W/\"{len:x}-{mtime:x}\"");

    // Hashed build assets can be cached forever; everything else briefly.
    let cache_control = if path.components().any(|c| c.as_os_str() == "build") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };

    // Conditional GET (tolerate the -br/-gz suffix a compressed variant carries).
    if let Some(inm) = headers
        .get(hyper::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm.split(',').any(|t| {
            let t = t.trim().trim_end_matches("-br").trim_end_matches("-gz");
            t == etag
        }) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(hyper::header::ETAG, &etag)
                .header(hyper::header::CACHE_CONTROL, cache_control)
                .body(full(Bytes::new()))
                .unwrap();
        }
    }

    // Compress small, compressible, non-Range static files on the fly (JS/CSS/
    // JSON/SVG assets). Large files keep streaming uncompressed.
    let ct = mime_for(path);
    if !headers.contains_key(hyper::header::RANGE)
        && len <= crate::compress::MAX_STATIC
        && crate::compress::compressible(ct)
    {
        let accept = headers
            .get(hyper::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if let Some(enc) = crate::compress::negotiate(accept) {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if let Some(compressed) = crate::compress::compress(&bytes, enc) {
                    if compressed.len() < bytes.len() {
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header(hyper::header::CONTENT_TYPE, ct)
                            .header(hyper::header::ETAG, format!("{etag}{}", enc.etag_suffix()))
                            .header(hyper::header::CACHE_CONTROL, cache_control)
                            .header(hyper::header::CONTENT_ENCODING, enc.header())
                            .header(hyper::header::VARY, "Accept-Encoding")
                            .body(full(Bytes::from(compressed)))
                            .unwrap_or_else(|_| {
                                text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response")
                            });
                    }
                }
            }
        }
    }

    let (start, end) = parse_range(headers, len);
    let partial =
        headers.contains_key(hyper::header::RANGE) && (start != 0 || end != len.saturating_sub(1));
    // For an empty file end==0 and start==0, so end+1-start would be 1 — send 0.
    // (Empty static assets are common: a Vite CSS-only entry emits an empty .js.)
    let send_len = if len == 0 { 0 } else { end + 1 - start };

    let mut builder = Response::builder()
        .header(hyper::header::CONTENT_TYPE, mime_for(path))
        .header(hyper::header::ETAG, &etag)
        .header(hyper::header::CACHE_CONTROL, cache_control)
        .header(hyper::header::ACCEPT_RANGES, "bytes");
    builder = if partial {
        builder.status(StatusCode::PARTIAL_CONTENT).header(
            hyper::header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{len}"),
        )
    } else {
        builder.status(StatusCode::OK)
    };

    let _ = method; // hyper suppresses the body for HEAD (using FileBody's size_hint)

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return text(StatusCode::NOT_FOUND, "askr: file not found"),
    };
    if start > 0 {
        use tokio::io::AsyncSeekExt;
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return text(StatusCode::INTERNAL_SERVER_ERROR, "askr: seek failed");
        }
    }
    builder
        .body(
            FileBody {
                file,
                remaining: send_len,
            }
            .boxed(),
        )
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"))
}

/// Parse a single HTTP Range header into an inclusive `(start, end)`. Falls back
/// to the whole file `(0, len-1)` for a missing/invalid/multi-range request.
fn parse_range(headers: &hyper::HeaderMap, len: u64) -> (u64, u64) {
    let full = (0, len.saturating_sub(1));
    if len == 0 {
        return full;
    }
    let Some(spec) = headers
        .get(hyper::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
    else {
        return full;
    };
    // Single range only.
    let Some((s, e)) = spec.split(',').next().unwrap_or("").trim().split_once('-') else {
        return full;
    };
    let (start, end) = match (s.trim(), e.trim()) {
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(n) if n > 0 => (len.saturating_sub(n), len - 1),
            _ => return full,
        },
        (a, "") => match a.parse::<u64>() {
            Ok(start) => (start, len - 1),
            _ => return full,
        },
        (a, b) => match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(start), Ok(end)) => (start, end.min(len - 1)),
            _ => return full,
        },
    };
    if start > end || start >= len {
        return full;
    }
    (start, end)
}

fn text(status: StatusCode, msg: &str) -> Response<ResBody> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from(msg.to_owned())))
        .unwrap()
}

/// Strip the leading slash and reject any `..`/absolute traversal.
fn sanitize(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in Path::new(path.trim_start_matches('/')).components() {
        if let Component::Normal(c) = comp {
            out.push(c);
        }
    }
    out
}

/// Paths that must never be served as static bytes:
///
/// - **PHP sources.** Serving `/index.php` returned the file's source instead of
///   running it — source disclosure, and any other `.php` under the docroot leaked
///   too (installers, legacy scripts, files holding credentials).
/// - **Dotfiles and dot-directories** — `.env`, `.git/*`, `.htaccess`. A docroot
///   pointed at an app root (a common misconfiguration) otherwise served secrets
///   verbatim.
///
/// `.well-known/` stays allowed: ACME HTTP-01, `security.txt` and friends live
/// there legitimately.
///
/// Blocked paths fall through to the front controller, so the app answers (a 404
/// from the framework) instead of Askr handing out bytes. Note that Askr only ever
/// *executes* the configured front controller — never an arbitrary `.php` found on
/// disk — so an uploaded script can't be run through this path either.
fn static_forbidden(rel: &Path) -> bool {
    let dotted = rel.components().any(|c| match c {
        Component::Normal(s) => {
            let s = s.to_string_lossy();
            s.starts_with('.') && s != ".well-known"
        }
        _ => false,
    });
    if dotted {
        return true;
    }
    // Editor and deploy leftovers: `index.php.bak`, `config.php~`, `db.php.save`.
    // A `.php.<anything>` file is still PHP source, and nobody serves `~`/`.bak`
    // on purpose. nginx and Apache hand these out by default — Askr ships with no
    // config to add rules to, so it refuses them itself.
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains(".php.") || name.ends_with('~') {
        return true;
    }
    const LEFTOVER: [&str; 8] = [
        ".bak", ".orig", ".save", ".swp", ".swo", ".old", ".rej", ".tmp",
    ];
    if LEFTOVER.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    matches!(
        rel.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "php" | "php3" | "php4" | "php5" | "php7" | "php8" | "phps" | "pht" | "phtml" | "phar"
        )
    )
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
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

    /// A bearer-authenticated API response is per-user as surely as a session-cookied
    /// page is. It read as anonymous, so an operator `[[cache.rule]]` on `/api/*`
    /// served one user's response to the next.
    #[test]
    fn a_bearer_token_is_identity_and_analytics_cookies_are_not() {
        let req = |headers: &[(&str, &str)]| {
            let mut b = hyper::Request::builder().uri("/api/me");
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            b.body(()).unwrap()
        };
        let ignore = vec!["_ga".to_string()];

        assert!(
            !carries_identity(&req(&[]), &ignore),
            "no headers: anonymous"
        );
        assert!(
            !carries_identity(&req(&[("cookie", "_ga=GA1.2.3")]), &ignore),
            "an analytics cookie alone is not identity"
        );
        assert!(carries_identity(
            &req(&[("cookie", "laravel_session=abc")]),
            &ignore
        ));
        assert!(
            carries_identity(&req(&[("authorization", "Bearer eyJ...")]), &ignore),
            "the case that was missing"
        );
        assert!(carries_identity(
            &req(&[("proxy-authorization", "Basic x")]),
            &ignore
        ));
        // Analytics cookie plus a token: the token decides.
        assert!(carries_identity(
            &req(&[("cookie", "_ga=1"), ("authorization", "Bearer t")]),
            &ignore
        ));
    }

    /// An http and an https render of the same URL used to share one cache entry, so
    /// a page holding absolute URLs could be served to the wrong scheme with the
    /// wrong links baked in.
    #[test]
    fn the_cache_key_separates_http_from_https() {
        let req = |xfp: Option<&str>| {
            let mut b = hyper::Request::builder().uri("/");
            if let Some(v) = xfp {
                b = b.header("x-forwarded-proto", v);
            }
            b.body(()).unwrap()
        };

        assert_eq!(request_scheme(&req(None), false), "http");
        assert_eq!(request_scheme(&req(None), true), "https");
        assert_eq!(request_scheme(&req(Some("https")), false), "https");
        assert_eq!(request_scheme(&req(Some("http")), false), "http");
        // A TLS listener is not downgraded by a header claiming otherwise.
        assert_eq!(request_scheme(&req(Some("http")), true), "https");
    }

    /// The key cannot express an arbitrary `Vary`, and the app's own header was
    /// dropped rather than honoured — so `Vary: Accept-Language` meant the first
    /// visitor's language was cached for everyone. Refusing to store is the fix.
    #[test]
    fn a_vary_the_key_cannot_express_is_not_cacheable() {
        assert!(vary_is_covered_by_key("Accept-Encoding", false));
        assert!(vary_is_covered_by_key("accept-encoding, ", false));
        assert!(vary_is_covered_by_key("Accept-Encoding, User-Agent", true));

        assert!(!vary_is_covered_by_key("Accept-Language", false));
        assert!(!vary_is_covered_by_key(
            "Accept-Encoding, Accept-Language",
            false
        ));
        assert!(!vary_is_covered_by_key("*", false));
        // User-Agent is only covered when the key actually splits on device class.
        assert!(!vary_is_covered_by_key("User-Agent", false));
    }

    #[test]
    fn sanitize_strips_traversal() {
        assert_eq!(sanitize("/build/app.js"), PathBuf::from("build/app.js"));
        // path traversal and absolute components are dropped
        assert_eq!(sanitize("/../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(sanitize("/a/../b/./c"), PathBuf::from("a/b/c"));
        assert!(sanitize("/").as_os_str().is_empty());
    }

    #[test]
    fn static_serving_refuses_sources_and_dotfiles() {
        // PHP sources are never handed out as bytes (and never executed from disk).
        assert!(static_forbidden(Path::new("index.php")));
        assert!(static_forbidden(Path::new("legacy/install.PHP")));
        assert!(static_forbidden(Path::new("a.phtml")));
        assert!(static_forbidden(Path::new("a.phar")));
        assert!(static_forbidden(Path::new("a.php5")));
        // Dotfiles and dot-directories: secrets and VCS metadata.
        assert!(static_forbidden(Path::new(".env")));
        assert!(static_forbidden(Path::new(".env.production")));
        assert!(static_forbidden(Path::new(".git/config")));
        assert!(static_forbidden(Path::new("sub/.htaccess")));
        // `.well-known` is legitimate (ACME HTTP-01, security.txt).
        assert!(!static_forbidden(Path::new(".well-known/security.txt")));
        assert!(!static_forbidden(Path::new(
            ".well-known/acme-challenge/tok123"
        )));
        // Editor / deploy leftovers — `.php.bak` is still PHP source.
        assert!(static_forbidden(Path::new("index.php.bak")));
        assert!(static_forbidden(Path::new("index.php.orig")));
        assert!(static_forbidden(Path::new("db.php.save")));
        assert!(static_forbidden(Path::new("index.PHP.BAK")));
        assert!(static_forbidden(Path::new("config.php~")));
        assert!(static_forbidden(Path::new("notes.txt~")));
        assert!(static_forbidden(Path::new("sub/logo.png.bak")));
        // Normal assets are unaffected — including ones that merely *contain* a
        // leftover-looking word.
        assert!(!static_forbidden(Path::new("img/photo.old.png")));
        assert!(!static_forbidden(Path::new("build/vendor.bak.js")));
        assert!(!static_forbidden(Path::new("build/app.js")));
        assert!(!static_forbidden(Path::new("img/logo.png")));
        assert!(!static_forbidden(Path::new("phpinfo.txt")));
        assert!(!static_forbidden(Path::new("graphql")));
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_for(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(
            mime_for(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_for(Path::new("a.woff2")), "font/woff2");
        assert_eq!(mime_for(Path::new("a.unknown")), "application/octet-stream");
        assert_eq!(mime_for(Path::new("noext")), "application/octet-stream");
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

    #[test]
    fn name_matches_exact_and_glob() {
        assert!(name_matches("gclid", "gclid"));
        assert!(name_matches("GCLID", "gclid")); // case-insensitive
        assert!(!name_matches("gclid", "gclid2"));
        assert!(name_matches("utm_*", "utm_source"));
        assert!(name_matches("utm_*", "utm_"));
        assert!(!name_matches("utm_*", "utm"));
        assert!(!name_matches("utm_*", "campaign"));
        // A trailing-* pattern must not panic on multi-byte input.
        assert!(!name_matches("utm_*", "æøå"));
    }

    #[test]
    fn cookies_only_analytics_stay_cacheable() {
        let ignore = vec!["_ga".to_string(), "_gid".to_string(), "_fbp*".to_string()];
        // Analytics-only visitor: still anonymous.
        assert!(cookies_ignorable("_ga=GA1.1.22; _gid=x", &ignore));
        assert!(cookies_ignorable("_fbp_extra=1", &ignore));
        // A session cookie is identity — not cacheable.
        assert!(!cookies_ignorable("_ga=1; laravel_session=abc", &ignore));
        assert!(!cookies_ignorable("laravel_session=abc", &ignore));
        // Empty ignore list keeps the old behaviour: any cookie defeats caching.
        assert!(!cookies_ignorable("_ga=1", &[]));
    }

    #[test]
    fn query_normalisation_strips_and_sorts() {
        let strip = vec!["utm_*".to_string(), "gclid".to_string()];
        // Tracking params dropped, real params kept.
        assert_eq!(normalize_query("utm_source=x&id=7&gclid=y", &strip), "id=7");
        // Param order doesn't fragment the cache.
        assert_eq!(
            normalize_query("b=2&a=1", &strip),
            normalize_query("a=1&b=2", &strip)
        );
        // All-tracking query collapses onto the bare path entry.
        assert_eq!(normalize_query("utm_source=x", &strip), "");
        // Repeated names (PHP arrays) keep their original order — sorting them
        // would collide two requests that render differently.
        assert_eq!(normalize_query("a[]=2&a[]=1", &[]), "a[]=2&a[]=1");
        assert_eq!(normalize_query("a[]=1&a[]=2", &[]), "a[]=1&a[]=2");
        assert_ne!(
            normalize_query("a[]=2&a[]=1", &[]),
            normalize_query("a[]=1&a[]=2", &[])
        );
    }

    #[test]
    fn cidr_parsing_and_matching() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        // Bare address = single host.
        let c = parse_cidr("192.168.1.5").unwrap();
        assert!(cidr_contains(&c, &ip("192.168.1.5")));
        assert!(!cidr_contains(&c, &ip("192.168.1.6")));
        // v4 prefix.
        let c = parse_cidr("10.0.0.0/8").unwrap();
        assert!(cidr_contains(&c, &ip("10.255.3.1")));
        assert!(!cidr_contains(&c, &ip("11.0.0.1")));
        let c = parse_cidr("192.168.1.0/24").unwrap();
        assert!(cidr_contains(&c, &ip("192.168.1.200")));
        assert!(!cidr_contains(&c, &ip("192.168.2.1")));
        // /0 matches everything of the same family, but not across families.
        let c = parse_cidr("0.0.0.0/0").unwrap();
        assert!(cidr_contains(&c, &ip("8.8.8.8")));
        assert!(!cidr_contains(&c, &ip("::1")));
        // v6.
        let c = parse_cidr("fd00::/8").unwrap();
        assert!(cidr_contains(&c, &ip("fd12::1")));
        assert!(!cidr_contains(&c, &ip("fe80::1")));
        assert!(cidr_contains(&parse_cidr("::1").unwrap(), &ip("::1")));
        // Rejected input.
        assert!(parse_cidr("not-an-ip").is_none());
        assert!(parse_cidr("10.0.0.0/99").is_none());
        assert!(parse_cidr("").is_none());
    }

    #[test]
    fn forwarded_for_is_only_believed_through_trusted_proxies() {
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let req = |xff: &str| {
            hyper::Request::builder()
                .uri("/")
                .header("x-forwarded-for", xff)
                .body(())
                .unwrap()
        };
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();

        // No trusted proxies: the header is ignored entirely, so a spoofed address
        // can't hand the client a fresh rate-limit bucket.
        assert_eq!(client_ip(&req("9.9.9.9"), peer, &[]), ip("127.0.0.1"));

        // Peer is a trusted proxy: believe the chain.
        let trusted = vec![parse_cidr("127.0.0.1").unwrap()];
        assert_eq!(client_ip(&req("9.9.9.9"), peer, &trusted), ip("9.9.9.9"));

        // Multi-hop: take the rightmost address that isn't itself a trusted proxy.
        let trusted = vec![
            parse_cidr("127.0.0.1").unwrap(),
            parse_cidr("10.0.0.0/8").unwrap(),
        ];
        assert_eq!(
            client_ip(&req("9.9.9.9, 10.1.1.1, 10.1.1.2"), peer, &trusted),
            ip("9.9.9.9"),
            "trusted hops are skipped from the right"
        );
        // A client-supplied fake in front of the real one doesn't win.
        assert_eq!(
            client_ip(&req("1.1.1.1, 9.9.9.9"), peer, &trusted),
            ip("9.9.9.9")
        );
        // Garbage in the chain falls back to the peer.
        assert_eq!(client_ip(&req("nonsense"), peer, &trusted), ip("127.0.0.1"));
        // An entry with a port is tolerated.
        assert_eq!(
            client_ip(&req("9.9.9.9:5678"), peer, &trusted),
            ip("9.9.9.9")
        );
    }

    #[test]
    fn cache_rules_first_match_wins() {
        let mk = |path: &str, action: Option<&str>, ttl: Option<u64>| crate::config::CacheRule {
            path: path.to_string(),
            action: action.map(str::to_owned),
            ttl,
            swr: 0,
            stale_if_error: 0,
            force: false,
        };
        let rules = vec![
            mk("/admin/*", Some("pass"), None),
            mk("/static/*", None, Some(86400)),
            mk("/*", None, Some(60)),
        ];
        // Specific rules win over the catch-all because the first match is taken.
        assert!(cache_rule_for("/admin/users", &rules).unwrap().is_pass());
        assert_eq!(
            cache_rule_for("/static/app.css", &rules).unwrap().ttl,
            Some(86400)
        );
        assert_eq!(
            cache_rule_for("/anything/else", &rules).unwrap().ttl,
            Some(60)
        );
        // No rules at all: no policy.
        assert!(cache_rule_for("/x", &[]).is_none());
        // A glob that doesn't match leaves the path unruled.
        assert!(cache_rule_for("/x", &[mk("/admin/*", Some("pass"), None)]).is_none());
    }

    #[test]
    fn cache_directive_parses_stale_if_error() {
        // ttl only
        assert_eq!(parse_cache_directive("60"), Some((60, 0, 0, vec![])));
        // swr + stale-if-error + tags, in one directive
        let (ttl, swr, sie, tags) =
            parse_cache_directive("300, swr=60, stale-if-error=86400, tags=posts,home").unwrap();
        assert_eq!((ttl, swr, sie), (300, 60, 86400));
        assert_eq!(tags, vec![b"posts".to_vec(), b"home".to_vec()]);
        // `sie=` is accepted as a short alias
        assert_eq!(parse_cache_directive("30, sie=600").unwrap().2, 600);
        // stale-if-error without swr: the grace window stands on its own
        let (ttl, swr, sie, _) = parse_cache_directive("300, stale-if-error=86400").unwrap();
        assert_eq!((ttl, swr, sie), (300, 0, 86400));
        // a bare directive with no ttl is not cacheable
        assert!(parse_cache_directive("tags=posts").is_none());
    }

    #[test]
    fn ua_class_splits_mobile_from_desktop() {
        assert_eq!(
            ua_class("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) Mobile/15E148"),
            "m"
        );
        assert_eq!(
            ua_class("Mozilla/5.0 (Linux; Android 14) Chrome/120 Mobile"),
            "m"
        );
        assert_eq!(
            ua_class("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"),
            "d"
        );
        assert_eq!(ua_class(""), "d");
    }
}
