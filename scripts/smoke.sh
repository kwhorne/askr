#!/usr/bin/env bash
#
# Post-deploy smoke test for an Askr-served Laravel site.
#
#   ./scripts/smoke.sh https://example.com [admin-url] [admin-token]
#
# Every check here is a failure that actually shipped. The point is not coverage — it is
# that each of these was, at some point, invisible: a 200 with an empty body, a page that
# worked once per worker, a form that lost its fields, a URL that said localhost over
# HTTP/2, a queue that accepted jobs nothing consumed.
#
# Two habits are built in deliberately:
#
#   - Check for the *absence* of the unexpected, not only the presence of the expected.
#     Grepping for <title> matched happily with deprecation warnings printed in front of it.
#   - Exercise HTTP/2 as well as 1.1. TLS negotiates h2 by default, every test client in
#     this repo speaks 1.1, and that is how a bug that made every generated URL say
#     localhost survived 23 million requests.
#
# Exit code is the number of failures, so CI can gate on it.

set -uo pipefail

BASE="${1:-}"
ADMIN="${2:-}"
TOKEN="${3:-${ASKR_ADMIN_TOKEN:-}}"

if [[ -z "$BASE" ]]; then
    echo "usage: $0 https://example.com [http://127.0.0.1:9000] [admin-token]" >&2
    exit 2
fi
BASE="${BASE%/}"

FAILED=0
SKIPPED=0

pass() { printf '  \033[32m✓\033[0m %s\n' "$1"; }
fail() {
    printf '  \033[31m✗\033[0m %s\n' "$1"
    FAILED=$((FAILED + 1))
}
skip() {
    printf '  \033[33m–\033[0m %s\n' "$1"
    SKIPPED=$((SKIPPED + 1))
}
head2() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# curl with a timeout everywhere: a smoke test that hangs is a smoke test nobody runs.
c() { curl -sS --max-time 20 "$@"; }

head2 "Reachability"

code=$(c -o /tmp/smoke-home.html -w '%{http_code}' "$BASE/")
if [[ "$code" == 200 ]]; then
    pass "GET / → 200"
else
    fail "GET / → $code (expected 200)"
fi

size=$(wc -c < /tmp/smoke-home.html | tr -d ' ')
if [[ "$size" -gt 500 ]]; then
    pass "home page has a body ($size bytes)"
else
    fail "home page is $size bytes — a 200 does not mean a body"
fi

# The absence check. PHP diagnostics belong in the log (1.4.1); anything leaking into the
# response means display_errors is on, and it will have truncated the page in worker mode.
if grep -qiE '(<br />)?<b>(Warning|Notice|Fatal error|Deprecated)</b>|on line <b>[0-9]' /tmp/smoke-home.html; then
    fail "PHP diagnostics are in the HTML — set display_errors=0 (they belong in the log)"
else
    pass "no PHP diagnostics in the response"
fi

if grep -qiE 'https?://(localhost|127\.0\.0\.1|[a-z0-9.-]+\.test)' /tmp/smoke-home.html; then
    fail "HTML contains localhost/.test URLs — check APP_URL, ASSET_URL and the Vite build"
else
    pass "no localhost/.test URLs in the HTML"
fi

head2 "HTTP/2 (what browsers actually negotiate over TLS)"

if [[ "$BASE" == https://* ]]; then
    ver=$(c --http2 -o /dev/null -w '%{http_version}' "$BASE/")
    code=$(c --http2 -o /tmp/smoke-h2.html -w '%{http_code}' "$BASE/")
    if [[ "$code" == 200 ]]; then
        pass "GET / over HTTP/$ver → 200"
    else
        fail "GET / over HTTP/2 → $code"
    fi
    # HTTP/2 sends no Host header; the authority is a pseudo-header. Reading only Host made
    # Laravel build every URL as https://localhost (fixed in 1.4.7) and let two domains
    # share cache entries.
    if grep -qiE 'https?://localhost' /tmp/smoke-h2.html; then
        fail "over HTTP/2 the page contains localhost URLs — the :authority is being ignored"
    else
        pass "generated URLs are correct over HTTP/2"
    fi
else
    skip "HTTP/2 checks (not an https:// URL)"
fi

head2 "Assets and interactivity"

# Livewire's script tag is emitted once per process unless the worker resets its state.
# Only the first response from each worker had it, and since Alpine ships in that bundle
# every later page had dead directives with a clean console (fixed in 1.4.8). One request
# cannot see this: it needs enough to land on every worker.
livewire_hits=0
livewire_total=10
for _ in $(seq "$livewire_total"); do
    if c "$BASE/" | grep -q 'livewire\(\.min\)\?\.js'; then
        livewire_hits=$((livewire_hits + 1))
    fi
done
if [[ "$livewire_hits" == 0 ]]; then
    skip "no Livewire on this site ($livewire_hits/$livewire_total)"
elif [[ "$livewire_hits" == "$livewire_total" ]]; then
    pass "Livewire script present in $livewire_hits/$livewire_total loads"
else
    fail "Livewire script in only $livewire_hits/$livewire_total loads — worker state is not being reset"
fi

# Every asset the page references, not the first one found.
#
# Two failures hide here. An empty 200: getContent() returns false for BinaryFileResponse
# and StreamedResponse, and `echo false` prints nothing (fixed 1.4.3). And a 404 for a
# stylesheet whose hash no longer matches what is on disk — cached HTML pointing at a
# previous build. The browser hides the second one completely, because it still holds the
# old file under immutable, max-age=1-year.
#
# Checking only the first match found the 404 on one run and missed it on the next, since
# the order of matches varies. A check that intermittently catches a real failure is barely
# better than no check.
assets=$(grep -oE '/(build|flux)/[A-Za-z0-9._/-]+\.(js|css)' /tmp/smoke-home.html | sort -u)
if [[ -n "$assets" ]]; then
    bad=0
    n=0
    while IFS= read -r asset; do
        [[ -z "$asset" ]] && continue
        n=$((n + 1))
        out=$(c -o /dev/null -w '%{http_code} %{size_download}' "$BASE$asset")
        code=${out%% *}
        bytes=${out##* }
        if [[ "$code" != 200 ]]; then
            fail "asset $asset → $code — the page references a file that is not being served"
            bad=$((bad + 1))
        elif [[ "$bytes" -lt 100 ]]; then
            fail "asset $asset → 200 but only $bytes bytes (an empty 200 is the worst answer)"
            bad=$((bad + 1))
        fi
    done <<<"$assets"
    if [[ "$bad" == 0 ]]; then
        pass "all $n referenced assets serve a non-empty 200"
    fi
else
    skip "no build assets found in the HTML to check"
fi

head2 "Forms (a 419 here means the request body never arrived)"

jar=$(mktemp)
form=$(c -c "$jar" "$BASE/login")
token=$(printf '%s' "$form" | grep -oE 'name="_token"[^>]*value="[^"]+' | grep -oE 'value="[^"]+' | cut -d'"' -f2 | head -1)
if [[ -n "$token" ]]; then
    # Wrong credentials on purpose: 302 (back with errors) or 422 both prove the body and
    # session survived. 419 means the CSRF token was not seen — the urlencoded body was
    # never parsed into the request (fixed in 1.4.3).
    code=$(c -b "$jar" -c "$jar" -o /dev/null -w '%{http_code}' \
        -X POST "$BASE/login" \
        -d "_token=$token" -d "email=smoke-test@invalid.example" -d "password=wrong-on-purpose")
    case "$code" in
    419) fail "form POST → 419: the request body is not reaching PHP" ;;
    302 | 422) pass "form POST → $code (body and session intact)" ;;
    *) fail "form POST → $code (expected 302 or 422)" ;;
    esac
