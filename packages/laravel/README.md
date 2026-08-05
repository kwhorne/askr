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


## `Driver [askr] not supported`

`SESSION_DRIVER=askr` (or `CACHE_STORE`/`QUEUE_CONNECTION`) without this package
installed. The drivers come from here, not from the server:

```bash
composer require kwhorne/askr-laravel
php artisan package:discover      # only needed if you installed with --no-scripts
```

In worker mode the failure is a 500 on every route that touches the driver, and Askr's log
names it: `uncaught InvalidArgumentException escaped the request handler: Driver [askr] not
supported.`

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

## Automatic page caching (`askr.cache`)

> **The server needs a response cache for this to do anything.** Start Askr with
> `--response-cache 512` (or `[cache] response_slots = 512`). Without it the middleware
> still sets its header, the server has nowhere to put the page, and nothing is cached —
> which looks like the middleware not working. `X-Askr-Cache` on the response tells you
> which it is: `MISS`/`HIT` means the cache is on, no header at all means it isn't.


Page caching is rare in Laravel not because it's slow to set up, but because keeping
the tags right is a job nobody wants: one forgotten dependency serves stale content,
so teams switch it off. Askr can watch instead.

```php
Route::get('/products/{product}', ProductController::class)
    ->middleware('askr.cache:300');              // fresh 300s

Route::get('/', HomeController::class)
    ->middleware('askr.cache:300,60,86400');     // ttl, swr, stale-if-error
```

There is no tag list. The middleware records every Eloquent model the response read
(via the `retrieved` event) and tags the cached page with them, so `$product->save()`
clears exactly the pages that showed that product — across every worker, immediately.

**Precision while it's cheap, safety when it isn't.** A cached entry holds up to 8
tags, so:

| The response read | It's tagged | A change clears |
| --- | --- | --- |
| a few models | per instance (`products:42`) | only the pages showing that product |
| many models | per class (`products`) | every page that listed products |
| more classes than fit | nothing — and it isn't cached | — |

The last row matters: Askr **refuses** to cache a response carrying more tags than an
entry holds, rather than storing one whose invalidation silently doesn't work. A page
you can't invalidate is worse than a page you didn't cache.

Creating a model clears the class tag too — a brand-new product has no page of its own
to invalidate, but the listing that should now include it does.

### When it declines to cache

The middleware only marks a response cacheable when it can tell the page is shared:

- the request is a `GET`/`HEAD` returning 200;
- nobody is authenticated (`$request->user() === null`);
- the response sets no cookie, and the session holds nothing beyond its own
  bookkeeping (a flash message, a cart, a session-bound form all count as "personal").

The server adds its own guards underneath: only anonymous requests are cacheable at
all, and `Set-Cookie` is stripped on store.

### Find out what's worth caching first

```bash
askr serve --traffic-log /tmp/traffic.jsonl   # run for an hour
askr cache-report /tmp/traffic.jsonl
```

That reports the hit rate each route would reach, the PHP time it would save, and —
the part that matters — whether the page was **byte-identical for every visitor**
during the sample. Add `askr.cache` to the routes it calls safe.
