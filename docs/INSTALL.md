# Installing Askr — step by step

From nothing to a Laravel app served by Askr. Follow one path; each is complete on its
own.

If you only want to *try* it, **path A (Docker)** takes about two minutes and leaves
nothing behind.

---

## First, pick a path

| | Pick this if | Time |
|---|---|---|
| **A. Docker** | You want to try it, or you already deploy containers | ~2 min |
| **B. Release tarball** | You're installing on a Linux server. **Recommended for production.** | ~5 min |
| **C. From source** | You're contributing, or need a PHP build we don't ship | ~30 min |

**One thing to know up front.** Askr runs PHP *inside itself* — there's no PHP-FPM, no
FastCGI, no separate process to configure. That means it needs a particular kind of PHP
library (non-ZTS, built with the embed SAPI). **Paths A and B bundle it**, so there is
nothing to install and it won't touch or conflict with a system PHP you already have.
Only path C builds it, and that's what takes the half hour.

You do **not** need nginx, Apache, or Redis. Askr replaces all three.

---

## Path A — Docker

### A1. Run it

```bash
docker run --rm -p 8080:8080 -p 9000:9000 \
  -v /path/to/your/app:/app \
  ghcr.io/kwhorne/askr:1.4 \
  serve --listen 0.0.0.0:8080 --root /app/public --admin 0.0.0.0:9000
```

Replace `/path/to/your/app` with your Laravel project directory. The app needs its
`vendor/` installed already — Askr serves your project, it doesn't build it.

> Note there's no `askr` in that command. The image's entrypoint is already the
> launcher, so you pass `serve …` directly; writing `askr serve …` gets you
> `unrecognized subcommand 'askr'`. (Yes, that's how the first draft of this page was
> written.)

### A2. Check it

```bash
curl -i http://localhost:8080/
curl -s http://localhost:9000/api/status
```

You should get your app's homepage, and a JSON blob with worker counts.

> **`--admin` is not optional in the image.** Its healthcheck polls
> `:9000/api/status`, so a container started without `--admin` will keep working but
> report itself `unhealthy` forever. That has confused us, in our own project. See
> [DOCKER.md](DOCKER.md).

### A3. Make it permanent

Use the ready-made compose file in
[`examples/docker/docker-compose.yml`](../examples/docker/docker-compose.yml), and pin an
exact version (`askr:1.4.0`) rather than `:1.4` or `:latest` in production. Full details:
**[DOCKER.md](DOCKER.md)**.

