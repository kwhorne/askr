<?php

declare(strict_types=1);

namespace Askr\Laravel\Http\Middleware;

use Askr\Laravel\Cache\ModelDependencies;
use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * Cache a route in Askr's shared-memory response cache, tagged automatically with the
 * models it read:
 *
 *     Route::get('/products/{product}', ProductController::class)
 *         ->middleware('askr.cache:300');           // fresh 300s
 *
 *     Route::get('/', HomeController::class)
 *         ->middleware('askr.cache:300,60,86400');  // ttl, swr, stale-if-error
 *
 * There is no tag list to maintain. The response is tagged with every model it
 * actually read, so `$product->save()` clears exactly the pages that showed it —
 * across every worker, immediately.
 *
 * The middleware refuses to mark a response cacheable when it can tell the page isn't
 * shared. That's the layer that makes this safe to switch on:
 *
 *   - anything but a `GET`/`HEAD` returning 200;
 *   - an authenticated request (the page is somebody's);
 *   - a response that sets a cookie, or a request whose session was written to;
 *   - dependencies too numerous to express as tags (see ModelDependencies).
 *
 * The server has its own guards on top: only anonymous requests are cacheable at all,
 * `Set-Cookie` is stripped on store, and a response carrying more tags than an entry
 * holds is refused outright. Use `askr cache-report` first to find out which routes
 * are worth this and — more importantly — which are genuinely identical for every
 * visitor.
 */
final class CacheResponse
{
    public function __construct(private ModelDependencies $deps)
    {
    }

    public function handle(Request $request, Closure $next, string $ttl = '60', string $swr = '0', string $staleIfError = '0'): Response
    {
        $cacheable = $request->isMethodCacheable();
        if ($cacheable) {
            $this->deps->start();
        }

        /** @var Response $response */
        $response = $next($request);

        if (! $cacheable) {
            return $response;
        }
        $this->deps->stop();

        if (! $this->shouldCache($request, $response)) {
            return $response;
        }

        $tags = $this->deps->tags();
        if ($this->deps->overflowed()) {
            // Too many distinct model classes to name. Caching it would mean an entry
            // nothing can invalidate, so leave it uncached and say why once.
            return $response;
        }

        $directive = (string) max(0, (int) $ttl);
        if ((int) $swr > 0) {
            $directive .= ', swr='.(int) $swr;
        }
        if ((int) $staleIfError > 0) {
            $directive .= ', stale-if-error='.(int) $staleIfError;
        }
        if ($tags !== []) {
            $directive .= ', tags='.implode(',', $tags);
        }

        // Consumed by the server and never forwarded to the client.
        $response->headers->set('Askr-Cache', $directive);

        return $response;
    }

    /**
     * Everything that disqualifies a response from being shared with the next
     * visitor. Erring towards "don't cache" — a missed cache hit costs milliseconds,
     * a wrongly shared page costs trust.
     */
    private function shouldCache(Request $request, Response $response): bool
    {
        if ($response->getStatusCode() !== 200) {
            return false;
        }
        // The page belongs to someone.
        if ($request->user() !== null) {
            return false;
        }
        // A cookie means state was established for this client specifically. The
        // server would strip it and cache the body anyway, which is exactly the
        // situation to avoid.
        if (count($response->headers->getCookies()) > 0) {
            return false;
        }
        if ($request->hasSession()) {
            $session = $request->session();
            // A session that holds anything beyond its own bookkeeping means this
            // response may reflect it (flash messages, a cart, a CSRF-bound form).
            $keys = array_diff(array_keys($session->all()), ['_token', '_previous', '_flash']);
            if ($keys !== []) {
                return false;
            }
        }

        return true;
    }
}
