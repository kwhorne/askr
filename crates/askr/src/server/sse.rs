//! Server-sent events: the hub that fans broadcast messages out to `/askr/sse` clients.

use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use hyper::{Response, StatusCode};
use tokio::sync::mpsc;

use super::{text, ResBody, Runtime};

/// Per-worker registry of live SSE subscribers. A background task tails the
/// shared broadcast ring and pushes matching events to these.
#[derive(Default)]
pub(super) struct SseHub {
    // Sharded by channel so delivering one event only touches that channel's
    // subscribers — O(subs-on-channel), not O(all-subs) — which matters when a box
    // fans out to thousands of SSE clients across many channels.
    channels: Mutex<std::collections::HashMap<String, Vec<mpsc::Sender<Bytes>>>>,
}

impl SseHub {
    fn subscribe(&self, channel: String) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(128);
        let _ = tx.try_send(Bytes::from_static(b": connected\n\n"));
        self.channels
            .lock()
            .unwrap()
            .entry(channel)
            .or_default()
            .push(tx);
        rx
    }

    pub(super) fn deliver(&self, channel: &str, data: &Bytes) {
        let mut map = self.channels.lock().unwrap();
        if let Some(subs) = map.get_mut(channel) {
            // Non-blocking: if a subscriber's 128-message buffer is full (a client
            // that can't keep up), try_send fails and we drop it — intentional
            // back-pressure, a slow client is disconnected rather than stalling the
            // fan-out. Prune the channel entry once it's empty.
            subs.retain(|tx| tx.try_send(data.clone()).is_ok());
            if subs.is_empty() {
                map.remove(channel);
            }
        }
    }

    pub(super) fn ping(&self) {
        let msg = Bytes::from_static(b": ping\n\n");
        // Keep-alive sweep (~15 s): prune dead subscribers and empty channels.
        self.channels.lock().unwrap().retain(|_, subs| {
            subs.retain(|tx| tx.try_send(msg.clone()).is_ok());
            !subs.is_empty()
        });
    }
}

/// Streaming body for an SSE connection: yields frames as events arrive.
pub(super) struct SseBody {
    rx: mpsc::Receiver<Bytes>,
}

impl Body for SseBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        match self.get_mut().rx.poll_recv(cx) {
            Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Subscribe to a channel and stream Server-Sent Events.
pub(super) fn sse_response(query: Option<&str>, rt: &Runtime) -> Response<ResBody> {
    let channel = query
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("channel=").map(|c| c.to_string()))
        })
        .unwrap_or_else(|| "default".to_string());

    // pusher.rs HMAC-verifies `private-`/`presence-` subscriptions before adding
    // them to a socket. This bridge has no socket id and no signature to check one
    // against, so it cannot honour that rule — and until it can, it must not be the
    // way round it: GET /askr/events?channel=private-orders would otherwise hand any
    // caller on the internet everything broadcast on a channel the WebSocket path
    // guards. Same case-sensitive prefixes as pusher.rs, so the two agree on which
    // names are privileged.
    if channel.starts_with("private-") || channel.starts_with("presence-") {
        return text(
            StatusCode::FORBIDDEN,
            "askr: private- and presence- channels are only available over the \
             WebSocket path, which authenticates the subscription",
        );
    }
    // The publish side refuses a channel name over CHAN_MAX; the subscribe side kept
    // whatever it was given, as a HashMap key held for the life of the connection. A
    // name nothing can ever publish to is only memory.
    if channel.len() > crate::broadcast::CHAN_MAX {
        return text(StatusCode::BAD_REQUEST, "askr: channel name too long");
    }

    let rx = rt.sse.subscribe(channel);
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(SseBody { rx }.boxed())
        .unwrap()
}
