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

- Only anonymous `GET`/`HEAD` are cacheable; `Set-Cookie` is stripped on store
  so a cached page can't pin one session onto every visitor.

### ESI — one page, many TTLs

The hard part of caching isn't the cache, it's the *one dynamic thing* on an
otherwise static page. A product page could sit in cache for a day if it weren't for
the cart widget in the corner. Edge-Side Includes solve that: the page is cached
**with holes**, and Askr fills the holes on the way out.

```php
// The shell: cached for an hour, tags and all
header('Askr-Cache: 3600');
header('Askr-ESI: on');            // opt in — Askr only scans bodies that ask
?>
<html><body>
  <esi:include src="/_esi/header"/>   <!-- its own 24h TTL -->
  <main>Article body…</main>
  <esi:include src="/_esi/cart"/>     <!-- no Askr-Cache header ⇒ per request -->
  <esi:remove><p>Shown only by caches that don't speak ESI</p></esi:remove>
</body></html>
```

Each fragment is an **ordinary request through your front controller**, so it routes
like any other URL and carries its own `Askr-Cache` header — which means its own TTL,
its own tags, and its own `PURGE`. A real run of the page above, three times in a row:

```
<html>SHELL=42d2 [HEADER cached=07fd][CART live=70afc0]</html>
<html>SHELL=42d2 [HEADER cached=07fd][CART live=785e09]</html>
<html>SHELL=42d2 [HEADER cached=07fd][CART live=db8c94]</html>
```

The shell and header are byte-identical (served from cache, PHP never ran for them);
the cart is fresh every time. `PURGE /_esi/header` swaps the header out and leaves the
shell alone.

What you should know:

- **Opt-in per response.** Without `Askr-ESI: on` a body is never scanned, so pages
  that happen to contain the text `<esi:` are untouched, and non-ESI traffic pays
  nothing (the pre-check is one substring search).
- **Fragments nest**, up to 3 passes — a cached fragment may itself contain includes,
  each with an independent TTL.
- **A broken fragment never breaks the page.** A non-200, a timeout or a stream
  attempt logs a warning and leaves the hole empty. Up to 32 fragments per request.
- **`src` must be a same-origin absolute path.** Absolute URLs, protocol-relative
  `//host`, schemes and `..` are refused — an ESI tag must never become an outbound
  fetch (that would make the server an SSRF proxy for anything that can influence a
  template).
- **Assembled per request, so compression happens after assembly.** ESI shells are
  stored uncompressed; the finished page is compressed on the way out (`Vary:
  Accept-Encoding`), which costs a little CPU that a fully static cached page doesn't
  pay.
- Streaming responses (PHP `flush()`) bypass the cache and therefore ESI.
- **Known limit:** because the cache key includes the negotiated encoding, an ESI
  shell is stored once per encoding class your clients negotiate (typically br and
  gzip). The bodies are identical and uncompressed, so this costs a few extra slots
  and one render per class — not correctness.

### PURGE & BAN over HTTP

Tag invalidation covers "this content changed". Sometimes you need to invalidate by
**URL** instead — from a deploy script, a CMS webhook, or by hand:

```bash
# Drop every cached variant of one URL (all encodings + device classes, GET & HEAD)
curl -X PURGE https://example.com/posts/123

# Drop everything under a path, by glob
curl -X BAN -H 'X-Ban-Url: /category/tech/*' https://example.com/
```

Both answer with a count, so a purge that matched nothing is visible rather than
silent: `{"purged":3}` / `{"banned":12}`.

- **`PURGE`** targets the request URL. With a query string it purges that exact URL;
  without one, every query variant of the path. Matching stops at a component
  boundary, so purging `/posts/1` never touches `/posts/12`.
- **`BAN`** takes a **glob** in `X-Ban-Url` (`*` and `?`), matched against the path.
  It is not a regex — a regex-looking pattern is rejected with a 400 rather than
  silently matching nothing. Entries stored *after* the ban are unaffected, which is
  what you want: they were rendered from current data.
