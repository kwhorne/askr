#!/usr/bin/env bash
# Serve a real Laravel application through Askr and check it the way a user would notice.
#
#   scripts/laravel-smoke.sh --binary target/debug/askr --create          # fresh app (CI)
#   scripts/laravel-smoke.sh --binary target/debug/askr --app ~/code/app  # an existing app
#   scripts/laravel-smoke.sh --image ghcr.io/kwhorne/askr:1.7.1 --app ~/code/app
#
# Why this exists: the request-shape faults Askr has shipped all passed the test suite,
# because the PHP behind it never did what a framework does with a request. 1.5.1 made
# every HTTP/1.x request to a Laravel app a 400 — Symfony rejected a `host, host:port`
# HTTP_HOST — while every test passed. The fixture-based e2e test now catches that shape;
# this catches the shapes nobody has thought of yet, by putting Laravel itself in the loop.
#
# The app is always copied to a scratch directory first, so an existing app is never
# modified. A probe route is added to the copy, and sessions and cache are forced to
# `array` so the check does not depend on the app's database.
#
# Exits 0 when every probe passes, 1 when one fails, 2 when the smoke could not run at all
# (no app, no binary, server never came up) — "could not tell" is not the same as "broken".

set -uo pipefail

MODE="" TARGET="" APP="" CREATE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) MODE=binary; TARGET=$2; shift 2 ;;
    --image) MODE=image; TARGET=$2; shift 2 ;;
    --app) APP=$2; shift 2 ;;
    --create) CREATE=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$MODE" ] || { sed -n '3,6p' "$0" | sed 's/^# //' >&2; exit 2; }
if [ "$CREATE" = 0 ] && [ -z "$APP" ]; then echo "--app PATH or --create" >&2; exit 2; fi

REPO=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/askr-smoke.XXXXXX")
CONTAINER="askr-smoke-$$"
PID=""
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null
  [ "$MODE" = image ] && docker rm -f "$CONTAINER" >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- the app, in a copy we are free to change --------------------------------------------
if [ "$CREATE" = 1 ]; then
  echo "creating a fresh Laravel app"
  composer create-project laravel/laravel "$WORK/app" --no-interaction --prefer-dist --quiet \
    || { echo "composer create-project failed" >&2; exit 2; }
else
  [ -f "$APP/artisan" ] || { echo "$APP is not a Laravel app (no artisan)" >&2; exit 2; }
  rsync -a --exclude node_modules --exclude .git "$APP/" "$WORK/app/"
  if [ ! -d "$WORK/app/vendor" ]; then
    (cd "$WORK/app" && composer install --no-interaction --no-progress --quiet) \
      || { echo "composer install failed" >&2; exit 2; }
  fi
fi

# A route that reports what Laravel concluded about the request. Every value here goes
# through Symfony's own parsing, so a malformed request fails the way it would in an app.
cat >> "$WORK/app/routes/web.php" <<'PHP'

Illuminate\Support\Facades\Route::get('/_askr_probe', fn (Illuminate\Http\Request $r) => response()->json([
    'host' => $r->getHost(),
    'http_host' => $r->getHttpHost(),
    'ip' => $r->ip(),
    'protocol' => $r->server('SERVER_PROTOCOL'),
]));
PHP
# A cached route table would not contain the probe.
rm -f "$WORK/app/bootstrap/cache/config.php" "$WORK/app/bootstrap/cache/routes"*.php
chmod -R a+rwX "$WORK/app/storage" "$WORK/app/bootstrap/cache"

# --- serve it ------------------------------------------------------------------------------
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
# Loopback (binary mode) and the private ranges a container runtime puts its gateway in
# (image mode): the peer is a trusted proxy in both, so X-Forwarded-For is believed.
TRUSTED='["127.0.0.1", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]'

if [ "$MODE" = binary ]; then
  [ -x "$TARGET" ] || { echo "no executable at $TARGET" >&2; exit 2; }
  cat > "$WORK/askr.toml" <<TOML
[server]
listen = "127.0.0.1:$PORT"
root = "$WORK/app/public"
workers = "2"
trusted_proxies = $TRUSTED

[worker]
script = "$REPO/examples/laravel-worker.php"
TOML
  ASKR_APP_BASE="$WORK/app" SESSION_DRIVER=array CACHE_STORE=array \
    "$TARGET" serve --config "$WORK/askr.toml" > "$WORK/askr.log" 2>&1 &
  PID=$!
  logs() { tail -40 "$WORK/askr.log"; }
