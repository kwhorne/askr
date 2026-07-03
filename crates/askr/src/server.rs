//! The HTTP front: accept connections, serve static files directly, and hand
//! dynamic requests to the embedded PHP interpreter.
//!
//! tokio/hyper here is the pragmatic A1 I/O layer. The share-nothing endgame
//! swaps this for a per-core io_uring loop (PRD §5.4) behind the same seam:
//! `Php::handle`.
//!
//! Recycling is graceful: after `recycle_after` requests we stop accepting new
//! connections, let the in-flight ones drain, and return — the caller then exits
//! the process and the supervisor respawns a fresh worker. No dropped requests.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_rustls::TlsAcceptor;

use crate::cgi;
use crate::php::Php;

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
    let rt = Arc::new(Runtime {
        config,
        php,
        served: AtomicUsize::new(0),
        recycle_after,
        shutdown: Notify::new(),
        active: AtomicUsize::new(0),
        tls,
    });

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
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
        match acceptor.accept(stream).await {
            Ok(tls) => serve_io(TokioIo::new(tls), rt, peer).await,
            Err(e) => tracing::debug!(error = %e, "TLS handshake failed"),
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
    if let Err(e) = auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await
    {
        tracing::debug!(error = %e, "connection closed");
    }
}

async fn handle(
    req: Request<Incoming>,
    rt: Arc<Runtime>,
    peer: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let config = &rt.config;
    let port = config.listen.port();

    // try_files: serve an existing static file directly.
    let rel = sanitize(req.uri().path());
    if !rel.as_os_str().is_empty() {
        let candidate = config.docroot.join(&rel);
        if candidate.is_file() {
            return Ok(serve_static(&candidate).await);
        }
    }

    let script = config.docroot.join(&config.front_controller);
    let script_name = format!("/{}", config.front_controller.display());

    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes().to_vec(),
        Err(_) => Vec::new(),
    };

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

    let response = match rt.php.handle(request).await {
        Ok(resp) => build_response(resp),
        Err(e) => {
            tracing::error!(error = %e, "php handling failed");
            text(StatusCode::BAD_GATEWAY, &format!("askr: {e}"))
        }
    };

    // Count the request; trigger a graceful recycle when we hit the cap.
    if rt.recycle_after > 0 {
        let n = rt.served.fetch_add(1, Ordering::SeqCst) + 1;
        if n == rt.recycle_after {
            rt.shutdown.notify_one();
        }
    }

    Ok(response)
}

fn build_response(resp: askr_php::Response) -> Response<Full<Bytes>> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK));

    for (name, value) in &resp.headers {
        if name.eq_ignore_ascii_case("Content-Length")
            || name.eq_ignore_ascii_case("Transfer-Encoding")
        {
            continue;
        }
        builder = builder.header(name, value);
    }

    builder
        .body(Full::new(Bytes::from(resp.body)))
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"))
}

async fn serve_static(path: &Path) -> Response<Full<Bytes>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, mime_for(path))
            .body(Full::new(Bytes::from(bytes)))
            .unwrap(),
        Err(_) => text(StatusCode::NOT_FOUND, "askr: file not found"),
    }
}

fn text(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(msg.to_owned())))
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
