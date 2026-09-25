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

/// Strip a trailing `:port` from an authority, leaving an IPv6 literal intact.
///
/// `authority.split(':').next()` turned `[::1]:8080` into `"["`, and that string became
/// `SERVER_NAME`, the virtual-host routing key and a field of the response-cache key.
/// Only a client addressing the server by IPv6 literal reaches it, which is why it
/// survived: every test client in this repo uses a name or an IPv4 address.
///
/// Brackets are kept — `[::1]` is the form an authority uses, and a bare `::1` would be
/// ambiguous with a host:port pair.
pub fn host_without_port(authority: &str) -> &str {
    let a = authority.trim();
    // Bracketed IPv6 literal: any port sits after the closing bracket.
    if let Some(end) = a.rfind(']') {
        return &a[..=end];
    }
    match a.rfind(':') {
        // More than one colon and no brackets: an unbracketed IPv6 literal, not a
        // host:port pair. Leave it whole rather than truncate it.
        Some(i) if a[..i].contains(':') => a,
        Some(i) if a[i + 1..].bytes().all(|b| b.is_ascii_digit()) => &a[..i],
        _ => a,
    }
}

/// Say, once per process, that an X-Forwarded-For was removed.
///
/// Removing it is right — an unvouched header is not evidence — but it can break a
/// deployment that put its proxy trust in the application (Laravel's `TrustProxies`)
/// instead of in Askr: that application read the header itself, and now does not get
/// it. That break would otherwise be silent, which is the thing to avoid. Once is
/// enough to be seen and not so often as to be noise under a flood of forged headers.
fn warn_untrusted_forwarded_for(peer: SocketAddr) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            peer = %peer.ip(),
            "X-Forwarded-For arrived from a peer that is not in [server] trusted_proxies, \
             so it was removed before reaching PHP and REMOTE_ADDR is the peer. If that \
             peer is your reverse proxy, add it to trusted_proxies — the application will \
             then see the forwarded client. (Logged once per process.)"
        );
    }
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
    trusted_proxies: &[crate::server::Cidr],
) -> Request {
    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(|q| q.to_string()).unwrap_or_default();
    let request_uri = match parts.uri.path_and_query() {
        Some(pq) => pq.as_str().to_string(),
        None => path.clone(),
    };

    // The client, not the TCP peer, when the peer is a proxy the operator has vouched
    // for. Behind nginx the peer is the proxy, so an application reading REMOTE_ADDR —
    // an IP allowlist that waives 2FA for known addresses, say — saw the proxy and
    // treated every visitor as unknown. Askr already resolved this for its own rate
    // limiter and simply never told PHP, so the two disagreed about who the client was.
    //
    // Same function as the rate limiter uses, deliberately: one definition means they
    // cannot drift apart. With no trusted proxies configured the header is ignored
    // entirely and this is the peer, because believing an unvouched X-Forwarded-For
    // would let any client claim any address.
    //
    // This is what nginx does with `real_ip` and Apache with `mod_remoteip`, and what
    // nginx + php-fpm does by passing `fastcgi_param REMOTE_ADDR $remote_addr`.
    let client = crate::server::client_ip_from(&parts.headers, peer, trusted_proxies);
    let peer_trusted = crate::server::peer_is_trusted(peer.ip(), trusted_proxies);
    let mut saw_forwarded_for = false;

    // The raw authority (may include a port) for HTTP_HOST, and the port-stripped form
    // for SERVER_NAME/vhost routing. Kept separate so HTTP_HOST stays conventional.
    let raw_host =
        effective_host(&parts.headers, &parts.uri).unwrap_or_else(|| "localhost".to_string());
    let host = host_without_port(&raw_host).to_string();

    let content_type = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    // Every Cookie field, not the first. HTTP/2 lets a client send one `cookie`
    // field per cookie (RFC 9113 §8.2.3) — Chrome and Firefox do — and hyper hands
    // them over as separate values; the server is required to join them with "; ".
    // `.get()` took the first, so over h2 a browser sending `laravel_session` and
    // `XSRF-TOKEN` as two fields reached PHP with one of them missing: a 419 on the
    // form, or an anonymous request from a logged-in user, depending on which was
    // first that time. Every test client in this repository speaks HTTP/1.1, which
    // is how it stayed.
    let cookie = {
        let joined = parts
            .headers
            .get_all(hyper::header::COOKIE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .collect::<Vec<_>>()
            .join("; ");
        (!joined.is_empty()).then_some(joined)
    };

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
        ("HTTP_HOST".into(), raw_host),
        ("REMOTE_ADDR".into(), client.to_string()),
        // The peer's port, not the client's: a forwarded chain carries addresses, not
        // ports, so there is nothing truer to put here. nginx's real_ip module rewrites
        // the address and leaves the port alone for the same reason.
        ("REMOTE_PORT".into(), peer.port().to_string()),
        // The TCP peer, always — the proxy when there is one. Kept because once
        // REMOTE_ADDR is the forwarded client, the address Askr actually accepted the
        // connection from has nowhere else to appear, and that is the one an operator
        // needs when a forwarding chain is misconfigured.
        ("ASKR_PEER_ADDR".into(), peer.ip().to_string()),
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
        // HTTP_HOST is authoritative from effective_host above (which also covers the
        // HTTP/2 case, where the host is the URI authority and not a header). Letting
        // the Host header through here would add a second HTTP_HOST that the merge
        // below joins into `host, host:port` — invalid, and a framework 400.
        if key.eq_ignore_ascii_case("host") {
            continue;
        }
        // Client-address headers: Askr has already decided who the client is, and PHP
        // must not be able to derive a different answer from the raw header.
        //
        // REMOTE_ADDR alone was not enough. An application with `trustProxies(at: '*')`
        // — common in containers — reads X-Forwarded-For itself, trusts every hop, and
        // takes the leftmost entry. Behind a proxy that appends, the chain is
        // `forged, client`, so the app lands on `forged` whatever REMOTE_ADDR says; with
        // no proxy at all, a client simply sends the header and is believed. Either way
        // an IP allowlist that waives 2FA for known addresses can be walked past.
        //
        // So the header is rewritten the way Apache's mod_remoteip does: from a peer in
        // `trusted_proxies` it is collapsed to the client Askr resolved (below, after the
        // loop), and from any other peer it is removed, because an unvouched
        // X-Forwarded-For is not evidence of anything. X-Real-IP is the same claim in a
        // different header and is removed from an untrusted peer for the same reason;
        // from a trusted one it passes, since it is that proxy's own statement.
        if key.eq_ignore_ascii_case("x-forwarded-for") {
            if peer_trusted {
                saw_forwarded_for = true;
            } else {
                warn_untrusted_forwarded_for(peer);
            }
            continue;
        }
        // Every other forwarding claim from an untrusted peer goes too, not only the
        // address. `X-Forwarded-Host` from a peer nobody vouched for is how a password
        // reset link gets pointed at an attacker's domain by any application that trusts
        // all proxies, and `-Proto`/`-Port`/`-Prefix` and RFC 7239 `Forwarded` are the
        // same kind of statement. Without this, the natural advice — "Askr has cleaned
        // X-Forwarded-For, so `trustProxies(at: '*')` is safe now" — would have opened
        // that hole instead of closing one. From a trusted proxy they pass unchanged:
        // they are that proxy's own statements.
        if !peer_trusted
            && (key.eq_ignore_ascii_case("x-real-ip")
                || key.eq_ignore_ascii_case("forwarded")
                || key.len() > "x-forwarded-".len()
                    && key[.."x-forwarded-".len()].eq_ignore_ascii_case("x-forwarded-"))
        {
            continue;
        }
        // Underscores collapse into the same $_SERVER key as dashes, so
        // `X_Forwarded_For:` and `X-Forwarded-For:` both become
        // HTTP_X_FORWARDED_FOR — and which one wins depends on header iteration
        // order. Anything that filters the dashed spelling (a WAF, a proxy that
        // rewrites X-Forwarded-For, Laravel's TrustProxies reading $_SERVER) is
        // then bypassed by sending the underscored one. This is why nginx ships
        // `underscores_in_headers off` as its default, and it is the same default
        // here: an underscore in a header name is dropped rather than merged.
        if key.contains('_') {
            continue;
        }
        if let Ok(v) = value.to_str() {
            let upper = key.to_ascii_uppercase().replace('-', "_");
            let name = format!("HTTP_{upper}");
            // A repeated field is one value, joined: "; " for Cookie (RFC 9113
            // §8.2.3), ", " for everything else (RFC 9110 §5.3). Pushing each
            // occurrence separately produced duplicate keys, and the PHP array built
            // from this list kept whichever came last.
            if let Some(existing) = server_vars.iter_mut().find(|(k, _)| *k == name) {
                existing
                    .1
                    .push_str(if name == "HTTP_COOKIE" { "; " } else { ", " });
                existing.1.push_str(v);
            } else {
                server_vars.push((name, v.to_string()));
            }
        }
    }
    // One value, and it is the one REMOTE_ADDR carries. A chain would hand the leftmost
    // — the part any upstream hop can forge — back to any app that trusts every proxy.
    if saw_forwarded_for {
        server_vars.push(("HTTP_X_FORWARDED_FOR".into(), client.to_string()));
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
        namespace: crate::ns::for_docroot(docroot),
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

    /// `X_Forwarded_For` used to collapse into the same $_SERVER key as
    /// `X-Forwarded-For`, and which one PHP saw depended on header iteration order.
    /// Anything filtering the dashed spelling — a WAF, a proxy that rewrites the
    /// header, Laravel's TrustProxies reading $_SERVER — was bypassed by sending the
    /// underscored one. nginx ships `underscores_in_headers off` for this reason.
    /// `authority.split(':').next()` turned `[::1]:8080` into `"["`, and that landed
    /// in SERVER_NAME, the virtual-host routing key and the response-cache key.
    /// HTTP/2 clients may send one `cookie` field per cookie, and browsers do. The
    /// first-only read meant PHP saw one cookie of two — sessions and CSRF tokens lost
    /// at random over the protocol browsers actually use.
    #[test]
    fn split_cookie_fields_are_joined_the_way_rfc_9113_requires() {
        let parts = parts_with(&[
            ("Cookie", "laravel_session=abc"),
            ("Cookie", "XSRF-TOKEN=def"),
            ("Accept", "text/html"),
            ("Accept", "application/json"),
        ]);
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
            &[],
        );
        assert_eq!(
            req.cookie.as_deref(),
            Some("laravel_session=abc; XSRF-TOKEN=def"),
            "both cookies, joined with a semicolon"
        );
        let var = |name: &str| -> Vec<&str> {
            req.server_vars
                .iter()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .collect()
        };
        assert_eq!(
            var("HTTP_COOKIE"),
            vec!["laravel_session=abc; XSRF-TOKEN=def"],
            "exactly one HTTP_COOKIE, holding both"
        );
        // Any other repeated field joins with a comma, per RFC 9110.
        assert_eq!(var("HTTP_ACCEPT"), vec!["text/html, application/json"]);
    }

    /// 1.5.1 taught the header loop to join repeated fields, and HTTP_HOST — already
    /// set from `effective_host` — was re-added from the `Host:` header and joined
    /// with it: `example.com, example.com:8080`. Symfony rejects a comma in the host
    /// (`SuspiciousOperationException`), so every HTTP/1.x request to a Laravel or
    /// Symfony app answered 400 while HTTP/2 kept working, because over h2 hyper puts
    /// the authority in the URI and there is no header to duplicate. Before the join
    /// the loop pushed a second HTTP_HOST and PHP's last-wins kept the valid one, so
    /// this was invisible until the two changes met.
    #[test]
    fn host_reaches_php_once_and_without_a_comma() {
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40000);
        let build = |parts: &Parts| {
            let req = build_request(
                parts,
                Vec::new(),
                Path::new("/srv"),
                Path::new("/srv/index.php"),
                "/index.php",
                peer,
                false,
                8080,
                &[],
            );
            let one = |name: &str| -> String {
                let all: Vec<&str> = req
                    .server_vars
                    .iter()
                    .filter(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
                    .collect();
                assert_eq!(all.len(), 1, "exactly one {name}, got {all:?}");
                all[0].to_string()
            };
            (one("HTTP_HOST"), one("SERVER_NAME"))
        };

        // HTTP/1.x: origin-form target, authority in the Host header.
        let h1 = build(&parts_with(&[("Host", "example.com:8080")]));
        assert_eq!(h1.0, "example.com:8080", "HTTP_HOST keeps the port");
        assert_eq!(h1.1, "example.com", "SERVER_NAME drops it");

        // HTTP/2: hyper hands the authority over in the URI, with no Host header.
        let h2 = hyper::Request::builder()
            .method("GET")
            .uri("http://example.com:8080/")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        assert_eq!(build(&h2), h1, "the two protocols agree");
    }

    /// `REMOTE_ADDR` must be the client, not the proxy, when the peer is vouched for.
    ///
    /// Behind nginx the TCP peer is the proxy, and Askr handed that to PHP. An
    /// application reading `REMOTE_ADDR` — an IP allowlist that waives 2FA for known
    /// addresses — therefore saw the proxy on every request and treated every visitor as
    /// unknown. The same deployment worked the moment nginx was taken out of the path.
    ///
    /// Askr already resolved the real client for its own rate limiter and simply never
    /// told PHP, so the two disagreed about who was calling. Both now go through one
    /// function, which is the only way they cannot drift apart again.
    ///
    /// The cases below are the ones a forwarding chain actually produces.
    #[test]
    fn remote_addr_is_the_client_through_a_trusted_proxy() {
        let trusted: Vec<crate::server::Cidr> = vec![
            crate::server::parse_cidr("172.18.0.0/16").unwrap(),
            crate::server::parse_cidr("2001:db8:ffff::/48").unwrap(),
        ];
        let proxy: SocketAddr = "172.18.0.1:36748".parse().unwrap();
        let direct: SocketAddr = "92.220.212.22:41000".parse().unwrap();

        let remote = |peer: SocketAddr, xff: Option<&str>, trusted: &[crate::server::Cidr]| {
            let mut headers: Vec<(&str, &str)> = vec![("Host", "example.test")];
            if let Some(v) = xff {
                headers.push(("X-Forwarded-For", v));
            }
            let parts = parts_with(&headers);
            let req = build_request(
                &parts,
                Vec::new(),
                Path::new("/srv"),
                Path::new("/srv/index.php"),
                "/index.php",
                peer,
                false,
                80,
                trusted,
            );
            let get = |k: &str| {
                req.server_vars
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            (get("REMOTE_ADDR"), get("ASKR_PEER_ADDR"))
        };

        // A single hop: the proxy forwarded the client it saw.
        assert_eq!(
            remote(proxy, Some("92.220.212.22"), &trusted).0,
            "92.220.212.22"
        );
        // A chain: the rightmost entry is the one the trusted proxy itself observed.
        // Anything to its left was supplied by the previous hop and may be forged.
        assert_eq!(
            remote(proxy, Some("198.51.100.9, 92.220.212.22"), &trusted).0,
            "92.220.212.22"
        );
        // Trusted addresses at the right are our own proxies; keep walking past them.
        assert_eq!(
            remote(proxy, Some("92.220.212.22, 172.18.0.1"), &trusted).0,
            "92.220.212.22"
        );
        // Nothing forwarded: the peer is all there is.
        assert_eq!(remote(proxy, None, &trusted).0, "172.18.0.1");
        // An untrusted peer's header is not evidence of anything. This is the case that
        // matters most: without it, any client could claim any address by sending one.
        assert_eq!(
            remote(direct, Some("198.51.100.9"), &trusted).0,
            "92.220.212.22"
        );
        // No trusted proxies configured at all — the header is ignored entirely.
        assert_eq!(remote(proxy, Some("198.51.100.9"), &[]).0, "172.18.0.1");
        // IPv6, bare and with a port, since a forwarded entry may carry either.
        let v6: SocketAddr = "[2001:db8:ffff::1]:443".parse().unwrap();
        assert_eq!(remote(v6, Some("2001:db8::1"), &trusted).0, "2001:db8::1");
        assert_eq!(
            remote(v6, Some("[2001:db8::1]:443"), &trusted).0,
            "2001:db8::1"
        );

        // And the peer is still reachable, because once REMOTE_ADDR is the forwarded
        // client the address Askr accepted the connection from has nowhere else to go.
        let (addr, peer_addr) = remote(proxy, Some("92.220.212.22"), &trusted);
        assert_eq!(addr, "92.220.212.22");
        assert_eq!(peer_addr, "172.18.0.1");
    }

    /// PHP must not be able to derive a different client than REMOTE_ADDR from the raw
    /// forwarding headers.
    ///
    /// Fixing REMOTE_ADDR alone left a hole: an app with `trustProxies(at: '*')` reads
    /// X-Forwarded-For itself, trusts every hop and takes the leftmost — so behind a
    /// proxy that appends (`forged, client`) it lands on `forged`, and with no proxy at
    /// all a client just sends the header. An allowlist that waives 2FA can be walked
    /// past either way. The header is now rewritten as mod_remoteip does.
    #[test]
    fn forwarding_headers_cannot_contradict_remote_addr() {
        let trusted = vec![crate::server::parse_cidr("172.18.0.0/16").unwrap()];
        let proxy: SocketAddr = "172.18.0.1:36748".parse().unwrap();
        let direct: SocketAddr = "92.220.212.22:41000".parse().unwrap();
        let vars = |peer: SocketAddr, headers: &[(&str, &str)], t: &[crate::server::Cidr]| {
            let parts = parts_with(headers);
            build_request(
                &parts,
                Vec::new(),
                Path::new("/srv"),
                Path::new("/srv/index.php"),
                "/index.php",
                peer,
                false,
                80,
                t,
            )
            .server_vars
        };
        let get = |v: &Vec<(String, String)>, k: &str| -> Vec<String> {
            v.iter()
                .filter(|(n, _)| n == k)
                .map(|(_, x)| x.clone())
                .collect()
        };

        // Behind a trusted proxy that appends: the chain collapses to the client, so the
        // forged leftmost entry is gone before any application can prefer it.
        let v = vars(
            proxy,
            &[("X-Forwarded-For", "203.0.113.66, 92.220.212.22")],
            &trusted,
        );
        assert_eq!(get(&v, "HTTP_X_FORWARDED_FOR"), vec!["92.220.212.22"]);
        assert_eq!(get(&v, "REMOTE_ADDR"), vec!["92.220.212.22"]);

        // No proxy: a client that sends its own X-Forwarded-For is not believed, and the
        // header does not reach PHP, where a trust-everything app would believe it.
        let v = vars(
            direct,
            &[
                ("X-Forwarded-For", "203.0.113.66"),
                ("X-Real-IP", "203.0.113.66"),
            ],
            &trusted,
        );
        assert!(
            get(&v, "HTTP_X_FORWARDED_FOR").is_empty(),
            "forged XFF removed"
        );
        assert!(
            get(&v, "HTTP_X_REAL_IP").is_empty(),
            "forged X-Real-IP removed"
        );
        assert_eq!(get(&v, "REMOTE_ADDR"), vec!["92.220.212.22"]);

        // Host poisoning: a direct client claiming a forwarded host. An app trusting all
        // proxies would build a password reset link to it.
        let v = vars(
            direct,
            &[
                ("X-Forwarded-Host", "evil.example"),
                ("X-Forwarded-Proto", "https"),
                ("X-Forwarded-Port", "443"),
                ("X-Forwarded-Prefix", "/x"),
                ("Forwarded", "for=203.0.113.66;host=evil.example"),
            ],
            &trusted,
        );
        for k in [
            "HTTP_X_FORWARDED_HOST",
            "HTTP_X_FORWARDED_PROTO",
            "HTTP_X_FORWARDED_PORT",
            "HTTP_X_FORWARDED_PREFIX",
            "HTTP_FORWARDED",
        ] {
            assert!(
                get(&v, k).is_empty(),
                "{k} from an untrusted peer must not reach PHP"
            );
        }
        // From the proxy, they are the proxy's own statements and pass.
        let v = vars(
            proxy,
            &[
                ("X-Forwarded-Host", "example.test"),
                ("X-Forwarded-Proto", "https"),
            ],
            &trusted,
        );
        assert_eq!(get(&v, "HTTP_X_FORWARDED_HOST"), vec!["example.test"]);
        assert_eq!(get(&v, "HTTP_X_FORWARDED_PROTO"), vec!["https"]);

        // X-Real-IP from a trusted proxy is that proxy's own statement, and passes.
        let v = vars(proxy, &[("X-Real-IP", "92.220.212.22")], &trusted);
        assert_eq!(get(&v, "HTTP_X_REAL_IP"), vec!["92.220.212.22"]);

        // A request with no forwarding header gains none.
        let v = vars(proxy, &[("Host", "example.test")], &trusted);
        assert!(get(&v, "HTTP_X_FORWARDED_FOR").is_empty());
    }

    #[test]
    fn strips_the_port_without_shredding_an_ipv6_literal() {
        assert_eq!(host_without_port("example.com:8080"), "example.com");
        assert_eq!(host_without_port("example.com"), "example.com");
        assert_eq!(host_without_port("[::1]:8080"), "[::1]");
        assert_eq!(host_without_port("[::1]"), "[::1]");
        assert_eq!(host_without_port("[2001:db8::1]:443"), "[2001:db8::1]");
        // Unbracketed and multi-colon: a bare IPv6 literal, not host:port.
        assert_eq!(host_without_port("::1"), "::1");
        // Not a port, so not stripped.
        assert_eq!(
            host_without_port("example.com:notaport"),
            "example.com:notaport"
        );
        assert_eq!(host_without_port("  example.com:80  "), "example.com");
    }

    #[test]
    fn drops_underscored_header_spellings() {
        let parts = parts_with(&[
            ("X_Forwarded_For", "9.9.9.9"),
            ("X-Forwarded-For", "10.0.0.1"),
            ("X-Foo", "bar"),
        ]);
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
            &[crate::server::parse_cidr("127.0.0.1").unwrap()],
        );
        let xff: Vec<&str> = req
            .server_vars
            .iter()
            .filter(|(k, _)| k == "HTTP_X_FORWARDED_FOR")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            xff,
            vec!["10.0.0.1"],
            "only the dashed spelling may reach PHP, and exactly once"
        );
        assert!(req
            .server_vars
            .iter()
            .any(|(k, v)| k == "HTTP_X_FOO" && v == "bar"));
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
            &[],
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
