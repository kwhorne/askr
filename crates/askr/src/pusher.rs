//! Pusher-compatible WebSocket endpoint + HTTP trigger — a drop-in Reverb for
//! the common case (public and authenticated channels), so Laravel Echo and
//! Livewire's streaming talk to Askr with no frontend config change.
//!
//! Two endpoints (both fed by the shared broadcast ring, so a publish from any
//! worker/sidecar reaches every subscriber in every process):
//!
//!   WS   /app/{key}                 client connections (subscribe / events)
//!   POST /apps/{app_id}/events       the Pusher HTTP API Laravel's broadcaster
//!                                    calls server-side to trigger events
//!
//! Scope: public channels work fully; `private-`/`presence-` subscriptions are
//! accepted (auth-signature verification is a follow-up). Enough to replace
//! Reverb for the common broadcasting case with zero infrastructure.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use bytes::Bytes;
use fastwebsockets::upgrade::UpgradeFut;
use fastwebsockets::{FragmentCollector, Frame, OpCode, Payload};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::sync::atomic::AtomicBool;
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

/// Constant-time equality for two byte strings.
///
/// A signature compared with `==` (or `eq_ignore_ascii_case`) returns at the first
/// differing byte, and how long that takes tells a patient caller how many leading
/// bytes they got right. Both signature checks in this file go through here.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pusher subscription signature: `HMAC-SHA256(secret, string_to_sign)`, hex.
/// For private channels the string is `socket_id:channel`; presence channels
/// append `:channel_data`.
fn sign(secret: &str, string_to_sign: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(string_to_sign.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Verify a `pusher:subscribe` auth token (`"{app_key}:{hex_signature}"`) for a
/// private/presence channel against the shared app secret.
fn verify_subscription(
    secret: &str,
    socket_id: &str,
    channel: &str,
    channel_data: Option<&str>,
    provided_auth: &str,
) -> bool {
    let sts = match channel_data {
        Some(cd) => format!("{socket_id}:{channel}:{cd}"),
        None => format!("{socket_id}:{channel}"),
    };
    let expected = sign(secret, &sts);
    // The token is "app_key:signature"; compare the signature half.
    let provided = provided_auth.rsplit(':').next().unwrap_or("");
    ct_eq(
        provided.to_ascii_lowercase().as_bytes(),
        expected.as_bytes(),
    )
}

/// Why a `POST /apps/{id}/events` trigger was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum TriggerRejected {
    /// One of the required `auth_*` / `body_md5` parameters is missing.
    MissingAuth,
    /// `body_md5` does not match the body that arrived.
    BodyMismatch,
    /// `auth_timestamp` is more than [`TRIGGER_MAX_SKEW_SECS`] from now.
    StaleTimestamp,
    /// The HMAC does not verify against the configured secret.
    BadSignature,
}

impl std::fmt::Display for TriggerRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TriggerRejected::MissingAuth => {
                "missing auth_key/auth_timestamp/auth_version/body_md5/auth_signature"
            }
            TriggerRejected::BodyMismatch => "body_md5 does not match the request body",
            TriggerRejected::StaleTimestamp => "auth_timestamp is outside the allowed window",
            TriggerRejected::BadSignature => "auth_signature does not verify",
        })
    }
}

/// How far `auth_timestamp` may be from the server clock. Pusher's own limit.
pub const TRIGGER_MAX_SKEW_SECS: u64 = 600;

/// Largest WebSocket message a client may send. Pusher's protocol messages are a
/// few hundred bytes of JSON; fastwebsockets' default was 64 *MiB*, buffered per
/// connection, for anyone who opened one.
pub const MAX_CLIENT_MESSAGE_BYTES: usize = 64 * 1024;
/// Channels one connection may hold. Echo subscribes to a handful; the set was
/// unbounded, and a channel name is a `String` kept for the life of the socket.
pub const MAX_SUBSCRIPTIONS: usize = 256;
/// Pusher's own limit on a channel name.
pub const MAX_CHANNEL_NAME: usize = 164;

