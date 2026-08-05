//! Map an HTTP request to the CGI-style `$_SERVER` environment PHP expects.
//! This is the same variable convention FastCGI uses, so it mirrors grove's
//! `build_fcgi_params` — but feeds the in-process interpreter instead.

use std::net::SocketAddr;
use std::path::Path;

use hyper::http::request::Parts;

use askr_php::Request;

/// The host this request was addressed to, from either HTTP version.
///
/// **HTTP/2 and HTTP/3 have no `Host` header.** The authority arrives in the `:authority`
/// pseudo-header, which hyper exposes on the URI. Reading only `Host` therefore found
/// nothing the moment a client negotiated h2 — which happens by default over TLS via
/// ALPN — and the fallbacks were quietly wrong in three different ways:
///
/// * `HTTP_HOST`/`SERVER_NAME` became `localhost`, so Laravel built every URL and
///   redirect as `https://localhost/…` and login flows dead-ended;
/// * virtual-host matching saw an empty host and fell through to the default site;
/// * the response-cache key had an empty host component, so two domains could share
///   entries.
///
/// None of it surfaced in testing, because every test client in this repo speaks
/// HTTP/1.1. Found on a real deployment, the first time Askr terminated TLS itself.
///
/// Returns the authority as sent (port included); callers strip the port if they need to.
pub fn effective_host(headers: &hyper::HeaderMap, uri: &hyper::Uri) -> Option<String> {
    if let Some(h) = headers
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(h.to_string());
    }
    uri.authority().map(|a| a.as_str().to_string())
}

/// Build an [`askr_php::Request`] for the front controller.
#[allow(clippy::too_many_arguments)]
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
    let query = parts.uri.query().map(|q| q.to_string()).unwrap_or_default();
    let request_uri = match parts.uri.path_and_query() {
        Some(pq) => pq.as_str().to_string(),
        None => path.clone(),
    };

    let host = effective_host(&parts.headers, &parts.uri)
        .map(|h| h.split(':').next().unwrap_or(&h).to_string())
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
        ("SERVER_PROTOCOL".into(), format!("{:?}", parts.version)),
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
        // httpoxy (CVE-2016-5385 et al.): a client-supplied `Proxy:` header must
        // never become `HTTP_PROXY`, which many HTTP clients (Guzzle, libcurl via
        // getenv) read to route outbound requests. Drop it unconditionally.
        if key.eq_ignore_ascii_case("proxy") {
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
        post_fields: Vec::new(),
        files: Vec::new(),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn parts_with(headers: &[(&str, &str)]) -> Parts {
        let mut b = hyper::Request::builder().method("GET").uri("/");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap().into_parts().0
    }

    #[test]
    fn drops_proxy_header_httpoxy() {
        let parts = parts_with(&[("Proxy", "http://evil.example"), ("X-Foo", "bar")]);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40000);
        let req = build_request(
            &parts,
            Vec::new(),
            Path::new("/srv"),
            Path::new("/srv/index.php"),
            "/index.php",
            peer,
            false,
            80,
        );
        // The httpoxy header must NOT reach PHP as HTTP_PROXY…
        assert!(!req.server_vars.iter().any(|(k, _)| k == "HTTP_PROXY"));
        // …but ordinary headers still map through.
        assert!(req
            .server_vars
            .iter()
            .any(|(k, v)| k == "HTTP_X_FOO" && v == "bar"));
    }
}

#[cfg(test)]
mod host_tests {
    use super::effective_host;

    fn h(pairs: &[(&str, &str)]) -> hyper::HeaderMap {
        let mut m = hyper::HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                hyper::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn http1_uses_the_host_header() {
        let uri: hyper::Uri = "/login".parse().unwrap();
        assert_eq!(
            effective_host(&h(&[("host", "works.example:8443")]), &uri).as_deref(),
            Some("works.example:8443")
        );
    }

    /// The regression: h2/h3 send no Host header, only `:authority`, which hyper puts on
    /// the URI. Before this, the host fell back to "localhost" (or to empty for vhost
    /// matching and cache keys) for every request over TLS, since ALPN picks h2.
    #[test]
    fn http2_falls_back_to_the_uri_authority() {
        let uri: hyper::Uri = "https://works.example/login".parse().unwrap();
        assert_eq!(
            effective_host(&hyper::HeaderMap::new(), &uri).as_deref(),
            Some("works.example")
        );
    }

    #[test]
    fn an_empty_host_header_does_not_win_over_the_authority() {
        let uri: hyper::Uri = "https://works.example/x".parse().unwrap();
        assert_eq!(
            effective_host(&h(&[("host", "  ")]), &uri).as_deref(),
            Some("works.example")
        );
    }

    #[test]
    fn neither_present_is_none_so_callers_choose_the_fallback() {
        let uri: hyper::Uri = "/x".parse().unwrap();
        assert_eq!(effective_host(&hyper::HeaderMap::new(), &uri), None);
    }
}