Then skip to [step 4: the Laravel side](#4-the-laravel-side).

---

## Path B — Release tarball (recommended for production)

Self-contained: the binary, `libphp`, OPcache and the example scripts, in one archive.
Nothing is installed system-wide and no system PHP is touched.

### B1. Download and unpack

```bash
VER=v1.4.0; ARCH=$(uname -m)
curl -fsSLO https://github.com/kwhorne/askr/releases/download/$VER/askr-${VER#v}-linux-$ARCH.tar.gz
tar xzf askr-${VER#v}-linux-$ARCH.tar.gz
cd askr-${VER#v}-linux-$ARCH
```

`x86_64` and `aarch64` are both published. Prefer the `-full` archive if you want the
optional features (HTTP/3, OpenTelemetry, the SQL storage backend) — the plain build
silently doesn't have them.

### B2. Install the runtime libraries

Usually already present on a server; harmless if they are:

```bash
sudo apt-get install -y libssl3 libxml2 libonig5 libsqlite3-0
```

### B3. Pre-flight check

```bash
./askr-run.sh doctor
```

`doctor` verifies the PHP build, the extensions your app will need, and platform
support. **Read its output before going further** — it's designed to tell you what's
wrong while nothing is at stake.

### B4. Start it

```bash
ASKR_APP_BASE=/var/www/app ./askr-run.sh serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" \
  --tls-self-signed \
  --admin 127.0.0.1:9000
```

What those mean:

- `--root` — your `public/` directory, exactly as with nginx.
- `--worker-script` — boot Laravel **once** and serve many requests from it
  (Octane-style). This is where the speed comes from; see
  [WORKER_MODE.md](WORKER_MODE.md).
- `ASKR_APP_BASE` — your project root, so sidecars (queue, scheduler) know where to run.
- `--workers` — one process per core. Askr scales by processes, not threads, because
  non-ZTS PHP isn't thread-safe.
- `--tls-self-signed` — a throwaway certificate to prove HTTPS works. For a real one,
  see [B6](#b6-real-certificates).
- `--admin 127.0.0.1:9000` — status/metrics/reload API, bound to loopback so it isn't
  exposed.

### B5. Check it

```bash
curl -ik https://localhost:8443/
curl -s http://127.0.0.1:9000/api/status
```

### B6. Real certificates

Askr can obtain and renew Let's Encrypt certificates itself — no certbot, no cron:

```bash
./askr-run.sh serve --root /var/www/app/public --acme you@example.com \
  --domain example.com --domain www.example.com
```

Port 80 must be reachable for the HTTP-01 challenge. See [AUTOTLS.md](AUTOTLS.md).

### B7. Run it as a service

Don't leave it in a terminal. **[UBUNTU.md](UBUNTU.md)** has a copy-pasteable systemd
unit plus the recommended production settings; [DEPLOYMENT.md](DEPLOYMENT.md) covers
zero-downtime reloads, worker recycling and hardening.

---

## Path C — From source

Only if you're contributing or need a PHP build we don't ship.

```bash
# Requirements: Rust 1.80+, a C toolchain.
# Ubuntu also needs: build-essential pkg-config libssl-dev libxml2-dev libonig-dev libsqlite3-dev

git clone https://github.com/kwhorne/askr && cd askr

# 1. Build a non-ZTS, embed-enabled libphp. This is the slow step.
PROFILE=laravel ./scripts/build-libphp.sh

# 2. Build Askr
cargo build --release --bin askr

./target/release/askr doctor
```

Full detail, including macOS: **[BUILDING.md](BUILDING.md)**.

---

## 4. The Laravel side

Askr serves any PHP app as-is. The optional package swaps Laravel's session, cache,
lock, queue and broadcasting drivers over to Askr's shared memory — which is how you
**delete Redis** — and adds automatic page caching.

```bash
composer require kwhorne/askr-laravel
```

The service provider is auto-discovered; there's nothing to register. Then in `.env`:

```dotenv
SESSION_DRIVER=askr
CACHE_STORE=askr
QUEUE_CONNECTION=askr
BROADCAST_CONNECTION=askr
```

Version the package with the server: `askr-laravel` `1.4.x` goes with an Askr `1.4.x`
server. Full walkthrough, including the queue and scheduler sidecars:
**[LARAVEL.md](LARAVEL.md)**.

---

## 5. Then turn on the interesting parts

In rough order of payoff:

1. **`askr tune`** — measures your app and prints the config it should have (workers,
   memory caps, cache sizes) with the reasoning for each number.
2. **Worker mode** ([WORKER_MODE.md](WORKER_MODE.md)) — if you skipped
   `--worker-script`, this is the single biggest win available to you.
3. **Full-page caching** — run `askr serve --traffic-log /tmp/t.jsonl` for an hour, then
   `askr cache-report /tmp/t.jsonl`. It tells you which routes are worth caching *and
   which only look safe*. Then let the
   [`askr.cache` middleware](../packages/laravel/README.md#automatic-page-caching-askrcache)
   handle invalidation for you. See [CACHE.md](CACHE.md).
4. **The admin dashboard** ([ADMIN.md](ADMIN.md)) and **Prometheus metrics** at
   `/metrics`.
5. **Sandboxing** ([SANDBOX.md](SANDBOX.md)) — seccomp no-exec plus a Landlock
   filesystem sandbox, on Linux.

---

## When it doesn't work

Every item here is a mistake we have actually made.

**A 404 on a `.php` file.** Correct behaviour: Askr routes `.php` requests through your
front controller and never serves PHP source (that was a real vulnerability, fixed in
1.0.1). Probe with a normal route instead.

**The container says `unhealthy` but the app works.** You started it without `--admin`.
See [A2](#a2-check-it).

**`askr.cache` seems to do nothing.** The server needs somewhere to put pages: start it
with `--response-cache 512` (or `[cache] response_slots`). Check `X-Askr-Cache` on the
response — `MISS`/`HIT` means the cache is on, no header at all means it isn't.

**`Address already in use`.** Something else holds the port — often an earlier Askr you
forgot: `pkill -f 'askr serve'`.

**`unrecognized subcommand 'askr'` from Docker.** Drop the leading `askr` — the
image's entrypoint is the launcher already. See [A1](#a1-run-it).

**A feature in the docs isn't in your binary.** You have the plain build, not `-full`.
Compare `askr --version` output and re-read [B1](#b1-download-and-unpack).

**Sessions or cache empty out at random.** With `SESSION_DRIVER=askr` the regions live
in shared memory sized at startup; a too-small region evicts. `askr tune` suggests
sizes, and [CACHE.md](CACHE.md) explains the trade-off.

Still stuck? `askr doctor` first, then the logs, then
[open an issue](https://github.com/kwhorne/askr/issues) — the output of `askr doctor`
and `askr --version` in the report saves a round trip.
