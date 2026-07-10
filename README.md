<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-full-dark.svg">
    <img src="assets/logo-full.svg" alt="Askr — the real server for Laravel & PHP" width="440">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/kwhorne/askr/actions/workflows/ci.yml"><img src="https://github.com/kwhorne/askr/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  &nbsp;·&nbsp; <strong>v0.8.4</strong> &nbsp;·&nbsp; MIT
</p>

**A standalone, memory-safe PHP application server, in Rust.**

Askr embeds the PHP interpreter in-process (no FastCGI, no FPM), serves it from a
memory-safe Rust hot path, and — in worker mode — boots your app **once** and
serves many requests against it, eliminating per-request framework bootstrap. It
is a complete single binary: TLS, HTTP/2, static files, worker supervision and an
admin dashboard, with no proxy required in front.

It's the server engine behind the [`grove`](https://github.com/wirelabs/grove)
ecosystem; Grove stays the local dev tool, Askr is the production server.

## Headline result

Real **Laravel 12 + Livewire**, served entirely in-process:

| | per-request (the FPM model) | **worker mode (boot once)** |
| --- | --- | --- |
| latency / request | ~110 ms | **~9 ms** |
| throughput (8 workers) | 37 req/s | **347 req/s** |

**~9×**, verified correct under load (300/300 `200`, each worker booted exactly
once, zero state bleed). Raw embedding overhead is ~0.02 ms/request
(~56k req/s single-core for a trivial script) — the framework bootstrap is the
cost, and worker mode removes it.

## Install

Grab a **self-contained** release for Linux (x86_64 or arm64) — the binary,
embedded PHP, opcache, and examples in one tarball, nothing else to install:

```bash
VER=v0.8.4; ARCH=$(uname -m)
curl -fsSLO https://github.com/kwhorne/askr/releases/download/$VER/askr-${VER#v}-linux-$ARCH.tar.gz
tar xzf askr-${VER#v}-linux-$ARCH.tar.gz && cd askr-${VER#v}-linux-$ARCH

./askr-run.sh doctor
ASKR_APP_BASE=/var/www/app ./askr-run.sh serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" --tls-self-signed --admin 127.0.0.1:9000
```

> Runtime libs (usually already present): `sudo apt-get install -y libssl3 libxml2 libonig5 libsqlite3-0`.

**Production setup** (systemd, TLS, hardening, recommended settings):
**[docs/UBUNTU.md](docs/UBUNTU.md)**. Building from source: [docs/BUILDING.md](docs/BUILDING.md).

## Documentation

Everything lives in [`docs/`](docs/README.md):

- [Ubuntu setup](docs/UBUNTU.md) — **recommended production install** (systemd, TLS, tuning)
- [Architecture](docs/ARCHITECTURE.md) — how it works, and why processes not threads
- [Building](docs/BUILDING.md) — `libphp` + `askr`, the extension matrix
- [Configuration](docs/CONFIGURATION.md) — `askr.toml`, env vars
- [CLI reference](docs/CLI.md) — every command and flag
- [Worker mode](docs/WORKER_MODE.md) — boot-once-serve-many, state reset, custom workers
- [Docker](docs/DOCKER.md) — one container replaces app+nginx+redis+queue+cron (GHCR, multi-arch)
- [Power features](docs/FEATURES.md) — response cache + tags, coalescing, Pusher WS, defer, autoscaling, record/replay, test runner
- [Shared cache](docs/CACHE.md) — `askr_cache_*` + Laravel driver (no Redis)
- [Broadcasting](docs/BROADCAST.md) — live updates via SSE (no Reverb/Pusher)
- [CoW template](docs/COW.md) — boot once, fork workers for ~ms warm respawn (experimental)
- [Admin dashboard](docs/ADMIN.md) — status/reload/metrics API and web UI
- [Auto-TLS (ACME)](docs/AUTOTLS.md) — obtain + renew Let's Encrypt certs (`--acme`)
- [Hardening / sandbox](docs/SANDBOX.md) — seccomp + Landlock (`--sandbox`)
- [Benchmarks](docs/BENCHMARKS.md) — vs FrankenPHP, FPM+nginx, RoadRunner (reproducible)
- [Deployment](docs/DEPLOYMENT.md) — systemd, TLS, zero-downtime reload, scaling