- Both are **scoped to the requesting `Host`**, so one virtual host can't wipe
  another's cache.
- **Authentication:** set `ASKR_ADMIN_TOKEN` and send
  `Authorization: Bearer <token>`. With no token configured, `PURGE`/`BAN` are
  accepted from **loopback only** — an open purge endpoint is a cache-wiping DoS.

BAN is an eager scan at ban time rather than a rule list consulted on every lookup,
so invalidation costs one pass over the cache slots and the request hot path stays
exactly as fast as before.

### stale-if-error & saint mode

When the database falls over, the choice is between a 500 page and *slightly old
content*. Most sites would rather serve the old content:

```php
// fresh 300s; for a whole day after that, this page may be served if PHP fails
header('Askr-Cache: 300, stale-if-error=86400');
```

A `stale-if-error` window (alias `sie=`) keeps the entry as a **failure fallback**.
It is never served proactively — past the fresh/`swr` window a request still runs
PHP — but if PHP answers `5xx`, times out, or the worker dies, Askr serves the held
response with `X-Askr-Cache: STALE-ERROR` instead of the error. The real failure is
still logged, counted, and written to `--record-errors` for `askr replay`, so an
outage stays visible in your telemetry while visitors keep browsing.

**Saint mode** stops a dying backend from being hammered while it's down:

```toml
[cache]
saint_seconds = 5   # 0 = off (default)
```

After a `5xx`, the worker treats PHP as unhealthy for that many seconds: requests
that hold a `stale-if-error` entry are served straight from cache **without running
PHP at all**, giving the database room to recover. Requests without a fallback still
go through, so recovery is detected automatically — and a page that never opted into
`stale-if-error` always gets the real error.

The window is measured from the fresh deadline and is independent of `swr`, so the
three can be combined: `Askr-Cache: 300, swr=60, stale-if-error=86400`.

### Cache-key normalisation

By default the key is `method + host + path?query + encoding`, and *any* cookie
makes a request non-cacheable. On a real site that's brutal: `?utm_source=…` gives
every campaign visitor a private cache entry, and a single Google Analytics cookie
makes the whole audience uncacheable. Three `[cache]` keys fix that:

```toml
[cache]
response_slots = 512
# Tracking params don't fragment the cache (trailing * globs)
strip_query_params = ["utm_*", "gclid", "fbclid", "_ga"]
# Analytics cookies aren't identity — these visitors stay cacheable
ignore_cookies = ["_ga", "_gid", "_fbp"]
# Optional: cache mobile and desktop HTML separately
vary_user_agent = false
```

With that config, `/p?id=7`, `/p?id=7&utm_source=fb` and
`/p?utm_source=x&id=7&gclid=z` all share **one** entry, and a visitor carrying only
`_ga`/`_gid` is served from the same entry as a cookie-less visitor.

Details worth knowing:

- **Stripping affects the cache key only.** PHP still receives the complete,
  untouched query string, so analytics and attribution code keeps working.
- **Parameter order is normalised**: `?a=1&b=2` and `?b=2&a=1` share an entry.
  Sorting is skipped when a name repeats (`a[]=1&a[]=2`), because PHP builds an
  array there and the order changes the response.
- **A cookie that isn't on the list still defeats caching** — a `laravel_session`
  or auth cookie is never treated as anonymous. Keep the list to cookies you know
  the server ignores.
- `vary_user_agent` splits the key on a coarse mobile/desktop class *and* sets
  `Vary: User-Agent`, so a shared proxy downstream can't hand mobile HTML to a
  desktop client. Background stale-while-revalidate refreshes forward the original
  `User-Agent`, so a refresh re-renders as the same class it's stored under.
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

Temp files land under `$TMPDIR/askr-uploads` (created `0700` on Unix so other local
users on a shared host can't read them) and are removed after each request.
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
