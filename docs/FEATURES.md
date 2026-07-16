# Askr power features

Seven capabilities that fall out of Askr's architecture — the shared-memory
substrate, the CoW template, and owning the whole request lifecycle in-process.

---

## 1. Response cache with instant tag invalidation

A full-response edge cache in the binary. PHP marks a response cacheable; a later
matching **anonymous** `GET`/`HEAD` is served straight from Rust, never touching
PHP — static-file speed for cacheable pages, no Varnish, no Redis.

Enable:

```bash
askr serve … --response-cache 512        # ~140 KB per slot
# askr.toml → [cache] response_slots = 512
```

Opt a response in (the app decides what's safe to cache):

```php
// cache for 60s, tagged so it can be invalidated later
header('Askr-Cache: 60, tags=posts,homepage');
```

Invalidate everything with a tag — instantly, across **all** workers (O(1)):

```php
askr_cache_forget_tag('posts');   // e.g. in a Post::saved() observer
```

**Stale-while-revalidate** — serve a warm page instantly and refresh it in the
background, so a client never waits for PHP on a hot page:

```php
// fresh 60s, then serve STALE for up to 600s more while ONE background
// refresh re-runs PHP off the request path and repopulates the cache
header('Askr-Cache: 60, swr=600, tags=posts');
```

Within the `swr` window the response is served immediately with
`X-Askr-Cache: STALE`; Askr fires a single coalesced background refresh (reusing
the request-coalescing inflight table so N concurrent stale hits trigger just one
recompute). Past `swr` it's a normal miss again. Compression is applied once at
store time, so hits and stale serves do zero per-request compression.

- Only cookie-less `GET`/`HEAD` are cacheable; `Set-Cookie` is stripped on store
  so a cached page can't pin one session onto every visitor.
- Responses carry `X-Askr-Cache: HIT|MISS|STALE`; hit-rate shows on the dashboard.
- `askr_cache_flush()` clears the response cache too.

## 2. Request coalescing (singleflight)

When identical cacheable requests hit a cold cache at the same time, **one** runs
PHP (the leader) and the rest wait for it to populate the cache, then are served
from it. Cache stampedes are eliminated across worker processes — automatic
whenever the response cache is enabled.

## 3. Pusher-compatible WebSocket (drop-in Reverb)

Real-time without Reverb or an external broker. Laravel Echo talks to Askr with
no frontend config change.

```bash
askr serve … --pusher          # auto-enables the broadcast ring
# askr.toml → [pusher] enabled = true
```

- WS endpoint `ws://…/app/{key}` — `pusher:connection_established`, subscribe /
  unsubscribe, ping/pong.
- HTTP trigger `POST /apps/{id}/events` — the Pusher API Laravel's broadcaster
  calls server-side; publishes into the shared broadcast ring so a trigger in any
  worker reaches subscribers in all of them.
- `askr_broadcast('channel', $json)` from PHP also reaches Pusher clients.

**Private & presence channels** are verified against the app secret (0.3.1):

```bash
askr serve … --pusher --pusher-secret "$PUSHER_APP_SECRET"
# or $ASKR_PUSHER_SECRET / [pusher] secret in askr.toml
```

A `private-`/`presence-` subscription must carry a valid `auth` token — the same
`HMAC-SHA256(secret, "socket_id:channel[:channel_data]")` Laravel's
`/broadcasting/auth` produces — or it's rejected with a `subscription_error`.
Point Laravel's `pusher` driver at Askr (matching key/secret) and Echo just works:

```php
// config/broadcasting.php → connections.pusher
'key'    => env('PUSHER_APP_KEY'),
'secret' => env('PUSHER_APP_SECRET'),   // must match --pusher-secret
'options' => [
    'host'   => env('PUSHER_HOST', '127.0.0.1'),
    'port'   => env('PUSHER_PORT', 443),
    'scheme' => env('PUSHER_SCHEME', 'https'),
],
```

Without a secret configured, private/presence subscriptions are accepted (dev).

## 4. `askr_defer()` — work after the response is sent

```php
askr_defer(function () use ($user) {
    Mail::to($user)->send(new Welcome());   // runs after the client has the reply
});
```

Rust flushes the response, then the worker runs deferred closures before taking
the next request. Octane-style deferred work with no queue infrastructure. Each
callback is isolated — a thrown exception can't poison the next one.

## 5. Elastic worker autoscaling (CoW)

Process autoscaling has never been practical for PHP (~300 ms cold boot). The CoW
template's ~ms warm respawn makes it cheap:

```bash
askr serve … --cow --worker-script … --workers-min 2 --workers-max 12
# askr.toml → [server] workers_min = 2 / workers_max = 12
```

The template reads a live queue-depth signal from shared memory, forks warm
workers when requests queue, and harvests them back to the floor when idle.

## 6. Record & replay of failing requests

```bash
askr serve … --record-errors /var/lib/askr/errors    # persist every 5xx
askr replay /var/lib/askr/errors/<id>.json           # reproduce it exactly
```

A 5xx writes its full CGI envelope (method, URI, `$_SERVER`, raw body). `askr
replay` reconstructs the exact request against a fresh interpreter and prints the
status, headers and body — production debugging goes from "try to reproduce" to
"replay it". Recent failures are listed on the admin dashboard.

> Captures request bodies — treat the directory as sensitive.

## 7. Fork-based parallel test runner

```bash
askr test --root /path/to/app --runner examples/askr-test.php tests/
```

Boots the interpreter once (opcache warm and shared), then forks a fresh process
per test file: perfect isolation (no state bleed between files), parallelism, and
no cold boot per file. Point `--runner` at `examples/askr-test.php` for
PHPUnit/Pest, or omit it to run files directly. Exits non-zero if any file fails.

## 8. File uploads that stream to disk (0.4.0)

`multipart/form-data` is streamed, not buffered: each file part goes straight to
a temp file, so a large upload costs **constant memory** (a 32 MB upload no
longer holds 32 MB in RAM), and form fields are parsed to POST params. Askr hands
PHP the `$_FILES`-shaped metadata and `examples/laravel-worker.php` rebuilds them
as Laravel `UploadedFile`s in test mode — so uploads work in worker mode:

```php
$request->file('avatar')->store('avatars');   // just works
$request->input('name');                       // multipart fields too
```

Temp files land under `$TMPDIR/askr-uploads` and are removed after each request.
The `--max-body-size` limit is enforced on the stream (`413` above it); set PHP's
`upload_max_filesize`/`post_max_size` via `[worker] ini` if your app checks them.

## 9. Compression, logging & observability (0.4.1)

- **Response compression** — compressible responses are compressed in Rust,
  negotiating `br` (preferred) or `gzip` from `Accept-Encoding` (often 5–10×
  fewer bytes). Dynamic, cached, and small static responses; large files keep
  streaming. Automatic — no config.
- **Access log** — `--access-log <path|->` / `[server] access_log` writes one
  JSON line per request (ts, ip, method, path, status, bytes, dur_ms).
- **Prometheus** — `GET /metrics` on the admin plane exposes Prometheus text
  format (requests, status classes, PHP-vs-I/O seconds, cache
  hits/misses/evictions, in-flight + live workers, latency histogram):

  ```
  scrape_configs:
    - job_name: askr
      static_configs: [{ targets: ["127.0.0.1:9000"] }]
  ```

## 10. Redis-free stack (0.5.0–0.6.1)

Everything a single-box Laravel app usually needs Redis for is built into the
binary and lives in shared memory across all workers — see [Cache](CACHE.md):

- **Cache** — `askr_cache_*` + the `AskrCacheStore` driver (small + 64 KB large
  region for sessions/fragments; `[cache] slots` / `large_slots`).
- **Counters & atomic locks** — `askr_cache_increment` and `askr_cache_add`
  (set-if-absent) back `Cache::lock()` for rate limiting and mutexes.
- **Sessions** — `SESSION_STORE=askr` (large region), no external store.
- **Job queue** — `askr_queue_*` + the `AskrQueue` driver with reserve/visibility
  timeout, delayed jobs and retries (`[queue] slots`).
- **Full extension set** — intl, gd, curl, zip, exif, pdo_mysql/pgsql — so
  Filament/Livewire/Inertia apps run unmodified.
- **Sidecars** — supervise any command (`--sidecar`, `[[sidecar]]`), e.g. Inertia
  SSR, respawned like a worker.

### The "Redis replacement" is data layer *plus* runtime

Redis is only the **data layer** — you still run Horizon/a supervisor to *consume*
the queue, and cron to run the scheduler. Askr owns **both**: the shared-memory
store *and* the worker pool, scheduler, and queue consumer (worker-mode + sidecars).
So "serverless queue" isn't a feature bolted on — it's what falls out when storage
and the process supervisor live in one binary.

That synthesis makes one thing possible that Redis + Horizon needs a separate
daemon for: **queue-worker autoscaling**. Askr sees the backlog (it's in shared
memory) and owns the pool (it forks/drains it), so it scales queue workers on
demand — Horizon `balance=auto`, natively:

```bash
askr serve … --queue 1 --queue-max 8 --queue-slots 8192 --queue-script worker.php --admin 127.0.0.1:9090
```

`--queue` is the floor, `--queue-max` the ceiling. On a burst the pool jumps to the
target (~1 worker per 10 ready jobs, clamped); as the backlog clears it drains one
worker every couple seconds (scaled-down workers get a graceful `SIGTERM` and are
not respawned). Backlog and pool size are exported on `/metrics`:
`askr_queue_workers`, `askr_queue_ready`, `askr_queue_total`, `askr_queue_oldest_seconds`.

## 11. Auto-TLS via ACME (0.7.0)

`--acme --acme-domain example.com --acme-email you@example.com` obtains and
renews a Let's Encrypt certificate over HTTP-01 — the master answers challenges
on port 80 before forking, workers serve HTTPS from the cache, and a renewal
thread rolls workers when the cert nears expiry. One binary, no certbot/proxy.
See [Auto-TLS](AUTOTLS.md).

## 12. Hardening / sandbox (0.8.0, Linux)

`--sandbox` shrinks the blast radius of a PHP exploit: a seccomp filter makes
`execve`/`ptrace` return `EPERM` (no shell from an RCE), and `--sandbox-write
<dir>` adds Landlock so the worker can read everywhere but **write only** under
the allowlist (no webshell into the docroot). See [Sandbox](SANDBOX.md).

## 13. Traffic shadowing for deploy validation

Validate the next version against **real production traffic** before you promote
it — without risking a single user request.

```bash
# mirror 10% of safe traffic to a staging deploy of the new version
askr serve … --shadow-to http://127.0.0.1:8081 --shadow-sample 10 --admin 127.0.0.1:9090
```

After Askr serves the real response, it mirrors a sampled fraction of **safe**
(GET/HEAD, cookie-less) requests to the shadow upstream on a fire-and-forget
background task, and compares the shadow's status + body hash to production:

- The client's response and latency are **never** touched.
- Only idempotent, non-user-specific requests are mirrored — a shadow deploy never
  gets writes or one visitor's session.
- Divergence is logged and counted on `/metrics`: `askr_shadow_total`,
  `askr_shadow_match_total`, `askr_shadow_mismatch_total`, `askr_shadow_error_total`.

If `askr_shadow_mismatch_total` stays at 0 under load, the new version is
byte-for-byte compatible; any mismatch is logged with the URL and both statuses so
you can investigate **before** flipping traffic over.
