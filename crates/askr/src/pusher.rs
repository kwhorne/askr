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
use tokio::sync::mpsc;

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
pub async fn serve(fut: UpgradeFut, hub: std::sync::Arc<PusherHub>) {
    let Ok(ws) = fut.await else {
        return;
    };
    let mut ws = FragmentCollector::new(ws);
    let mut rx = hub.register();
    let mut subs: HashSet<String> = HashSet::new();

    let est = format!(
        r#"{{"event":"pusher:connection_established","data":"{{\"socket_id\":\"{}\",\"activity_timeout\":120}}"}}"#,
        socket_id()
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
                        if let Some(reply) = handle_client_message(&frame.payload, &mut subs) {
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
fn handle_client_message(payload: &[u8], subs: &mut HashSet<String>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    match v.get("event").and_then(|e| e.as_str())? {
        "pusher:ping" => Some(r#"{"event":"pusher:pong","data":"{}"}"#.to_string()),
        "pusher:subscribe" => {
            let channel = v
                .get("data")
                .and_then(|d| d.get("channel"))
                .and_then(|c| c.as_str())?
                .to_string();
            subs.insert(channel.clone());
            // presence channels expect a member payload; empty is accepted.
            let data = if channel.starts_with("presence-") {
                r#"{\"presence\":{\"count\":0,\"ids\":[],\"hash\":{}}}"#
            } else {
                "{}"
            };
            Some(format!(
                r#"{{"event":"pusher_internal:subscription_succeeded","channel":"{channel}","data":"{data}"}}"#
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