else
    skip "no CSRF form found at /login"
fi
rm -f "$jar"

head2 "HTTPS hygiene"

if [[ "$BASE" == https://* ]]; then
    plain="http://${BASE#https://}"
    code=$(c -o /dev/null -w '%{http_code}' "$plain/pricing")
    if [[ "$code" == 30* ]]; then
        pass "port 80 → $code redirect"
    else
        fail "port 80 → $code (expected a redirect; needs --force-https and --http-redirect)"
    fi

    days=$(echo | openssl s_client -connect "${BASE#https://}:443" -servername "${BASE#https://}" 2>/dev/null |
        openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2)
    if [[ -n "$days" ]]; then
        pass "certificate valid until $days"
    else
        fail "could not read the certificate"
    fi
else
    skip "HTTPS checks (not an https:// URL)"
fi

for path in /.env /.git/config /storage/logs/laravel.log; do
    code=$(c -o /dev/null -w '%{http_code}' "$BASE$path")
    if [[ "$code" == 404 || "$code" == 403 ]]; then
        pass "$path → $code"
    else
        fail "$path → $code — this must not be served"
    fi
done

head2 "Liveness and queues"

if [[ -n "$ADMIN" ]]; then
    ADMIN="${ADMIN%/}"
    code=$(c -o /dev/null -w '%{http_code}' "$ADMIN/healthz")
    if [[ "$code" == 200 ]]; then
        pass "/healthz → 200"
    else
        fail "/healthz → $code"
    fi

    auth=()
    [[ -n "$TOKEN" ]] && auth=(-H "Authorization: Bearer $TOKEN")
    if status=$(c "${auth[@]}" "$ADMIN/api/status") && [[ -n "$status" ]]; then
        # A queue with jobs and no consumer is the failure that stopped a site's mail with
        # no error anywhere. Per queue, because the aggregate cannot show it.
        stuck=$(printf '%s' "$status" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
bad = [q for q in d.get("queues", [])
       if q.get("pending", 0) > 0 and q.get("oldest_pending_secs", 0) > 30]
for q in bad:
    print(f"{q[\"queue\"]}: {q[\"pending\"]} pending, oldest {q[\"oldest_pending_secs\"]}s")
' 2>/dev/null)
        if [[ -n "$stuck" ]]; then
            while IFS= read -r line; do
                fail "queue backlog not being consumed — $line"
            done <<<"$stuck"
        else
            pass "no stale queue backlog"
        fi

        printf '%s' "$status" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(f"    workers {d[\"workers_alive\"]}/{d[\"workers_configured\"]}"
      f" | queue workers {d[\"queue_workers\"]}"
      f" | respawns {d[\"respawns\"]}"
      f" | rss {d[\"rss_kb_total\"]//1024} MB")
' 2>/dev/null || true
    else
        skip "/api/status (no token, or admin plane not reachable)"
    fi
else
    skip "liveness and queue checks (pass the admin URL as the second argument)"
fi

head2 "Result"
if [[ "$FAILED" == 0 ]]; then
    printf '  \033[32mall checks passed\033[0m'
    [[ "$SKIPPED" -gt 0 ]] && printf ' (%s skipped)' "$SKIPPED"
    printf '\n\n'
else
    printf '  \033[31m%s check(s) failed\033[0m\n\n' "$FAILED"
fi
exit "$FAILED"
