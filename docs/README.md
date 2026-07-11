# Askr documentation

Askr is a standalone, memory-safe **PHP application server** written in Rust. It
embeds the PHP interpreter in-process (no FastCGI, no FPM), serves it from a
memory-safe hot path, and — in worker mode — boots your app once and serves many
requests against it, eliminating per-request framework bootstrap.

> Version **0.9.0**. Production target is Linux; development also works on macOS.

## Start here

| Guide | What it covers |
| --- | --- |
| [Architecture](ARCHITECTURE.md) | How Askr works: embedding, non-ZTS process-per-core, the worker loop, request lifecycle, TLS, recycling, reload, the admin plane. |
| [Building](BUILDING.md) | Building `libphp` (macOS & Ubuntu) and the `askr` binary; the extension matrix. |
| [Configuration](CONFIGURATION.md) | The `askr.toml` reference, CLI flags, and environment variables. |
| [CLI reference](CLI.md) | Every command and flag (`serve`, `doctor`, `config-check`). |
| [Worker mode](WORKER_MODE.md) | Boot-once-serve-many, the Laravel worker script, per-request state reset, writing your own worker. |
| [Power features](FEATURES.md) | Response cache + tag invalidation, coalescing, Pusher WS, `askr_defer`, CoW autoscaling, record/replay, fork test runner. |
| [Auto-TLS (ACME)](AUTOTLS.md) | Obtain + renew Let's Encrypt certs over HTTP-01 (`--acme`) — no proxy. |
| [Hardening / sandbox](SANDBOX.md) | seccomp no-exec + Landlock filesystem sandbox (`--sandbox`, Linux). |
| [Docker](DOCKER.md) | Official multi-arch GHCR image — one container replaces app+nginx+redis+queue+cron. |
| [Benchmarks](BENCHMARKS.md) | Reproducible comparison vs FrankenPHP, PHP-FPM+nginx and RoadRunner — and the PHP-vs-I/O split that shaped the roadmap. |
| [io_uring core (plan)](IO-URING.md) | Design notes for a Linux io_uring I/O core. **Deprioritised** — benchmarks show PHP is ~99.5% of request time, so I/O isn't the bottleneck. |
| [CoW template](COW.md) | Boot once, fork workers (copy-on-write) — ~ms warm respawn + shared memory (experimental). |
| [Shared cache](CACHE.md) | In-binary cache, atomic counters and rate limiting (no Redis); the Laravel driver. |
| [Broadcasting](BROADCAST.md) | Live updates to browsers via SSE + `askr_broadcast()` (no Reverb/Pusher). |
| [Admin dashboard](ADMIN.md) | The built-in status/reload/metrics API and web dashboard. |
| [Deployment](DEPLOYMENT.md) | Production: systemd, TLS, zero-downtime reload, recycling, scaling, hardening. |
| [Ubuntu setup](UBUNTU.md) | **Recommended production install** on Ubuntu (release tarball, systemd, TLS, tuning). |

## 60-second tour

Install a self-contained release (Linux x86_64 / arm64) and serve a Laravel app:

```bash
VER=v0.9.0; ARCH=$(uname -m)
curl -fsSLO https://github.com/kwhorne/askr/releases/download/$VER/askr-${VER#v}-linux-$ARCH.tar.gz
tar xzf askr-${VER#v}-linux-$ARCH.tar.gz && cd askr-${VER#v}-linux-$ARCH

./askr-run.sh doctor
ASKR_APP_BASE=/var/www/app ./askr-run.sh serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" --tls-self-signed --admin 127.0.0.1:9000
```

Production setup (systemd, TLS, hardening): [Ubuntu setup](UBUNTU.md).
Building from source: [Building](BUILDING.md).

## What works today (0.9.0)

- Embedded PHP (non-ZTS) running real Laravel 12, **~9× the per-request/FPM model**
- Multi-core via one worker **process per core** on a shared listen socket
- **Worker mode** (Octane-style) with per-request state reset — no bleed
- **`--paranoid`** state-bleed detector — is your app worker-safe?
- **CoW template** (`--cow`) — boot once, fork workers for ~ms warm respawn (experimental)
- **Queue workers + scheduler + sidecars** in the same binary (no Horizon/cron)
- **Shared cache / sessions / locks / job queue** (`askr_cache_*`, `askr_queue_*` + Laravel drivers) — **fully replaces Redis** on a single box
- **Broadcasting** — SSE + `askr_broadcast()`, plus a **Pusher-compatible WebSocket** (`--pusher`, drop-in Reverb with auth)
- **Response cache** with tag invalidation, request **coalescing**, `askr_defer()` post-response work
- **Multipart uploads** (`$_FILES`) + response **compression** (br/gzip)
- **Record & replay** failing requests (`--record-errors` / `askr replay`)
- Graceful **recycling** + auto-respawn + crash resilience
- **TLS** (rustls) + **HTTP/2**; `--tls-self-signed` for dev; **auto-TLS via ACME** (`--acme`)
- **Hardening** (`--sandbox`, Linux): seccomp no-exec + Landlock write-restriction
- Zero-downtime **rolling reload** on `SIGHUP`, with optional **canary**
- Request hardening: body-size limit (413), HEAD, GET/POST
- Typed **`askr.toml`** config + `config-check`
- Built-in **admin dashboard + API** (status, reload, live metrics)
- **In-process metrics** — PHP-vs-I/O split, latency histogram, per-worker RSS
- `askr doctor` pre-flight checks

## Not yet

**HTTP/3** (QUIC) and **OpenTelemetry** trace export. The per-core **io_uring**
core is **deprioritised**: our
[benchmarks](BENCHMARKS.md) show PHP execution is ~99.5% of request time, so an
I/O-syscall optimisation would move ~0.5% — the engine, not I/O, is the ceiling.
