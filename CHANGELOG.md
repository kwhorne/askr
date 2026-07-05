# Changelog

All notable changes to Askr. This is pre-1.0 exploratory work.

## 0.8.0 — 2026-07-05

- **Hardening / sandbox (Linux)** — `--sandbox` shrinks the blast radius of a
  PHP-level exploit:
  - **seccomp** (all threads): `execve`/`execveat`/`ptrace`/`process_vm_*` return
    `EPERM` — a compromised request can't spawn a shell.
  - **Landlock** (with `--sandbox-write <dir>`, repeatable): read everywhere, but
    write only under the listed paths — can't drop a webshell into the docroot.
  Applied before the PHP/tokio threads spawn (so it covers the thread PHP runs
  on); sidecars are left unsandboxed (jobs may shell out). `[server] sandbox` /
  `sandbox_write` in askr.toml. No effect off Linux; Landlock degrades gracefully.
  See docs/SANDBOX.md.
  - Verified in a Linux container: `shell_exec` → blocked, write to `/tmp` → ok,
    write into the docroot → denied, normal pages unchanged.

## 0.7.0 — 2026-07-05

- **Automatic TLS (ACME / Let's Encrypt)** — the last piece of "single binary, no
  proxy". `--acme --acme-domain example.com --acme-email you@example.com` obtains
  a certificate over **HTTP-01** and renews it automatically. Prefork-safe: the
  **master** answers challenges on `--acme-http` (default `0.0.0.0:80`) and
  obtains the cert *before* forking; workers only serve HTTPS from the cache, and
  a background renewal thread rolls them with zero downtime when the cert renews.
  `--acme-staging` for Let's Encrypt staging; `--acme-directory`/`--acme-ca-root`
  for a private CA / Pebble. See docs/AUTOTLS.md.
  - Uses `instant-acme`; a process-wide ring `CryptoProvider` is pinned (instant-
    acme brings aws-lc-rs alongside our ring stack).
  - Verified end to end against **Pebble**: account → order → finalize →
    certificate issued (by "Pebble Intermediate CA"), and Askr serves HTTPS with
    it; the HTTP-01 challenge server is unit-tested.

## 0.6.1 — 2026-07-05

- **Shared-memory job queue** — the last common Redis use. A fixed-slot job table
  in shared memory (`--queue-slots N` / `[queue] slots`) backs new `askr_queue_*`
  builtins: `push`(delayed), `pop`(reserve with a visibility timeout), `delete`
  (ack), `release`(retry), `size`. Delayed jobs, attempt counting, per-queue
  isolation, and reclaim of jobs whose reserving worker died. `examples/AskrQueue.php`
  is a Laravel queue driver on top; the existing `--queue`/`--queue-script`
  sidecar runs the workers. On a single box, Redis is now replaceable for cache,
  counters, locks, sessions, pub/sub **and queues**.
  - Verified: push/size, FIFO pop by availability, reserve (second pop skips the
    reserved job), release→retry with incremented attempts, delayed jobs not
    popped early, queue isolation. Unit-tested + exercised over HTTP.

## 0.6.0 — 2026-07-05

- **Redis-free sessions, locks and bigger cache values.** The shared cache now
  has two **size classes**: the small region (`--cache-slots`, 4 KB — counters,
  locks, small entries) and an optional large region (`--cache-large-slots` /
  `[cache] large_slots`, 64 KB — sessions, cached fragments, serialized
  collections). `set` routes by size and clears the key from the other region;
  `get`/`delete` check both.
  - New **`askr_cache_add`** — atomic set-if-absent, the primitive behind
    `Cache::add()` and `Cache::lock()`. `AskrCacheStore` now implements Laravel's
    `LockProvider`, so `Cache::lock()` is truly atomic across all workers in
    shared memory.
  - With the large region, Laravel **sessions** run on the cache
    (`SESSION_DRIVER=cache`, `SESSION_STORE=askr`).
  - Internals: `cache.rs` is generic over value size (const generics); eviction
    (oldest-first) + `askr_cache_evictions_total` carried over.
  - So on a single box, Redis is replaceable for cache, counters, locks, sessions
    and pub/sub — queues still use the DB driver. See docs/CACHE.md.

## 0.5.2 — 2026-07-05

- **Supervised external sidecars.** The supervisor can now run arbitrary external
  commands alongside the web/queue/scheduler slots — spawned, respawned if they
  die, and stopped gracefully with the rest (run via `sh -c` in `$ASKR_APP_BASE`).
  Enables **Inertia SSR** (`--sidecar "node bootstrap/ssr/ssr.mjs"` /
  `[[sidecar]] command = …`) and any other helper process in the same container.
  Verified: a node SSR-style server spawns, is respawned on kill, and drains on
  shutdown.

## 0.5.1 — 2026-07-05

- **Fix: empty static files.** A 0-byte static asset was served with
  `Content-Length: 1` and a truncated (empty) body, so the browser saw a broken
  response — which breaks a `<script type="module">` load. This is common with a
  Vite **CSS-only entry** (`resources/js/app.js` is empty, so its built `.js` is
  0 bytes). Empty files are now served correctly (`Content-Length: 0`). Found
  while running a real Livewire Flux app in a container.

## 0.5.0 — 2026-07-05

- **Run any Laravel app, including Filament.** The laravel-profile `libphp` now
  bundles the extensions heavier apps need: **intl** (Filament requires it),
  **gd** (+ jpeg/freetype/webp) + **exif**, **curl**, **zip**, **zlib**, and
  **pdo_mysql** (mysqlnd) / **pdo_pgsql** — on Linux, where the release tarballs
  and Docker image are built. The macOS dev build keeps the core set (its
  static-dependency build is for the test suite). `askr doctor` now reports a
  RECOMMENDED extension set (intl/curl/gd/pdo_mysql/zip).
  - Build deps added (CI + release + docs): `libicu-dev libcurl4-openssl-dev
    libpng-dev libjpeg-dev libfreetype-dev libwebp-dev libzip-dev zlib1g-dev
    libpq-dev`; matching runtime libs in the Docker image / release notes
    (`libicu74 libcurl4 libpng16-16 libjpeg-turbo8 libfreetype6 libwebp7 libzip4
    libpq5 zlib1g`).
  - `examples/docker/` bumped to the `:0.5` base and uses
    `composer install --ignore-platform-reqs` (build PHP ≠ Askr's runtime PHP).

## 0.4.2 — 2026-07-05

- **Docker support** — an official multi-arch image on GHCR
  (`ghcr.io/kwhorne/askr`, `linux/amd64` + `linux/arm64`), packaged from the
  relocatable release tarball on `ubuntu:24.04` (glibc match with CI; not Alpine
  — see docs/DOCKER.md). One container is the whole environment: web workers,
  queue, scheduler, cache and broadcasting in one process tree — replacing the
  usual app+nginx+redis+queue+cron stack. Ships a `HEALTHCHECK` (admin API),
  `STOPSIGNAL SIGTERM` (graceful drain), non-root, `EXPOSE 8000 9000`. New
  `Dockerfile`, `.dockerignore`, `docker.yml` workflow, and `docs/DOCKER.md`
  (compose, signals, read-only + tmpfs, TLS-behind-LB).
- **cgroup-aware workers** — the default worker count now reads the container's
  CPU limit (cgroup v2 `cpu.max`, v1 fallback) instead of the host core count, so
  a `cpus: 2` container forks 2 workers, not `nproc`. Falls back to host cores
  outside a limited cgroup.

## 0.4.1 — 2026-07-05

Server-environment completeness: compression, logging, observability.

- **Response compression** — compressible responses (HTML/JSON/JS/CSS/SVG/…) are
  compressed in the Rust hot path, negotiating `br` (preferred) or `gzip` from
  `Accept-Encoding`; often 5–10× fewer bytes on the wire. Applies to dynamic PHP
  responses, cached responses, and small static files (large files keep
  streaming). Pure-Rust encoders (`flate2` + `brotli`) — the self-contained build
  is unaffected. Adds `Content-Encoding` + `Vary`; compressed static ETags get a
  `-br`/`-gz` suffix and conditional GET tolerates it.
- **Structured access log** — `--access-log <path|->` / `[server] access_log`
  writes one JSON line per request (ts, ip, method, path, status, bytes, dur_ms),
  covering every response path (static, cache, SSE, Pusher, PHP). Off by default.
- **Prometheus `/metrics`** — the admin plane now exposes Prometheus text format
  (requests/errors/bytes, PHP-vs-total seconds, status classes, cache
  hits/misses/coalesced/evictions, in-flight + live-workers gauges, a request
  latency histogram) so Askr is scrapeable by standard tooling.
- **KV cache eviction** — under pressure the cache now evicts an expired entry,
  else the oldest-written one (was: overwrite the primary slot blindly), with a
  new `askr_cache_evictions_total` metric.

## 0.4.0 — 2026-07-05

- **Multipart file uploads (worker mode)** — the last big thing blocking "run any
  Laravel app". `multipart/form-data` is now **streamed**: each file part is
  written straight to a temp file (constant memory regardless of size — a 32 MB
  upload no longer costs 32 MB of RAM), and form fields are parsed to POST
  params. Askr hands PHP the `$_FILES`-shaped metadata (name, type, tmp path,
  size); `examples/laravel-worker.php` rebuilds them as Laravel `UploadedFile`s
  in test mode so `$request->file('avatar')->store(...)` works (the Octane model).
  Temp files are cleaned up after each request; the existing `--max-body-size`
  limit is enforced on the stream (413). New request-contract fields + shim
  setters (`askr_req_add_post`/`askr_req_add_file`).
  - Verified: a 2 MB upload round-trips with a matching SHA-1, POST fields arrive,
    the temp file is removed afterward, and an over-limit upload gets a 413.

## 0.3.2 — 2026-07-05

- **io_uring groundwork** (Linux is where the runtime swap lands):
  - `askr doctor` now *probes* io_uring via `io_uring_setup(2)` instead of only
    guessing from the kernel version — a recent kernel can still have it disabled
    (`kernel.io_uring_disabled`). Non-fatal: Askr falls back to the epoll/tokio path.
  - `scripts/bench.sh` — a benchmark harness (auto-detects oha/wrk/hey/ab) for
    comparing scenarios (tokio vs io_uring, and vs FrankenPHP / php-fpm).
  - `docs/IO-URING.md` — the design & de-risking plan (seam, monoio/tokio-uring
    tradeoffs, Linux+capability gating, phased rollout, benchmark methodology).

## 0.3.1 — 2026-07-05

- **Pusher private/presence auth** — `private-`/`presence-` subscriptions are now
  verified against the app secret (`--pusher-secret` / `$ASKR_PUSHER_SECRET` /
  `[pusher] secret`): a subscription must carry the same
  `HMAC-SHA256(secret, "socket_id:channel[:channel_data]")` token Laravel's
  `/broadcasting/auth` issues, or it's rejected with a `subscription_error`.
  Without a secret configured they're still accepted (dev). Closes the honest gap
  from 0.3.0; private channels are actually private now. Unit-tested end to end.

## 0.3.0 — 2026-07-05

Seven features that fall out of Askr's architecture (shared-memory substrate +
CoW + full request-lifecycle control) — several are things no other PHP server
can do.

### Edge cache
- **Response cache with instant tag invalidation** (`--response-cache <slots>`).
  PHP opts a response in with `header('Askr-Cache: 60, tags=posts,homepage')`;
  matching anonymous `GET`/`HEAD` requests are served straight from Rust,
  bypassing PHP entirely — static-file speed for cacheable pages.
  `askr_cache_forget_tag('posts')` bumps a generation counter in a shared tag
  table, invalidating every entry with that tag across **all** workers at once
  (O(1), no scan). `Set-Cookie` is stripped on store; only cookie-less GET/HEAD
  are cacheable. `X-Askr-Cache: HIT|MISS` + hit-rate on the dashboard.
- **Request coalescing (singleflight)** — when identical cacheable requests hit
  a cold cache together, one runs PHP and the rest wait for the fill. Cache
  stampedes are eliminated across worker processes.

### Real-time
- **Pusher-compatible WebSocket + trigger** (`--pusher`) — a drop-in Reverb:
  WS `/app/{key}` (connect / subscribe / ping) and the HTTP trigger
  `POST /apps/{id}/events` that Laravel's broadcaster calls. Rides the shared
  broadcast ring, so a trigger in any worker reaches subscribers in all of them.
  Laravel Echo works with no frontend config change. (Auth-signature
  verification for private/presence channels is a follow-up.)

### Lifecycle
- **`askr_defer()`** — register work that runs after the response is sent to the
  client, before the worker takes the next request (email, webhooks, logging) —
  Octane-style deferred work with no queue.
- **Elastic worker autoscaling** in CoW mode (`--workers-min`/`--workers-max`).
  The template sizes the pool on a live queue-depth signal, adding warm workers
  (~ms respawn) under load and harvesting them when idle. Process autoscaling has
  never been practical for PHP (~300ms cold boot) — CoW makes it cheap.

### Operations
- **Record & replay** (`--record-errors <dir>`) — a 5xx persists its full CGI
  envelope; `askr replay <id.json>` re-runs the exact request against a fresh
  interpreter. Recent failures are listed on the dashboard.
- **Fork-based parallel test runner** (`askr test`) — boot once, fork a warm,
  isolated process per test file (PHPUnit/Pest via `examples/askr-test.php`).

### Maintenance
- Deps: `rcgen` 0.13 → 0.14 (`CertifiedKey::key_pair` → `signing_key`),
  `toml` 0.8 → 1.1, `thiserror` 1 → 2. CI actions: `actions/checkout` 5 → 7,
  `actions/cache` 4 → 6.
- shim: `run_script` returns `EG(exit_status)` (correct exit(0)=0 handling).

## 0.2.1 — 2026-07-04

Hardening and distribution — no new user-facing features, but a tougher hot path,
deterministic CI, and downloadable releases.

### Server
- **Static files are streamed** in 64 KB chunks (a large file no longer buffers
  entirely in RAM per request), with **ETag** + **Cache-Control** (`immutable`
  for hashed `/build/` assets), **conditional GET** (`304` on `If-None-Match`),
  and single-**Range** (`206`) support.
- **Slowloris hardening** — TLS handshake timeout (10s), HTTP/1 header-read
  timeout (15s), and a per-worker connection cap that sheds load; important since
  Askr is designed to run with no proxy in front.
- `try_files` now stats with async `tokio::fs::metadata` (no blocking syscall on
  the async path); connections are served with upgrades enabled.

### Distribution
- **Self-contained release packages** — `scripts/package-release.sh` + a
  `release.yml` workflow build relocatable tarballs (binary + libphp + opcache +
  examples, rpath fixed to `$ORIGIN/lib`) for **Linux x86_64 and arm64** and
  attach them to the GitHub Release on each tag.
- **Ubuntu production setup guide** — `docs/UBUNTU.md`: recommended hardened
  install (release tarball, non-root systemd on `:443` via capabilities, Let's
  Encrypt via webroot, tuned opcache, canary deploys, recommended settings).

### CI / toolchain
- **Pinned Rust** (`rust-toolchain.toml` → 1.95.0) so a new release can't turn
  `main` red under `clippy -D warnings` without a code change; CI reads the pin.
- **Cached libphp** in CI (keyed on the build script) — skips recompiling PHP on
  a cache hit, the slowest step. Bumped `checkout@v5` / `cache@v4`.

## 0.2.0 — 2026-07-04

Seven differentiators beyond the core server (see the guides in `docs/`):

- **CoW template mode (`--cow`, experimental)** — boot the app once in a template
  process and fork the workers from it (copy-on-write). Workers inherit the warm,
  booted heap: **~ms warm respawn** (measured ~35 ms vs ~300 ms cold) and shared
  opcache/class tables. The template is single-threaded when it forks (tokio
  starts only in children), so the fork is safe. New code is picked up by
  restarting the process. `examples/laravel-worker.php` calls `askr_cow_ready()`.

- **Canary reload (`--canary`)** — a `SIGHUP` reload rolls one worker first and
  health-checks it (alive, no error spike) for a few seconds before rolling the
  rest; a broken deploy aborts the reload and takes down one worker instead of
  the whole fleet. Reuses the shared metrics for the health signal.
- **Broadcasting (SSE)** — push live updates to browsers with no external broker.
  `askr_broadcast($channel, $payload)` from PHP publishes into a shared-memory
  ring; each worker tails it and fans events out to the SSE connections it holds,
  so a publish from any process reaches subscribers on any process. Browsers
  subscribe at `GET /askr/events?channel=NAME` (true streaming body). Enable with
  `--broadcast` / `[broadcast]`. Verified cross-process incl. channel filtering.
- **Shared-memory cache exposed to PHP** — a fixed-slot hash table in an
  anonymous shared mmap (created before fork, shared by all workers) backs
  `askr_cache_get/set/delete/increment/flush`: cache, **atomic counters** (rate
  limiting) and locks in the Askr binary, no Redis for small/mid deployments.
  Per-slot spinlock (stolen if a holder dies), lazy TTL, length-clamped reads
  (memory-safe under races). Enable with `--cache-slots N` / `[cache] slots`.
  Ships a Laravel cache `Store` (`examples/AskrCacheStore.php`). Verified
  cross-process: set on one worker → get on others, 100/100 concurrent
  increments exact, `Cache::remember` computed once and shared.
- **In-process metrics + admin observability** — a shared-memory metrics region
  (mmap'd before fork, so all workers share the same atomic counters, no IPC)
  records throughput, latency (avg, slowest, histogram), status classes, and the
  **PHP-vs-I/O time split** that only an in-process server can measure. Exposed
  at `GET /api/metrics`, with per-worker RSS added to `/api/status` (the leak
  signal), and rendered live on the admin dashboard. Seeds the shared-memory
  substrate for a future cross-process cache/broadcast.
- **Whole Laravel runtime in one binary** — the master now supervises **queue
  workers** (`--queue N --queue-script`, or `[queue]`) and the **scheduler**
  (built-in cron; `--scheduler-script`, or `[scheduler]`) alongside the web
  workers: forked as sidecar processes running `queue:work` / `schedule:run`
  in-process, respawned on exit, drained on shutdown. No separate `php artisan`
  processes, systemd units, or Horizon/crontab needed for basic setups.
  `examples/askr-queue.php`, `examples/askr-scheduler.php`; `Interpreter::run_script`.
- **State-bleed detector (`--paranoid`)** — dev-only worker-mode diagnostic that
  snapshots app state (static properties, `$GLOBALS`, Laravel container
  bindings/instances) after each request's reset and reports anything that keeps
  growing, so Askr can tell you whether your app is worker-safe. Warms up a
  couple of requests to avoid flagging one-time boot drift; verified clean on a
  real Laravel app and catching a deliberate leak.
  `examples/askr-paranoid.php`, `[worker] paranoid`.

## 0.1.0 — 2026-07-03

First tagged release. A complete, deployable PHP application server: embedded
non-ZTS PHP running real Laravel 12 in worker mode (~9× the FPM model),
multi-core, TLS + HTTP/2, graceful recycling and zero-downtime reload, a typed
config and an admin dashboard. See [`docs/`](docs/README.md).

### Server (`askr`)
- **A1** — standalone `askr serve`: serves a real app over HTTP through the
  in-process interpreter (no FastCGI, no FPM).
- **A3** — multi-core scaling: the master forks one worker process per core,
  all accepting on a shared inherited listen socket (portable prefork).
- **A4a** — persistent worker loop: `askr_handle_request($handler)` lets a worker
  boot the app once and serve many requests (Octane model, in-process).
- **A4b** — real Laravel 12 in worker mode via `examples/laravel-worker.php`;
  ~9× the per-request (FPM) model on a Livewire app.
- **A5a** — graceful worker recycling (`--max-requests`) with drain + auto-respawn
  and crash resilience; staggered per worker.
- **A5b** — Octane-style per-request state reset (scoped instances, request, auth
  guards, DB transactions, `Str` caches) — no state bleed between requests.
- **A5c** — TLS via rustls (ring; no OpenSSL/C toolchain) + HTTP/2 (ALPN);
  `askr doctor` pre-flight (non-ZTS, required extensions, io_uring kernel).
- **A5d** — graceful **rolling reload** on `SIGHUP` (zero-downtime code deploys);
  `--tls-self-signed` (rcgen).
- **A2** — request hardening: `--max-body-size` (413 on oversize, incl. chunked),
  HEAD, and verified GET/POST (form + JSON) handling.
- **A6** — typed `askr.toml` config (source of truth for tooling/GUI),
  `askr config-check`, and a built-in **admin dashboard + API** in the master
  (`GET /`, `GET /api/status`, `POST /api/reload`) — the server-appropriate GUI
  for maintaining/configuring a live server.

### Embedded PHP (`askr-php`)
- **M0** — proved PHP embed SAPI runs in-process from Rust (non-ZTS), capturing
  output via a SAPI `ub_write` override.
- **M0+** — full request contract: `$_SERVER` injection, `php://input` body, and
  captured HTTP status + headers + body. Discovered the extension matrix and
  built oniguruma/OpenSSL/libxml2 (statically on macOS) so real Laravel renders.

### Build / platform
- OS-aware `scripts/build-libphp.sh`: system dev libs via pkg-config on Linux
  (`libphp.so`); from-source static deps on macOS (`libphp.dylib`).
- [`docs/UBUNTU.md`](docs/UBUNTU.md): full Ubuntu build + deploy guide (systemd).

### Not yet
- HTTP/3 (QUIC), the per-core io_uring I/O core (Linux), multipart `$_FILES`,
  and the `askr-laravel` composer package.
