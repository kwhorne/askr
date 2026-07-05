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
    echo $response->getContent();

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
