<?php

/**
 * Askr worker script for a real Laravel application (A4b).
 *
 * Boots the Laravel app ONCE, then serves every request against the already
 * booted app — the Octane model, entirely in-process (no FastCGI, no IPC). This
 * eliminates the per-request framework bootstrap (~110 ms on a typical app).
 *
 * Usage:
 *   ASKR_APP_BASE=/path/to/app \
 *     askr serve --root /path/to/app/public \
 *                --worker-script /path/to/askr/examples/laravel-worker.php \
 *                --workers 8 --https
 *
 * This is a hand-written template; the future `askr-laravel` package will
 * generate and maintain it (with production-grade state reset between requests).
 *
 * Key design choice: instead of refreshing PHP superglobals between requests
 * (fragile Zend surgery), we build a fresh Illuminate\Http\Request from the
 * request data Askr hands us via `askr_handle_request($handler)`. The `headers`
 * entry Askr passes is the full CGI $_SERVER map, so it maps straight onto
 * Request::create()'s $server argument.
 */

define('LARAVEL_START', microtime(true));

$base = getenv('ASKR_APP_BASE') ?: dirname(__DIR__);

require $base . '/vendor/autoload.php';

/** @var \Illuminate\Foundation\Application $app */
$app = require $base . '/bootstrap/app.php';

/** @var \Illuminate\Contracts\Http\Kernel $kernel */
$kernel = $app->make(Illuminate\Contracts\Http\Kernel::class);

// State-bleed detector (dev only), enabled by `askr serve --paranoid`.
$paranoid = null;
if (getenv('ASKR_PARANOID')) {
    require __DIR__ . '/askr-paranoid.php';
    $paranoid = new AskrParanoid($base, $app);
}

$requestNo = 0;

$handler = function (array $r) use ($app, $kernel): int {
    // Askr passes the CGI $_SERVER map as `headers`.
    $server = $r['headers'];

    $query = [];
    if (!empty($r['query'])) {
        parse_str($r['query'], $query);
    }

    $cookies = [];
    if (!empty($server['HTTP_COOKIE'])) {
        foreach (explode('; ', $server['HTTP_COOKIE']) as $pair) {
            $kv = explode('=', $pair, 2);
            if (count($kv) === 2) {
                $cookies[urldecode($kv[0])] = urldecode($kv[1]);
            }
        }
    }

    // Multipart uploads: Askr streamed each file to a temp path and parsed the
    // form fields. Rebuild them as Laravel UploadedFile instances in *test* mode
    // so ->store()/->move() use rename() instead of move_uploaded_file() (the
    // request didn't go through PHP's rfc1867 handler). This is the Octane model.
    $post = $r['post'] ?? [];

    // Askr parses multipart bodies (it has to, to stream the files to disk), but a
    // plain `application/x-www-form-urlencoded` body arrives as raw bytes — and
    // Request::create() fills the POST bag from its $parameters argument only, never
    // from $content. Without this, every classic HTML form post loses its fields: the
    // visible symptom is a 419 on submit, because _token isn't there to compare, and
    // the invisible one is $request->input('email') being empty.
    //
    // Only urlencoded bodies are parsed. Multipart is already done, JSON is decoded by
    // Laravel itself on demand, and anything else must stay untouched — a webhook that
    // verifies a signature over the raw body would break if we reinterpreted it.
    if ($post === [] && $r['body'] !== '' && $r['body'] !== null) {
        $type = strtolower($server['CONTENT_TYPE'] ?? '');
        if (str_starts_with($type, 'application/x-www-form-urlencoded')) {
            parse_str($r['body'], $post);
        }
    }
    $files = [];
    foreach ($r['files'] ?? [] as $f) {
        $uploaded = new Illuminate\Http\UploadedFile(
            $f['tmp_name'],
            $f['name'],
            $f['type'] ?: null,
            $f['error'] ?? 0,
            true // test mode
        );
        $field = $f['field'];
        if (str_ends_with($field, '[]')) {
            $files[substr($field, 0, -2)][] = $uploaded;
        } else {
            $files[$field] = $uploaded;
        }
    }

    $request = Illuminate\Http\Request::create(
        $r['uri'],
        $r['method'],
        array_merge($query, $post), // query + parsed multipart fields
        $cookies,
        $files,                     // $request->file('avatar') now works
        $server,
        $r['body']
    );

    $response = $kernel->handle($request);

    // Emit the response — header()/echo are captured by Askr's SAPI shim.
    http_response_code($response->getStatusCode());
    foreach ($response->headers->allPreserveCaseWithoutCookies() as $name => $values) {
        foreach ((array) $values as $value) {
            header($name . ': ' . $value, false);
        }
    }
    foreach ($response->headers->getCookies() as $cookie) {
        header('Set-Cookie: ' . $cookie->__toString(), false);
    }
    // A plain Response hands us a string. A BinaryFileResponse or StreamedResponse —
    // `response()->file()`, `->stream()`, `->streamDownload()`, `Storage::download()` —
    // returns *false* from getContent() and produces its body in sendContent() instead.
    // Echoing false printed nothing, so those routes answered 200 with an empty body.
    // That's how Flux UI's /flux/flux.js arrived as 0 bytes, which silently killed dark
    // mode and every other piece of Flux interactivity in a real app; file downloads and
    // streamed exports were empty the same way.
    $content = $response->getContent();
    if ($content === false) {
        $response->sendContent();
    } else {
        echo $content;
    }

    $kernel->terminate($request, $response);

    askr_reset_state($app);

    return $response->getStatusCode();
};

