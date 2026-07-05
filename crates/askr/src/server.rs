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
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use crate::php::Php;
use crate::pusher::{self, PusherHub};
use crate::rcache;

/// Response body: buffered (Full) or streaming (SSE / files), unified as a box.
type ResBody = BoxBody<Bytes, std::io::Error>;

/// Max simultaneous connections per worker — a backstop against connection
/// exhaustion (slowloris); combined with the handshake/header timeouts, idle
/// connections can't pile up.
const MAX_CONNECTIONS: usize = 8192;

/// Bounded wait for a TLS handshake / a client to send request headers.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a coalesced follower waits for the leader before running PHP itself.
const COALESCE_WAIT: Duration = Duration::from_secs(5);

fn full(bytes: Bytes) -> ResBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

#[derive(Clone)]
pub struct Config {
    pub docroot: PathBuf,
    pub front_controller: PathBuf, // relative, e.g. index.php
    pub listen: SocketAddr,
    pub https: bool,
    pub worker_script: Option<PathBuf>,
    pub max_requests: usize,
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
}

/// Shared per-worker runtime state for recycling/draining.
struct Runtime {
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
}

/// Per-worker registry of live SSE subscribers. A background task tails the
/// shared broadcast ring and pushes matching events to these.
#[derive(Default)]
struct SseHub {
    subs: Mutex<Vec<Sub>>,
}

struct Sub {
    channel: String,
    tx: mpsc::Sender<Bytes>,
}

impl SseHub {
    fn subscribe(&self, channel: String) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(128);
        let _ = tx.try_send(Bytes::from_static(b": connected\n\n"));
        self.subs.lock().unwrap().push(Sub { channel, tx });
        rx
    }

    fn deliver(&self, channel: &str, data: &Bytes) {
        self.subs.lock().unwrap().retain(|s| {
            if s.channel == channel {
                // Non-blocking: if a subscriber's 128-message buffer is full
                // (a client that can't keep up), try_send fails and we drop that
                // subscriber. This is intentional back-pressure — a slow client
                // is disconnected rather than stalling the broadcast fan-out.
                s.tx.try_send(data.clone()).is_ok()
            } else {
                !s.tx.is_closed()
            }
        });
    }

    fn ping(&self) {
        let msg = Bytes::from_static(b": ping\n\n");
        self.subs
            .lock()
            .unwrap()
            .retain(|s| s.tx.try_send(msg.clone()).is_ok());
    }
}

/// Streaming body for an SSE connection: yields frames as events arrive.
struct SseBody {
    rx: mpsc::Receiver<Bytes>,
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
) -> anyhow::Result<()> {
    let pusher_enabled = config.pusher;
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
    });

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
                let (stream, peer) = accepted?;
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
                tracing::info!(served = rt.served.load(Ordering::SeqCst), "recycling: draining");
                break;
            }
            _ = sigterm.recv() => {
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
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
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
    let service = service_fn(move |req| handle(req, rt.clone(), peer));
    let mut builder = auto::Builder::new(TokioExecutor::new());
    // Bound how long a client may take to send request headers (slowloris).
    // header_read_timeout needs a timer registered on the builder.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!(error = %e, "connection closed");
    }
}

