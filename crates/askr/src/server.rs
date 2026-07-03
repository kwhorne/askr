//! The HTTP front: accept connections, serve static files directly, and hand
//! dynamic requests to the embedded PHP interpreter.
//!
//! tokio/hyper here is the pragmatic A1 I/O layer. The share-nothing endgame
//! swaps this for a per-core io_uring loop (PRD §5.4) behind the same seam:
//! `Php::handle`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::cgi;
use crate::php::Php;

#[derive(Clone)]
pub struct Config {
    pub docroot: PathBuf,
    pub front_controller: PathBuf, // relative, e.g. index.php
    pub listen: SocketAddr,
    pub https: bool,
    pub worker_script: Option<PathBuf>,
}

/// Serve on an already-bound listener (built with SO_REUSEPORT by the worker).
pub async fn run(listener: TcpListener, config: Arc<Config>, php: Php) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let config = config.clone();
        let php = php.clone();

        tokio::task::spawn(async move {
            let service = service_fn(move |req| {
                handle(req, config.clone(), php.clone(), peer)
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(error = %e, "connection closed");
            }
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    config: Arc<Config>,
    php: Php,
    peer: SocketAddr,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let port = config.listen.port();

    // try_files: serve an existing static file (built assets, images, …)
    // directly; otherwise fall through to the front controller.
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

    match php.handle(request).await {
        Ok(resp) => Ok(build_response(resp)),
        Err(e) => {
            tracing::error!(error = %e, "php handling failed");
            Ok(text(StatusCode::BAD_GATEWAY, &format!("askr: {e}")))
        }
    }
}

fn build_response(resp: askr_php::Response) -> Response<Full<Bytes>> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK));

    for (name, value) in &resp.headers {
        // hyper sets these itself from the body/framing.
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
        match comp {
            Component::Normal(c) => out.push(c),
            _ => {} // drop RootDir, CurDir, ParentDir, Prefix
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
