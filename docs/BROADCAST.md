# Broadcasting (SSE)

Askr can push live updates to browsers without an external broker (Reverb,
Pusher, Redis pub/sub). PHP publishes an event with `askr_broadcast()`; Rust
holds the client connections and fans events out — across **all** worker
processes, via a shared-memory ring.

Today this is **Server-Sent Events (SSE)**, which covers most "live update" needs
(notifications, dashboards, progress, Livewire refreshes). Raw WebSockets and
Pusher-protocol (drop-in Reverb) compatibility are planned.

## Enabling it

```bash
askr serve --root ./public --worker-script examples/laravel-worker.php \
  --broadcast
```

or in `askr.toml`:

```toml
[broadcast]
enabled = true
```

## Publishing (PHP)

```php
askr_broadcast(string $channel, string $payload): bool
```

```php
// anywhere PHP runs under Askr — a web request, a queue job, the scheduler:
askr_broadcast('orders', json_encode(['id' => 42, 'status' => 'shipped']));
```

Channels are arbitrary strings (≤ 64 bytes); payloads are ≤ ~3 KB. A publish from
any worker or sidecar reaches subscribers held by any worker.

## Subscribing (browser)

Askr serves an SSE stream at **`GET /askr/events?channel=NAME`**:

```js
const es = new EventSource('/askr/events?channel=orders');
es.onmessage = (e) => {
    const order = JSON.parse(e.data);
    console.log('order update', order);
};
```

The stream sends `data: <payload>\n\n` for each event on the channel, plus a
`: ping` comment roughly every 15 s to keep the connection alive. `EventSource`
reconnects automatically.

## How it works

- A fixed-size **ring buffer** lives in an anonymous shared mmap (created before
  fork), so publishes from any process are visible to all.
- Each worker runs a background task that **tails the ring** every 50 ms and
  pushes matching events to the SSE connections it holds locally.
- SSE responses are true **streaming bodies** — the connection stays open and
  frames are written as events arrive.

Latency is ~50 ms (the tail interval). A subscriber that falls more than a ring
behind drops the oldest events (fine for live updates). Events are not
persisted.

## Notes

- SSE is one-way (server → client). For client → server, make a normal request.
- Behind a proxy, disable response buffering for `/askr/events` (Askr sets
  `X-Accel-Buffering: no` for nginx; other proxies may need their own setting).
- For guaranteed delivery, cross-host fan-out, or the Pusher protocol, use a
  dedicated broker. Askr's broadcasting targets the common single-host,
  best-effort case with zero extra moving parts.
