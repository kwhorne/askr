# Worker mode

Worker mode is where Askr wins big. Instead of running the front controller from
scratch on every request (per-request mode, like PHP-FPM), a long-lived **worker
script** boots the application **once** and then loops, serving every request
against the already-booted app — the Laravel Octane model, but entirely
in-process (no IPC).

On a real Laravel + Livewire app this drops per-request latency from ~110 ms to
~9 ms and roughly **9×**'s throughput.


## Symptoms and their causes

Worker-mode bugs share a shape: something the framework expects to be thrown away per
request survives, and the failure is **silent**. This index is every one we've actually
shipped and fixed, with the symptom that identifies it — because each of these cost hours
before the cause was obvious.

| What you see | What it was |
|---|---|
| Interactivity works for the first page load or two, then stops. `wire:`/`x-` do nothing. **Console completely clean.** A Flux appearance toggle shows *both* icons. | Livewire emits its `<script>` tag once per process. Only the first response from each worker carried `livewire.js` — and Alpine ships inside it. Nothing failed; the script was never there. Fixed 1.4.8. |
| An anonymous visitor is served as a logged-in user, with no cookie at all | `session.store` is a separate singleton holding the loaded session; forgetting the guards wasn't enough. Fixed 1.4.3. |
| Every HTML form post answers **419**, and `$request->input()` is empty | The urlencoded body was never parsed into the request. Reads as a CSRF bug; is an empty request. Fixed 1.4.3. |
| **Over HTTPS only**, users are logged out at random or get a **419** — and it never reproduces with `curl` | HTTP/2 sends one `cookie` field per cookie and PHP was given the first one. `laravel_session` or `XSRF-TOKEN` went missing depending on which came first. Only browsers speak h2 to it, so `curl` (HTTP/1.1) and the test suite never saw it. Fixed 1.5.1. |
| File downloads, `flux.js`, streamed exports arrive **empty** with a 200 | `getContent()` returns `false` for `BinaryFileResponse`/`StreamedResponse`, and `echo false` prints nothing. Fixed 1.4.3. |
| A worker dies after the first file response, ~1 request in 3 fails | PHP's output layer kept a "sent" flag across requests, which ext-zlib turned into an `ErrorException` outside the kernel's try. Fixed 1.4.5. |
| `askr: php worker died mid-request` (502) | Something fatalled or `exit()`ed. **Read the log** — since 1.4.5 it names the class, method, file and line. Most recently: a queue driver missing a contract method added by a new Laravel major. |
| Generated URLs and redirects point at `localhost` over HTTPS | HTTP/2 sends no `Host` header. Fixed 1.4.7. |
| Some page loads reference a CSS/JS file that 404s, others are fine — and it looks correct in *your* browser | Laravel caches Vite's `manifest.json` per process. Workers that booted before `npm run build` hand out the previous build's filenames forever. Your browser hides it: it still holds the old file under `immutable, max-age=1 year`. **Reload after every frontend build.** |

The pattern to take away: **a clean console does not mean working JavaScript**, and a 200 does
not mean a body. Count what the browser actually received before theorising.

## What has to be reset between requests

A booted app that serves many requests must forget the previous one, and the list is
longer than it looks. `examples/laravel-worker.php` clears:

| State | Why it matters |
|---|---|
| `session.store` **and** the session manager's drivers | A separate singleton holds the loaded Store. Forgetting only the drivers left the previous visitor's session in the container, and a fresh guard built from a fresh driver still resolved **their user** — anonymous requests were served as someone who had logged in. |
| Auth guards | The resolved user. |
| Queued cookies | Otherwise one visitor's `Set-Cookie` is attached to the next response. |
| Shared view state (incl. `$errors`) | One visitor's validation errors appearing on another's page. |
| Scoped instances + the `request` | Anything bound per request. |
| Open DB transactions | A request that died mid-transaction would poison the next one. |
| Locale, log context, `Str` caches | Smaller drift, same class of bug. |
| `Livewire::flushState()` | Livewire's `hasRenderedScripts` singleton decides whether `@livewireScripts` emits its `<script>` tag. Left set, only the **first** response from each worker included `livewire.js` — and Alpine ships in that bundle, so every later page had `x-data`/`x-show`/`wire:` doing nothing, with a completely clean console. Nothing failed; the script was never there. |