else
  cat > "$WORK/askr.toml" <<TOML
[server]
listen = "0.0.0.0:8080"
root = "/var/www/app/public"
workers = "2"
trusted_proxies = $TRUSTED

[admin]
listen = "127.0.0.1:9000"

[worker]
script = "/opt/askr/examples/laravel-worker.php"
TOML
  docker run -d --name "$CONTAINER" -p "127.0.0.1:$PORT:8080" \
    -e ASKR_APP_BASE=/var/www/app -e SESSION_DRIVER=array -e CACHE_STORE=array \
    -v "$WORK/app:/var/www/app" -v "$WORK/askr.toml:/etc/askr/askr.toml:ro" \
    "$TARGET" serve --config /etc/askr/askr.toml >/dev/null \
    || { echo "docker run failed" >&2; exit 2; }
  logs() { docker logs --tail 40 "$CONTAINER" 2>&1; }
fi

BASE="http://127.0.0.1:$PORT"
# Wait for a *definitive* answer, bounded by the wall clock rather than a count of tries.
# 200 is up; a 4xx is Laravel answering — and refusing — which is a finding to report at
# once, not a reason to keep waiting (against 1.5.1 every probe was an instant 400, and
# waiting only for 200 sat out the whole deadline before saying so). Only "no answer yet"
# (000) and a 5xx while the workers boot mean try again.
up=""
deadline=$((SECONDS + 90))
while [ "$SECONDS" -lt "$deadline" ]; do
  up=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$BASE/up" 2>/dev/null)
  case "$up" in
    200 | 4??) break ;;
  esac
  sleep 1
done
if [ "$up" != 200 ]; then
  # A 4xx here is a finding, not a failure to start: 400 is exactly what 1.5.1 did.
  case "$up" in
    4??)
      echo "FAIL  /up answers $up over HTTP/1.1 — Laravel is refusing the request itself"
      logs; exit 1 ;;
  esac
  echo "the server never answered /up (last status: ${up:-none})" >&2
  logs >&2; exit 2
fi

# --- probes --------------------------------------------------------------------------------
fails=0
check() { # label, condition-result ("ok" or a reason)
  if [ "$2" = ok ]; then printf 'ok    %s\n' "$1"; else printf 'FAIL  %s — %s\n' "$1" "$2"; fails=$((fails + 1)); fi
}
status() { curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$@"; }
field() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get(sys.argv[1]))' "$1"; }

for proto in --http1.0 --http1.1 --http2-prior-knowledge; do
  s=$(status "$proto" "$BASE/up")
  check "/up over ${proto#--}" "$([ "$s" = 200 ] && echo ok || echo "status $s")"
done

j=$(curl -s --http1.1 -H "Host: askr.test:$PORT" "$BASE/_askr_probe")
h=$(echo "$j" | field host); hh=$(echo "$j" | field http_host)
check "HTTP/1.1: Laravel sees the host without the port" "$([ "$h" = askr.test ] && echo ok || echo "getHost() = $h ($j)")"
check "HTTP/1.1: and with it, for URL generation" "$([ "$hh" = "askr.test:$PORT" ] && echo ok || echo "getHttpHost() = $hh")"

j=$(curl -s --http2-prior-knowledge "$BASE/_askr_probe")
h=$(echo "$j" | field host); p=$(echo "$j" | field protocol)
check "HTTP/2: the authority reaches Laravel, not localhost" "$([ "$h" = 127.0.0.1 ] && echo ok || echo "getHost() = $h ($j)")"
check "HTTP/2: it really was HTTP/2" "$([ "$p" = HTTP/2.0 ] && echo ok || echo "SERVER_PROTOCOL = $p")"

j=$(curl -s --http1.1 -H "X-Forwarded-For: 203.0.113.9" "$BASE/_askr_probe")
ip=$(echo "$j" | field ip)
check "behind a trusted proxy, request()->ip() is the client" "$([ "$ip" = 203.0.113.9 ] && echo ok || echo "ip() = $ip")"

if [ "$fails" -gt 0 ]; then
  echo "$fails probe(s) failed"; logs; exit 1
fi
echo "all probes passed"