/// Verify the Pusher HTTP API signature on a trigger request.
///
/// This is the authentication the endpoint never had. Without it, anyone who could
/// reach the server could `POST /apps/{anything}/events` and publish into every
/// channel — `private-` and `presence-` included — and each subscribed client would
/// receive it as a legitimate server event, straight past the HMAC that
/// [`verify_subscription`] enforces on the way *in*. A read-side check with an open
/// write side is not a check.
///
/// The scheme is Pusher's (and therefore what `pusher-php-server`, which Laravel's
/// broadcaster uses, sends on every request): the query carries `auth_key`,
/// `auth_timestamp`, `auth_version`, `body_md5` and `auth_signature`; the signature
/// is `HMAC-SHA256(secret, "POST\n{path}\n{query sorted by key, minus
/// auth_signature}")`, hex. `now` is a parameter so the timestamp window is testable
/// against the published 2012 vector.
pub fn verify_trigger(
    secret: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    now: u64,
) -> Result<(), TriggerRejected> {
    let mut params: Vec<(String, String)> = query
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_ascii_lowercase(), v.to_string()),
            None => (kv.to_ascii_lowercase(), String::new()),
        })
        .collect();

    let take = |params: &mut Vec<(String, String)>, name: &str| -> Option<String> {
        let i = params.iter().position(|(k, _)| k == name)?;
        Some(params.remove(i).1)
    };
    let signature = take(&mut params, "auth_signature").ok_or(TriggerRejected::MissingAuth)?;
    let find = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let (Some(_key), Some(ts), Some(_ver), Some(md5)) = (
        find("auth_key"),
        find("auth_timestamp"),
        find("auth_version"),
        find("body_md5"),
    ) else {
        return Err(TriggerRejected::MissingAuth);
    };

    // The body hash is checked first and in constant time: it is the cheapest thing
    // to get wrong, and it pins the signature to *this* body rather than to any body
    // an attacker can replay a captured signature over.
    let got = {
        use md5::{Digest, Md5};
        let mut h = Md5::new();
        h.update(body);
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    if !ct_eq(md5.to_ascii_lowercase().as_bytes(), got.as_bytes()) {
        return Err(TriggerRejected::BodyMismatch);
    }

    let ts: u64 = ts.parse().map_err(|_| TriggerRejected::StaleTimestamp)?;
    if now.abs_diff(ts) > TRIGGER_MAX_SKEW_SECS {
        return Err(TriggerRejected::StaleTimestamp);
    }

    params.sort();
    let query_sorted = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let string_to_sign = format!("POST\n{path}\n{query_sorted}");
    let expected = sign(secret, &string_to_sign);
    if !ct_eq(
        signature.to_ascii_lowercase().as_bytes(),
        expected.as_bytes(),
    ) {
        return Err(TriggerRejected::BadSignature);
    }
    Ok(())
}

static UNAUTHENTICATED_TRIGGER_WARNED: AtomicBool = AtomicBool::new(false);

/// Say, once, that triggers are being accepted with no secret to check them against.
///
/// Mirrors the subscription side, which also accepts `private-` channels when no
/// secret is configured — documented as a development mode. The write side deserves
/// the louder message, because here "development mode" means anyone can publish.
pub fn warn_unauthenticated_trigger_once() {
    if !UNAUTHENTICATED_TRIGGER_WARNED.swap(true, Ordering::SeqCst) {
        tracing::warn!(
            "pusher: accepting POST /apps/*/events with NO --pusher-secret configured — \
             anyone who can reach this server can publish into every channel, private \
             ones included. Set the secret before exposing this to a network."
        );
    }
}

/// One event to fan out: (channel, ready-to-send Pusher frame JSON).
type Item = (String, Bytes);

/// A per-worker registry of live WebSocket connections. The broadcast-ring
/// tailer pushes every event to every connection task, which filters by its own
/// subscriptions (so a client only ever receives channels it subscribed to).
#[derive(Default)]
pub struct PusherHub {
    conns: Mutex<Vec<mpsc::Sender<Item>>>,
}

