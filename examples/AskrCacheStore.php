<?php

/**
 * A Laravel cache Store backed by Askr's shared-memory cache (`askr_cache_*`).
 *
 * Enable the cache with `askr serve --cache-slots N` (or `[cache] slots`), then
 * register this store in your worker script (see examples/laravel-worker.php)
 * or a service provider:
 *
 *     use Illuminate\Support\Facades\Cache;
 *     require '/opt/askr/examples/AskrCacheStore.php';
 *     Cache::extend('askr', fn ($app) =>
 *         Cache::repository(new AskrCacheStore(config('cache.prefix', ''))));
 *
 * Then add a store to config/cache.php (or set CACHE_STORE=askr):
 *
 *     'askr' => ['driver' => 'askr'],
 *
 * This removes the Redis dependency for cache, rate limiting and atomic counters
 * in small/mid deployments — it all lives in the Askr binary.
 *
 * Note: values are capped at ~4 KB (larger values simply aren't cached).
 * Integers/floats are stored unserialized so `increment()`/`decrement()` (used
 * by the rate limiter) are truly atomic in shared memory.
 */
final class AskrCacheStore implements Illuminate\Contracts\Cache\Store
{
    public function __construct(private string $prefix = '')
    {
    }

    private function k(string $key): string
    {
        return $this->prefix . $key;
    }

    public function get($key)
    {
        $v = askr_cache_get($this->k($key));
        if ($v === null) {
            return null;
        }
        return is_numeric($v) ? $v + 0 : unserialize($v);
    }

    public function many(array $keys)
    {
        $out = [];
        foreach ($keys as $key) {
            $out[$key] = $this->get($key);
        }
        return $out;
    }

    public function put($key, $value, $seconds)
    {
        $v = (is_int($value) || is_float($value)) ? (string) $value : serialize($value);
        return askr_cache_set($this->k($key), $v, (int) $seconds);
    }

    public function putMany(array $values, $seconds)
    {
        $ok = true;
        foreach ($values as $key => $value) {
            $ok = $this->put($key, $value, $seconds) && $ok;
        }
        return $ok;
    }

    public function increment($key, $value = 1)
    {
        return askr_cache_increment($this->k($key), (int) $value, 0);
    }

    public function decrement($key, $value = 1)
    {
        return askr_cache_increment($this->k($key), -(int) $value, 0);
    }

    public function forever($key, $value)
    {
        return $this->put($key, $value, 0);
    }

    public function forget($key)
    {
        return askr_cache_delete($this->k($key));
    }

    public function flush()
    {
        askr_cache_flush();
        return true;
    }

    public function getPrefix()
    {
        return $this->prefix;
    }
}
