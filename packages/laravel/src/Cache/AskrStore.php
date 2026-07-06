<?php

declare(strict_types=1);

namespace Askr\Laravel\Cache;

use Illuminate\Cache\CacheLock;
use Illuminate\Contracts\Cache\LockProvider;
use Illuminate\Contracts\Cache\Store;

/**
 * A Laravel cache store backed by Askr's shared-memory cache (`askr_cache_*`).
 *
 * Removes the Redis dependency for cache, rate limiting, atomic counters and
 * atomic locks (`Cache::lock`) — all in the Askr binary, on a single box.
 *
 * Sizes: values ≤ 4 KB use the main region (`--cache-slots`); larger values
 * (fragments, serialized collections, sessions) need the large region
 * (`--cache-large-slots`, up to 64 KB). Integers/floats are stored unserialized
 * so `increment()`/`decrement()` (the rate limiter) are truly atomic.
 */
final class AskrStore implements Store, LockProvider
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

    /** Atomic set-if-absent — makes Cache::add() and Cache::lock() truly atomic. */
    public function add($key, $value, $seconds)
    {
        $v = (is_int($value) || is_float($value)) ? (string) $value : serialize($value);

        return askr_cache_add($this->k($key), $v, (int) $seconds);
    }

    public function lock($name, $seconds = 0, $owner = null)
    {
        return new CacheLock($this, $name, $seconds, $owner);
    }

    public function restoreLock($name, $owner)
    {
        return $this->lock($name, 0, $owner);
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

    /** Update a key's TTL without touching its value (Laravel 11+). */
    public function touch($key, $seconds)
    {
        $v = askr_cache_get($this->k($key));
        if ($v === null) {
            return false;
        }

        return (bool) askr_cache_set($this->k($key), $v, (int) $seconds);
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
