# Benchmarks

> **Honest framing.** These are directional, single-box results from a shared
> development VM (OrbStack on Apple Silicon), not a datacenter benchmark. They
> are fully reproducible with the script at the bottom. The point is not a
> precise number — it's the *shape* of the result and one strategic question:
> **is I/O the bottleneck for Laravel? (No — see below.)**

## What we measured

The same **Laravel 13.18.1** app, the same **PHP 8.5.8** engine, the same 4
worker processes, `opcache` + JIT on, served by five stacks — run **sequentially**
so they never steal CPU from each other:

| Stack | Model |
| --- | --- |
| **Askr — worker** | embedded PHP, boot-once worker per core (NTS, process-per-core) |
| **Askr — CoW** | same, workers forked copy-on-write from a warm template |
| **Octane + FrankenPHP** | embedded PHP, worker threads (ZTS) — the closest peer |
| **Octane + RoadRunner** | Go supervisor + PHP worker processes |
| **PHP-FPM + nginx** | classic FastCGI, boot-per-request |

### Fairness controls

- **Same PHP** — all five run PHP **8.5.8** (FrankenPHP v1.12.4 bundles 8.5.8;
  FPM/RoadRunner use `php8.5` from ondrej; Askr's libphp is 8.5.8).
- **Same opcache + JIT** (`opcache.jit=tracing`, `jit_buffer_size=128M`,
  `validate_timestamps=0`) for every stack.
- **Same worker count** (4) for every stack.
- **CPU isolation** — server pinned to cores 0–3, load generator (`wrk`) pinned
  to cores 4–7 (`taskset`), so the benchmark tool never competes with the server.
- **Warm-up** (8 s) discarded before each 20 s measurement.
- **Response validation** — every run is rejected unless it returns HTTP `200`
  with the expected JSON body and **zero** non-2xx responses. (See the pitfall
  below — this matters more than you'd think.)

### A confound we found (and why validation matters)

Our first run had `SESSION_DRIVER=database` (the Laravel default). The `/bench`
route lives under the `web` middleware, so every request ran `StartSession`, and
because `wrk` sends no cookies, **every request wrote a new session row to
SQLite** — whose single-writer lock serialised *all* the servers under
concurrency. FrankenPHP collapsed to **75 req/s**, and an unvalidated Askr run
reported a fantasy **279 000 req/s** that was actually **HTTP 502s** counted as
"requests". Neither number meant anything.

Fixing this — `SESSION_DRIVER=array` (no session persistence, applied to all
stacks equally) plus strict response validation — is what makes the numbers
below trustworthy. The lesson: **a req/s figure without response validation is
noise.**

## Results — `/bench` (JSON, isolates server + framework overhead)

> **Correction (0.8.3).** An earlier version of this page reported Askr worker
> mode at ~18k req/s (≈2× FrankenPHP). That number was **wrong** — it was
> inflated by `502` responses that `wrk` counted as "requests". A concurrency
> sweep exposed it: above `--workers` concurrency the worker was 502-flooding,
> because a **memory leak in the long-lived worker eventually hit PHP's
> `memory_limit`, the worker died, and the process kept answering 502s**. We
> fixed the crash-recovery (the worker now respawns instead of flooding — see
> the CHANGELOG) and re-measured with **zero-error validation on every run**. The
> honest, validated numbers are below. Lesson, reinforced: *a req/s figure
> without response validation is noise.*

Representative validated run at c64 (0 non-2xx responses on every stack), req/s:

| Stack | req/s | p50 | p99 | RSS¹ |
| --- | ---: | ---: | ---: | ---: |
| **Askr — CoW** | **13 213** | ~3.5 ms | ~11 ms | ~470 MB |
| **Askr — worker** (fixed) | **10 909** | ~4 ms | ~14 ms | ~490 MB |
| Octane + FrankenPHP | 8 217 | 7.3 ms | 11 ms | ~220 MB |
| PHP-FPM + nginx | 4 380 | 14.4 ms | 17 ms | ~90 MB |
| Octane + RoadRunner² | 3 861 | 12.9 ms | 100–155 ms | ~390 MB |

So on pure server + framework overhead, **Askr CoW is ~1.6× FrankenPHP** (the
closest embedded-PHP peer) and **~3× PHP-FPM**; **Askr worker is ~1.3×
FrankenPHP**. Worker trails CoW here because this app *leaks* — the worker OOMs
and cold-respawns under sustained load, while CoW replaces a dead worker with a
warm re-fork in ~ms. **For leaky apps, prefer CoW** (`--cow`) and/or
`--max-requests` to recycle proactively.

## Results — `/bench-db` (a `SELECT count(*)` — realistic read)

Single run, req/s — the database work narrows the field, as it should. *(These
predate the 0.8.3 worker fix; the worker figure is directional.)*

| Stack | req/s | p50 | p99 |
| --- | ---: | ---: | ---: |
| **Askr — CoW** | **13 424** | 3.8 ms | 32 ms |
| **Askr — worker** | 8 973 | 8.6 ms | 19 ms |
| Octane + FrankenPHP | 7 249 | 8.2 ms | 19 ms |
| PHP-FPM + nginx | 3 868 | 16 ms | 20 ms |
| Octane + RoadRunner² | 3 307 | 16 ms | 99 ms |

Askr still leads, but the gap compresses — because now real work (the query)
dominates, and the *server* matters less. That's the honest takeaway: **Askr's
advantage is largest where framework/boot overhead dominates, and shrinks as your
app does more real work per request.**

## The strategic result: where does the time actually go?

During a 20 s `/bench` load, Askr's own metrics reported:

```
askr_php_seconds_total      1261.2
askr_request_seconds_total  1267.5
```

**PHP execution is 99.5 % of request time. I/O + the Rust layer is ~0.5 %.**

This answers the question we built the benchmark to answer:

> **Is a per-core io_uring I/O core worth it for Laravel?** **No.** io_uring
> optimises the I/O syscalls (accept/read/write) that are already only ~0.5 % of
> the time here. The bottleneck is the PHP engine, not I/O. The efficiency levers
> that *do* matter — booting the app once, opcache + JIT, avoiding FastCGI
> serialisation — are exactly the ones Askr already pulls. io_uring would be
> polishing half a percent.

That's the benchmark earning its keep: it **redirected the roadmap away from a
multi-week runtime rewrite that the data says wouldn't move the needle.**

## Why Askr comes out ahead

- **No per-request boot.** Like Octane/FrankenPHP, the app boots once; unlike
  FPM, there's no framework bootstrap per request.
- **No FastCGI hop.** PHP runs *in-process*; there's no socket serialisation
  between a web server and a PHP pool.
- **Process-per-core (NTS), not threads (ZTS).** No thread-safety locking on the
  hot path; the OS scheduler does the work. This is the main structural
  difference from FrankenPHP, and where the ~1.6× (CoW) on pure overhead comes from.
- **CoW** shares a warm template across workers — competitive throughput at
  slightly lower memory.

## Honest caveats

1. **Shared dev VM.** Run-to-run variance was real (one Askr-CoW sample came in
   ~20 % low; the tables use medians). Treat these as *ratios and shapes*, not
   lab-grade absolutes.
2. **¹ RSS is summed resident memory** and *overcounts* shared pages (opcache,
   CoW). It flatters thread-model servers (FrankenPHP: one process) and
   penalises process-model servers (Askr, FPM, RoadRunner: N processes). PSS
   would be fairer; we didn't measure it here.
3. **² RoadRunner was left on Octane's default config** (not tuned); its ~100 ms
   p99 suggests there's headroom we didn't chase. Don't read its number as
   RoadRunner's best.
4. **Small payloads, `session=array`.** This measures *server + framework
   overhead*, not database-bound throughput or large-response streaming.
5. **arm64.** The VM is Apple-Silicon; x86_64 numbers may differ.
6. **4 workers.** FPM in particular would scale up with more children (at a
   memory cost); we held the worker budget equal for all stacks on purpose.

## Reproduce it

Everything runs in one Ubuntu 24.04 container. Install PHP 8.5 (ondrej PPA),
`nginx`, `wrk`, Composer; create a Laravel 13 app with a `/bench` route; install
`laravel/octane` (+ FrankenPHP and RoadRunner engines) and the Askr 0.8.2
release. Set `SESSION_DRIVER=array` and `php artisan optimize`. Then run the
harness (server on cores 0–3, `wrk` on 4–7, validated):

```bash
#!/usr/bin/env bash
# bench.sh — ROUTE=/bench DUR=20s CONN=64 bash bench.sh
ROUTE="${ROUTE:-/bench}"; DUR="${DUR:-20s}"; CONN="${CONN:-64}"; EXPECT="${EXPECT:-\"ok\":true}"
cd /app
export ASKR_APP_BASE=/app
export ASKR_PHP_INI=$'opcache.enable=1\nopcache.enable_cli=1\nopcache.jit=tracing\nopcache.jit_buffer_size=128M\nopcache.validate_timestamps=0'

run_bench() {
  local name="$1"; shift
  pkill -9 -x rr 2>/dev/null; rm -f /app/*.pid 2>/dev/null
  setsid "$@" >/tmp/srv.log 2>&1 & local PG=$!
  local ok=""
  for i in $(seq 1 50); do
    [ "$(curl -s -o /tmp/v.json -w %{http_code} "http://127.0.0.1:8000$ROUTE")" = "200" ] && { ok=1; break; }; sleep 0.5
  done
  { [ -z "$ok" ] || ! grep -q "$EXPECT" /tmp/v.json; } && { echo "$name: START/VALIDATION FAILED"; kill -9 -$PG; return; }
  taskset -c 4-7 wrk -t4 -c$CONN -d8s "http://127.0.0.1:8000$ROUTE" >/dev/null 2>&1   # warmup
  ( sleep 10; ps -o rss= -g $PG | awk '{s+=$1} END{printf "%.0f", s/1024}' > /tmp/rss.txt ) &
  taskset -c 4-7 wrk -t4 -c$CONN -d$DUR --latency "http://127.0.0.1:8000$ROUTE" > /tmp/wrk.txt 2>&1
  local rps=$(grep 'Requests/sec' /tmp/wrk.txt | awk '{print $2}')
  local nn=$(grep -oE 'Non-2xx or 3xx responses: [0-9]+' /tmp/wrk.txt | grep -oE '[0-9]+')
  printf "%-16s %10s req/s  RSS %sMB  %s\n" "$name" "$rps" "$(cat /tmp/rss.txt)" "${nn:+INVALID:$nn non-2xx}"
  kill -TERM -$PG 2>/dev/null; sleep 3; kill -9 -$PG 2>/dev/null; pkill -9 -x rr 2>/dev/null; sleep 3
}

run_bench "FPM+nginx"      taskset -c 0-3 bash -c "php-fpm8.5 -F & nginx -g 'daemon off;' & wait"
run_bench "Askr-worker"    taskset -c 0-3 /opt/askr/askr serve --root /app/public --worker-script /opt/askr/examples/laravel-worker.php --workers 4 --listen 127.0.0.1:8000
run_bench "Askr-CoW"       taskset -c 0-3 /opt/askr/askr serve --root /app/public --worker-script /opt/askr/examples/laravel-worker.php --cow --workers 4 --listen 127.0.0.1:8000
run_bench "Octane-Franken" taskset -c 0-3 php artisan octane:start --server=frankenphp --workers=4 --port=8000 --host=127.0.0.1
run_bench "Octane-RoadRun" taskset -c 0-3 php artisan octane:start --server=roadrunner --workers=4 --port=8000 --host=127.0.0.1
```

## Environment

- Host: Apple Silicon, OrbStack Linux VM (kernel 7.0.11-orbstack), container
  `ubuntu:24.04`, `--cpus 8` (server pinned to 4, `wrk` to 4).
- PHP **8.5.8** (NTS for Askr/FPM; FrankenPHP bundles its own 8.5.8), opcache + JIT.
- Laravel **13.18.1**, `APP_ENV=production`, `php artisan optimize`, `SESSION_DRIVER=array`.
- Askr **0.8.2**, FrankenPHP **1.12.4**, Laravel Octane **2.17.5**, `wrk` 4.1.0.