/**
 * Reset per-request state so the long-lived worker doesn't bleed data between
 * requests (an Octane-style subset). The future `askr-laravel` package will
 * own the full, framework-version-aware reset.
 */
function askr_reset_state($app): void
{
    // Scoped instances (request, and anything bound via scoped()).
    if (method_exists($app, 'forgetScopedInstances')) {
        $app->forgetScopedInstances();
    }

    // Drop the resolved request so the next one is fresh.
    $app->forgetInstance('request');

    // Auth: forget resolved guards so a user from a prior request can't leak.
    if ($app->resolved('auth')) {
        $app->make('auth')->forgetGuards();
    }

    // Session: forgetting the guards is not enough on its own. SessionManager caches
    // the *driver*, and the driver holds the loaded session id and payload — so the
    // next request reused the previous visitor's session and resolved their user, even
    // with no cookie sent at all. Observed in a real app: after one login, anonymous
    // requests landing on that worker were redirected to the dashboard as that user.
    if ($app->resolved('session')) {
        $app->make('session')->forgetDrivers();
    }

    // …and `session.store` with it. This is the one that actually bit: it's a *separate*
    // singleton binding holding the Store instance, so forgetDrivers() alone left the
    // previous visitor's loaded session in the container. SessionGuard is constructed
    // with session.store, so a brand-new guard built from a brand-new driver still
    // resolved the old user. Measured in a real app: after one login, 6 of 6 anonymous
    // requests with no cookie were served as that user.
    $app->forgetInstance('session.store');

    // Cookies queued for the response we just sent. Left in place they are attached to
    // the *next* response — which for a session cookie means handing one visitor's
    // session to another.
    if ($app->resolved('cookie')) {
        $cookies = $app->make('cookie');
        if (method_exists($cookies, 'flushQueuedCookies')) {
            $cookies->flushQueuedCookies();
        }
    }

    // View state: shared data and, importantly, the shared $errors bag — otherwise one
    // visitor's validation errors show up on another's page.
    if ($app->resolved('view')) {
        $view = $app->make('view');
        if (method_exists($view, 'flushState')) {
            $view->flushState();
        }
    }

    // Locale, if a request switched it.
    if ($app->resolved('translator')) {
        $translator = $app->make('translator');
        $locale = $app->make('config')->get('app.locale');
        if ($locale !== null && method_exists($translator, 'setLocale')) {
            $translator->setLocale($locale);
        }
    }

    // Livewire keeps per-request state in container singletons, and the most visible one
    // decides whether `@livewireScripts` emits its <script> tag at all: after the first
    // response from a worker, `hasRenderedScripts` stayed true and every later page went
    // out WITHOUT livewire.js. Alpine ships in that bundle, so on those pages every
    // `x-data`/`x-show`/`wire:` silently did nothing — and the console was clean, because
    // nothing failed; the script was simply never there. A Flux dark-mode toggle showed
    // both its sun and moon icons at once, which is what finally gave it away.
    //
    // Livewire already knows how to reset this; it just has to be asked. `flushState()`
    // fires its `flush-state` hook, which is what Octane relies on too.
    if ($app->bound('livewire') && $app->resolved('livewire')) {
        $app->make('livewire')->flushState();
    }

    // Per-request log context (Laravel 10+), so one request's context doesn't annotate
    // the next one's lines.
    if ($app->resolved('log')) {
        $log = $app->make('log');
        if (method_exists($log, 'withoutContext')) {
            $log->withoutContext();
        }
    }

    // Database: roll back any transaction a request left open.
    if ($app->resolved('db')) {
        foreach ($app->make('db')->getConnections() as $connection) {
            while ($connection->transactionLevel() > 0) {
                $connection->rollBack();
            }
        }
    }

    // String helper caches (locale/snake/camel etc.).
    if (class_exists(\Illuminate\Support\Str::class)) {
        \Illuminate\Support\Str::flushCache();
    }
}

// CoW mode (askr serve --cow): fork the workers from this booted template now.
// No-op in every other mode.
if (function_exists('askr_cow_ready')) {
    askr_cow_ready();
}

// Serve until Askr shuts the worker down.
$paranoid?->baseline();
while (askr_handle_request($handler)) {
    $paranoid?->check(++$requestNo);
}