If you maintain your own worker script, copy that function rather than writing your own
list — the session one in particular is not obvious, and its failure mode is silent.
`askr serve --paranoid` snapshots mutable state after each reset and reports anything that
keeps growing.

## How it works

The embed shim registers one PHP function:

```php
bool askr_handle_request(callable $handler)
```

Each call **blocks** until Askr delivers the next request, invokes
`$handler($request)` against the warm app, ships the captured output / headers /
status back to Rust, and returns `true` (or `false` when the worker is being
shut down). The worker is simply:

```php
$app = /* boot the framework once */;

while (askr_handle_request(function (array $request) use ($app) {
    // handle $request against the warm $app
    // echo body; header(...); http_response_code(...);
})) {
    // one request per iteration
}
```

The `$request` array Askr passes the handler:

| Key | Value |
| --- | --- |
| `method` | HTTP method (`GET`, `POST`, …) |
| `uri` | request URI incl. query string |
| `query` | raw query string |
| `headers` | the full CGI `$_SERVER` map (REQUEST_METHOD, HTTP_*, HTTPS, CONTENT_TYPE, …) |
| `body` | raw request body |

The handler produces its response the normal PHP way — `echo`/`print` for the
body, `header()` for headers, `http_response_code()` for the status — all
captured by the shim. Nothing is written to a socket by PHP.

## The Laravel worker

[`examples/laravel-worker.php`](../examples/laravel-worker.php) is a ready
template (the future `askr-laravel` package will generate and maintain it). It:

1. `require`s the autoloader and boots `bootstrap/app.php` **once**;
2. per request, builds a fresh `Illuminate\Http\Request` via `Request::create()`
   from the data Askr passes — no fragile PHP-superglobal surgery;
3. runs `$kernel->handle($request)`, emits the response via `header()`/`echo`,
   and `$kernel->terminate(...)`;
4. **resets per-request state** (below).

Point Askr at it and set the app base:

```bash
ASKR_APP_BASE=/var/www/app askr serve \
  --root /var/www/app/public \
  --worker-script /opt/askr/examples/laravel-worker.php \
  --workers "$(nproc)"
```

or in `askr.toml`:

```toml
[worker]
script = "/opt/askr/examples/laravel-worker.php"
app_base = "/var/www/app"
```

## State reset — no bleed between requests

A long-lived worker must not leak state across requests. `askr_reset_state()` in
the template performs an Octane-style reset after each request:

- `forgetScopedInstances()` — scoped bindings (and anything `scoped()`),
- forget the resolved `request`,
- `auth` → `forgetGuards()` so a prior request's user can't leak,
- roll back any DB transaction a request left open,
- flush `Str` caches.

This is verified with a deliberate bleed probe: a `scoped()` binding returns the
**same** id on every request *without* the reset (bleed) and a **distinct** id
*with* it (isolated). Under load: 500/500 requests `200`, zero errors. (The reset
stops per-request *bleed*; a slow framework-level memory *accumulation* remains —
see **Memory growth & recycling** below.)

> The full, framework-version-aware reset (covering every flow: sessions, auth,
> config sandboxing, …) will live in the `askr-laravel` package. The template
> covers the common sources of bleed; audit your app's own static/singleton
> state.

## Memory growth & recycling

A long-lived PHP worker can accumulate memory. We traced exactly where it comes
from — and it's more specific (and more fixable) than "the framework leaks":

- **Askr itself does not leak.** A minimal worker (no framework) held **flat at
  2 MB across 3,000,000+ requests** — the loop, the shim and the FFI boundary
  add nothing over time.
- **The dominant leak is `SESSION_DRIVER=array`.** We instrumented the worker and
  the only thing that grew linearly was the **array session handler's storage —
  one entry per request** (`memory_get_usage` and GC roots both tracked the
  request count exactly). The `array` driver keeps every session in the worker's
  heap and only garbage-collects *expired* ones, so under load — especially a
  cookie-less load test, where every request mints a *new* session — it grows
  without bound until PHP hits `memory_limit`. **Turn off the session middleware
  and the worker is flat at 8 MB over 60k+ requests.**