async fn handle(
    mut req: Request<Incoming>,
    rt: Arc<Runtime>,
    peer: SocketAddr,
) -> Result<Response<ResBody>, Infallible> {
    let t_start = Instant::now();
    let config = &rt.config;
    let port = config.listen.port();
    let accept_encoding = req
        .headers()
        .get(hyper::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

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
    // syscall on the async path).
    let rel = sanitize(req.uri().path());
    if !rel.as_os_str().is_empty() {
        let candidate = config.docroot.join(&rel);
        if let Ok(meta) = tokio::fs::metadata(&candidate).await {
            if meta.is_file() {
                return Ok(serve_static(&candidate, &meta, req.method(), req.headers()).await);
            }
        }
    }

    // --- response cache: read before touching PHP (#1) -----------------
    // Only anonymous (no Cookie) GET/HEAD requests are cacheable — a request
    // that carries a session/auth cookie may see user-specific content.
    let cacheable = rcache::enabled()
        && matches!(*req.method(), Method::GET | Method::HEAD)
        && !req.headers().contains_key(hyper::header::COOKIE);
    let cache_key = cacheable.then(|| response_cache_key(&req));
    // #2 request coalescing: when a cacheable key misses, exactly one request
    // (the leader) runs PHP; the rest wait for it to populate the cache.
    let mut coalesce_leader = false;
    if let Some(key) = &cache_key {
        if let Some(c) = rcache::get(key) {
            let response = cached_response(c, &accept_encoding);
            finish(&rt, &response, t_start, 0);
            return Ok(response);
        }
        match rcache::begin(key) {
            rcache::Lead::Leader => coalesce_leader = true,
            rcache::Lead::Follower => {
                // Wait (fail-open) for the leader to fill the cache.
                let deadline = Instant::now() + COALESCE_WAIT;
                let mut served = None;
                while Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    if let Some(c) = rcache::peek(key) {
                        served = Some(c);
                        break;
                    }
                    if !rcache::is_inflight(key) {
                        break; // leader finished without caching → run PHP ourselves
                    }
                }
                if let Some(c) = served {
                    rcache::note_coalesced();
                    let response = cached_response(c, &accept_encoding);
                    finish(&rt, &response, t_start, 0);
                    return Ok(response);
                }
                // fall through: run PHP uncoalesced (leader didn't cache / timed out)
            }
        }
    }

    let script = config.docroot.join(&config.front_controller);
    let script_name = format!("/{}", config.front_controller.display());

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

    let (request, upload_temp_paths) = if let Some(boundary) = multipart_boundary {
        match crate::upload::parse(body.into_data_stream(), &boundary, max).await {
            Ok(parsed) => {
                let mut request = cgi::build_request(
                    &parts,
                    Vec::new(), // body consumed while streaming; PHP uses $_POST/$_FILES
                    &config.docroot,
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
            &config.docroot,
            &script,
            &script_name,
            peer,
            config.https,
            port,
        );
        (request, Vec::new())
    };

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

    let response = match php_result {
        Ok(resp) => {
            // Cache store: the app opts in per-response with an `Askr-Cache`
            // header (which we consume, never forwarding it to the client).
            if let Some(key) = &cache_key {
                maybe_store(key, &resp);
            }
            let state = rcache::enabled().then_some("MISS");
            build_response(resp, state, &accept_encoding)
        }
        Err(e) => {
            tracing::error!(error = %e, "php handling failed");
            if let Some(m) = crate::metrics::Metrics::get() {
                m.note_error();
            }
            text(StatusCode::BAD_GATEWAY, &format!("askr: {e}"))
        }
    };

    // Release any followers waiting on this key (the cache is now populated, or
    // this response wasn't cacheable and they should run PHP themselves).
    if coalesce_leader {
        if let Some(key) = &cache_key {
            rcache::end(key);
        }
    }

    // Clean up any uploaded temp files (the app may already have moved them).
    for p in &upload_temp_paths {
        let _ = tokio::fs::remove_file(p).await;
    }

    // Record a failing request so it can be replayed later (#5).
    if response.status().as_u16() >= 500 {
        if let (Some(dir), Some(req)) = (&config.record_dir, &record_copy) {
            crate::record::record_failure(dir, req, response.status().as_u16());
        }
    }

    finish(&rt, &response, t_start, php_us);
    Ok(response)
}

/// Record metrics and advance the recycle counter for a finished request.
fn finish(rt: &Runtime, response: &Response<ResBody>, t_start: Instant, php_us: u64) {
    if let Some(m) = crate::metrics::Metrics::get() {
        let total_us = t_start.elapsed().as_micros() as u64;
        let bytes = response.body().size_hint().exact().unwrap_or(0);
        m.record(response.status().as_u16(), bytes, php_us, total_us);
    }
    if rt.recycle_after > 0 {
        let n = rt.served.fetch_add(1, Ordering::SeqCst) + 1;
        if n == rt.recycle_after {
            rt.shutdown.notify_one();
        }
    }
}

/// Cache key: `METHOD \0 host \0 path?query`.
fn response_cache_key(req: &Request<Incoming>) -> Vec<u8> {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    format!(
        "{}\0{}\0{}",
        req.method().as_str(),
        host.to_ascii_lowercase(),
        pq
    )
    .into_bytes()
}

/// Store a 200 response if the app opted in via an `Askr-Cache` header.
/// `Set-Cookie` is stripped so a cached page can't pin one client's session
/// onto every anonymous visitor.
fn maybe_store(key: &[u8], resp: &askr_php::Response) {
    if resp.status != 200 {
        return;
    }
    let Some(dir) = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("askr-cache"))
        .map(|(_, v)| v.as_str())
    else {
        return;
    };
    let Some((ttl, tags)) = parse_cache_directive(dir) else {
        return;
    };
    let stored: Vec<(String, String)> = resp
        .headers
        .iter()
        .filter(|(k, _)| storable_header(k))
        .cloned()
        .collect();
    rcache::store(key, resp.status, &stored, &resp.body, ttl, &tags);
}

fn storable_header(name: &str) -> bool {
    !(name.eq_ignore_ascii_case("set-cookie")
        || name.eq_ignore_ascii_case("askr-cache")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("transfer-encoding"))
}

/// Parse a directive like `60, tags=posts,homepage` → `(60, [posts, homepage])`.
fn parse_cache_directive(v: &str) -> Option<(u64, Vec<Vec<u8>>)> {
    let (head, tagstr) = match v.find("tags=") {
        Some(i) => (&v[..i], &v[i + 5..]),
        None => (v, ""),
    };
    let ttl = head
        .split([',', ';', ' '])
        .find_map(|t| t.trim().parse::<u64>().ok())?;
    let tags = tagstr
        .split([',', ';', ' '])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
        .collect();
    Some((ttl, tags))
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

/// Build a hyper response from a cached entry.
fn cached_response(c: rcache::Cached, accept_encoding: &str) -> Response<ResBody> {
    let mut builder =
        Response::builder().status(StatusCode::from_u16(c.status).unwrap_or(StatusCode::OK));
    let mut content_type = String::new();
    for (name, value) in &c.headers {
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value.clone();
        }
        builder = builder.header(name, value);
    }
    builder = builder.header("X-Askr-Cache", "HIT");
    finish_body(builder, c.body, &content_type, accept_encoding)
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
    let send_len = end + 1 - start;

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

    #[test]
    fn sanitize_strips_traversal() {
        assert_eq!(sanitize("/build/app.js"), PathBuf::from("build/app.js"));
        // path traversal and absolute components are dropped
        assert_eq!(sanitize("/../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(sanitize("/a/../b/./c"), PathBuf::from("a/b/c"));
        assert!(sanitize("/").as_os_str().is_empty());
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
}
