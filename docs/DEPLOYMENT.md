# Deployment

This is the operational reference (concepts: reload, canary, scaling, security).
For the full step-by-step production install on Ubuntu — release tarball, systemd,
TLS, hardening and recommended settings — follow **[Ubuntu setup](UBUNTU.md)**.
For build details see [Building](BUILDING.md).

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

> **One unexplained observation (Askr-51).** On a live deployment a worker was seen
> still serving the previous release after a reload had reported success. It has not
> been reproduced -- a regression test that records every PID, sends `SIGHUP` and
> polls until no pre-reload PID remains passes repeatedly against the same fleet
> shape, sidecars included -- and the diagnosis originally reasoned from it has been
> withdrawn. The mixed content is unexplained rather than explained.
>
> A reload that leaves one worker on the old code serves the previous release from a
> fraction of requests *and reports success*, which is why it is written down here
> rather than left in the issue tracker. Until it is understood, a deploy that must
> be certain should recreate the process rather than reload it, and accept the
> dropped connections that costs.

Workers restart **one at a time**, each draining in-flight requests before
exiting; the master keeps the listen socket open and waits for each fresh worker
to boot before rolling the next. With `opcache.validate_timestamps=0`, fresh
workers recompile the new code — old workers keep serving the old code until
they roll.

### Canary reload (zero-bad-deploy)

Add `--canary` (or `[reload] canary = true`) and a reload rolls **one** worker
first, watches it, and only rolls the rest if it stays healthy. If the new code is
broken the rollout **aborts**, and the failed canary is drained so it stops serving
traffic at all:

```
INFO  canary healthy — rolling the rest requests=695 err_pct="0.00%"
ERROR canary UNHEALTHY — aborting reload
      reason=error rate 63.35% vs fleet 0.00% (allowed +2.00 points)
WARN  draining the failed canary; the fleet keeps serving on 3 worker(s)
```

The canary is compared **against the rest of the fleet in the same window**, using
per-worker counters in shared memory. That matters: an absolute, fleet-wide error
count can't tell a bad new worker from a site that always serves a few 5xx, and it
charges the canary for errors the *old* workers produced.

```toml
[reload]
canary = true
canary_window = 5              # seconds to watch
canary_min_requests = 20       # below this the verdict is "inconclusive"
canary_max_error_rate = 2.0    # percentage points above the fleet
canary_max_latency_factor = 3.0
```

The outcome shows up in `/api/status` as `rollout`: `rolling`, `ok`, `aborted` or
`inconclusive`.

What each verdict means:

- **ok** — the canary matched the fleet; the rest of the workers rolled.
- **aborted** — the rollout stopped, and the bad canary was **drained and its slot
  quarantined**, so you run one worker short on the old code rather than serving a
  broken deploy from 1/N of the fleet. Respawning it would only boot the same bad
  build. Fix the code and reload again: the quarantine clears and the slot refills.
  With only one worker configured the canary is kept alive instead — no workers at
  all is worse than a bad one.
- **inconclusive** — the canary served fewer than `canary_min_requests`, so there was
  nothing to judge. The rollout **continues** (a deploy shouldn't be blocked by an
  absence of evidence) and logs a warning. On a quiet site, raise `canary_window` or
  lower `canary_min_requests` to make the gate meaningful.

> **Worker mode vs per-request mode.** In worker mode the surviving workers hold the
> *previous* app in memory, so an abort genuinely keeps the old code serving. In
> per-request mode every worker reads the current files from disk, so a bad deploy
> affects all of them regardless — the gate still detects it and drains the canary,
> but it can't roll you back to code that's no longer on disk. Deploy atomically
> (symlink swap) if you rely on this.

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

## Behind a reverse proxy

With nginx, a load balancer or a Docker port mapping in front, the TCP peer Askr sees is
the proxy, not the visitor. **Tell Askr which addresses are your proxies**, and it will
hand PHP the client:

```toml
[server]
trusted_proxies = ["172.18.0.0/16"]   # the Docker network, or your LB's addresses
```

What PHP then sees in `$_SERVER`:

| Variable | Value |
| --- | --- |
| `REMOTE_ADDR` | The client: the rightmost `X-Forwarded-For` entry that is not itself a trusted proxy. The peer when nothing was forwarded. |
| `HTTP_X_FORWARDED_FOR` | That same client, **one** address — never the raw chain. |
| `ASKR_PEER_ADDR` | The TCP peer, always — your proxy. The address to look at when a forwarding chain is misconfigured. |
| `REMOTE_PORT` | The peer's port. A forwarded chain carries addresses, not ports; nginx's `real_ip` leaves the port alone for the same reason. |

This is what nginx does with `real_ip` and Apache with `mod_remoteip`, and what nginx +
php-fpm does by passing `fastcgi_param REMOTE_ADDR $remote_addr` — so an application that
worked there sees the same thing here. Rightmost-untrusted is the only safe reading: every
entry to its left was supplied by a hop you did not vouch for, and may be forged.

**Before 1.7.1 this was only half true.** `trusted_proxies` resolved the client for Askr's
own rate limiter and never reached PHP: `REMOTE_ADDR` was the proxy. It was found behind
nginx in front of a container, where an IP allowlist that waived 2FA for known addresses
asked every visitor for a second factor, and started working again the moment nginx was
taken out of the path.

