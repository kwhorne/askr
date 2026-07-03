# Askr documentation

Askr is a standalone, memory-safe **PHP application server** written in Rust. It
embeds the PHP interpreter in-process (no FastCGI, no FPM), serves it from a
memory-safe hot path, and — in worker mode — boots your app once and serves many
requests against it, eliminating per-request framework bootstrap.

> Version **0.1.0**. Production target is Linux; development also works on macOS.

## Start here

| Guide | What it covers |
| --- | --- |
| [Architecture](ARCHITECTURE.md) | How Askr works: embedding, non-ZTS process-per-core, the worker loop, request lifecycle, TLS, recycling, reload, the admin plane. |
| [Building](BUILDING.md) | Building `libphp` (macOS & Ubuntu) and the `askr` binary; the extension matrix. |
| [Configuration](CONFIGURATION.md) | The `askr.toml` reference, CLI flags, and environment variables. |
| [CLI reference](CLI.md) | Every command and flag (`serve`, `doctor`, `config-check`). |
| [Worker mode](WORKER_MODE.md) | Boot-once-serve-many, the Laravel worker script, per-request state reset, writing your own worker. |
| [Admin dashboard](ADMIN.md) | The built-in status/reload API and web dashboard. |
| [Deployment](DEPLOYMENT.md) | Production: systemd, TLS, zero-downtime reload, recycling, scaling, hardening. |
| [Ubuntu quickstart](UBUNTU.md) | End-to-end build + run on Ubuntu. |

## 60-second tour

```bash
# 1. Build an embed-enabled, non-ZTS libphp (Ubuntu shown; see Building for macOS)
sudo apt-get install -y build-essential pkg-config curl git \
  libssl-dev libxml2-dev libonig-dev libsqlite3-dev
PROFILE=laravel ./scripts/build-libphp.sh

# 2. Build Askr
cargo build --release

# 3. Pre-flight, then serve a Laravel app in worker mode
./target/release/askr doctor
ASKR_APP_BASE=/var/www/app ./target/release/askr serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" --tls-self-signed --admin 127.0.0.1:9000
```

## What works today (0.1.0)

- Embedded PHP (non-ZTS) running real Laravel 12, **~9× the per-request/FPM model**
- Multi-core via one worker **process per core** on a shared listen socket
- **Worker mode** (Octane-style) with per-request state reset — no bleed
- Graceful worker **recycling** + auto-respawn + crash resilience
- **TLS** (rustls) + **HTTP/2** (ALPN); `--tls-self-signed` for dev
- Zero-downtime **rolling reload** on `SIGHUP`
- Request hardening: body-size limit (413), HEAD, GET/POST
- Typed **`askr.toml`** config + `config-check`
- Built-in **admin dashboard + API**
- `askr doctor` pre-flight checks

## Not yet

HTTP/3 (QUIC), the per-core **io_uring** I/O core (the biggest efficiency step,
Linux), multipart `$_FILES`, response cache, OpenTelemetry, seccomp/Landlock
sandboxing, and the `askr-laravel` composer package.
