<?php

/**
 * Askr scheduler sidecar — the built-in cron.
 *
 * Boots the app once and runs `schedule:run` every interval (default 60s), so
 * you don't need a `* * * * * php artisan schedule:run` crontab entry. Enable
 * with `askr serve --scheduler-script examples/askr-scheduler.php` (or the
 * `[scheduler]` section of askr.toml).
 *
 * The process exits after a while so the supervisor respawns a fresh one
 * (bounding any long-run state drift). Tunables via env:
 *   ASKR_SCHEDULER_INTERVAL   seconds between runs (default 60)
 *   ASKR_SCHEDULER_TICKS      runs before self-recycle (default 60)
 */

use Symfony\Component\Console\Output\StreamOutput;

// The embed SAPI has no CLI argv/$_SERVER; Symfony Console reads a few of these
// (e.g. DumpCompletionCommand uses $_SERVER['PHP_SELF']). Provide sane values.
$_SERVER['PHP_SELF'] = $_SERVER['SCRIPT_NAME'] = $_SERVER['SCRIPT_FILENAME'] = 'artisan';
$_SERVER['argv'] = $argv = ['artisan', 'schedule:run'];
$_SERVER['argc'] = $argc = count($argv);

$base = getenv('ASKR_APP_BASE') ?: dirname(__DIR__);

require $base . '/vendor/autoload.php';
$app = require $base . '/bootstrap/app.php';

/** @var \Illuminate\Contracts\Console\Kernel $kernel */
$kernel = $app->make(Illuminate\Contracts\Console\Kernel::class);
$kernel->bootstrap();

$output = new StreamOutput(fopen('php://stdout', 'w'));

$interval = max(1, (int) (getenv('ASKR_SCHEDULER_INTERVAL') ?: 60));
$maxTicks = max(1, (int) (getenv('ASKR_SCHEDULER_TICKS') ?: 60));

for ($tick = 0; $tick < $maxTicks; $tick++) {
    // Align to the interval boundary (like cron's minute boundary at 60s).
    $now = time();
    $sleep = $interval - ($now % $interval);
    sleep($sleep);

    $kernel->call('schedule:run', [], $output);
}

exit(0);
