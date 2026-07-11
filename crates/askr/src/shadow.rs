//! Traffic shadowing (mirroring) for safe deploy validation.
//!
//! When `--shadow-to <url>` is set, Askr mirrors a sampled fraction of *safe*
//! (GET/HEAD, cookie-less) requests to a shadow upstream — typically a staging
//! deploy running the next version — after serving the real response. It compares
//! the shadow's status + body to production and records match/mismatch/error
//! counters (visible on `/metrics`), logging any divergence. The client's
//! response and latency are never affected: the mirror is fire-and-forget on a
//! background task.
//!
//! Only idempotent, non-user-specific requests are mirrored, so a shadow deploy
//! never receives writes or one visitor's session.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;

type HttpClient = Client<HttpConnector, Full<Bytes>>;

/// A configured shadow upstream.
pub struct Shadow {
    client: HttpClient,
    base: String,
    sample: u8,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

impl Shadow {
    pub fn new(base: String, sample: u8) -> Self {
        let client: HttpClient = Client::builder(TokioExecutor::new()).build_http();
        Shadow {
            client,
            base: base.trim_end_matches('/').to_string(),
            sample: sample.clamp(1, 100),
        }
    }

    /// Should this eligible request be mirrored? Simple 1-in-N counter sampling.
    pub fn sampled(&self) -> bool {
        self.sample >= 100 || (SEQ.fetch_add(1, Ordering::Relaxed) % 100) < self.sample as u64
    }

    /// A clone of the (cheap, `Arc`-backed) client for a background task.
    pub fn clone_client(&self) -> HttpClient {
        self.client.clone()
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }
}

/// Only safe, side-effect-free, non-user-specific requests are mirrored.
pub fn eligible(method: &Method, has_cookie: bool) -> bool {
    matches!(*method, Method::GET | Method::HEAD) && !has_cookie
}

/// A stable hash of a response body, for equality comparison.
pub fn hash_body(body: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    h.finish()
}

/// Mirror one request to the shadow upstream and compare its response to
/// production's `(prod_status, prod_hash)`. Runs on a background task with owned
/// values; failures only touch the shadow counters, never the client.
pub async fn compare_owned(
    client: HttpClient,
    base: String,
    method: Method,
    path_qs: String,
    prod_status: u16,
    prod_hash: u64,
) {
    let url = format!("{base}{path_qs}");
    let Ok(req) = Request::builder()
        .method(method)
        .uri(&url)
        // Compare uncompressed bodies (prod's hash is over the raw body too).
        .header(hyper::header::ACCEPT_ENCODING, "identity")
        .header("X-Askr-Shadow", "1")
        .body(Full::new(Bytes::new()))
    else {
        return;
    };

    let m = crate::metrics::Metrics::get();
    if let Some(m) = m {
        m.shadow_total.fetch_add(1, Ordering::Relaxed);
    }

    match client.request(req).await {
        Ok(resp) => {
            let sstatus = resp.status().as_u16();
            let Ok(collected) = resp.into_body().collect().await else {
                if let Some(m) = m {
                    m.shadow_error.fetch_add(1, Ordering::Relaxed);
                }
                return;
            };
            let shash = hash_body(&collected.to_bytes());
            let same = sstatus == prod_status && shash == prod_hash;
            if let Some(m) = m {
                if same {
                    m.shadow_match.fetch_add(1, Ordering::Relaxed);
                } else {
                    m.shadow_mismatch.fetch_add(1, Ordering::Relaxed);
                }
            }
            if !same {
                tracing::warn!(
                    url = %url,
                    prod_status,
                    shadow_status = sstatus,
                    body_match = (shash == prod_hash),
                    "shadow: response differs from production"
                );
            }
        }
        Err(e) => {
            if let Some(m) = m {
                m.shadow_error.fetch_add(1, Ordering::Relaxed);
            }
            tracing::debug!(url = %url, error = %e, "shadow: upstream unreachable");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_safe_cookieless_is_eligible() {
        assert!(eligible(&Method::GET, false));
        assert!(eligible(&Method::HEAD, false));
        assert!(!eligible(&Method::GET, true)); // cookie ⇒ user-specific
        assert!(!eligible(&Method::POST, false)); // not idempotent
        assert!(!eligible(&Method::PUT, false));
    }

    #[test]
    fn body_hash_is_stable_and_discriminating() {
        assert_eq!(hash_body(b"abc"), hash_body(b"abc"));
        assert_ne!(hash_body(b"abc"), hash_body(b"abd"));
    }
}
