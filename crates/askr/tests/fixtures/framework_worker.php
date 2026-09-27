<?php
// Worker-mode entry for framework_check.php: the same checks, on the map a worker gets as
// $r['headers'] rather than $_SERVER. Both modes build that map from one function, but
// "both modes share it" is a claim, and this is how it gets tested rather than assumed.
define('ASKR_FRAMEWORK_CHECK_LIBRARY', true);
require __DIR__ . '/framework_check.php';

while (askr_handle_request(function (array $r): int {
    $result = askr_framework_check($r['headers'], (string) ($r['body'] ?? ''));
    askr_framework_respond($result);
    return $result[0];
})) {
}
