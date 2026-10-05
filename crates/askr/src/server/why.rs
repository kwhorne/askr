//! `askr why`: what Askr decided about one request, and why.
//!
//! Most of the questions an operator asks about a request — why was this not cached,
//! which site served it, which address did the rate limiter count — have answers Askr
//! computes on every request and then throws away. This keeps them for one request,
//! on demand.
//!
//! The admin plane (`GET /api/why`) sends the request to this server's own listener with
//! an `Askr-Explain` header carrying a secret the master made at startup, before the
//! workers were forked. A worker that sees the right secret runs the request as usual,
//! but inside a [`traced`] scope where every [`note`] is kept, and returns them on the
//! response in `Askr-Explain-Trace`. Anyone else's `Askr-Explain` header is removed
//! before PHP can see it, and changes nothing.
//!
//! Outside a traced request a `note` is one thread-local lookup that finds nothing; the
//! closure that would format the step is never called.

use std::cell::RefCell;
use std::future::Future;
use std::sync::OnceLock;

use hyper::HeaderMap;

/// Request header carrying the secret. Never reaches PHP.
pub const REQUEST_HEADER: &str = "askr-explain";
/// Request header naming the peer to explain *as* — the load balancer's address, say,
/// so trusted-proxy handling is explained the way production sees it. Honoured only
/// alongside the secret.
pub const PEER_HEADER: &str = "askr-explain-peer";
/// Response header carrying the trace, as ASCII JSON.
pub const TRACE_HEADER: &str = "askr-explain-trace";

/// One decision: which part of the request path made it, what it decided, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub stage: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl Step {
    pub fn new(stage: &str, outcome: impl Into<String>) -> Self {
        Step {
            stage: stage.to_string(),
            outcome: outcome.into(),
            why: None,
        }
    }

    pub fn because(mut self, why: impl Into<String>) -> Self {
        self.why = Some(why.into());
        self
    }
}

tokio::task_local! {
    static TRACE: RefCell<Vec<Step>>;
}

/// Record a decision, if this request is being explained. The closure runs only then.
pub(crate) fn note(step: impl FnOnce() -> Step) {
    let _ = TRACE.try_with(|t| t.borrow_mut().push(step()));
}

/// Run `fut` with its decisions recorded, and return them with its output.
pub(crate) async fn traced<F: Future>(fut: F) -> (F::Output, Vec<Step>) {
    TRACE
        .scope(RefCell::new(Vec::new()), async move {
            let out = fut.await;
            let steps = TRACE.with(|t| std::mem::take(&mut *t.borrow_mut()));
            (out, steps)
        })
        .await
}

static SECRET: OnceLock<String> = OnceLock::new();

/// Make this boot's secret. Called by the master before it forks, so every worker has
/// the same one; a no-op after the first call.
pub fn init_secret() {
    SECRET.get_or_init(|| {
        let mut b = [0u8; 32];
        // A failure leaves the secret all zeros, which would make it guessable — so
        // refuse to have one at all, and `askr why` says it is unavailable.
        match std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        {
            Ok(()) => b.iter().map(|x| format!("{x:02x}")).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "askr why: no randomness, explaining disabled");
                String::new()
            }
        }
    });
}

/// This boot's secret, if there is a usable one.
pub fn secret() -> Option<&'static str> {
    SECRET.get().map(String::as_str).filter(|s| !s.is_empty())
}

/// Remove the explain headers from an incoming request and say whether it asked, with
/// the right secret, to be explained — and as which peer.
///
/// Always removes them: a header PHP never sees cannot be used to probe for the secret
/// through the application, and a wrong one is indistinguishable from none.
pub(crate) fn take_request(headers: &mut HeaderMap) -> Option<Option<std::net::IpAddr>> {
    let given = headers.remove(REQUEST_HEADER);
    let peer = headers.remove(PEER_HEADER);
    let given = given?;
    let want = secret()?;
    if !constant_time_eq(given.as_bytes(), want.as_bytes()) {
        return None;
    }
    Some(
        peer.and_then(|p| p.to_str().ok().map(str::trim).map(str::to_string))
            .and_then(|p| p.parse().ok()),
    )
}

/// Remove the explain headers without honouring them — for a path that cannot carry a
/// trace back (HTTP/3), where they must still never reach PHP.
#[cfg(feature = "http3")]
pub(crate) fn strip(headers: &mut HeaderMap) {
    headers.remove(REQUEST_HEADER);
    headers.remove(PEER_HEADER);
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The trace as a header value: JSON with everything outside printable ASCII escaped,
/// since a header value may not carry it raw.
pub(crate) fn encode(steps: &[Step]) -> String {
    let json = serde_json::to_string(steps).unwrap_or_else(|_| "[]".to_string());
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if (' '..='~').contains(&c) {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notes_are_kept_only_inside_a_traced_request() {
        let mut formatted = false;
        note(|| {
            formatted = true;
            Step::new("x", "y")
        });
        assert!(!formatted, "outside a trace the step is never even built");

        let ((), steps) = traced(async {
            note(|| Step::new("cache", "MISS").because("nothing stored yet"));
            note(|| Step::new("php", "200"));
        })
        .await;
        assert_eq!(
            steps,
            vec![
                Step::new("cache", "MISS").because("nothing stored yet"),
                Step::new("php", "200")
            ]
        );
    }

    #[test]
    fn only_the_right_secret_explains_and_the_headers_never_survive() {
        init_secret();
        let secret = secret().expect("a secret").to_string();

        let mut h = HeaderMap::new();
        h.insert(REQUEST_HEADER, "not-it".parse().unwrap());
        h.insert(PEER_HEADER, "10.0.0.4".parse().unwrap());
        assert_eq!(take_request(&mut h), None);
        assert!(h.is_empty(), "a wrong secret is still stripped: {h:?}");

        let mut h = HeaderMap::new();
        h.insert(REQUEST_HEADER, secret.parse().unwrap());
        h.insert(PEER_HEADER, "10.0.0.4".parse().unwrap());
        assert_eq!(
            take_request(&mut h),
            Some(Some("10.0.0.4".parse().unwrap()))
        );
        assert!(h.is_empty());

        let mut h = HeaderMap::new();
        h.insert(PEER_HEADER, "10.0.0.4".parse().unwrap());
        assert_eq!(
            take_request(&mut h),
            None,
            "a peer without the secret is ignored"
        );
        assert!(h.is_empty());
    }

    #[test]
    fn the_trace_survives_as_a_header_value() {
        let steps = vec![Step::new("site", "/srv/blåbær/public").because("Host \"x\"")];
        let v = encode(&steps);
        assert!(v.bytes().all(|b| (b' '..=b'~').contains(&b)), "{v}");
        hyper::header::HeaderValue::from_str(&v).expect("valid header value");
        let back: Vec<Step> = serde_json::from_str(&v).unwrap();
        assert_eq!(back, steps);
    }
}
