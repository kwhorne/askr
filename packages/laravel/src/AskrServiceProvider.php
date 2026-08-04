<?php

declare(strict_types=1);

namespace Askr\Laravel;

use Askr\Laravel\Broadcasting\AskrBroadcaster;
use Askr\Laravel\Cache\AskrStore;
use Askr\Laravel\Cache\ModelDependencies;
use Askr\Laravel\Http\Middleware\CacheResponse;
use Askr\Laravel\Queue\AskrConnector;
use Askr\Laravel\Session\AskrSessionHandler;
use Illuminate\Contracts\Broadcasting\Factory as BroadcastingFactory;
use Illuminate\Database\Eloquent\Model;
use Illuminate\Support\ServiceProvider;

/**
 * Wires Askr's in-binary, shared-memory services into Laravel's driver system:
 *
 *   SESSION_DRIVER=askr        — sessions in shared memory (no heap leak, no lock, no server)
 *   CACHE_STORE=askr           — cache, counters, rate limiting, Cache::lock()
 *   QUEUE_CONNECTION=askr      — a job queue with reserve/visibility/retry/delay
 *   BROADCAST_CONNECTION=askr  — pub/sub for Laravel Echo (SSE / Pusher-compatible)
 *
 * It also provides the `askr.cache` middleware: page caching whose tags are derived
 * from the models a response read, so invalidation needs no bookkeeping.
 *
 * Each backend transparently gains a durable, replicated tier when the server
 * runs with the L2 SQL Anywhere backend (`--features sql-backend` +
 * `ASKR_{QUEUE,CACHE,BROADCAST}_DB`); the PHP-facing drivers are unchanged.
 *
 * Run Askr with the matching regions enabled:
 *
 *   askr serve --cache-slots 16384 --cache-large-slots 4096 --queue-slots 8192 …
 *
 * Registered automatically via Laravel package auto-discovery.
 */
final class AskrServiceProvider extends ServiceProvider
{
    public function register(): void
    {
        // One collector per request; the middleware turns it on and off.
        $this->app->singleton(ModelDependencies::class);

        $this->registerDefaultConnections();
    }

    /**
     * Give the cache store and queue connection a definition so the drivers are usable.
     *
     * Registering a driver with `extend()` is not enough: Laravel's CacheManager and
     * QueueManager look the *name* up in config first, so `CACHE_STORE=askr` used to fail
     * with "Cache store [askr] is not defined" — a message that says nothing about this
     * package or what to add, and which only appears on the first request that touches
     * the cache, so the app looks fine until it suddenly isn't.
     *
     * Only fills in what the application hasn't defined, so an app that sets its own
     * `cache.stores.askr` (a different prefix, say) keeps it.
     */
    private function registerDefaultConnections(): void
    {
        $config = $this->app['config'];

        if (! $config->get('cache.stores.askr')) {
            $config->set('cache.stores.askr', ['driver' => 'askr']);
        }

        if (! $config->get('queue.connections.askr')) {
            $config->set('queue.connections.askr', [
                'driver' => 'askr',
                'queue' => 'default',
                'retry_after' => 90,
                'after_commit' => false,
            ]);
        }

        // Sessions need no store definition — SESSION_DRIVER=askr works off the custom
        // creator alone — and broadcasting resolves through the factory, so neither is
        // listed here.
    }

    public function boot(): void
    {
        $this->registerAutomaticCacheTagging();

        // Session: SESSION_DRIVER=askr. The custom creator returns the handler;
        // Laravel's SessionManager wraps it in a Store for us.
        $this->app->make('session')->extend('askr', function ($app): AskrSessionHandler {
            return new AskrSessionHandler((int) $app['config']->get('session.lifetime', 120) * 60);
        });

        // Cache: CACHE_STORE=askr. The store definition is supplied by
        // registerDefaultConnections() unless the app defines its own.
        $this->app->make('cache')->extend('askr', function ($app) {
            return $app->make('cache')->repository(
                new AskrStore((string) $app['config']->get('cache.prefix', ''))
            );
        });

        // Queue: QUEUE_CONNECTION=askr. Register the connector on the queue manager
        // whenever it resolves (and now, if it already has).
        $this->app->resolving('queue', function ($manager): void {
            $manager->addConnector('askr', fn (): AskrConnector => new AskrConnector());
        });
        if ($this->app->resolved('queue')) {
            $this->app->make('queue')->addConnector('askr', fn (): AskrConnector => new AskrConnector());
        }

        // Broadcasting: BROADCAST_CONNECTION=askr. Register the driver on the
        // broadcast factory whenever it resolves (and now, if it already has).
        $register = function ($factory): void {
            $factory->extend('askr', fn (): AskrBroadcaster => new AskrBroadcaster());
        };
        $this->app->resolving(BroadcastingFactory::class, $register);
        if ($this->app->resolved(BroadcastingFactory::class)) {
            $register($this->app->make(BroadcastingFactory::class));
        }
    }

    /**
     * Watch Eloquent so cached pages can be tagged with what they read, and cleared
     * when it changes.
     *
     * `retrieved` names every model a request hydrated; the write events name what
     * just became stale. Both are wildcards, so this works for models the package has
     * never heard of — including ones in packages you didn't write.
     */
    private function registerAutomaticCacheTagging(): void
    {
        if (! function_exists('askr_cache_forget_tag')) {
            return; // not running under Askr; stay completely out of the way
        }

        $this->app->make('router')->aliasMiddleware('askr.cache', CacheResponse::class);

        $events = $this->app->make('events');

        // The read side is only active while the middleware is collecting, so a
        // request that isn't being cached pays one boolean check per hydrated model.
        $events->listen('eloquent.retrieved: *', function (string $event, array $models): void {
            $deps = $this->app->make(ModelDependencies::class);
            if (! $deps->collecting()) {
                return;
            }
            foreach ($models as $model) {
                if ($model instanceof Model) {
                    $deps->record($model);
                }
            }
        });

        // The write side always listens: a page cached by an earlier request has to
        // be invalidated even if the request doing the writing isn't cacheable at all
        // (a POST, a queue job, an artisan command).
        foreach (['created', 'updated', 'saved', 'deleted', 'restored', 'forceDeleted'] as $verb) {
            $events->listen("eloquent.{$verb}: *", function (string $event, array $models): void {
                foreach ($models as $model) {
                    if ($model instanceof Model) {
                        ModelDependencies::forget($model);
                    }
                }
            });
        }
    }
}
