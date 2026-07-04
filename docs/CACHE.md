# Shared cache

Askr ships a small **shared-memory cache** exposed to PHP. It's a fixed-slot hash
table living in an anonymous shared mmap the master creates before forking, so
every worker process sees the same physical table — no IPC, no locks on the hot
path beyond a per-slot spinlock.

This gives you **cache, atomic counters (rate limiting) and locks in the Askr
binary** — no Redis for small/mid deployments.

## Enabling it

```bash
askr serve --root ./public --worker-script examples/laravel-worker.php \
  --cache-slots 16384
```

or in `askr.toml`:

```toml
[cache]
slots = 16384   # each slot is ~4.3 KB; 16384 ≈ 70 MB
```

The cache is off when `slots = 0` (the default). Sizing: pick roughly
`expected_entries × 1.3`. Each slot holds one entry inline; keys are capped at
250 bytes and values at ~4 KB (larger values simply aren't cached).

## PHP API

When the cache is enabled, these functions are available to any PHP running under
Askr (web workers, the worker loop, and queue/scheduler sidecars):

| Function | |
| --- | --- |
| `askr_cache_get(string $key): ?string` | Value, or `null` on miss/expiry. |
| `askr_cache_set(string $key, string $value, int $ttl = 0): bool` | `ttl` seconds (`0` = forever). `false` if too large. |
| `askr_cache_delete(string $key): bool` | `true` if it existed. |
| `askr_cache_increment(string $key, int $delta = 1, int $ttl = 0): int` | Atomic add; returns the new value. |
| `askr_cache_flush(): void` | Empty the table. |

```php
askr_cache_set('greeting', 'hello', 60);
echo askr_cache_get('greeting');              // hello  (from any worker)
$n = askr_cache_increment('rate:'.$ip, 1, 60); // atomic across all workers
```

The counter is atomic across every worker process — ideal for rate limiting.

## Laravel cache driver

[`examples/AskrCacheStore.php`](../examples/AskrCacheStore.php) is a Laravel
`Store` backed by these functions. Register it in your worker script or a service
provider:

```php
use Illuminate\Support\Facades\Cache;

require '/opt/askr/examples/AskrCacheStore.php';

Cache::extend('askr', fn ($app) =>
    Cache::repository(new AskrCacheStore(config('cache.prefix', ''))));
```

Add a store in `config/cache.php` (or set `CACHE_STORE=askr`):

```php
'askr' => ['driver' => 'askr'],
```

Then use the cache normally — it's shared across all workers:

```php
Cache::put('user:1', $user, 300);
$user = Cache::get('user:1');
Cache::increment('hits');                       // atomic
Cache::remember('report', 600, fn () => build()); // computed once, shared
```

Integers/floats are stored unserialized so `increment()`/`decrement()` (used by
Laravel's rate limiter) are truly atomic in shared memory; other values are
serialized.

## Semantics & limits

- **Eviction:** on a hash collision beyond the probe window (16 slots), the
  colliding entry is overwritten. Sizing the table generously avoids this.
- **TTL** is lazy (checked on read) — expired entries free their slot on the next
  access to it.
- **Best-effort under crashes:** a per-slot spinlock is stolen if a holder dies
  mid-operation, so a crashed worker can't deadlock the table; a torn write can
  only yield a stale/garbage *value* (never memory unsafety — reads are length
  clamped), which a cache tolerates.
- **Not persistent:** the table lives in RAM and is empty on restart.

For very large values, many GB of data, or cross-host sharing, use a real cache
(Redis/Memcached) — Askr's cache targets the common single-host case.
