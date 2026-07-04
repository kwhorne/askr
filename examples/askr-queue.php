<?php

/**
 * Askr queue-worker sidecar.
 *
 * Runs `queue:work` in-process against the embedded interpreter, supervised by
 * the Askr master (respawned if it exits). Start N of these with
 * `askr serve --queue N --queue-script examples/askr-queue.php` (or the
 * `[queue]` section of askr.toml) — no separate `php artisan queue:work` /
 * systemd unit / Horizon needed for basic queues.
 *
 * Tunables via env:
 *   ASKR_QUEUE_CONNECTION   queue connection (default: app default)
 *   ASKR_QUEUE              queue name(s), comma-separated
 */

use Symfony\Component\Console\Output\StreamOutput;

// The embed SAPI has no CLI argv/$_SERVER; Symfony Console reads a few of these
// (e.g. DumpCompletionCommand uses $_SERVER['PHP_SELF']). Provide sane values.
$_SERVER['PHP_SELF'] = $_SERVER['SCRIPT_NAME'] = $_SERVER['SCRIPT_FILENAME'] = 'artisan';
$_SERVER['argv'] = $argv = ['artisan', 'queue:work'];
$_SERVER['argc'] = $argc = count($argv);

$base = getenv('ASKR_APP_BASE') ?: dirname(__DIR__);

require $base . '/vendor/autoload.php';
$app = require $base . '/bootstrap/app.php';

/** @var \Illuminate\Contracts\Console\Kernel $kernel */
$kernel = $app->make(Illuminate\Contracts\Console\Kernel::class);
$kernel->bootstrap();

// Stream command output straight to stdout (no ever-growing buffer).
$output = new StreamOutput(fopen('php://stdout', 'w'));

$params = [
    '--tries'    => 3,
    '--sleep'    => 3,
    '--max-jobs' => 1000,  // self-recycle; the supervisor respawns a fresh one
    '--max-time' => 3600,
];
if ($conn = getenv('ASKR_QUEUE_CONNECTION')) {
    $params['connection'] = $conn;
}
if ($queue = getenv('ASKR_QUEUE')) {
    $params['--queue'] = $queue;
}

// queue:work installs its own SIGTERM/SIGINT handlers (pcntl) and stops
// gracefully after the current job when the master signals a shutdown/reload.
exit($kernel->call('queue:work', $params, $output));
