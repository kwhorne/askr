<?php

declare(strict_types=1);

/**
 * Does every class in this package still satisfy the framework contracts?
 *
 * Loading a class is enough to find out: PHP raises a fatal at *link* time when a
 * concrete class is missing an abstract method. That is exactly how Laravel 13 broke
 * this package in production — it added `pendingSize()`, `delayedSize()`,
 * `reservedSize()` and `creationTimeOfOldestPendingJob()` to the Queue contract, and
 * `AskrQueue` didn't have them, so the driver killed the worker the moment anything
 * resolved the queue. Nothing here tested against any Laravel version, while
 * composer.json claimed three of them.
 *
 * Run under each supported major (see .github/workflows/ci.yml):
 *
 *   php tests/contracts.php
 */

require __DIR__ . '/../vendor/autoload.php';

$classes = [
    Askr\Laravel\AskrServiceProvider::class,
    Askr\Laravel\Broadcasting\AskrBroadcaster::class,
    Askr\Laravel\Cache\AskrStore::class,
    Askr\Laravel\Cache\ModelDependencies::class,
    Askr\Laravel\Http\Middleware\CacheResponse::class,
    Askr\Laravel\Queue\AskrConnector::class,
    Askr\Laravel\Queue\AskrJob::class,
    Askr\Laravel\Queue\AskrQueue::class,
    Askr\Laravel\Session\AskrSessionHandler::class,
];

$laravel = Composer\InstalledVersions::getPrettyVersion('illuminate/support');
echo "illuminate/support {$laravel}, PHP " . PHP_VERSION . "\n";

$failed = 0;
foreach ($classes as $class) {
    // class_exists() links the class, which is where a missing abstract method fatals.
    if (! class_exists($class)) {
        echo "  MISSING  {$class}\n";
        $failed++;
        continue;
    }

    $r = new ReflectionClass($class);

    // A concrete class that isn't instantiable means an unimplemented abstract method
    // (or a private constructor, which none of these have).
    if (! $r->isAbstract() && ! $r->isInstantiable()) {
        echo "  ABSTRACT {$class} — unimplemented method(s) from a parent or interface\n";
        $failed++;
        continue;
    }

    // Spell out which interface method is missing, so the failure names the fix rather
    // than just the symptom.
    foreach ($r->getInterfaces() as $interface) {
        foreach ($interface->getMethods() as $method) {
            if (! $r->hasMethod($method->getName())) {
                echo "  MISSING METHOD {$class}::{$method->getName()}() from {$interface->getName()}\n";
                $failed++;
            }
        }
    }

    echo "  ok       {$class}\n";
}

if ($failed > 0) {
    echo "\n{$failed} problem(s) — this package declares support for a Laravel version it doesn't satisfy.\n";
    exit(1);
}

echo "\nall " . count($classes) . " classes satisfy their contracts.\n";