## What works today (0.8.4)

- Embedded **PHP 8.5** (**non-ZTS**, OPcache + JIT built in) running real Laravel 13 — no FastCGI, no FPM
- **All of Laravel's required extensions** + more (intl, gd, curl, zip, pdo_mysql/pgsql, …) — runs Filament apps
- **File uploads** stream to temp files (constant memory) with `$request->file()` in worker mode
- **Response compression** (br/gzip by `Accept-Encoding`), **JSON access log**, and **Prometheus** `/metrics`
- Multi-core: one worker **process per core** on a shared listen socket
- **Worker mode** (Octane-style) with per-request state reset — no bleed
- **Response cache with tag invalidation** (`--response-cache`): cacheable pages
  served from Rust at static-file speed; `askr_cache_forget_tag()` invalidates
  across all workers instantly — a Varnish-effect, app-driven, zero infra
- **Request coalescing**: identical cold-cache requests run PHP once (no stampede)
- **Pusher-compatible WebSocket** (`--pusher`): drop-in Reverb — Echo works with
  no frontend change (WS `/app/{key}` + trigger `POST /apps/{id}/events`)
- **`askr_defer()`**: run work after the response is sent (email/webhooks, no queue)
- **Elastic autoscaling** (CoW, `--workers-min/--workers-max`): warm workers added
  under load and harvested when idle — practical only because respawn is ~ms
- **Record & replay** (`--record-errors`): a 5xx is replayable with `askr replay`
- **Fork-based test runner** (`askr test`): boot once, warm isolated process per file
- **`--paranoid`** state-bleed detector: tells you if your app is worker-safe
- **CoW template** (`--cow`): boot once, fork workers — ~ms warm respawn + shared memory
- **Queue workers + scheduler** supervised in the same binary (no Horizon/cron)
- **Shared cache** (`askr_cache_*` + Laravel driver): cache, counters, atomic **locks** (`Cache::lock`) and **sessions** (large region) — no Redis
- **[`askr-laravel`](packages/laravel) package**: drop-in `askr` **session** (shared-memory, no heap leak, ~11–15k req/s flat), **cache** and **queue** drivers via `composer require`
- **Broadcasting**: live updates to browsers via SSE + `askr_broadcast()` — no Reverb/Pusher
- Graceful worker **recycling** (`--max-requests`) + auto-respawn + crash resilience
- **TLS** (rustls, ring) + **HTTP/2** (ALPN); `--tls-self-signed` for dev, or **auto-TLS via ACME/Let's Encrypt** (`--acme`)
- Zero-downtime **rolling reload** on `SIGHUP` — with optional **canary** (bad deploys hit one worker, not all)
- Request hardening: body-size limit (`413`), HEAD, GET/POST
- Typed **`askr.toml`** config + `config-check`
- Built-in **admin dashboard + API** (status, graceful reload, live metrics)
- **In-process metrics**: PHP-vs-I/O time split, latency histogram, per-worker RSS
- `askr doctor` pre-flight checks, and **`askr upgrade`** (verified self-update: sha256 + atomic swap)
- **Hardening** (`--sandbox`, Linux): seccomp no-exec + Landlock write-restriction
- Memory-safe: all `unsafe` confined to the PHP FFI boundary

## Roadmap

