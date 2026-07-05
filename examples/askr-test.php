<?php

/**
 * Askr fork-based test runner (#7).
 *
 * `askr test --runner examples/askr-test.php` boots the app once in a template
 * process, then forks a fresh, warm process per test file. This script is what
 * each forked child runs: it invokes PHPUnit on the single file named in
 * $ASKR_TEST_FILE, against the already-warm autoloader/opcache the child
 * inherited — so there's no cold boot per file and each file is perfectly
 * isolated (no shared state between files).
 *
 * Usage:
 *   ASKR_PHP_INI="zend_extension=/path/opcache.so\nopcache.enable=1\nopcache.enable_cli=1" \
 *     askr test --root /path/to/app --runner examples/askr-test.php tests/Unit
 *
 * Exit code 0 = the file's tests passed; non-zero = failures (Askr aggregates).
 */

$base = getenv('ASKR_APP_BASE') ?: getcwd();
$file = getenv('ASKR_TEST_FILE');

if (!$file || !is_file($file)) {
    fwrite(STDERR, "askr-test: ASKR_TEST_FILE not set or missing\n");
    exit(2);
}

require $base . '/vendor/autoload.php';

if (!class_exists(\PHPUnit\TextUI\Application::class)) {
    fwrite(STDERR, "askr-test: PHPUnit not found (composer require --dev phpunit/phpunit)\n");
    exit(2);
}

// Build a PHPUnit argv for this one file. Honour the project's phpunit.xml
// (which usually sets the bootstrap and, for Laravel, the CreatesApplication
// trait) when present.
$argv = ['phpunit'];
foreach (['phpunit.xml', 'phpunit.xml.dist'] as $cfg) {
    if (is_file($base . '/' . $cfg)) {
        $argv[] = '--configuration';
        $argv[] = $base . '/' . $cfg;
        break;
    }
}
$argv[] = $file;

// PHPUnit reads $_SERVER['argv'].
$_SERVER['argv'] = $argv;
$_SERVER['argc'] = count($argv);

$app = new \PHPUnit\TextUI\Application();
exit($app->run($argv));