impl PusherHub {
    fn register(&self) -> mpsc::Receiver<Item> {
        let (tx, rx) = mpsc::channel(256);
        self.conns.lock().unwrap().push(tx);
        rx
    }

    /// Deliver an event to every connection (each filters by subscription).
    pub fn deliver(&self, channel: &str, payload: &[u8]) {
        let frame = build_event_frame(channel, payload);
        let item = (channel.to_string(), frame);
        self.conns
            .lock()
            .unwrap()
            .retain(|tx| tx.try_send(item.clone()).is_ok());
    }

    /// Drop closed connections (called periodically).
    pub fn prune(&self) {
        self.conns.lock().unwrap().retain(|tx| !tx.is_closed());
    }
}

static SOCKET_SEQ: AtomicU64 = AtomicU64::new(1);

fn socket_id() -> String {
    let n = SOCKET_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}.{}", std::process::id(), n)
}

/// Build the client frame for a delivered event. If the ring payload is a JSON
/// object with an `event` key (as the HTTP trigger publishes), forward it with
/// the channel injected; otherwise wrap the raw payload as a `message` event
/// (so `askr_broadcast()` also reaches Pusher clients).
fn build_event_frame(channel: &str, payload: &[u8]) -> Bytes {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
        if let Some(event) = v.get("event").and_then(|e| e.as_str()) {
            let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
            let out = serde_json::json!({"event": event, "channel": channel, "data": data});
            return Bytes::from(out.to_string());
        }
    }
    let out = serde_json::json!({
        "event": "message",
        "channel": channel,
        "data": String::from_utf8_lossy(payload),
    });
    Bytes::from(out.to_string())
}

