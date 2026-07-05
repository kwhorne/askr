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
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

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
    provided.eq_ignore_ascii_case(&expected)
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
    let Ok(ws) = fut.await else {
        return;
    };
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
                        return Some(format!(
                            r#"{{"event":"pusher_internal:subscription_error","channel":"{channel}","data":{{"type":"AuthError","status":401,"error":"auth signature mismatch"}}}}"#
                        ));
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

    #[test]
    fn public_channel_needs_no_auth() {
        let mut subs = HashSet::new();
        let msg = br#"{"event":"pusher:subscribe","data":{"channel":"orders"}}"#;
        let reply = handle_client_message(msg, &mut subs, "1.1", Some("s")).unwrap();
        assert!(reply.contains("subscription_succeeded"));
        assert!(subs.contains("orders"));
    }
}
