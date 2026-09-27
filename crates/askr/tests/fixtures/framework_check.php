<?php
/**
 * Look at the request the way a framework does, and refuse it the way a framework would.
 *
 * Nearly every request-shape bug Askr has shipped passed its own tests, because the PHP
 * behind those tests echoed a string and never looked at what it was given:
 *
 *   - HTTP_HOST arrived as `host, host:port` over HTTP/1.x (1.5.1). Symfony throws
 *     SuspiciousOperationException on that, so every Laravel request answered 400 — and
 *     every e2e test passed, because none of them read the host.
 *   - Over HTTP/2 there is no Host header, and HTTP_HOST fell back to `localhost` (1.4.7).
 *   - Over HTTP/2 a browser sends one `cookie` field per cookie, and PHP got the first
 *     (1.5.1). Every test client spoke HTTP/1.1.
 *   - Behind a trusted proxy REMOTE_ADDR was the proxy (1.7.1).
 *
 * So this checks the invariants a framework relies on, using the framework's own rules
 * where it has them — the host check below is Symfony's Request::getHost(), regex and all
 * — and answers 400 with the list of what it found, exactly as a Laravel app would have
 * failed. On success it answers 200 with the facts it normalised, for a test to assert on.
 *
 * Used as a front controller directly (per-request mode), or from framework_worker.php in
 * worker mode, where the same map arrives as $r['headers'].
 */

function askr_framework_check(array $server, string $body): array
{
    $bad = [];

    // Symfony\Component\HttpFoundation\Request::getHost(), minus the trusted-proxy branch
    // (Askr resolves forwarding itself before PHP runs). HTTP_HOST first, then SERVER_NAME.
    $raw = $server['HTTP_HOST'] ?? ($server['SERVER_NAME'] ?? ($server['SERVER_ADDR'] ?? ''));
    $host = strtolower(preg_replace('/:\d+$/', '', trim($raw)));
    if ($host !== '' && '' !== preg_replace('/(?:^\[)?[a-zA-Z0-9-:\]_]+\.?/', '', $host)) {
        $bad[] = "Invalid Host \"$host\" (Symfony: SuspiciousOperationException)";
    }
    if (!isset($server['HTTP_HOST'])) {
        $bad[] = 'no HTTP_HOST — over HTTP/2 the authority must still reach PHP';
    }

    $name = $server['SERVER_NAME'] ?? '';
    // `host:port` has exactly one colon; an IPv6 literal has several and is left alone.
    if ($name === '' || preg_match('/[,\s]/', $name) || preg_match('/^[^:\[\]]+:\d+$/', $name)) {
        $bad[] = "SERVER_NAME \"$name\" must be a bare host, without a port";
    }

    // inet_pton rather than filter_var, and a regex rather than ctype_digit below: this
    // has to run on the minimal libphp the e2e suite links against, which has neither
    // ext/filter nor ext/ctype. A checker that fatals is a 500 that reads like a finding.
    $addr = $server['REMOTE_ADDR'] ?? '';
    if ($addr === '' || @inet_pton($addr) === false) {
        $bad[] = "REMOTE_ADDR \"$addr\" is not an IP address";
    }

    $port = $server['SERVER_PORT'] ?? '';
    if (!preg_match('/^\d+$/', (string) $port) || (int) $port < 1 || (int) $port > 65535) {
        $bad[] = "SERVER_PORT \"$port\" is not a port";
    }

    $proto = $server['SERVER_PROTOCOL'] ?? '';
    if (!preg_match('#^HTTP/(1\.0|1\.1|2(\.0)?|3(\.0)?)$#', $proto)) {
        $bad[] = "SERVER_PROTOCOL \"$proto\" is not an HTTP version";
    }

    $method = $server['REQUEST_METHOD'] ?? '';
    if (!preg_match('/^[A-Z]+$/', $method)) {
        $bad[] = "REQUEST_METHOD \"$method\" is not a method";
    }

    // Origin-form: routing matches on the path, and an absolute URI here would not match.
    $uri = $server['REQUEST_URI'] ?? '';
    if ($uri === '' || $uri[0] !== '/') {
        $bad[] = "REQUEST_URI \"$uri\" is not origin-form";
    }
    $qs = $server['QUERY_STRING'] ?? '';
    $uriQuery = (string) (parse_url('http://x' . $uri, PHP_URL_QUERY) ?? '');
    if ($qs !== $uriQuery) {
        $bad[] = "QUERY_STRING \"$qs\" disagrees with REQUEST_URI \"$uri\"";
    }

    // A comma in the Cookie header is the mark of a wrong join: cookie values may not
    // contain one (RFC 6265 cookie-octet), and fields must be joined with "; ".
    $cookie = $server['HTTP_COOKIE'] ?? '';
    $cookies = [];
    if ($cookie !== '') {
        if (strpos($cookie, ',') !== false) {
            $bad[] = "HTTP_COOKIE \"$cookie\" contains a comma — fields joined with the wrong separator";
        }
        foreach (explode(';', $cookie) as $pair) {
            $pair = trim($pair);
            if ($pair === '') {
                continue;
            }
            if (strpos($pair, '=') === false) {
                $bad[] = "HTTP_COOKIE pair \"$pair\" has no '='";
                continue;
            }
            $cookies[] = strtok($pair, '=');
        }
    }

    if (isset($server['CONTENT_LENGTH']) && (int) $server['CONTENT_LENGTH'] !== strlen($body)) {
        $bad[] = "CONTENT_LENGTH {$server['CONTENT_LENGTH']} but the body is " . strlen($body) . ' bytes';
    }

    foreach ($server as $k => $v) {
        if (is_string($v) && (strpos($v, "\n") !== false || strpos($v, "\r") !== false)) {
            $bad[] = "$k contains a line break";
        }
    }

    if ($bad) {
        return [400, ['ok' => false, 'violations' => $bad]];
    }
    return [200, [
        'ok' => true,
        'host' => $server['HTTP_HOST'],
        'server_name' => $name,
        'remote_addr' => $addr,
        'protocol' => $proto,
        'method' => $method,
        'uri' => $uri,
        'cookies' => $cookies,
        'x_forwarded_for' => $server['HTTP_X_FORWARDED_FOR'] ?? null,
    ]];
}

function askr_framework_respond(array $result): void
{
    [$status, $payload] = $result;
    http_response_code($status);
    header('Content-Type: application/json');
    echo json_encode($payload, JSON_UNESCAPED_SLASHES);
}

// Front controller in per-request mode. framework_worker.php defines the constant first,
// so including this from there only brings in the functions.
if (!defined('ASKR_FRAMEWORK_CHECK_LIBRARY')) {
    askr_framework_respond(askr_framework_check($_SERVER, (string) file_get_contents('php://input')));
}
