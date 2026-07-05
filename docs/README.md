# Askr documentation

Askr is a standalone, memory-safe **PHP application server** written in Rust. It
embeds the PHP interpreter in-process (no FastCGI, no FPM), serves it from a
memory-safe hot path, and — in worker mode — boots your app once and serves many
requests against it, eliminating per-request framework bootstrap.

> Version **0.5.2**. Production target is Linux; development also works on macOS.

## Start here

| Guide | What it covers |
| --- | --- |
| [Architecture](ARCHITECTURE.md) | How Askr works: embedding, non-ZTS process-per-core, the worker loop, request lifecycle, TLS, recycling, reload, the admin plane. |
| [Building](BUILDING.md) | Building `libphp` (macOS & Ubuntu) and the `askr` binary; the extension matrix. |
| [Configuration](CONFIGURATION.md) | The `askr.toml` reference, CLI flags, and environment variables. |
| [CLI reference](CLI.md) | Every command and flag (`serve`, `doctor`, `config-check`). |
| [Worker mode](WORKER_MODE.md) | Boot-once-serve-many, the Laravel worker script, per-request state reset, writing your own worker. |
| [Power features](FEATURES.md) | Response cache + tag invalidation, coalescing, Pusher WS, `askr_defer`, CoW autoscaling, record/replay, fork test runner. |
| [Docker](DOCKER.md) | Official multi-arch GHCR image — one container replaces app+nginx+redis+queue+cron. |
| [io_uring core (plan)](IO-URING.md) | The remaining efficiency step: a Linux io_uring I/O core behind the `Php::handle` seam, with the tokio path as fallback. |
| [CoW template](COW.md) | Boot once, fork workers (copy-on-write) — ~ms warm respawn + shared memory (experimental). |
| [Shared cache](CACHE.md) | In-binary cache, atomic counters and rate limiting (no Redis); the Laravel driver. |
| [Broadcasting](BROADCAST.md) | Live updates to browsers via SSE + `askr_broadcast()` (no Reverb/Pusher). |
| [Admin dashboard](ADMIN.md) | The built-in status/reload/metrics API and web dashboard. |
| [Deployment](DEPLOYMENT.md) | Production: systemd, TLS, zero-downtime reload, recycling, scaling, hardening. |
| [Ubuntu setup](UBUNTU.md) | **Recommended production install** on Ubuntu (release tarball, systemd, TLS, tuning). |

## 60-second tour

Install a self-contained release (Linux x86_64 / arm64) and serve a Laravel app:

```bash
VER=v0.5.2; ARCH=$(uname -m)
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

## What works today (0.5.2)

- Embedded PHP (non-ZTS) running real Laravel 12, **~9× the per-request/FPM model**
- Multi-core via one worker **process per core** on a shared listen socket
- **Worker mode** (Octane-style) with per-request state reset — no bleed
- **`--paranoid`** state-bleed detector — is your app worker-safe?
- **CoW template** (`--cow`) — boot once, fork workers for ~ms warm respawn (experimental)
- **Queue workers + scheduler** in the same binary (no Horizon/cron)
- **Shared cache** (`askr_cache_*` + Laravel driver) — cache/counters/rate limiting, no Redis
- **Broadcasting** — live updates via SSE + `askr_broadcast()`, no Reverb/Pusher
- Graceful **recycling** + auto-respawn + crash resilience
- **TLS** (rustls) + **HTTP/2**; `--tls-self-signed` for dev
- Zero-downtime **rolling reload** on `SIGHUP`, with optional **canary**
- Request hardening: body-size limit (413), HEAD, GET/POST
- Typed **`askr.toml`** config + `config-check`
- Built-in **admin dashboard + API** (status, reload, live metrics)
- **In-process metrics** — PHP-vs-I/O split, latency histogram, per-worker RSS
- `askr doctor` pre-flight checks

## Not yet

The per-core **io_uring** I/O core (the biggest efficiency step, Linux), HTTP/3
(QUIC), raw WebSockets / Reverb-protocol compatibility, multipart `$_FILES`,
OpenTelemetry export, seccomp/Landlock sandboxing, and the `askr-laravel`
composer package.
