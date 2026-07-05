# Ubuntu server setup (recommended)

A production, step-by-step setup for running a Laravel app with Askr on Ubuntu —
using the pre-built release, a hardened systemd service, TLS, and recommended
settings. Building from source instead? See [Building](BUILDING.md).

**You'll end up with:** Askr running as a non-root service on `:443`, serving your
app in worker mode (booted once, ~9× the FPM model), with opcache tuned, a shared
cache, zero-downtime canary deploys, and a localhost admin dashboard.

Tested on Ubuntu 22.04 / 24.04 LTS (x86_64 and arm64).

---

## 1. System user and layout

```bash
sudo useradd --system --home /opt/askr --shell /usr/sbin/nologin askr
sudo mkdir -p /opt/askr /etc/askr/tls /var/www/app
```

Recommended layout:

| Path | Purpose |
| --- | --- |
| `/opt/askr` | the Askr release (binary + `lib/` + `examples/`) |
| `/etc/askr/askr.toml` | configuration |
| `/etc/askr/tls/` | TLS certificate + key |
| `/var/www/app` | your Laravel application |

## 2. Install the release

Download the tarball for your architecture from the
[releases page](https://github.com/kwhorne/askr/releases) and extract it into
`/opt/askr`:

```bash
cd /tmp
VER=v0.6.0
ARCH=$(uname -m)            # x86_64 or aarch64
curl -fsSLO https://github.com/kwhorne/askr/releases/download/$VER/askr-${VER#v}-linux-$ARCH.tar.gz
tar xzf askr-${VER#v}-linux-$ARCH.tar.gz
sudo cp -r askr-${VER#v}-linux-$ARCH/* /opt/askr/
sudo chown -R root:root /opt/askr
```

Install the PHP runtime libraries the embedded interpreter links (usually already
present):

```bash
sudo apt-get update
sudo apt-get install -y libssl3 libxml2 libonig5 libsqlite3-0 \
  libicu74 libcurl4 libpng16-16 libjpeg-turbo8 libfreetype6 libwebp7 \
  libzip4 libpq5 zlib1g
```

Verify:

```bash
/opt/askr/askr-run.sh doctor
```

You should see the PHP version, a non-ZTS build, all required extensions, and —
on Linux — the io_uring kernel check.

## 3. Deploy your app

Put your Laravel code in `/var/www/app` (git clone, rsync, CI artifact — your
choice), install dependencies, and set permissions:

```bash
cd /var/www/app
composer install --no-dev --optimize-autoloader
php artisan config:cache   # optional; see the reload note below
sudo chown -R askr:askr /var/www/app
```

Make sure `storage/` and `bootstrap/cache/` are writable by the `askr` user, and
that `.env` has a real `APP_KEY` and production settings (`APP_ENV=production`,
`APP_DEBUG=false`).

Copy the worker script into place (or reference it from the release):

```bash
# examples ship inside the release at /opt/askr/examples/
ls /opt/askr/examples/laravel-worker.php
```

## 4. Configure Askr

Find the opcache path (the directory name encodes the PHP API version):

```bash
ls /opt/askr/lib/php/extensions/*/opcache.so
```

Create `/etc/askr/askr.toml` (adjust the opcache path to match):

```toml
[server]
listen = "0.0.0.0:443"
root = "/var/www/app/public"
workers = "auto"          # one process per CPU core
max_requests = 1000       # recycle each worker after N requests
max_body_size = "32M"

[worker]
script = "/opt/askr/examples/laravel-worker.php"
app_base = "/var/www/app"
# Tuned opcache. Match the extensions/ directory name to your build.
ini = "zend_extension=/opt/askr/lib/php/extensions/no-debug-non-zts-20240924/opcache.so\nopcache.enable=1\nopcache.enable_cli=1\nopcache.validate_timestamps=0\nopcache.memory_consumption=256\nopcache.interned_strings_buffer=32\nopcache.max_accelerated_files=20000\nopcache.jit=tracing\nopcache.jit_buffer_size=128M"

[tls]
cert = "/etc/askr/tls/fullchain.pem"
key = "/etc/askr/tls/privkey.pem"

[cache]
slots = 16384             # ~70 MB shared cache (askr_cache_* / no Redis)

[admin]
listen = "127.0.0.1:9000" # localhost only

[reload]
canary = true             # zero-bad-deploy reloads
```

Validate it:

```bash
/opt/askr/askr config-check /etc/askr/askr.toml
```

> `opcache.validate_timestamps=0` maximises throughput (no per-file `stat`). New
> code is picked up on **reload** (below), not automatically — so don't skip the
> reload step in your deploys. If you `php artisan config:cache`, re-run it and
> reload after each deploy.

## 5. TLS certificate

Askr terminates TLS itself. Get a certificate with Let's Encrypt using the
**webroot** method — Askr serves the ACME challenge files as static files from
`public/`, so you don't need to stop it:

```bash
sudo apt-get install -y certbot
sudo certbot certonly --webroot -w /var/www/app/public \
  -d example.com -d www.example.com \
  --deploy-hook "systemctl reload askr"
sudo ln -sf /etc/letsencrypt/live/example.com/fullchain.pem /etc/askr/tls/fullchain.pem
sudo ln -sf /etc/letsencrypt/live/example.com/privkey.pem  /etc/askr/tls/privkey.pem
sudo chown -R askr:askr /etc/askr/tls
```

The `--deploy-hook` reloads Askr on renewal so it picks up the new cert (a reload
re-forks workers, which reload the cert files). For local testing use
`--tls-self-signed` instead of `[tls]`.

*Behind a load balancer that terminates TLS?* Drop `[tls]`, set `https = true`
under `[server]`, and listen on a plain port (e.g. `0.0.0.0:8080`).

## 6. systemd service (hardened)

`/etc/systemd/system/askr.service`:

```ini
[Unit]
Description=Askr PHP application server
Documentation=https://github.com/kwhorne/askr
After=network.target

[Service]
Type=simple
User=askr
Group=askr
WorkingDirectory=/opt/askr
Environment=ASKR_APP_BASE=/var/www/app
ExecStart=/opt/askr/askr serve --config /etc/askr/askr.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=2
KillSignal=SIGTERM
TimeoutStopSec=30
LimitNOFILE=65536

# Bind :443 as a non-root user, nothing more.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

# Sandboxing
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/www/app/storage /var/www/app/bootstrap/cache
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictSUIDSGID=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now askr
sudo systemctl status askr
```

`AmbientCapabilities=CAP_NET_BIND_SERVICE` lets the non-root `askr` user bind
`:443` without running as root. `ProtectSystem=strict` makes the filesystem
read-only except the app's `storage/` and `bootstrap/cache/`.

## 7. Firewall

```bash
sudo ufw allow OpenSSH
sudo ufw allow 443/tcp
sudo ufw enable
```

The admin plane is bound to `127.0.0.1` — reach it over an SSH tunnel:

```bash
ssh -N -L 9000:127.0.0.1:9000 user@server   # then open http://127.0.0.1:9000
```

## 8. Verify

```bash
curl -I https://example.com/                 # 200, HTTP/2
sudo journalctl -u askr -f                   # logs
```

---

## Operating Askr

### Zero-downtime deploys

```bash
# 1. put the new code in place
cd /var/www/app && git pull && composer install --no-dev --optimize-autoloader
php artisan migrate --force
php artisan config:cache    # if you cache config

# 2. reload (canary: rolls one worker, health-checks it, then the rest)
sudo systemctl reload askr
```

With `[reload] canary = true`, a broken deploy takes down **one** worker instead
of the whole fleet, and the reload aborts — check `journalctl -u askr` for
`canary UNHEALTHY`. Fix and reload again.

### Health checks & metrics

```bash
curl -s http://127.0.0.1:9000/api/status     # workers alive, RSS, uptime
curl -s http://127.0.0.1:9000/api/metrics    # req/s, PHP vs I/O, latency
```

Use `workers_alive > 0` on `/api/status` as a liveness probe. See
[Admin](ADMIN.md).

### Logs

Askr logs to stdout → journald: `journalctl -u askr`. Set verbosity with
`Environment=RUST_LOG=askr=debug` in the unit if needed. Your app's logs go
wherever Laravel is configured (`storage/logs` or `stderr`).

---

## Recommended settings

| Setting | Recommended | Why |
| --- | --- | --- |
| `[server] workers` | `auto` | one process per core; matches CPU. |
| `[server] max_requests` | `500`–`2000` | recycle workers to bound drift/leaks. |
| `[server] max_body_size` | your largest upload + headroom | rejects oversized bodies (413). |
| `opcache.validate_timestamps` | `0` | max throughput; reload on deploy. |
| `opcache.memory_consumption` | `256` | enough for a large app's opcode cache. |
| `opcache.jit` | `tracing` | JIT for CPU-bound code. |
| `[cache] slots` | size to your working set | shared cache/rate-limit; `0` to disable. |
| `[admin] listen` | `127.0.0.1:9000` | never expose the admin plane publicly. |
| `[reload] canary` | `true` | stop bad deploys at one worker. |

### Sizing & memory

Each worker holds a booted app (tens of MB for Laravel). Budget roughly
`workers × per-worker RSS` + the shared cache. Worker RSS is flat across requests
(no per-request growth), so recycling is about long-term drift. Watch per-worker
RSS on `/api/status`.

### Whole runtime in one binary (optional)

Run queue workers and the scheduler in the same service — no Horizon, no crontab:

```toml
[queue]
workers = 2
script = "/opt/askr/examples/askr-queue.php"

[scheduler]
script = "/opt/askr/examples/askr-scheduler.php"
```

The queue needs your app's queue connection configured as usual. See
[Deployment](DEPLOYMENT.md).

### CoW mode (optional, experimental)

For ~ms warm respawns and shared memory, add `--cow` (not via config yet) with a
worker script that calls `askr_cow_ready()` — see [CoW](COW.md). It's
experimental and disables the admin plane and sidecars; validate under load
first.

---

## Troubleshooting

- **`libphp.so: cannot open shared object file`** — run via `/opt/askr/askr`
  (its rpath points at `./lib`), or from `/opt/askr`. Install the runtime libs in
  step 2.
- **`could not bind 0.0.0.0:443`** — the `CAP_NET_BIND_SERVICE` capability is
  missing from the unit, or another service holds `:443`.
- **New code not showing** — you have `validate_timestamps=0`; run
  `systemctl reload askr` after deploying.
- **`askr doctor` reports a missing extension** — you're on the `minimal` libphp;
  use the release tarball (built with the `laravel` profile).
- **io_uring** — `askr doctor` reports kernel support (≥ 5.1; 5.10+ recommended).
  The current I/O layer is epoll-based; io_uring is the next step.
