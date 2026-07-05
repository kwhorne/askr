#!/usr/bin/env bash
#
# Askr benchmark harness.
#
# Runs a load test against a URL and prints normalized numbers (requests/sec,
# latency p50/p99, errors), auto-detecting a load tool (oha > wrk > hey > ab).
# Use it to compare scenarios — start each server/config separately and point
# this at it:
#
#   # Askr, worker mode (Octane-style)
#   askr serve --root public --worker-script examples/laravel-worker.php --workers $(nproc) &
#   scripts/bench.sh http://127.0.0.1:8000/
#
#   # Askr, worker + response cache (cacheable route)
#   askr serve ... --response-cache 512 &
#   scripts/bench.sh http://127.0.0.1:8000/posts
#
#   # Comparators (same app, same host)
#   scripts/bench.sh http://127.0.0.1:8080/    # FrankenPHP / Octane
#   scripts/bench.sh http://127.0.0.1:9000/    # nginx + php-fpm
#
# On Linux this is also how we measure the io_uring core vs the epoll/tokio path
# (build each, run the same scenario, compare) — see docs/IO-URING.md.
#
# Usage: scripts/bench.sh <url> [duration_secs] [concurrency]
set -euo pipefail

URL="${1:-http://127.0.0.1:8000/}"
DURATION="${2:-10}"
CONCURRENCY="${3:-50}"

echo "── askr bench ──────────────────────────────────────────"
echo "  target:      $URL"
echo "  duration:    ${DURATION}s"
echo "  concurrency: $CONCURRENCY"

# Warm up so the app/opcache/cache is hot before we measure.
echo "  warming up…"
for _ in $(seq 1 50); do curl -sk -o /dev/null "$URL" || true; done

pick() { command -v "$1" >/dev/null 2>&1; }

if pick oha; then
  echo "  tool:        oha"
  echo "────────────────────────────────────────────────────────"
  oha -z "${DURATION}s" -c "$CONCURRENCY" --no-tui --insecure "$URL"
elif pick wrk; then
  echo "  tool:        wrk"
  echo "────────────────────────────────────────────────────────"
  wrk -d "${DURATION}s" -c "$CONCURRENCY" -t "$(command -v nproc >/dev/null && nproc || echo 4)" --latency "$URL"
elif pick hey; then
  echo "  tool:        hey"
  echo "────────────────────────────────────────────────────────"
  hey -z "${DURATION}s" -c "$CONCURRENCY" "$URL"
elif pick ab; then
  echo "  tool:        ab (ApacheBench)"
  echo "────────────────────────────────────────────────────────"
  ab -t "$DURATION" -c "$CONCURRENCY" "$URL"
else
  # Portable fallback: a rough RPS from a fixed number of sequential requests.
  echo "  tool:        curl fallback (install 'oha' for real numbers)"
  echo "────────────────────────────────────────────────────────"
  N=500
  start=$(date +%s.%N)
  ok=0
  for _ in $(seq 1 "$N"); do
    code=$(curl -sk -o /dev/null -w '%{http_code}' "$URL" || echo 000)
    [ "$code" = "200" ] && ok=$((ok + 1))
  done
  end=$(date +%s.%N)
  elapsed=$(echo "$end - $start" | bc)
  rps=$(echo "scale=0; $N / $elapsed" | bc)
  echo "  requests:    $N ($ok ok)"
  echo "  elapsed:     ${elapsed}s"
  echo "  ~requests/s: $rps  (sequential; use a real tool for concurrency)"
fi
echo "────────────────────────────────────────────────────────"
echo "tip: hit the admin /api/metrics for the PHP-vs-I/O split during the run."
