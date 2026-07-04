# Changelog

All notable changes to Askr. This is pre-1.0 exploratory work.

## Unreleased

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
