# Askr — the real server for Laravel & PHP

[![CI](https://github.com/kwhorne/askr/actions/workflows/ci.yml/badge.svg)](https://github.com/kwhorne/askr/actions/workflows/ci.yml)
&nbsp;·&nbsp; **v0.1.0** &nbsp;·&nbsp; MIT

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

## Quick start

```bash
# Ubuntu: install PHP build deps (see docs/BUILDING.md for macOS)
sudo apt-get install -y build-essential pkg-config curl git \
  libssl-dev libxml2-dev libonig-dev libsqlite3-dev

git clone git@github.com:kwhorne/askr.git && cd askr

PROFILE=laravel ./scripts/build-libphp.sh   # build a non-ZTS embed libphp
cargo build --release                        # build the askr binary

./target/release/askr doctor                 # pre-flight checks

ASKR_APP_BASE=/var/www/app ./target/release/askr serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" --tls-self-signed --admin 127.0.0.1:9000
```

Full walkthrough: [docs/UBUNTU.md](docs/UBUNTU.md).

## Documentation

Everything lives in [`docs/`](docs/README.md):

- [Architecture](docs/ARCHITECTURE.md) — how it works, and why processes not threads
- [Building](docs/BUILDING.md) — `libphp` + `askr`, the extension matrix
- [Configuration](docs/CONFIGURATION.md) — `askr.toml`, env vars
- [CLI reference](docs/CLI.md) — every command and flag
- [Worker mode](docs/WORKER_MODE.md) — boot-once-serve-many, state reset, custom workers
- [Admin dashboard](docs/ADMIN.md) — status/reload API and web UI
- [Deployment](docs/DEPLOYMENT.md) — systemd, TLS, zero-downtime reload, scaling

## What works today (0.1.0)

- Embedded PHP (**non-ZTS**) running real Laravel 12 — no FastCGI, no FPM
- Multi-core: one worker **process per core** on a shared listen socket
- **Worker mode** (Octane-style) with per-request state reset — no bleed
- Graceful worker **recycling** (`--max-requests`) + auto-respawn + crash resilience
- **TLS** (rustls, ring — no OpenSSL) + **HTTP/2** (ALPN); `--tls-self-signed` for dev
- Zero-downtime **rolling reload** on `SIGHUP`
- Request hardening: body-size limit (`413`), HEAD, GET/POST
- Typed **`askr.toml`** config + `config-check`
- Built-in **admin dashboard + API** (status, graceful reload)
- `askr doctor` pre-flight checks
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
| **Next** — io_uring core (Linux), HTTP/3, `$_FILES`, response cache, OTel, seccomp/Landlock, `askr-laravel` package | ⏳ |

The biggest remaining step is the per-core **io_uring** I/O core and a
benchmark against FrankenPHP/FPM — both Linux-native work.

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
