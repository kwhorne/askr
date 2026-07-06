# Worker mode

Worker mode is where Askr wins big. Instead of running the front controller from
scratch on every request (per-request mode, like PHP-FPM), a long-lived **worker
script** boots the application **once** and then loops, serving every request
against the already-booted app — the Laravel Octane model, but entirely
in-process (no IPC).

On a real Laravel + Livewire app this drops per-request latency from ~110 ms to
~9 ms and roughly **9×**'s throughput.

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

A long-lived PHP worker gradually accumulates memory. We measured exactly where
it comes from:

- **Askr itself does not leak.** A minimal worker (no framework) held **flat at
  2 MB across 3,000,000+ requests** — the loop, the shim and the FFI boundary
  add nothing over time.
- **Laravel's framework accumulates ~1.5 KB per request** of *held* references
  (not cyclic garbage — forcing `gc_collect_cycles()` doesn't help) that the
  template's reset subset doesn't clear. This is inherent to running the
  framework long-lived, and it's why **Laravel Octane itself defaults to
  recycling workers** (`--max-requests=500`) rather than trying to zero it out.

So the practical guidance is the same as Octane's — **recycle**, don't chase a
perfect reset:

- **`--max-requests N`** — recycle each worker after N requests (staggered across
  workers so there's always a live one). The proactive, smooth option.
- **`--cow`** — CoW mode replaces a finished/dead worker with a **warm re-fork in
  ~ms** instead of a cold boot, so recycling is nearly free. Recommended for
  long-running deployments.
- **Resilience (0.8.3+)** — if a worker *does* exhaust `memory_limit` and PHP
  fatals, Askr exits that worker and the supervisor respawns a fresh one (with
  the triggering error logged), instead of the process getting stuck answering
  `502`s. So a leak degrades gracefully; it never floods.

The eventual `askr-laravel` package will carry an Octane-grade, version-aware
reset to push the per-request accumulation as close to zero as the framework
allows.

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

## Recycling

Long-lived workers can still drift or leak over time (in app code or extensions).
Recycle them periodically with `--max-requests N` (or `[server] max_requests`):
each worker gracefully drains and exits after N requests and the master respawns
a fresh one. See [Deployment](DEPLOYMENT.md).

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
