<?php

declare(strict_types=1);

namespace Askr\Laravel\Cache;

use Illuminate\Database\Eloquent\Model;

/**
 * Collects which Eloquent models a response actually read, so the cached page can be
 * tagged with them and invalidated the moment one of them changes.
 *
 * Writing cache tags by hand is the part people get wrong: a page that reads a
 * setting nobody remembered to tag goes stale, so teams stop trusting page caching
 * and turn it off. Askr can watch instead — Eloquent's `retrieved` event names every
 * model a request touched, and shared-memory tag invalidation makes forgetting them
 * an O(1) operation across every worker.
 *
 * A cached entry can hold a limited number of tags (8 — the server refuses to cache a
 * response with more, rather than storing one whose invalidation silently doesn't
 * work). So this class degrades deliberately:
 *
 *   - a request that touched few models is tagged per instance (`posts:3`) — precise,
 *     and only that page dies when that post changes;
 *   - a request that touched many is tagged per class (`posts`) — coarser, so any post
 *     changing clears it, but still correct;
 *   - a request that touched more *classes* than fit isn't tagged at all, and the
 *     caller doesn't cache it.
 *
 * Precision while it's cheap, safety when it isn't. Never a page that can't be
 * invalidated.
 */
final class ModelDependencies
{
    /** Tags an entry can hold; mirrors MAX_TAGS in the server's response cache. */
    public const MAX_TAGS = 8;

    /** @var array<string, true> `posts:3` => true */
    private array $instances = [];

    /** @var array<string, true> `posts` => true */
    private array $classes = [];

    private bool $collecting = false;

    /** Start watching. Called by the middleware only for cacheable routes. */
    public function start(): void
    {
        $this->instances = [];
        $this->classes = [];
        $this->collecting = true;
    }

    public function stop(): void
    {
        $this->collecting = false;
    }

    public function collecting(): bool
    {
        return $this->collecting;
    }

    /** Note that the response read this model. */
    public function record(Model $model): void
    {
        if (! $this->collecting) {
            return;
        }
        $class = self::classTag($model);
        $this->classes[$class] = true;

        $key = $model->getKey();
        // A model without a key (or with a composite one) can't be identified, so
        // fall back to its class: better coarse than wrong.
        if (is_int($key) || is_string($key)) {
            $this->instances[$class.':'.$key] = true;
        }
    }

    /**
     * The tags to put on the response, or an empty array when it shouldn't be cached.
     *
     * @return list<string>
     */
    public function tags(int $max = self::MAX_TAGS): array
    {
        if ($this->instances === [] && $this->classes === []) {
            // Nothing was read from the database — a static page. Nothing to
            // invalidate, so no tags: the TTL is the only thing keeping it fresh.
            return [];
        }
        if (count($this->instances) <= $max) {
            return array_keys($this->instances);
        }
        if (count($this->classes) <= $max) {
            return array_keys($this->classes);
        }

        return [];
    }

    /** True when there were dependencies but too many to express. */
    public function overflowed(int $max = self::MAX_TAGS): bool
    {
        return $this->classes !== [] && count($this->classes) > $max;
    }

    /**
     * Invalidate everything that read this model: its own pages, and the pages
     * tagged by class because they read too many to name individually.
     */
    public static function forget(Model $model): void
    {
        if (! function_exists('askr_cache_forget_tag')) {
            return;
        }
        $class = self::classTag($model);
        $key = $model->getKey();
        if (is_int($key) || is_string($key)) {
            askr_cache_forget_tag($class.':'.$key);
        }
        // Also the class tag: a listing page tagged `posts` must die when any post
        // changes, and a *new* post has no page of its own to invalidate.
        askr_cache_forget_tag($class);
    }

    /**
     * The tag for a model's class. The table name rather than the class name: it's
     * short, stable across namespace moves, and identical on both sides of this
     * exchange.
     */
    private static function classTag(Model $model): string
    {
        return $model->getTable();
    }
}
