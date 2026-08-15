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

    // Why this is wrapped.
    //
    // A TypeError has been seen escaping `schedule:run` in production — six times over
    // three days, always in the second after a scheduled job ran, then nothing for
    // 22 hours across two restarts and an upgrade. The events themselves had already run:
    // the failure is on the way out, not on the way in, so no work was lost.
    //
    // The cause is not known. `Kernel::call()` is declared `: int`, so the TypeError is
    // thrown inside Laravel as it returns, and the offending value never reaches us —
    // which is why the message below records the exception rather than the return value.
    // It is all that can be observed from here.
    //
    // Catching matters for its own sake regardless: uncaught, this ends the process, the
    // supervisor respawns it, and the scheduler misses the boundary it was sleeping for.
    // A cosmetic error should not cost a tick.
    try {
        $rc = $kernel->call('schedule:run', [], $output);

        // Belt and braces: the declared return type makes a non-int impossible in theory,
        // and the whole reason for this block is that something impossible happened.
        if (! is_int($rc)) {
            fwrite(STDERR, sprintf(
                "askr-scheduler: schedule:run returned %s, expected int%s\n",
                get_debug_type($rc),
                is_object($rc) ? ' (' . $rc::class . ')' : ''
            ));
        }
    } catch (\Throwable $e) {
        // Class, message, and origin — the three things missing when this was investigated
        // from a bare log line. Scheduled events have already run by this point; say so, so
        // the next person does not start by looking for lost work.
        fwrite(STDERR, sprintf(
            "askr-scheduler: %s escaped schedule:run at %s:%d — %s\n" .
            "askr-scheduler:   scheduled events had already run; this is the return path, " .
            "not the work. Continuing.\n",
            $e::class,
            $e->getFile(),
            $e->getLine(),
            $e->getMessage()
        ));
    }
}

exit(0);