So the first fix is a **config fix**: don't use `array` sessions in a long-lived
worker. Pick a driver that doesn't pile sessions into the PHP heap. Each has a
trade-off, and Askr's own store is the one that wins on all of them:

| `SESSION_DRIVER` | Fast? | No heap leak? | No lock? | No extra server? |
| --- | :---: | :---: | :---: | :---: |
| `array` | ✅ | ❌ (OOMs) | ✅ | ✅ |
| `file` | ❌ (disk I/O/req) | ✅ | ✅ | ✅ |
| `database` (SQLite) | ❌ (write lock) | ✅ | ❌ | ✅ |
| `redis` | ✅ | ✅ | ✅ | ❌ |
| **`askr`** (shared memory) | ✅ | ✅ | ✅ | ✅ |

(The `askr` shared-memory session driver ships in the
[`askr-laravel`](../packages/laravel) package: `composer require
kwhorne/askr-laravel`, then `SESSION_DRIVER=askr`. Measured ~11–15k req/s with
**flat 8 MB per worker** and zero OOMs. `redis`/`file`/`database` also avoid the
heap leak.)

Beyond sessions, the residual per-request growth is tiny. Still, treat recycling
as the safety net — same as Octane, which defaults to `--max-requests=500`:

- **`--max-requests N`** — recycle each worker after N requests (staggered across
  workers so there's always a live one). The proactive, smooth option.
- **`--max-rss <MB>`** — *leak-aware, predictive* recycling (Linux). The
  supervisor samples each worker's RSS ~once a second and, when one crosses the
  cap, drains it gracefully and respawns a fresh one **before** it hits PHP's
  `memory_limit` and OOMs. Unlike a crash-and-respawn, this is zero-error: no
  `502`s at all. Set it with margin below `memory_limit` (e.g. `--max-rss 400`
  with a 512 MB limit). Measured: under a synthetic leak, RSS stayed bounded at
  ~230 MB against a 200 MB cap over 10 000+ requests with **0 OOMs and 0 errors**,
  where the same leak without it OOM-floods.
- **`--cow`** — CoW mode replaces a finished/dead worker with a **warm re-fork in
  ~ms** instead of a cold boot, so recycling is nearly free. Recommended for
  long-running deployments.
- **Resilience (0.8.3+)** — if a worker *does* exhaust `memory_limit` and PHP
  fatals, Askr exits that worker and the supervisor respawns a fresh one (with
  the triggering error logged), instead of the process getting stuck answering
  `502`s. So a leak degrades gracefully; it never floods. `--max-rss` is the
  proactive complement: recycle *before* the fatal.

The [`askr-laravel`](../packages/laravel) package ships the **`askr` session
driver** (shared-memory, lock-free, no heap growth — the only option that's fast
*and* serverless *and* leak-free), plus the `askr` cache store and queue
connector. `composer require kwhorne/askr-laravel` and set `SESSION_DRIVER=askr`.

## Is my app worker-safe? — `--paranoid`

Fear of state leaking between requests is the #1 reason people avoid the worker
model. Askr can tell you. Run with `--paranoid` (dev only) and it snapshots your
app's mutable state after each request's reset and reports anything that keeps
growing:

```
$ askr serve --root ./public --worker-script examples/laravel-worker.php \
    --workers 1 --paranoid
 WARN askr: paranoid mode ON — state-bleed detection (dev only)
[askr paranoid] baseline set after 2 requests — watching 95 app classes for state bleed
```

On a worker-safe app that's all you'll see — silence means clean. If something
leaks, you get the culprit and the growth, every request:

```
[askr paranoid] request #42 — state changed after reset (possible bleed):
  ↑ App\Services\Foo::$cache  array:2 → array:3  (+1)
```

How it works ([`examples/askr-paranoid.php`](../examples/askr-paranoid.php)):

- it reflects over your **app** classes (non-`vendor/`) and fingerprints their
  static properties, plus `$GLOBALS`, declared class/function counts, and (for
  Laravel) container bindings/instances;
- the first couple of requests establish a **baseline** (a framework fully boots
  on its first request, and services resolve lazily over the first few), so
  one-time warmup isn't reported as a leak;
- from then on it reports counters that **grew since the previous request** — a
  one-time bump when a singleton first resolves is normal and self-limiting;
  something that grows on *every* request is a leak.

It's expensive (reflection every request) — **dev only**, and use `--workers 1`
for readable output. Enable it in a config file with `[worker] paranoid = true`.

## Streaming responses

Output is normally buffered and sent as one response (so it can be cached and
compressed). But when a worker script calls `flush()` mid-request, Askr switches
that response to **streaming**: it sends the headers once, then each chunk as PHP
produces it — chunked transfer, no `Content-Length`. This makes Server-Sent Events,
a Symfony `StreamedResponse`, and large `readfile()`/export downloads work without
buffering the whole body in memory:

```php
header('Content-Type: text/event-stream');
while (true) {
    echo "data: " . json_encode($tick()) . "\n\n";
    flush();            // ← streams this chunk to the client now
    usleep(500_000);
}
```

Back-pressure is built in: the body channel is bounded, so a slow client pauses the
worker (like a blocking write under FPM) rather than growing memory. A response that
never calls `flush()` mid-request stays on the buffered path (cacheable, compressible).

## Recycling

Long-lived workers can still drift or leak over time (in app code or extensions).
Recycle them periodically with `--max-requests N` (or `[server] max_requests`):
each worker gracefully drains and exits after N requests and the master respawns
a fresh one. See [Deployment](DEPLOYMENT.md).

## Fixed in 1.4.5: file responses could end a worker's loop

**Askr-46 is fixed** — the section below is kept for anyone on 1.4.4 or earlier.
The cause was PHP's output layer keeping a "sent" flag across worker requests, which
turned a zlib INI check into an `ErrorException` on every file response after the first.
1.4.5 resets the output layer per request, treats `exit()` as end-of-request rather than
end-of-worker, and fails a request (500) whose exception escapes the handler instead of
letting it kill the worker.

**On 1.4.4 or earlier:** In one real Laravel
application, the request *following* a `BinaryFileResponse` ends the worker's loop —
costing roughly **one request in three with a single worker**, since a worker that leaves
its loop is replaced and the next request lands on a fresh one.

What is ruled out, and what is not:

| | |
|---|---|
| The transport | **Clean.** 619 KB through `echo`, through `readfile`, and through static serving all complete |
| A from-source Linux build | Reproduces **only** with the application in the picture |
| The contradiction that gave it away | PHP reported a normal completion while the Rust side said it never stopped handing over work. That was the clue: since PHP 8.0 `exit` is an *unwind* exit, so the script really had completed normally |

Until you can upgrade, two things reduce the exposure rather than fix it:

- **Run more than one worker** (the default is one per core), so a replaced worker is not
  the only one serving.
- **Serve downloads outside the app** where you can — a static path, or object storage —
  which avoids `BinaryFileResponse` entirely for the large files most likely to hit it.

As of 1.4.4 you can at least see it happening: a worker that dies mid-request answers
**502** rather than an empty 200, and the interpreter reports `rc`, `exit_status` and
PHP's last error whenever the loop ends.

## Writing your own worker

Any framework works — implement the same loop:

```php
<?php
$app = boot_my_framework();

while (askr_handle_request(function (array $r) use ($app) {
    $response = $app->handle($r['method'], $r['uri'], $r['headers'], $r['body']);
    http_response_code($response->status);
    foreach ($response->headers as $name => $value) {
        header("$name: $value", false);
    }
    echo $response->body;
    // reset per-request state here
})) {}
```

Guidelines:

- Boot everything expensive **before** the loop.
- Build request objects from the passed array — don't rely on PHP superglobals
  being refreshed.
- Reset per-request/scoped state at the end of each iteration.
- Avoid mutating global/static state that should be per-request.
- `STDIN`/`STDOUT`/`STDERR` constants are **not** defined (this is the embed
  SAPI, not CLI) — don't `fwrite(STDERR, …)`.
