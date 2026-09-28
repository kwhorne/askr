//! The HTTP side of the response cache: what may be cached, under which key, for how
//! long, how `Vary` splits it, stale-while-revalidate and stale-if-error, and the purge
//! API. The storage itself is [`crate::rcache`].

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use hyper::{Method, Request, Response, StatusCode};

use crate::php::Reply;
use crate::{cgi, rcache};

use super::esi::esi_requested;
use super::{full, text, Config, ResBody, Runtime};

/// Unix seconds until which the local PHP backend is treated as unhealthy
/// ("saint mode"). Per worker process: each one notices failures on its own, and
/// no shared-memory write is needed on the failure path.
pub(super) static SAINT_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Note that PHP just failed: hold the backend in saint mode for `secs` so the
/// next requests prefer a stale fallback over hammering a dying app (0 = off).
pub(super) fn saint_mark(secs: u64) {
    if secs > 0 {
        SAINT_UNTIL.store(unix_secs() + secs, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn saint_active() -> bool {
    SAINT_UNTIL.load(std::sync::atomic::Ordering::Relaxed) > unix_secs()
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
pub(super) fn carries_identity<B>(req: &Request<B>, ignore_cookies: &[String]) -> bool {
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
pub(super) fn cache_rule_for<'a>(
    path: &str,
    rules: &'a [crate::config::CacheRule],
) -> Option<&'a crate::config::CacheRule> {
    rules.iter().find(|r| rcache::glob_match(&r.path, path))
}

/// Constant-time bearer check against `ASKR_ADMIN_TOKEN`.
pub(super) fn control_token_ok<B>(req: &Request<B>) -> Option<bool> {
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
pub(super) fn invalidate_request<B>(
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

pub(super) fn json_response(body: &str) -> Response<ResBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(full(Bytes::from(body.to_owned())))
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"))
}

/// Serve a held entry as a **failure fallback** (`stale-if-error`): the origin
/// returned 5xx or the handler errored, and stale content beats an error page.
pub(super) fn stale_error_fallback(
    key: &[u8],
    headers: &hyper::HeaderMap,
) -> Option<Response<ResBody>> {
    let (c, _) = resolve_cached(key, headers, rcache::stale_on_error)?;
    let mut resp = cached_response(c);
    resp.headers_mut().insert(
        hyper::header::HeaderName::from_static("x-askr-cache"),
        hyper::header::HeaderValue::from_static("STALE-ERROR"),
    );
    Some(resp)
}

/// Match a cookie/parameter name against a pattern with an optional trailing
/// `*` wildcard (`utm_*`). Case-insensitive.
pub(super) fn name_matches(pat: &str, name: &str) -> bool {
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
pub(super) fn cookies_ignorable(header: &str, ignore: &[String]) -> bool {
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
pub(super) fn normalize_query(query: &str, strip: &[String]) -> String {
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
pub(super) fn ua_class(ua: &str) -> &'static str {
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
pub(super) fn response_cache_key<B>(req: &Request<B>, host: &str, cfg: &Config) -> Vec<u8> {
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
pub(super) fn maybe_store(
    key: &[u8],
    resp: &askr_php::Response,
    accept_encoding: &str,
    vary_ua: bool,
    rule: Option<&crate::config::CacheRule>,
    app: &crate::ns::App,
    // The request that produced `resp`, for the values of the headers it varies on.
    request_headers: &hyper::HeaderMap,
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
    // A `Vary` the key already covers needs nothing. `Vary: *` means "never the same
    // twice" and is refused. Anything else is stored as a *variant*: an index under the
    // primary key naming the headers, and the response itself under a key that carries
    // this request's values for them. Until now such a response was simply not cached.
    let app_vary: Option<String> = header_value(&resp.headers, "vary")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && !vary_is_covered_by_key(v, vary_ua));
    if app_vary
        .as_deref()
        .is_some_and(|v| v.split(',').any(|t| t.trim() == "*"))
    {
        return;
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
    if let Some(names) = &app_vary {
        vary.extend(names.split(',').map(str::trim).filter(|n| !n.is_empty()));
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
            let mut k = Vec::with_capacity(crate::ns::PREFIX_LEN + t.len());
            k.extend_from_slice(app.as_str().as_bytes());
            k.push(crate::ns::SEP);
            k.extend_from_slice(&t);
            k
        })
        .collect();
    match &app_vary {
        None => {
            rcache::store(
                key,
                resp.status,
                &stored,
                &body,
                ttl,
                swr,
                sie,
                &tags,
                Some(app),
            );
        }
        Some(names) => {
            // Same lifetimes and tags for the index as for the entry, so it lives as
            // long as any variant can and `forget_tag` takes it down with them.
            let index_headers = [(VARY_INDEX_HEADER.to_string(), names.clone())];
            rcache::store(key, 0, &index_headers, b"", ttl, swr, sie, &tags, Some(app));
            let sk = secondary_key(key, names, request_headers);
            rcache::store(
                &sk,
                resp.status,
                &stored,
                &body,
                ttl,
                swr,
                sie,
                &tags,
                Some(app),
            );
        }
    }
}

/// Which scheme a request arrived on. Determined the same way the redirect engine
/// determines it, so the cache key and the http→https redirect cannot disagree.
pub(super) fn request_scheme<B>(req: &Request<B>, https: bool) -> &'static str {
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

/// The header a *vary index* entry carries: the names the origin said its response
/// varies on. An index is stored under the primary key with status 0 and is never
/// served; it tells a lookup which request headers select the real entry.
pub(super) const VARY_INDEX_HEADER: &str = "askr-vary-index";

pub(super) fn is_vary_index(c: &rcache::Cached) -> bool {
    c.status == 0 && header_value(&c.headers, VARY_INDEX_HEADER).is_some()
}

/// The request-side half of a `Vary`: this request's value for every named header,
/// in sorted-name order, as a key suffix. A header the request lacks contributes an
/// empty value, so "no Accept-Language" is its own variant rather than a wildcard.
/// Repeated fields join with ", " — one value, per RFC 9110 §5.3.
pub(super) fn vary_suffix(names: &str, headers: &hyper::HeaderMap) -> Vec<u8> {
    let mut list: Vec<String> = names
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    list.sort();
    list.dedup();
    let mut out = Vec::new();
    for name in &list {
        out.extend_from_slice(name.as_bytes());
        out.push(b'=');
        let joined = headers
            .get_all(name.as_str())
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(", ");
        out.extend_from_slice(joined.as_bytes());
        out.push(crate::ns::SEP);
    }
    out
}

/// Where the variant of `primary` selected by this request's headers lives.
///
/// The primary key keeps its first three fields, so `PURGE`/`BAN` — which match on
/// method, host and path — drop the index and every variant together.
pub(super) fn secondary_key(primary: &[u8], names: &str, headers: &hyper::HeaderMap) -> Vec<u8> {
    let mut k = primary.to_vec();
    k.extend_from_slice(b"\0vary\0");
    k.extend_from_slice(&vary_suffix(names, headers));
    k
}

/// Two-level lookup: the entry at `primary`, or — when that is a vary index — the
/// variant this request's headers select. Returns whether a variant was involved,
/// because a variant is not refreshed in the background (see the call site).
///
/// This is the design the cache refused to do until now. The primary key cannot be
/// computed from the request alone once the response gets a say in it — the request
/// does not know which of its headers matter until an earlier response has said so.
/// The index is that earlier response speaking: it is written when a response with an
/// uncovered `Vary` is stored, and read here before the request commits to a key.
///
/// `fetch` is `rcache::get`, `peek` or `stale_on_error`. With `get`, an index hit
/// counts as a hit before the variant is looked up, so the hit ratio flatters varied
/// pages slightly; a metric skew, chosen over a second read on every unvaried hit.
pub(super) fn resolve_cached(
    primary: &[u8],
    headers: &hyper::HeaderMap,
    fetch: impl Fn(&[u8]) -> Option<rcache::Cached>,
) -> Option<(rcache::Cached, bool)> {
    let first = fetch(primary)?;
    if !is_vary_index(&first) {
        return Some((first, false));
    }
    let names = header_value(&first.headers, VARY_INDEX_HEADER)?.to_string();
    let entry = fetch(&secondary_key(primary, &names, headers))?;
    if is_vary_index(&entry) {
        return None; // an index must never be served as a response
    }
    Some((entry, true))
}

/// Can the cache key represent every dimension this `Vary` names?
///
/// The key varies on negotiated encoding, and on device class when
/// `vary_user_agent` is on. Anything else — `Accept-Language`, a custom header, `*` —
/// it cannot express, so the entry would be served to clients it was not rendered
/// for.
pub(super) fn vary_is_covered_by_key(vary: &str, vary_ua: bool) -> bool {
    vary.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .all(|t| {
            t.eq_ignore_ascii_case("accept-encoding")
                || (vary_ua && t.eq_ignore_ascii_case("user-agent"))
        })
}

/// First value of `name` in a PHP response's header list, case-insensitively.
pub(super) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub(super) fn storable_header(name: &str) -> bool {
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
pub(super) fn directive_secs(v: &str, name: &str) -> u64 {
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
pub(super) fn parse_cache_directive(v: &str) -> Option<(u64, u64, u64, Vec<Vec<u8>>)> {
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
pub(super) fn spawn_swr_refresh<B>(
    rt: &Arc<Runtime>,
    key: &[u8],
    req: &Request<B>,
    peer: SocketAddr,
) {
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
pub(super) async fn refresh_entry(
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
    // Only a buffered response is cacheable; a streaming one is skipped.
    if let Ok(Reply::Buffered(resp)) = rt.php.handle(request).await {
        maybe_store(
            &key,
            &resp,
            &accept_encoding,
            rt.config.cache_vary_user_agent,
            cache_rule_for(&uri_path, &rt.config.cache_rules),
            &crate::ns::for_docroot(docroot),
            &parts.headers,
        );
    }
    rcache::end(&key);
}

/// Build a hyper response from a cached entry. The body is already in its final
/// form (compressed at store time, per the encoding baked into the cache key), so
/// this serves the stored bytes and headers verbatim — no per-HIT compression.
pub(super) fn cached_response(c: rcache::Cached) -> Response<ResBody> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The two-level lookup, over a fake store. The primary key alone cannot say which
    /// variant a request wants; the index says which headers decide, and the request's
    /// values for them select the entry.
    #[test]
    fn a_vary_index_routes_each_request_to_its_own_variant() {
        use std::collections::HashMap;
        let cached = |status: u16, headers: &[(&str, &str)], body: &str| rcache::Cached {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
            stale: false,
            error_only: false,
        };
        let req = |lang: Option<&str>| {
            let mut b = hyper::Request::builder().uri("/");
            if let Some(l) = lang {
                b = b.header("accept-language", l);
            }
            b.body(()).unwrap()
        };
        let primary = b"GET\0site\0/\0id\0\0http".to_vec();

        let mut store: HashMap<Vec<u8>, rcache::Cached> = HashMap::new();
        store.insert(
            primary.clone(),
            cached(0, &[(VARY_INDEX_HEADER, "Accept-Language")], ""),
        );
        for lang in ["nb", "en"] {
            let sk = secondary_key(&primary, "Accept-Language", req(Some(lang)).headers());
            store.insert(sk, cached(200, &[("content-type", "text/html")], lang));
        }
        let fetch = |k: &[u8]| store.get(k).cloned();

        let (nb, varied) = resolve_cached(&primary, req(Some("nb")).headers(), fetch).unwrap();
        assert_eq!(nb.body, b"nb");
        assert!(varied);
        let (en, _) = resolve_cached(&primary, req(Some("en")).headers(), fetch).unwrap();
        assert_eq!(en.body, b"en");
        // A value nobody has rendered yet is a miss, not somebody else's page.
        assert!(resolve_cached(&primary, req(Some("de")).headers(), fetch).is_none());
        assert!(resolve_cached(&primary, req(None).headers(), fetch).is_none());

        // An entry stored directly at the primary is returned as before.
        let plain = b"GET\0site\0/plain\0id\0\0http".to_vec();
        store.insert(plain.clone(), cached(200, &[], "plain"));
        let fetch = |k: &[u8]| store.get(k).cloned();
        let (p, varied) = resolve_cached(&plain, req(None).headers(), fetch).unwrap();
        assert_eq!(p.body, b"plain");
        assert!(!varied);
    }

    /// The suffix is what makes two requests the same variant or not: header names
    /// are case-insensitive and order-insensitive, repeated fields are one value, and
    /// a missing header is a variant in its own right.
    #[test]
    fn the_variant_suffix_is_canonical() {
        let h = |pairs: &[(&str, &str)]| {
            let mut m = hyper::HeaderMap::new();
            for (k, v) in pairs {
                m.append(
                    hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                );
            }
            m
        };
        let a = vary_suffix(
            "Accept-Language, X-Tenant",
            &h(&[("accept-language", "nb"), ("x-tenant", "acme")]),
        );
        let b = vary_suffix(
            "x-tenant,ACCEPT-LANGUAGE",
            &h(&[("x-tenant", "acme"), ("accept-language", "nb")]),
        );
        assert_eq!(a, b, "name case and order do not make a new variant");
        assert_ne!(
            a,
            vary_suffix(
                "Accept-Language, X-Tenant",
                &h(&[("accept-language", "en"), ("x-tenant", "acme")])
            )
        );
        assert_ne!(
            a,
            vary_suffix("Accept-Language, X-Tenant", &h(&[("x-tenant", "acme")])),
            "missing is its own variant"
        );
        assert_eq!(
            vary_suffix(
                "Accept-Language",
                &h(&[("accept-language", "nb"), ("accept-language", "en")])
            ),
            vary_suffix("Accept-Language", &h(&[("accept-language", "nb, en")])),
            "repeated fields are one value"
        );
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