| Phase | Status |
| --- | --- |
| M0 — embedding spike (PHP in-process from Rust) | ✅ |
| A1 — standalone `askr serve` over HTTP | ✅ |
| A3 — multi-core (fork per core, shared listener) | ✅ |
| A4 — worker mode: real Laravel, zero per-request bootstrap | ✅ |
| A5 — recycling, state reset, TLS+HTTP/2, rolling reload, doctor | ✅ |
| A2 — request hardening (body limit, HEAD, POST) | ✅ |
| A6 — typed config + admin dashboard/API | ✅ |
| **0.2.0** — paranoid, shared cache, SSE broadcast, queue+scheduler, metrics, canary reload, CoW template | ✅ |
| self-contained Linux releases (x86_64 + arm64) | ✅ |
| **0.2.1** — static caching/streaming/Range, slowloris timeouts, pinned & cached CI | ✅ |
| **0.3.0** — response cache + tag invalidation, coalescing, Pusher WS, `askr_defer`, CoW autoscaling, record/replay, fork test runner | ✅ |
| **0.3.1** — Pusher private/presence auth (HMAC subscription verification) | ✅ |
| **0.3.2** — io_uring groundwork: `doctor` probe, benchmark harness, design plan | ✅ |
| **0.4.0** — multipart file uploads (streamed to temp files, `$_FILES` in worker mode) | ✅ |
| **0.4.1** — response compression (br/gzip), JSON access log, Prometheus `/metrics`, KV cache eviction | ✅ |
| **0.4.2** — Docker image (GHCR, multi-arch), cgroup-aware worker default | ✅ |
| **0.5.0** — full extension set (intl/gd/curl/zip/pdo_mysql/pgsql) — runs Filament | ✅ |
| **0.5.1** — fix: empty static files (Vite CSS-only entry) served with correct Content-Length | ✅ |
| **0.5.2** — supervised external sidecars (Inertia SSR: `node bootstrap/ssr/ssr.mjs`) | ✅ |
| **0.6.0** — cache size classes (64 KB values), atomic `add`/`Cache::lock`, sessions — Redis-free | ✅ |
| **0.6.1** — shared-memory job queue (`askr_queue_*` + AskrQueue driver): delayed jobs, retries | ✅ |
| **0.7.0** — auto-TLS via ACME/Let's Encrypt (`--acme`, HTTP-01) — single binary, no proxy | ✅ |
| **0.8.0** — hardening: seccomp no-exec + Landlock filesystem sandbox (`--sandbox`) | ✅ |
| **0.8.2** — PHP 8.5 + Laravel 13, OPcache/JIT built in | ✅ |
| Benchmarks: CoW ~1.6× FrankenPHP, ~3× FPM on server overhead (validated) ([details](docs/BENCHMARKS.md)) | ✅ |
| **0.8.3** — fix: worker mode respawns instead of 502-flooding on OOM under load | ✅ |
| `askr-laravel` composer package: shared-memory session/cache/queue drivers | ✅ |
| **0.8.4** — security/robustness: httpoxy filter, PID-aware shm lock, fork-safe admin start, upload temp-file RAII cleanup | ✅ |
| **Next** — HTTP/3 (QUIC), OTel traces. *(io_uring deprioritised: benchmarks show PHP is 99.5% of request time, I/O ~0.5%)* | ⏳ |

The biggest remaining step is the per-core **io_uring** I/O core and a
benchmark against FrankenPHP/FPM — both Linux-native work. The plan is written up
in [docs/IO-URING.md](docs/IO-URING.md), `askr doctor` probes io_uring support,
and [`scripts/bench.sh`](scripts/bench.sh) is the measurement harness.

## Project layout

```
crates/
  askr/          the standalone server binary
  askr-php/      embeds PHP (embed SAPI) via FFI
scripts/
  build-libphp.sh   reproducible libphp build (minimal | laravel)
examples/
  laravel-worker.php   worker-mode template for Laravel
  askr.toml            example configuration
docs/              full documentation
```

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for how to
build (you need an embed `libphp` first) and the checks CI runs. Please follow
the [Code of Conduct](CODE_OF_CONDUCT.md), and report security issues privately
per the [Security Policy](SECURITY.md) — not in public issues.

## License

MIT © Wirelabs AS — see [LICENSE](LICENSE).
