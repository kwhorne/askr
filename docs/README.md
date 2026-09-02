# Askr documentation

Askr is a standalone, memory-safe **PHP application server** written in Rust. It
embeds the PHP interpreter in-process (no FastCGI, no FPM), serves it from a
memory-safe hot path, and — in worker mode — boots your app once and serves many
requests against it, eliminating per-request framework bootstrap.

> Version **1.5.1**. Production target is Linux; development also works on macOS.

> 📘 These pages are also published, with navigation and search, at
> **[elyracode.com/docs/askr](https://elyracode.com/docs/askr)** — product information is
> at **[elyracode.com/askr](https://elyracode.com/askr)**.

## Start here

| Guide | What it covers |
| --- | --- |
| [Architecture](ARCHITECTURE.md) | How Askr works: embedding, non-ZTS process-per-core, the worker loop, request lifecycle, TLS, recycling, reload, the admin plane. |
| [**Installation**](INSTALL.md) | **Start here** — step by step from nothing to a served Laravel app (Docker, tarball or source) |
| [Building](BUILDING.md) | Building `libphp` (macOS & Ubuntu) and the `askr` binary; the extension matrix. |
| [Releasing](RELEASING.md) | Maintainer checklist: version bump, tag, and verifying the release actually reached users |
| [Configuration](CONFIGURATION.md) | The `askr.toml` reference, CLI flags, and environment variables. |
| [CLI reference](CLI.md) | Every command and flag (`serve`, `doctor`, `config-check`). |
| [Hosting multiple domains](HOSTING.md) | Virtual hosts (`[[site]]`), redirects (www→apex, http→https), multi-domain TLS. |
| [Stability & compatibility](STABILITY.md) | The 1.0 compatibility contract: stable surfaces + deprecation policy. |
| [**Laravel setup**](LARAVEL.md) | **Recommended Laravel guide** — `composer require kwhorne/askr-laravel`, `.env`, runner scripts, queue/scheduler/broadcasting, durable L2, production checklist. |
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
| [Storage backends](STORAGE_BACKEND.md) | L1 shared memory + L2 SQL Anywhere: durable, replicated, multi-box cache/queue/pub-sub (epic elyra-2). |
| [Admin dashboard](ADMIN.md) | The built-in status/reload/metrics API and web dashboard. |
| [Observability](OBSERVABILITY.md) | Ship per-request logs to ElyraSQL / any MySQL-wire database (`--features observ`, `ASKR_OBSERV_DSN`) and query them with SQL. |
| [Deployment](DEPLOYMENT.md) | Production: systemd, TLS, zero-downtime reload, recycling, scaling, hardening. |
| [Upgrading](UPGRADING.md) | How to upgrade and roll back, what's worth adopting at each version, and an honest list of what can bite you. |
| [Ubuntu setup](UBUNTU.md) | **Recommended production install** on Ubuntu (release tarball, systemd, TLS, tuning). |
| [Maintenance](MAINTENANCE.md) | **After it's running**: the 30-second check, deploys, clearing the three caches, certificates, log rotation, backups, and the mistakes already made in production. |

## 60-second tour

Install a self-contained release (Linux x86_64 / arm64) and serve a Laravel app:

```bash
VER=v1.5.1; ARCH=$(uname -m)
curl -fsSLO https://github.com/kwhorne/askr/releases/download/$VER/askr-${VER#v}-linux-$ARCH.tar.gz
tar xzf askr-${VER#v}-linux-$ARCH.tar.gz && cd askr-${VER#v}-linux-$ARCH

./askr-run.sh doctor
ASKR_APP_BASE=/var/www/app ./askr-run.sh serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" --tls-self-signed --admin 127.0.0.1:9000
```

Production setup (systemd, TLS, hardening): [Ubuntu setup](UBUNTU.md). Then
[Maintenance](MAINTENANCE.md) for the operating side.
Building from source: [Building](BUILDING.md).

## Capabilities

| | |
| --- | --- |
| **Runtime** | Embedded PHP 8.5 (non-ZTS, OPcache + JIT), Laravel's full extension set, one worker process per core on a shared listener, worker mode with per-request state reset, `--paranoid` bleed detector, CoW template for ~ms warm respawn |
| **HTTP** | TLS (rustls) with HTTP/2 and optional HTTP/3, auto-TLS via ACME, virtual hosts, redirects, br/gzip compression, streamed static files and multipart uploads |
| **Caching** | Response cache with tag invalidation, ESI fragment assembly, `PURGE`/`BAN`, per-path `[[cache.rule]]`, request coalescing, `stale-if-error`, and it survives restarts |
| **Without Redis** | Shared-memory cache, sessions, atomic locks, counters, job queue and broadcasting, plus a Pusher-compatible WebSocket — with Laravel drivers in [`packages/laravel`](../packages/laravel) |
| **Operations** | Zero-downtime rolling reload with canary judging, supervised queue workers, scheduler and sidecars, fleet-wide rate limiting, leak-aware recycling, record & replay, admin dashboard, Prometheus metrics, OpenTelemetry traces |
| **Security** | Signed and verified self-update, seccomp + Landlock sandbox, body-size limits, `unsafe` confined to the PHP FFI boundary |

Each of these has its own page below, and the same content is published at
[elyracode.com/docs/askr](https://elyracode.com/docs/askr/).

## Where the engine's ceiling is

Per-core **io_uring** is deprioritised, and the reason is measured rather than assumed:
[our benchmarks](BENCHMARKS.md) put PHP execution at ~99.5 % of request time and I/O at
~0.5 %, so an I/O-syscall optimisation would move half a percent. The interpreter, not
the I/O path, is the ceiling — which is why worker mode (removing bootstrap) and the
response cache (removing the interpreter from the path entirely) are where the work has
gone. See [IO-URING.md](IO-URING.md) for the full argument.
