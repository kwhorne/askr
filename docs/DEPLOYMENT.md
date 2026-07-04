# Deployment

This guide covers running Askr in production on Linux. For a first end-to-end
walkthrough see the [Ubuntu quickstart](UBUNTU.md); for build details see
[Building](BUILDING.md).

## Overview

A production deployment is:

- one Askr **master** binding your listen port, supervising N **worker**
  processes (one per core),
- **worker mode** with a booted app for throughput,
- **TLS** terminated by Askr (or a load balancer in front),
- periodic **recycling** (`max_requests`) and zero-downtime **reload** on deploy,
- managed by **systemd**, configured by `askr.toml`.

## systemd

`/etc/systemd/system/askr.service`:

```ini
[Unit]
Description=Askr PHP application server
After=network.target

[Service]
Type=simple
User=www-data
WorkingDirectory=/opt/askr
ExecStart=/opt/askr/target/release/askr serve --config /etc/askr/askr.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

`/etc/askr/askr.toml` (see [Configuration](CONFIGURATION.md)):

```toml
[server]
listen = "0.0.0.0:443"
root = "/var/www/app/public"
workers = "auto"
max_requests = 1000

[worker]
script = "/opt/askr/examples/laravel-worker.php"
app_base = "/var/www/app"
ini = "zend_extension=/opt/askr/vendor/php-build/install/lib/php/extensions/no-debug-non-zts-20240924/opcache.so\nopcache.enable=1\nopcache.validate_timestamps=0"

[tls]
cert = "/etc/askr/fullchain.pem"
key = "/etc/askr/privkey.pem"

[admin]
listen = "127.0.0.1:9000"

# Whole Laravel runtime in one process: queue workers + scheduler, no extra
# systemd units or Horizon needed for basic setups.
[queue]
workers = 2
script = "/opt/askr/examples/askr-queue.php"

[scheduler]
script = "/opt/askr/examples/askr-scheduler.php"
```

The master supervises the queue workers and the scheduler alongside the web
workers — they respawn on exit, drain on shutdown, and roll on reload. Queue
workers run `queue:work` (with `--max-jobs`/`--max-time` self-recycling); the
scheduler runs `schedule:run` on an interval, so no `* * * * *` crontab entry is
needed. Both run entirely in-process (no separate `php artisan` invocation). The
queue needs the app's queue connection configured as usual.

```bash
askr config-check /etc/askr/askr.toml   # validate before enabling
sudo systemctl daemon-reload
sudo systemctl enable --now askr
```

- `systemctl reload askr` → `SIGHUP` → **rolling reload** (new code, no downtime).
- `systemctl stop askr` → `SIGTERM` → drain all workers, then exit.
- `Restart=on-failure` complements Askr's own per-worker crash respawn (that
  handles individual workers; systemd handles the whole master).

## Zero-downtime deploys

1. Put the new code in place (`rsync`, `git pull`, atomic symlink swap, …).
2. Reload: `systemctl reload askr` (or `curl -X POST http://127.0.0.1:9000/api/reload`).

Workers restart **one at a time**, each draining in-flight requests before
exiting; the master keeps the listen socket open and waits for each fresh worker
to boot before rolling the next. With `opcache.validate_timestamps=0`, fresh
workers recompile the new code — old workers keep serving the old code until
they roll.

## TLS

Askr terminates TLS itself (rustls, ring provider) with ALPN negotiating HTTP/2
or HTTP/1.1 — no OpenSSL, no proxy required. Provide a **v3** certificate
(rustls rejects v1):

```toml
[tls]
cert = "/etc/askr/fullchain.pem"   # e.g. Let's Encrypt
key = "/etc/askr/privkey.pem"
```

Alternatively terminate TLS at a load balancer / edge proxy and run Askr over
HTTP with `https = true` (so `$_SERVER['HTTPS']` is set and Laravel emits
`secure` cookies). Reload Askr after renewing certificates so workers pick up the
new files.

## Scaling & recycling

- **`workers = "auto"`** runs one process per core. Each serves one request at a
  time (like an FPM worker); concurrency comes from having many workers.
- **`max_requests`** recycles workers to bound memory growth / state drift. The
  quota is staggered per worker so they never all recycle at once. Pick a value
  that amortises the ~cold-boot cost (e.g. 500–5000 depending on app weight).
- Front Askr with a load balancer for multi-host scaling and connection retries;
  during a rolling reload a rare in-flight connection may reset under aggressive
  hammering — retries make this a non-issue.

## Memory budget

Each worker holds a booted app in memory (tens of MB for Laravel). Budget
roughly `workers × per-worker RSS`. Worker RSS is flat across requests in worker
mode (verified: ~64→66 MB over 600 requests), so recycling is about long-term
drift, not per-request growth.

## Health checks & monitoring

- **Liveness:** `GET /api/status` on the admin port; assert `workers_alive > 0`.
- **App health:** hit a lightweight app route through the main listener.
- `askr doctor` as a pre-deploy gate (non-ZTS, extensions, io_uring kernel).

See [Admin](ADMIN.md) for scripting examples.

## Security notes

- The admin plane has **no built-in auth** in 0.1.0 — bind it to `127.0.0.1` and
  reach it via SSH / private network, or front it with your own auth.
- The entire server hot path is memory-safe Rust; PHP is the single `unsafe`
  frontier. seccomp/Landlock sandboxing of the PHP boundary is planned.
- Run as a non-root user (`User=www-data`) and keep `--max-body-size` sane for
  your app to bound request memory.

## Kernels & io_uring

`askr doctor` reports whether the kernel supports io_uring (≥ 5.1; 5.10+
recommended). The current I/O layer is tokio/epoll; the per-core io_uring core
is the next architectural step and is Linux-only.
