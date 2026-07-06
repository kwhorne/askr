<?php

declare(strict_types=1);

namespace Askr\Laravel\Session;

use SessionHandlerInterface;

/**
 * A session handler backed by Askr's shared-memory cache (`askr_cache_*`).
 *
 * This is the driver you want for the worker model. Unlike `array` (which piles
 * every session into the PHP worker's heap until it OOMs), `file` (a disk write
 * per request), or `database` (a SQLite/DB write lock), Askr sessions live in a
 * shared-memory region: **no heap growth, no lock, no external server**, and they
 * are shared across every worker process on the box.
 *
 * Enable the store with `askr serve --cache-slots N --cache-large-slots M`, then
 * set `SESSION_DRIVER=askr`. Registered automatically by {@see \Askr\Laravel\AskrServiceProvider}.
 */
final class AskrSessionHandler implements SessionHandlerInterface
{
    public function __construct(private int $lifetime = 7200, private string $prefix = 'askr:sess:')
    {
    }

    public function open($path, $name): bool
    {
        return true;
    }

    public function close(): bool
    {
        return true;
    }

    public function read($id): string
    {
        if (!\function_exists('askr_cache_get')) {
            return '';
        }
        $v = askr_cache_get($this->prefix . $id);

        return $v === null ? '' : (string) $v;
    }

    public function write($id, $data): bool
    {
        if (!\function_exists('askr_cache_set')) {
            return false;
        }

        return (bool) askr_cache_set($this->prefix . $id, $data, $this->lifetime);
    }

    public function destroy($id): bool
    {
        if (\function_exists('askr_cache_delete')) {
            askr_cache_delete($this->prefix . $id);
        }

        return true;
    }

    /** TTL on the shared-memory slot handles expiry; nothing to sweep. */
    public function gc($max): int|false
    {
        return 0;
    }
}
