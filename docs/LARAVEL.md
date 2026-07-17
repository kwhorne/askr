# Laravel on Askr — the recommended setup

This is the end-to-end guide to running a Laravel application on Askr with the
official [`kwhorne/askr-laravel`](https://packagist.org/packages/kwhorne/askr-laravel)
package. Follow it top to bottom and you get a single-box (or multi-box) stack
with **no Redis, no Horizon, no separate WebSocket server, and no cron** — session,
cache, locks, queue *and* broadcasting all served from the Askr binary.

> New to Askr's model? Skim [Worker mode](WORKER_MODE.md) first — Laravel boots
> once and serves many requests (Octane-style), so a few things behave differently
> from PHP-FPM.

---

## What the package gives you

| `.env` | Replaces | Backed by |
| --- | --- | --- |
| `SESSION_DRIVER=askr` | Redis / DB / `file` sessions | shared-memory large region |
| `CACHE_STORE=askr` | Redis cache, counters, rate limiting, `Cache::lock()` | shared-memory cache |
| `QUEUE_CONNECTION=askr` | Redis / DB queues + Horizon | shared-memory job queue + supervised workers |
| `BROADCAST_CONNECTION=askr` | Redis pub/sub + Reverb/Pusher | in-binary pub/sub + Pusher-compatible WS |

Each driver is registered automatically by `Askr\Laravel\AskrServiceProvider`
(Laravel package auto-discovery) — no manual wiring. They all call Askr's
`askr_*` builtins, which exist only when the app is **served by Askr** with the
matching shared-memory regions enabled (see [Regions & sizing](#regions--sizing)).

Two tiers are available per driver:

- **L1 (default)** — shared memory, mapped before fork, shared across every worker
  on the box. Fast, lock-free-ish, and gone on reboot.
- **L2 (optional)** — a durable, replicated backend over SQL Anywhere, selected at
  runtime with `ASKR_*_DB`. Same PHP API, only the backend differs. See
  [Durable L2](#durable-l2-optional).

---

## Requirements

- **Askr ≥ 0.9.3** (broadcasting driver + the complete Laravel surface).
- **PHP 8.5** (bundled in the Askr release/Docker image).
- **Laravel 11, 12 or 13.**
- A worker-mode deployment (`--worker-script`). Per-request mode works too, but the
  session/cache/queue regions and the recommended performance all assume workers.

---

## 1. Install

```bash
composer require kwhorne/askr-laravel
```

That's it — the service provider is auto-discovered. Nothing to add to
`config/app.php`.

---

## 2. Configure `.env`

```dotenv
SESSION_DRIVER=askr
CACHE_STORE=askr
QUEUE_CONNECTION=askr
BROADCAST_CONNECTION=askr
```

## 3. Register the stores/connections

Add the matching definitions so Laravel knows what `askr` means. (Cache and
session usually resolve from the driver alone, but being explicit is clearer and
required for queue + broadcasting.)

```php
// config/cache.php  →  'stores'
'askr' => ['driver' => 'askr'],
```

```php
// config/queue.php  →  'connections'
'askr' => [
    'driver'      => 'askr',
    'queue'       => 'default',
    'retry_after' => 90,
],
```

```php
// config/broadcasting.php  →  'connections'
'askr' => ['driver' => 'askr'],
```

Sessions ride on the cache large region, so no extra config is needed beyond
`SESSION_DRIVER=askr` — just make sure Askr runs with `--cache-large-slots`
(below).

---

## 4. The runner scripts

Askr serves your app through small **runner scripts** that boot Laravel once and
then handle requests/jobs in a loop. Copy the three bundled examples into your app
so they're versioned with your code:

```bash
mkdir -p askr
cp /opt/askr/examples/laravel-worker.php   askr/worker.php     # HTTP
cp /opt/askr/examples/askr-queue.php       askr/queue.php      # queue:work
cp /opt/askr/examples/askr-scheduler.php   askr/schedule.php   # schedule:run
```

(The Docker image ships them under `/opt/askr/examples`; the release tarball under
`<prefix>/examples`.) They read `ASKR_APP_BASE` to find your app root and reset
per-request state between requests. See [Worker mode](WORKER_MODE.md) if you want
to customise the reset.

---

## 5. Run Askr

### Development

```bash
ASKR_APP_BASE="$PWD" askr serve \
  --root public \
  --worker-script askr/worker.php \
  --workers 2 \
  --cache-slots 8192 --cache-large-slots 4096 \
  --queue-slots 8192 \
  --pusher \
  --listen 127.0.0.1:8000
```

### Production (the full stack in one command)

```bash
ASKR_APP_BASE=/srv/app askr serve \
  --root public \
  --worker-script askr/worker.php \
  --workers auto \
  --max-rss 400 \
  --cache-slots 16384 --cache-large-slots 4096 \
  --queue-slots 8192 \
  --queue 1 --queue-max 8 --queue-script askr/queue.php \
  --scheduler-script askr/schedule.php \
  --pusher \
  --admin 127.0.0.1:9090 \
  --acme --acme-domain app.example.com --acme-email ops@example.com \
  --listen 0.0.0.0:443
```

One process tree now covers: HTTP, cache, sessions, locks, the job queue **and its
autoscaled workers**, the scheduler, broadcasting, TLS, and a metrics/admin plane.
Prefer a config file in production — see [Configuration](CONFIGURATION.md) for the
`askr.toml` equivalent, and [Ubuntu setup](UBUNTU.md) for the systemd unit.

---

## Regions & sizing

The shared-memory regions are fixed at startup and evict oldest-first when full.
Size them for your peak concurrency:

| Flag | Backs | Guidance |
| --- | --- | --- |
| `--cache-slots N` | cache, counters, rate limiting, `Cache::lock()` (≤ 4 KB values) | a few × your working set of keys |
| `--cache-large-slots N` | **sessions**, cache fragments, serialized collections (≤ 64 KB) | ≈ your peak concurrent session count |
| `--queue-slots N` | the job queue (`askr_queue_*`) | ≥ your peak pending + delayed jobs (32 KB/slot) |

Integers/floats are stored unserialized, so `Cache::increment()` (the rate
limiter) is truly atomic across all workers.

---

## Queue workers & autoscaling

`--queue N` runs N supervised queue-worker processes. Add `--queue-max M` to turn
it into a **backlog-driven autoscaling range** — Horizon's `balance=auto`, native,
with no extra daemon (Askr sees the backlog in shared memory *and* owns the worker
pool):

```bash
--queue 1 --queue-max 8 --queue-script askr/queue.php
```

On a burst the pool jumps toward the target (~1 worker per 10 ready jobs), then
drains one worker every couple of seconds as the backlog clears — gracefully
(scaled-down workers finish their current job, then exit). Watch it on
`/metrics`: `askr_queue_workers`, `askr_queue_ready`, `askr_queue_total`,
`askr_queue_oldest_seconds`.

Named queues / priority work as usual — set `--queue-script` to a runner that does
`queue:work --queue=high,default,low`.

---

## Scheduler

`--scheduler-script askr/schedule.php` runs Laravel's scheduler in-process — no
system cron entry. It's a supervised sidecar; it's respawned if it dies.

---

## Broadcasting (Laravel Echo)

With `BROADCAST_CONNECTION=askr` and `--pusher`, `broadcast(new Event())` publishes
a Pusher-shaped frame through Askr's in-binary pub/sub, and Askr's
Pusher-compatible WebSocket fan-out delivers it to Echo clients — **no Reverb, no
Pusher account, no Redis**.

Point Laravel Echo at Askr (it speaks the Pusher protocol):

```js
import Echo from 'laravel-echo';
import Pusher from 'pusher-js';
window.Pusher = Pusher;

window.Echo = new Echo({
    broadcaster: 'pusher',
    key: 'askr',                       // any non-empty key
    wsHost: window.location.hostname,
    wsPort: 443,                       // your Askr listen port
    forceTLS: true,
    enabledTransports: ['ws', 'wss'],
    disableStats: true,
});
```

Public channels work out of the box. Private/presence channels use Laravel's
standard channel authorization (`routes/channels.php`) — no extra config. There's
also a plain **SSE** endpoint (`GET /askr/events?channel=NAME`) if you'd rather not
use Echo; see [Broadcasting](BROADCAST.md).

---

## Durable L2 (optional)

For durability across restarts or multiple boxes, enable the SQL Anywhere L2 tier.
It's behind a build feature, so the default build is unaffected:

```bash
# build once with the feature
cargo build --release --features sql-backend
```

```dotenv
# select per subsystem at runtime; unset falls back to L1 shared memory
ASKR_CACHE_DB=/var/lib/askr/cache.db
ASKR_QUEUE_DB=/var/lib/askr/queue.db
ASKR_BROADCAST_DB=/var/lib/askr/events.db
```

The PHP API and Laravel drivers don't change — only the backend does. When both L1
and L2 are on, L1 becomes a **write-through read cache** in front of L2: hot reads
skip the database round-trip, writes go to L2 (the source of truth). Queue
autoscaling reads its backlog from L2 automatically. See
[Storage backends](STORAGE_BACKEND.md).

---

## Production checklist

- **Sessions:** `--cache-large-slots` ≥ peak concurrent sessions. Never use
  `SESSION_DRIVER=array` in workers — it leaks into the PHP heap until OOM.
- **Leak safety:** set `--max-rss <MB>` (well below `memory_limit`) so a worker is
  recycled gracefully *before* it OOMs, and/or use `--cow` for ~ms warm respawns.
- **Config/route/view cache:** run `php artisan config:cache route:cache view:cache`
  at deploy, then reload Askr (`SIGHUP`) so workers pick up the new code.
- **TLS:** `--acme` gets and renews Let's Encrypt certs with no proxy
  ([Auto-TLS](AUTOTLS.md)).
- **Hardening:** `--sandbox` (seccomp no-exec) and `--sandbox-write storage /tmp`
  (Landlock) shrink the blast radius of an RCE ([Sandbox](SANDBOX.md), Linux).
- **Observability:** `--admin 127.0.0.1:9090` exposes `/metrics` (Prometheus) and a
  dashboard ([Admin](ADMIN.md)).

---

## Verify it works

```bash
# 1) the package resolves + installs from Packagist
composer require kwhorne/askr-laravel

# 2) drivers are live (with --admin on): the queue gauges appear
curl -s http://127.0.0.1:9090/metrics | grep askr_queue_

# 3) a cache round-trip from tinker (while served by Askr)
php artisan tinker --execute="Cache::put('k','v',60); echo Cache::get('k');"
```

`doctor` also checks the required extension set:

```bash
askr doctor --root public
```

---

## Migrating from Redis

| You had | You now set | Notes |
| --- | --- | --- |
| `CACHE_STORE=redis` | `CACHE_STORE=askr` | counters/rate limiter/`Cache::lock()` all carry over |
| `SESSION_DRIVER=redis` | `SESSION_DRIVER=askr` | needs `--cache-large-slots` |
| `QUEUE_CONNECTION=redis` + Horizon | `QUEUE_CONNECTION=askr` + `--queue`/`--queue-max` | autoscaling replaces Horizon `balance=auto` |
| `BROADCAST_CONNECTION=reverb`/`pusher` | `BROADCAST_CONNECTION=askr` + `--pusher` | Echo config points at Askr |
| `redis-server`, `horizon`, `reverb`, `cron` | *(nothing)* | all folded into the Askr process tree |

Drop Redis entirely on a single box; reach for the **L2 SQL Anywhere** tier when
you need durability or more than one box.

---

## Troubleshooting

- **`Driver [askr] not supported`** — the app wasn't served by Askr (the `askr_*`
  builtins are absent), or the region for that driver isn't enabled. Check you
  passed `--cache-slots` / `--queue-slots` etc., and that you're running under
  `askr serve`, not `php artisan serve`.
- **Sessions not persisting / evicted** — `--cache-large-slots` is too small for
  your concurrency; increase it.
- **`php artisan …` outside Askr** — CLI commands run under system PHP where the
  `askr_*` builtins don't exist; the session driver degrades to a no-op and the
  cache/queue drivers will error if resolved. Don't point long-running CLI at the
  `askr` drivers — run queue/schedule through Askr's runner scripts instead.
- **Broadcasting silent** — confirm `--pusher` is set and the Echo `wsPort`/host
  match your `--listen`.

See also: [Worker mode](WORKER_MODE.md) · [Shared cache](CACHE.md) ·
[Broadcasting](BROADCAST.md) · [Storage backends](STORAGE_BACKEND.md) ·
[Deployment](DEPLOYMENT.md).
