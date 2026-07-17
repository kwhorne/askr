<?php

declare(strict_types=1);

namespace Askr\Laravel;

use Askr\Laravel\Broadcasting\AskrBroadcaster;
use Askr\Laravel\Cache\AskrStore;
use Askr\Laravel\Queue\AskrConnector;
use Askr\Laravel\Session\AskrSessionHandler;
use Illuminate\Contracts\Broadcasting\Factory as BroadcastingFactory;
use Illuminate\Support\ServiceProvider;

/**
 * Wires Askr's in-binary, shared-memory services into Laravel's driver system:
 *
 *   SESSION_DRIVER=askr        — sessions in shared memory (no heap leak, no lock, no server)
 *   CACHE_STORE=askr           — cache, counters, rate limiting, Cache::lock()
 *   QUEUE_CONNECTION=askr      — a job queue with reserve/visibility/retry/delay
 *   BROADCAST_CONNECTION=askr  — pub/sub for Laravel Echo (SSE / Pusher-compatible)
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
    public function boot(): void
    {
        // Session: SESSION_DRIVER=askr. The custom creator returns the handler;
        // Laravel's SessionManager wraps it in a Store for us.
        $this->app->make('session')->extend('askr', function ($app): AskrSessionHandler {
            return new AskrSessionHandler((int) $app['config']->get('session.lifetime', 120) * 60);
        });

        // Cache: CACHE_STORE=askr (add 'askr' => ['driver' => 'askr'] to config/cache.php,
        // or set CACHE_STORE=askr with the default store definition).
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
}
