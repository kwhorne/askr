//! Map an HTTP request to the CGI-style `$_SERVER` environment PHP expects.
//! This is the same variable convention FastCGI uses, so it mirrors grove's
//! `build_fcgi_params` — but feeds the in-process interpreter instead.

use std::net::SocketAddr;
use std::path::Path;

use hyper::http::request::Parts;

use askr_php::Request;

/// Build an [`askr_php::Request`] for the front controller.
pub fn build_request(
    parts: &Parts,
    body: Vec<u8>,
    docroot: &Path,
    script: &Path,
    script_name: &str,
    peer: SocketAddr,
    https: bool,
    server_port: u16,
) -> Request {
    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let query = parts
        .uri
        .query()
        .map(|q| q.to_string())
        .unwrap_or_default();
    let request_uri = match parts.uri.path_and_query() {
        Some(pq) => pq.as_str().to_string(),
        None => path.clone(),
    };

    let host = parts
        .headers
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.split(':').next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "localhost".to_string());

    let content_type = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let cookie = parts
        .headers
        .get(hyper::header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let mut server_vars: Vec<(String, String)> = vec![
        ("REQUEST_METHOD".into(), method.clone()),
        ("REQUEST_URI".into(), request_uri.clone()),
        ("QUERY_STRING".into(), query.clone()),
        ("PATH_INFO".into(), path.clone()),
        ("SCRIPT_NAME".into(), script_name.to_string()),
        ("PHP_SELF".into(), script_name.to_string()),
        (
            "SCRIPT_FILENAME".into(),
            script.to_string_lossy().into_owned(),
        ),
        (
            "DOCUMENT_ROOT".into(),
            docroot.to_string_lossy().into_owned(),
        ),
        (
            "SERVER_PROTOCOL".into(),
            format!("{:?}", parts.version),
        ),
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(), "askr".into()),
        ("SERVER_NAME".into(), host.clone()),
        ("SERVER_PORT".into(), server_port.to_string()),
        ("SERVER_ADDR".into(), "127.0.0.1".into()),
        ("HTTP_HOST".into(), host),
        ("REMOTE_ADDR".into(), peer.ip().to_string()),
        ("REMOTE_PORT".into(), peer.port().to_string()),
        ("REQUEST_TIME".into(), now_secs().to_string()),
    ];

    if https {
        server_vars.push(("HTTPS".into(), "on".into()));
    }
    if let Some(ct) = &content_type {
        server_vars.push(("CONTENT_TYPE".into(), ct.clone()));
    }
    if !body.is_empty() {
        server_vars.push(("CONTENT_LENGTH".into(), body.len().to_string()));
    }

    // All request headers become HTTP_* (dashes -> underscores, upper-cased).
    for (name, value) in parts.headers.iter() {
        let key = name.as_str();
        if key.eq_ignore_ascii_case("content-type") || key.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if let Ok(v) = value.to_str() {
            let upper = key.to_ascii_uppercase().replace('-', "_");
            server_vars.push((format!("HTTP_{upper}"), v.to_string()));
        }
    }

    Request {
        script_filename: script.to_string_lossy().into_owned(),
        method,
        query_string: query,
        content_type,
        cookie,
        body,
        server_vars,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
