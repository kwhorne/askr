//! Edge Side Includes: expanding `<esi:include>` in a PHP response, each fragment
//! through the same handler (and the same cache) as a request of its own.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use hyper::Method;

use crate::php::Reply;
use crate::{cgi, rcache};

use super::cache_policy::{cache_rule_for, maybe_store, response_cache_key};
use super::{ratelimit_check, Runtime};

/// How many passes of ESI expansion to run — i.e. how deeply fragments may nest.
pub(super) const ESI_MAX_PASSES: usize = 3;

/// Total fragment fetches allowed per request, to bound a page (or a loop) that
/// asks for hundreds of includes.
pub(super) const ESI_MAX_INCLUDES: usize = 32;

/// Did the app opt this response into ESI processing (`Askr-ESI: on`)?
pub(super) fn esi_requested(headers: &[(String, String)]) -> bool {
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
pub(super) async fn esi_expand(
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
pub(super) async fn esi_fragment(
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
        &cgi::Context {
            docroot,
            script: &script,
            script_name: &script_name,
            peer,
            https: config.https,
            server_port: config.listen.port(),
            trusted_proxies: &config.trusted_proxies,
        },
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
                    &parts.headers,
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
