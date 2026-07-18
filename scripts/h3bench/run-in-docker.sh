#!/usr/bin/env bash
# Honest h2-vs-h3-under-loss benchmark. Runs entirely inside one Ubuntu 24.04
# container: askr (0.9.6-full, with --http3) + the h3bench load client + tc netem
# on loopback. Same server, same payload, identical impairment per row.
set -euo pipefail

# Base image = ghcr.io/kwhorne/askr:0.9.6-full (askr + libphp + system deps + http3).
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq iproute2 util-linux openssl curl >/dev/null 2>&1
ASKR=/opt/askr/askr

cd /tmp
mkdir -p /tmp/pub
# small, realistic dynamic response (~1 KB) so we measure transport, not PHP
cat > /tmp/pub/index.php <<'PHP'
<?php echo str_repeat("askr-http3-benchmark-payload-", 34); // ~1 KB
PHP

openssl req -x509 -newkey rsa:2048 -keyout /tmp/key.pem -out /tmp/cert.pem -days 1 -nodes \
  -subj "/CN=localhost" -addext "subjectAltName=IP:127.0.0.1,DNS:localhost" >/dev/null 2>&1

"$ASKR" serve --root /tmp/pub --tls-cert /tmp/cert.pem --tls-key /tmp/key.pem \
  --http3 --workers 4 --listen 127.0.0.1:8443 >/tmp/askr.log 2>&1 &
sleep 4
echo "askr up: $(grep -c 'HTTP/3' /tmp/askr.log || true) h3 listeners"

BENCH=/bench/target/release/h3bench
CONC=50
PER=100   # 50*100 = 5000 requests per run
URL_T="https://127.0.0.1:8443/"

warm() { curl -sk "$URL_T" >/dev/null 2>&1 || true; }

clear_netem() { tc qdisc del dev lo root 2>/dev/null || true; }
set_netem() { clear_netem; tc qdisc add dev lo root netem "$@"; }

run_row() {
  local label="$1"; shift
  if [ "$#" -gt 0 ]; then set_netem "$@"; else clear_netem; fi
  warm
  echo "── ${label} ──"
  taskset -c 0-3 "$BENCH" h2 "$URL_T" "$CONC" "$PER"
  taskset -c 0-3 "$BENCH" h3 "$URL_T" "$CONC" "$PER"
}

echo "=== h2 vs h3 · conc=${CONC} · $((CONC*PER)) reqs/run · ~1KB body ==="
run_row "baseline (no impairment)"
run_row "delay 30ms"                 delay 30ms
run_row "loss 1%"                    loss 1%
run_row "loss 3%"                    loss 3%
run_row "loss 2% + delay 30ms"       loss 2% delay 30ms
run_row "loss 5% + delay 20ms"       loss 5% delay 20ms
clear_netem
echo "=== done ==="
