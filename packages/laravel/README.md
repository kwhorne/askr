# askr-laravel

Laravel integration for the [Askr](https://github.com/kwhorne/askr) application
server. It wires Askr's **in-binary, shared-memory** services into Laravel's
driver system, so a single-box app needs **no Redis**:

| Driver | What it replaces |
| --- | --- |
| `SESSION_DRIVER=askr` | Redis / DB / `file` sessions |
| `CACHE_STORE=askr` | Redis cache, counters, rate limiting, `Cache::lock()` |
| `QUEUE_CONNECTION=askr` | Redis / DB queues |
| `BROADCAST_CONNECTION=askr` | Redis pub/sub + a WebSocket server (Laravel Echo) |

### Durable, replicated (multi-box)

The drivers above are unchanged whether the server uses the L1 shared-memory
tier (single box, ephemeral) or the durable, replicated **L2 SQL Anywhere**
tier. Run the server built with `--features sql-backend` and set
`ASKR_QUEUE_DB` / `ASKR_CACHE_DB` / `ASKR_BROADCAST_DB` to a database path (an
embedded file, an embedded replica, or a `sqld`-managed database) to get durable
jobs, a shared/edge cache, and cross-node broadcasting — no app changes.

## Why the session driver matters

Running Laravel in a long-lived worker (Octane-style, which is how Askr serves)
exposes a real trap: **`SESSION_DRIVER=array` leaks** — the array handler keeps
every session in the PHP heap until it hits `memory_limit`. The alternatives each
give something up. Askr's shared-memory driver gives up nothing:

| `SESSION_DRIVER` | Fast? | No heap leak? | No lock? | No extra server? |
| --- | :---: | :---: | :---: | :---: |
| `array` | ✅ | ❌ (OOMs) | ✅ | ✅ |
| `file` | ❌ | ✅ | ✅ | ✅ |
| `database` (SQLite) | ❌ | ✅ | ❌ | ✅ |
| `redis` | ✅ | ✅ | ✅ | ❌ |
| **`askr`** | ✅ | ✅ | ✅ | ✅ |

Measured: **~11–15k req/s with flat 8 MB per worker** (sessions live in shared
memory, not the heap), persisting across every worker process.

## Install

```bash
composer require kwhorne/askr-laravel
```

The service provider is auto-discovered — no manual registration.

## Configure

```dotenv
SESSION_DRIVER=askr
CACHE_STORE=askr
QUEUE_CONNECTION=askr
```

Add the cache/queue store definitions (or rely on the defaults):

```php
// config/cache.php → 'stores'
'askr' => ['driver' => 'askr'],

// config/queue.php → 'connections'
'askr' => ['driver' => 'askr', 'queue' => 'default', 'retry_after' => 90],
```

## Run

Start Askr with the matching shared-memory regions enabled:

```bash
askr serve \
  --root public --worker-script vendor/askr/worker.php \
  --workers auto \
  --cache-slots 16384 --cache-large-slots 4096 \
  --queue-slots 8192
```

- `--cache-slots` — the small region (≤ 4 KB values: counters, locks, cache).
- `--cache-large-slots` — the large region (up to 64 KB: sessions, fragments).
- `--queue-slots` — the job queue.

Run queue workers in the same binary:

```bash
askr serve … --queue 4 --queue-script vendor/laravel/framework/… # or artisan queue:work
```

## Notes

- These drivers call Askr's `askr_cache_*` / `askr_queue_*` builtins, which exist
  only when the app is served by Askr with the regions enabled. Under a plain
  `php artisan` invocation the session driver degrades to a no-op; don't point a
  non-Askr process at these drivers for real work.
- The shared-memory regions are sized at startup and evict oldest-first when
  full — size `--cache-large-slots` for your peak concurrent session count.

MIT © Knut W. Horne