### Why the header is rewritten, not passed through

Fixing `REMOTE_ADDR` alone would have left a hole. An application that trusts every proxy
— Laravel's `trustProxies(at: '*')`, common in containers — reads `X-Forwarded-For` itself
and takes the **leftmost** entry. Behind a proxy that appends (`$proxy_add_x_forwarded_for`)
the chain is `forged, client`, so that application lands on `forged` whatever `REMOTE_ADDR`
says; and with no proxy at all, a visitor simply sends the header. Either way, an allowlist
keyed on the client address can be walked past.

So Askr owns the answer, and PHP cannot derive a different one:

- **From a trusted proxy**, `X-Forwarded-For` is collapsed to the client Askr resolved.
  The other forwarding headers — `X-Real-IP`, `X-Forwarded-Host`, `-Proto`, `-Port`,
  `-Prefix`, and RFC 7239 `Forwarded` — pass through: they are that proxy's own statements.
- **From any other peer**, **all** of them are removed. An unvouched forwarding header is
  not evidence of anything, and the address is not the only one that matters:
  `X-Forwarded-Host` from a peer nobody vouched for is how a password reset link gets
  pointed at an attacker's domain by an application that trusts every proxy.

That second rule is what makes the Laravel advice below safe. Without it, "Askr has cleaned
`X-Forwarded-For`, so `'*'` is fine" would have closed one hole and opened another.

### If your proxy trust lives in Laravel instead

Some deployments never set `trusted_proxies` and configure `TrustProxies` in the
application instead, letting Laravel read `X-Forwarded-For` itself. **From 1.7.1 that
stops working**: without `trusted_proxies` every peer is untrusted, so the header is
removed before PHP sees it, and `$request->ip()` becomes the proxy.

The fix is to move the trust to where the connection is accepted:

```toml
[server]
trusted_proxies = ["172.18.0.1"]      # what TrustProxies used to list
```

and then set `TrustProxies` to `'*'`. That sounds like the opposite of careful and is the
right answer here: Askr has already decided which peers are proxies, removed every
forwarding header that did not come from one, and collapsed `X-Forwarded-For` to a single
value, so there is nothing left for the application to be fooled by. Leaving
`TrustProxies` listing the proxy's own address does **not** keep working: Symfony checks
that list against `REMOTE_ADDR`, which is now the client, so it would stop honouring
`X-Forwarded-Proto` and `X-Forwarded-Host` altogether. Askr logs the first time it removes a
forwarding header from an untrusted peer (`X-Forwarded-For arrived from a peer that is
not in [server] trusted_proxies`), so this break names itself instead of passing silently.

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

- The admin plane is **unauthenticated by default** — bind it to `127.0.0.1` and
  reach it via SSH / private network, or set `ASKR_ADMIN_TOKEN` to require a
  `Authorization: Bearer <token>` header on the API (see [Admin](ADMIN.md)). Askr
  warns at startup if the admin plane is bound to a non-loopback address.
- **Static files never expose sources or dotfiles.** A request that resolves to a
  `.php`/`.phtml`/`.phar` file, or to any dotfile or dot-directory (`.env`,
  `.git/*`, `.htaccess`), is *not* served as bytes — it falls through to the front
  controller, so your app answers (normally a 404). `.well-known/` stays servable
  for ACME HTTP-01 and `security.txt`. Askr also only ever executes the configured
  front controller, never an arbitrary `.php` found on disk, so a file uploaded
  into the docroot can't be run.
- **Editor/deploy leftovers are refused too** — anything containing `.php.` or ending
  in `~`, `.bak`, `.orig`, `.save`, `.swp`, `.swo`, `.old`, `.rej`, `.tmp`. A stray
  `index.php.bak` is still source code.
- **Symlinks are followed**, including out of the document root — `php artisan
  storage:link` depends on exactly that, and nginx/Apache behave the same way. Askr
  will not read outside the docroot on its own (traversal, percent-encoded traversal
  and NUL tricks are all rejected), but a symlink you place there *is* published.
  Keep the docroot free of links you don't mean to expose.
- The entire server hot path is memory-safe Rust; PHP is the single `unsafe`
  frontier. On Linux, harden that boundary with `--sandbox` (seccomp no-exec +
  optional Landlock write-restriction) — see [Sandbox](SANDBOX.md).
- Run as a non-root user (`User=www-data`) and keep `--max-body-size` sane for
  your app to bound request memory.
- Add `[[ratelimit]]` rules for login and API paths so one client can't spend the
  whole worker pool — refused requests never reach PHP. Behind a load balancer, set
  `[server] trusted_proxies`, or `X-Forwarded-For` is ignored and every client shares
  one bucket — and PHP sees the proxy in `REMOTE_ADDR` too. Askr warns at startup if
  limits are configured without it. See [Behind a reverse proxy](#behind-a-reverse-proxy).

## Kernels & io_uring

`askr doctor` reports whether the kernel supports io_uring (≥ 5.1; 5.10+
recommended). The current I/O layer is tokio/epoll; the per-core io_uring core
is the next architectural step and is Linux-only.