/// Handle one upgraded WebSocket connection: Pusher handshake + subscribe /
/// unsubscribe / ping, and fan out matching broadcast events.
pub async fn serve(fut: UpgradeFut, hub: std::sync::Arc<PusherHub>, secret: Option<String>) {
    let Ok(mut ws) = fut.await else {
        return;
    };
    // On the socket itself: the collector wraps a socket that already knows its limit.
    ws.set_max_message_size(MAX_CLIENT_MESSAGE_BYTES);
    let mut ws = FragmentCollector::new(ws);
    let mut rx = hub.register();
    let mut subs: HashSet<String> = HashSet::new();
    let sid = socket_id();

    let est = format!(
        r#"{{"event":"pusher:connection_established","data":"{{\"socket_id\":\"{sid}\",\"activity_timeout\":120}}"}}"#,
    );
    if ws
        .write_frame(Frame::text(Payload::Owned(est.into_bytes())))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            biased;
            item = rx.recv() => {
                match item {
                    Some((chan, frame)) if subs.contains(&chan) => {
                        if ws.write_frame(Frame::text(Payload::Owned(frame.to_vec()))).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {} // not subscribed to that channel
                    None => break,
                }
            }
            frame = ws.read_frame() => {
                let Ok(frame) = frame else { break };
                match frame.opcode {
                    OpCode::Close => break,
                    OpCode::Text | OpCode::Binary => {
                        if let Some(reply) = handle_client_message(&frame.payload, &mut subs, &sid, secret.as_deref()) {
                            if ws.write_frame(Frame::text(Payload::Owned(reply.into_bytes()))).await.is_err() {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Parse a client message and update subscriptions. Returns an optional reply.
/// `secret` (when set) enforces auth on `private-`/`presence-` channels.
fn handle_client_message(
    payload: &[u8],
    subs: &mut HashSet<String>,
    socket_id: &str,
    secret: Option<&str>,
) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    match v.get("event").and_then(|e| e.as_str())? {
        "pusher:ping" => Some(r#"{"event":"pusher:pong","data":"{}"}"#.to_string()),
        "pusher:subscribe" => {
            let data = v.get("data");
            let channel = data
                .and_then(|d| d.get("channel"))
                .and_then(|c| c.as_str())?
                .to_string();

            // Bounds first, before any work is done on the name.
            if channel.len() > MAX_CHANNEL_NAME {
                return Some(subscription_error(&channel, "channel name too long"));
            }
            if subs.len() >= MAX_SUBSCRIPTIONS && !subs.contains(&channel) {
                return Some(subscription_error(&channel, "too many subscriptions"));
            }

            // Authenticate private/presence channels against the app secret.
            let needs_auth = channel.starts_with("private-") || channel.starts_with("presence-");
            if needs_auth {
                if let Some(secret) = secret {
                    let auth = data.and_then(|d| d.get("auth")).and_then(|a| a.as_str());
                    let channel_data = data
                        .and_then(|d| d.get("channel_data"))
                        .and_then(|c| c.as_str());
                    let ok = auth.is_some_and(|a| {
                        verify_subscription(secret, socket_id, &channel, channel_data, a)
                    });
                    if !ok {
                        return Some(subscription_error(&channel, "auth signature mismatch"));
                    }
                }
                // No secret configured → accept (dev; documented).
            }

            subs.insert(channel.clone());
            // presence channels expect a member payload; empty is accepted.
            let payload = if channel.starts_with("presence-") {
                r#"{\"presence\":{\"count\":0,\"ids\":[],\"hash\":{}}}"#
            } else {
                "{}"
            };
            Some(format!(
                r#"{{"event":"pusher_internal:subscription_succeeded","channel":"{channel}","data":"{payload}"}}"#
            ))
        }
        "pusher:unsubscribe" => {
            if let Some(channel) = v
                .get("data")
                .and_then(|d| d.get("channel"))
                .and_then(|c| c.as_str())
            {
                subs.remove(channel);
            }
            None
        }
        _ => None,
    }
}

/// A `pusher_internal:subscription_error` frame. The channel name is JSON-escaped:
/// it came from the client, and the old inline format string interpolated it raw.
fn subscription_error(channel: &str, error: &str) -> String {
    serde_json::json!({
        "event": "pusher_internal:subscription_error",
        "channel": channel,
        "data": {"type": "AuthError", "status": 401, "error": error},
    })
    .to_string()
}

/// Handle `POST /apps/{app_id}/events` — the Pusher HTTP trigger API. Parses
/// `{name, channel|channels, data}` and publishes into the broadcast ring, which
/// the WS tailer fans out. Returns the JSON body for the 200 response.
pub fn trigger(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "{}".to_string();
    };
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("message");
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);

    let mut channels: Vec<String> = Vec::new();
    if let Some(c) = v.get("channel").and_then(|c| c.as_str()) {
        channels.push(c.to_string());
    }
    if let Some(arr) = v.get("channels").and_then(|c| c.as_array()) {
        channels.extend(arr.iter().filter_map(|c| c.as_str().map(String::from)));
    }

    // Publish the inner Pusher payload; build_event_frame injects the channel.
    let inner = serde_json::json!({"event": name, "data": data}).to_string();
    for ch in &channels {
        crate::broadcast::publish(ch.as_bytes(), inner.as_bytes());
    }
    "{}".to_string()
}

/// Is this a `POST /apps/{id}/events` trigger request?
pub fn is_trigger(path: &str) -> bool {
    path.starts_with("/apps/") && path.ends_with("/events")
}

/// Is this a `/app/{key}` WebSocket request?
pub fn is_ws_path(path: &str) -> bool {
    path.starts_with("/app/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A published HMAC-SHA256 vector. The round-trip test below signs and verifies
    /// with the same code, so it would still pass if a dependency bump changed what
    /// we compute — this one wouldn't. Pusher clients reject a wrong signature, and a
    /// silently different one looks like an auth bug in the app.
    #[test]
    fn hmac_sha256_matches_the_known_vector() {
        assert_eq!(
            sign("key", "The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn subscription_signature_roundtrip() {
        let (secret, sid, chan) = ("appsecret", "1234.5678", "private-orders");
        let good = format!("appkey:{}", sign(secret, &format!("{sid}:{chan}")));
        assert!(verify_subscription(secret, sid, chan, None, &good));
        // wrong secret / wrong socket / tampered signature all fail
        assert!(!verify_subscription("other", sid, chan, None, &good));
        assert!(!verify_subscription(secret, "9.9", chan, None, &good));
        assert!(!verify_subscription(
            secret,
            sid,
            chan,
            None,
            "appkey:deadbeef"
        ));
        // presence includes channel_data in the signed string
        let cd = r#"{"user_id":7}"#;
        let pres = format!("appkey:{}", sign(secret, &format!("{sid}:presence-x:{cd}")));
        assert!(verify_subscription(
            secret,
            sid,
            "presence-x",
            Some(cd),
            &pres
        ));
        assert!(!verify_subscription(secret, sid, "presence-x", None, &pres));
    }

    /// Pusher's worked example from the HTTP API documentation — a published vector,
    /// so this catches the string-to-sign being assembled wrong, not just our code
    /// agreeing with itself. Laravel's broadcaster produces exactly this shape.
    const V_SECRET: &str = "7ad3773142a6692b25b8";
    const V_PATH: &str = "/apps/3/events";
    const V_QUERY: &str =
        "auth_key=278d425bdf160c739803&auth_timestamp=1353088179&auth_version=1.0\
&body_md5=ec365a775a4cd0599faeb73354201b6f\
&auth_signature=da454824c97ba181a32ccc17a72625ba02771f50b50e1e7430e47a1f3f457e6c";
    const V_BODY: &[u8] =
        br#"{"name":"foo","channels":["project-3"],"data":"{\"some\":\"data\"}"}"#;
    const V_NOW: u64 = 1353088179;

    #[test]
    fn a_trigger_signed_the_way_pusher_documents_it_verifies() {
        assert_eq!(
            verify_trigger(V_SECRET, V_PATH, Some(V_QUERY), V_BODY, V_NOW),
            Ok(())
        );
        // Parameter order in the query must not matter: it is sorted before signing.
        let shuffled = "body_md5=ec365a775a4cd0599faeb73354201b6f&auth_version=1.0\
&auth_signature=da454824c97ba181a32ccc17a72625ba02771f50b50e1e7430e47a1f3f457e6c\
&auth_key=278d425bdf160c739803&auth_timestamp=1353088179";
        assert_eq!(
            verify_trigger(V_SECRET, V_PATH, Some(shuffled), V_BODY, V_NOW),
            Ok(())
        );
        // Ten minutes of skew is allowed either way.
        assert_eq!(
            verify_trigger(V_SECRET, V_PATH, Some(V_QUERY), V_BODY, V_NOW + 599),
            Ok(())
        );
    }

    /// Every way a forged or replayed trigger can be wrong, refused for the right
    /// reason — the reason is what an operator sees in the 401 body.
    #[test]
    fn a_forged_or_replayed_trigger_is_refused() {
        use TriggerRejected::*;
        let v =
            |q: Option<&str>, body: &[u8], now: u64| verify_trigger(V_SECRET, V_PATH, q, body, now);

        // The attack this closes: no auth at all.
        assert_eq!(v(None, V_BODY, V_NOW), Err(MissingAuth));
        assert_eq!(v(Some("auth_key=x"), V_BODY, V_NOW), Err(MissingAuth));

        // A captured signature replayed over a different body.
        assert_eq!(
            v(
                Some(V_QUERY),
                br#"{"name":"foo","channels":["private-admin"],"data":"x"}"#,
                V_NOW
            ),
            Err(BodyMismatch)
        );
        // …or replayed later than the window allows.
        assert_eq!(v(Some(V_QUERY), V_BODY, V_NOW + 601), Err(StaleTimestamp));
        assert_eq!(v(Some(V_QUERY), V_BODY, V_NOW - 601), Err(StaleTimestamp));

        // Wrong secret, wrong path, tampered signature.
        assert_eq!(
            verify_trigger("other-secret", V_PATH, Some(V_QUERY), V_BODY, V_NOW),
            Err(BadSignature)
        );
        assert_eq!(
            verify_trigger(V_SECRET, "/apps/4/events", Some(V_QUERY), V_BODY, V_NOW),
            Err(BadSignature)
        );
        let tampered = V_QUERY.replace("da454824", "00000000");
        assert_eq!(v(Some(&tampered), V_BODY, V_NOW), Err(BadSignature));
    }

    #[test]
    fn private_channel_requires_valid_auth_when_secret_set() {
        let mut subs = HashSet::new();
        let sid = "1.1";
        let secret = Some("s3cr3t");
        // No auth token → rejected, not subscribed.
        let msg = br#"{"event":"pusher:subscribe","data":{"channel":"private-x"}}"#;
        let reply = handle_client_message(msg, &mut subs, sid, secret).unwrap();
        assert!(reply.contains("subscription_error"));
        assert!(!subs.contains("private-x"));
        // Correct auth → subscribed.
        let good = format!("k:{}", sign("s3cr3t", &format!("{sid}:private-x")));
        let msg = format!(
            r#"{{"event":"pusher:subscribe","data":{{"channel":"private-x","auth":"{good}"}}}}"#
        );
        let reply = handle_client_message(msg.as_bytes(), &mut subs, sid, secret).unwrap();
        assert!(reply.contains("subscription_succeeded"));
        assert!(subs.contains("private-x"));
    }

    /// One socket could hold an unbounded set of channel names, each a `String` kept
    /// until it closed. The cap is generous for Echo and fatal for a loop.
    #[test]
    fn a_connection_cannot_subscribe_without_bound() {
        let mut subs = HashSet::new();
        for i in 0..MAX_SUBSCRIPTIONS {
            let msg = format!(r#"{{"event":"pusher:subscribe","data":{{"channel":"c{i}"}}}}"#);
            let reply = handle_client_message(msg.as_bytes(), &mut subs, "1.1", None).unwrap();
            assert!(reply.contains("subscription_succeeded"), "#{i}: {reply}");
        }
        assert_eq!(subs.len(), MAX_SUBSCRIPTIONS);

        let one_more = br#"{"event":"pusher:subscribe","data":{"channel":"overflow"}}"#;
        let reply = handle_client_message(one_more, &mut subs, "1.1", None).unwrap();
        assert!(reply.contains("too many subscriptions"), "{reply}");
        assert!(!subs.contains("overflow"));

        // Re-subscribing to a channel already held is not a new subscription.
        let again = br#"{"event":"pusher:subscribe","data":{"channel":"c0"}}"#;
        let reply = handle_client_message(again, &mut subs, "1.1", None).unwrap();
        assert!(reply.contains("subscription_succeeded"), "{reply}");

        let long = format!(
            r#"{{"event":"pusher:subscribe","data":{{"channel":"{}"}}}}"#,
            "x".repeat(MAX_CHANNEL_NAME + 1)
        );
        let mut fresh = HashSet::new();
        let reply = handle_client_message(long.as_bytes(), &mut fresh, "1.1", None).unwrap();
        assert!(reply.contains("channel name too long"), "{reply}");
        assert!(fresh.is_empty());
    }

    /// The error frame used to interpolate the client's channel name into a JSON
    /// string with `format!`. A quote in the name produced a frame that was not JSON.
    #[test]
    fn a_subscription_error_is_valid_json_whatever_the_channel_is_called() {
        let frame = subscription_error(r#"evil"chan"#, "nope");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("must parse");
        assert_eq!(v["channel"], r#"evil"chan"#);
        assert_eq!(v["data"]["error"], "nope");
    }

    #[test]
    fn public_channel_needs_no_auth() {
        let mut subs = HashSet::new();
        let msg = br#"{"event":"pusher:subscribe","data":{"channel":"orders"}}"#;
        let reply = handle_client_message(msg, &mut subs, "1.1", Some("s")).unwrap();
        assert!(reply.contains("subscription_succeeded"));
        assert!(subs.contains("orders"));
    }
}
