# Running Askr on Ubuntu: maintenance

[Ubuntu install](UBUNTU.md) gets a server serving. This is what comes after — the checks
worth doing, the operations you'll repeat, and the mistakes that have actually been made
in production rather than the ones that sound plausible.

Everything here assumes the layout from [UBUNTU.md](UBUNTU.md) (`/opt/askr`, systemd unit
`askr`, app in `/var/www/<site>`) or the Docker layout from [DOCKER.md](DOCKER.md). Where
the two differ, both commands are given.

---

## Before you deploy: `doctor --app`

```bash
docker compose exec askr /opt/askr/askr doctor --app /var/www/example.com
sudo -u askr /opt/askr/askr doctor --app /var/www/example.com     # tarball install
```

It checks the *application* against the environment it will run in, and exits non-zero on a
configuration that cannot work — so it can gate a deploy. Every check in it is a failure
that has actually happened: a queue name no worker polls, `SESSION_DRIVER=askr` without
slots, a mailer configured under the wrong variable name, scheduled `->command()` tasks with
no `php` binary to shell out to. See [CLI.md](CLI.md#--app-path).

Run it after any change to `.env`, and after adding a job or notification — a new
`onQueue('reports')` is a queue nothing polls until you say so.

## The 30-second check

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9000/healthz    # 200
curl -s http://127.0.0.1:9000/api/status -H "Authorization: Bearer $ASKR_ADMIN_TOKEN" | jq
```

`/healthz` is **unauthenticated and two words long** — 200 while at least one worker can
serve, 503 otherwise. Use it for every probe: systemd, Docker `HEALTHCHECK`, Kubernetes, an
uptime monitor. `/api/status` returns PIDs and memory figures and is therefore gated, which
is why pointing a probe at it breaks the moment you set an admin token.

What to actually look at in `/api/status`:

| Field | Healthy | What a bad value means |
| --- | --- | --- |
| `workers_alive` | equals `workers_configured` | Below it: workers are dying. Check the log. |
| `respawns` | stable between checks | Climbing: a worker dies repeatedly. This is the single most informative number on the server. |
| `rss_kb_total` | flat over days | Climbing without bound: a leak your `max_rss` isn't catching. |
| `uptime_secs` | grows | Resets you didn't cause: crash-loop or OOM killer. |

A `respawns` count that increases by itself is worth ten minutes now rather than an outage
later. Everything in [worker mode's symptom index](WORKER_MODE.md) shows up here first.

---

## Deploying new code

**Reload, don't restart.**

```bash
sudo systemctl reload askr                      # tarball install
docker compose kill -s HUP askr                 # Docker
```

A reload rolls workers one at a time; in-flight requests finish, and **shared memory
survives** — sessions, response cache and queued jobs are all in it. A `restart` throws
that away: every user is logged out and queued jobs are gone.

With `[reload] canary = true` a broken deploy takes down one worker instead of the fleet
and aborts the roll. Look for `canary UNHEALTHY` in the log, fix, reload again.

Order matters, because a reload picks up code the moment it happens:

```bash
cd /var/www/example.com
git pull
composer install --no-dev --optimize-autoloader
php artisan migrate --force
npm ci && npm run build          # see the VITE trap below
php artisan config:cache
sudo systemctl reload askr       # last
```

**The reload is last for a reason, and a frontend build is the sharpest case.** Laravel caches
Vite's `manifest.json` for the life of the process, so workers that started before
`npm run build` keep serving the *previous* build's filenames — which now 404. You will not
see it: your browser still holds the old asset under `immutable, max-age=1 year`. Every new
visitor gets a page with no stylesheet. `scripts/smoke.sh` checks every referenced asset for
exactly this.

### The Vite trap

`VITE_*` variables are **compiled into the JavaScript bundle**. A frontend built before
`.env` is correct ships whatever was in `.env.example` — most often `localhost` URLs for
the WebSocket, which then fail in every visitor's browser while the server is perfectly
configured. Fix `.env` first, then build.

---

## Clearing caches

There are three, they are independent, and "clear the cache" almost never means all three.

**1. Askr's response cache** (full-page HTML in shared memory):

```bash
curl -X BAN -H "X-Ban-Url: /*" -H "Authorization: Bearer $ASKR_ADMIN_TOKEN" https://example.com/
curl -X PURGE -H "Authorization: Bearer $ASKR_ADMIN_TOKEN" https://example.com/one/page
```

Both answer with a count. See [CACHE.md](CACHE.md).

**2. Laravel's own caches** (config, routes, views, application cache):

```bash
php artisan optimize:clear
```

In Docker there is no `php` in the Askr image — run the [artisan sidecar](DOCKER.md#no-php-cli-in-the-image),
and set `CACHE_STORE=array SESSION_DRIVER=array` when you do. The `askr` drivers live in the
running server's shared memory and don't exist in a detached container, so `cache:clear`
against them would clear nothing and report success.

**3. Shared memory itself** (sessions, cache, queue) — only a restart:

```bash
sudo systemctl restart askr
```

Logs everyone out. Reach for it when you mean it.

**And a fourth that isn't on the server.** Vite assets are served `immutable, max-age=1
year`, which is safe because filenames are content-hashed — but a file that *was* served
broken stays broken in that browser for a year, and a normal reload won't fix it: Askr
correctly answers `304`, so the client keeps its own empty copy. Only a hard refresh
(`Ctrl/Cmd+Shift+R`) clears it. A cached error outlives its own fix.

---

## Certificates

With [auto-TLS](AUTOTLS.md) there is nothing to renew by hand. Askr checks every six hours,
renews inside 30 days of expiry, and rolls workers afterwards.

```bash
# what is actually being served, from outside the box
echo | openssl s_client -connect example.com:443 -servername example.com 2>/dev/null \
  | openssl x509 -noout -subject -issuer -dates
```

Three things to know:

- **The ACME directory must persist.** If `/var/lib/askr/acme` is wiped on every deploy you
  will hit Let's Encrypt's rate limits (per domain, per week) and then you have no
  certificate and no way to get one for days. In Docker use a **bind mount**, not a named
  volume — a named volume inherits the image's uid (999) while the container may run as
  yours.
- **Use `staging = true` first** on any new domain. Untrusted cert, far higher limits.
- `renew_at` in the ACME directory is the decision timestamp. If renewal seems stuck, that
  file and the log are where the answer is.

---

## Logs

The server logs to stdout → journald:

```bash
journalctl -u askr -f
journalctl -u askr --since '1 hour ago' | grep -iE 'error|warn'
docker compose logs askr -f --since 10m          # Docker
```

Turn up detail with `Environment=RUST_LOG=askr=debug` in the unit.

**PHP diagnostics go to the log, not to visitors** (since 1.4.1): `display_errors=0`,
`log_errors=1`. So when a page 500s, the reason is in `journalctl`, not in the browser. Opt
back in for local debugging only, with `ASKR_PHP_INI="display_errors=1"`.

### ⚠️ Access and traffic logs do not rotate

`--access-log` and `--traffic-log` are plain append-only files. Askr holds the descriptor
open and never truncates. Left alone on a busy site they will fill the disk.

```
/var/log/askr/*.log {
    daily
    rotate 14
    compress
    missingok
    notifempty
    copytruncate
}
```

`copytruncate` is required, not stylistic: Askr keeps writing to the same descriptor, so a
renamed file would go on receiving lines invisibly.

**The traffic log is for a campaign, not for always.** It writes a line per PHP-served
request, including body hashes, so it's both large and sensitive. Turn it on, gather a
representative day, run [`askr cache-report`](CACHE.md), turn it off.

---

## After every deploy: `scripts/smoke.sh`

```bash
./scripts/smoke.sh https://example.com http://127.0.0.1:9000 "$ASKR_ADMIN_TOKEN"
```

Exits with the number of failures, so CI can gate on it. Every check in it is a failure that
actually shipped: a 200 with an empty body, a page that worked once per worker, a form that
lost its fields, a URL that said `localhost` over HTTP/2, an asset referencing a build that
no longer exists, a queue accepting jobs nothing consumed.

Two of its habits are worth copying into anything else you write:

- It checks for the **absence of the unexpected**, not only the presence of the expected.
  Grepping for `<title>` matched happily with deprecation warnings printed in front of it.
- It exercises **HTTP/2 as well as 1.1**. TLS negotiates h2 by default, and that is how a bug
  making every generated URL say `localhost` survived 23 million requests.

It found a real fault the first time it ran against production — a stylesheet 404 nobody
could see because every browser still had the file cached.

## Backups

Three things, and only one of them is obvious.

```bash
# 1. the database
docker run --rm -v elyrasql:/data -v "$PWD":/out ubuntu:24.04 \
  tar czf /out/db-$(date +%F).tar.gz -C /data .

# 2. the ACME directory — a lost account key plus rate limits is a bad afternoon
tar czf acme-$(date +%F).tar.gz -C /var/lib/askr acme

# 3. .env, which is not in git and never should be
cp /var/www/example.com/.env ~/env-backup-$(date +%F)
```

`storage/app` too, if your app stores uploads there rather than in object storage.

Restore-test them. An untested backup is a belief, not a backup.

---

## Upgrading Askr

Read [UPGRADING.md](UPGRADING.md) first — it lists behaviour changes per version, which is
where the surprises are.

```bash
sudo /opt/askr/askr upgrade --check       # is there a newer release?
sudo /opt/askr/askr upgrade --restart     # download, verify sha256, swap, restart
```

The old version stays at `/opt/askr.old` for rollback. Pin with `--version X.Y.Z`.

Docker: bump the tag in `compose.yml`, then

```bash
docker compose pull askr && docker compose up -d askr
```

Never let a `docker compose` error scroll past into `/dev/null`. An orphaned container held
a port through several "successful" recreates once because the error was silenced.

If you also use the Laravel package, upgrade it in the same window and make sure
`composer.json`, `composer.lock` and `vendor/` all agree afterwards:

```bash
composer update kwhorne/askr-laravel
grep askr-laravel composer.json && ls vendor/kwhorne/askr-laravel
```

Checking that the file exists is not the same as checking that the dependency is
registered. A package present in `vendor/` but missing from `composer.json` is deleted by
the next `composer install`, and every route 500s.

---

## Capacity

| Setting | Start at | Why |
| --- | --- | --- |
| `workers` | number of cores | PHP is non-ZTS: Askr scales by processes, not threads. |
| `workers_min` / `workers_max` | cores / 2× cores | Autoscales on backlog instead of guessing. |
| `max_requests` | 500–2000 | Recycles a worker before a slow leak matters. |
| `max_rss` | (RAM − 1 GB) / workers | A worker over budget is replaced instead of inviting the OOM killer. |

Memory is the constraint that bites first. Each worker is a full PHP interpreter with your
app booted; measure `rss_kb_total / workers_alive` under real traffic rather than
estimating. A Laravel 13 + Livewire + Flux app in worker mode measures about **77 MB per
worker** on a live site, so four workers is roughly 300 MB — but measure yours, because a
package that loads a large config or a wide Eloquent model changes it.

---

## Queues

```bash
php artisan queue:monitor default:100
```

From 1.4.10 all four counters are real — pending, delayed, reserved, and the oldest pending
job's age. Before that `delayedSize()` and `reservedSize()` reported 0, so a threshold that
never fired may start firing after the upgrade; it was reading zeros, not calm.

Queue workers are sidecars of the same binary (`[queue] workers`, `[queue] script`). They
share the shared-memory ring with the web workers, which is why a `restart` loses queued
jobs and a reload doesn't.

---

## Security hygiene

- **Set `ASKR_ADMIN_TOKEN`.** Without it the admin API is open to anything that reaches the
  port. Bind the admin listener to `127.0.0.1` as well — both, not either.
- `.env` should be `0600` and owned by the app user. The ACME `key.pem` and `account.json`
  are written `0600` by Askr itself (since 1.4.2).
- `--sandbox` on Linux blocks process creation from PHP (`execve`/`ptrace` → EPERM). If your
  app shells out, it will fail loudly — that's the point. See [SANDBOX.md](SANDBOX.md).
- Verify the boring things after every deploy: `curl -sI https://example.com/.env` should be
  404, and so should `/.git/config`.

---

## When something breaks

**Read the log first.** Since 1.4.4 the failure messages name what actually happened
instead of guessing:

| In the log | What it means |
| --- | --- |
| `php worker died mid-request` | A fatal or `exit()`. The next lines name class, method, file and line. Recently: a queue driver missing a contract method added by a new Laravel major. |
| `loop ended rc=… exit_status=…` | The worker script returned. `rc=0` with no error means something unwound cleanly — an uncaught exception outside the kernel's try, historically. |
| `accept failed` with `EMFILE` | Out of file descriptors. Raise `LimitNOFILE` in the unit. |
| `canary UNHEALTHY` | The reload aborted and you're still on the old code. Good. |
| `tag_overflow` | A response had more cache tags than Askr can track, so it wasn't cached. Not an error, but it means that page is uncached. |
| `queue backlog is not being consumed` | Jobs are available and nothing is taking them. The line names the queue. Either no queue worker is running (`--queue` with `--queue-script`), or it doesn't poll that name (`ASKR_QUEUE`, comma-separated). |

For anything that *looks* fine but behaves wrong — interactivity that dies after the first
page load, an anonymous visitor served as somebody else, 419 on every form, empty
downloads, `localhost` in generated URLs — go straight to the [symptom index in
WORKER_MODE.md](WORKER_MODE.md#symptoms-and-their-causes). Those are all state that should
have been thrown away between requests, and they all fail silently.

Two habits that would have saved days:

- **Count what the browser actually received.** A clean console does not mean working
  JavaScript, and a 200 does not mean a body. `curl -s … | grep -c` before theorising.
- **Verify over HTTP/2, not just HTTP/1.1.** Every test client shipped in this repo speaks
  1.1, which is why a bug that made every generated URL say `localhost` over h2 survived 23
  million requests. `curl --http2` explicitly.

---

## A monthly ten minutes

```bash
# 1. is it healthy, and has it been?
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9000/healthz
curl -s http://127.0.0.1:9000/api/status -H "Authorization: Bearer $ASKR_ADMIN_TOKEN" \
  | jq '{workers_alive, respawns, rss_kb_total, uptime_secs}'

# 2. certificate — expiry and issuer
echo | openssl s_client -connect example.com:443 -servername example.com 2>/dev/null \
  | openssl x509 -noout -dates

# 3. disk, which logs and images quietly eat
df -h / && du -sh /var/log/askr /var/lib/askr /var/www/example.com/storage/logs

# 4. anything angry lately?
journalctl -u askr --since '30 days ago' | grep -ciE 'error|died|respawn'

# 5. is there a new release, and does it change behaviour?
sudo /opt/askr/askr upgrade --check
docker image prune -f
```

If `respawns` is flat, the certificate has more than 30 days, and the disk isn't filling,
there is nothing to do. That's the normal outcome.

---

## Things not to do

Each of these has actually gone wrong.

| Don't | Because |
| --- | --- |
| `restart` to deploy | Logs everyone out and drops queued jobs. Reload. |
| `--config` alongside other flags | Askr refuses to start (since 1.4.6) rather than silently ignore them. Put everything in the file. |
| Bind-mount an app without matching uids | The image runs uid 999; every PHP route 500s while static files serve fine. `user: "1000:1000"`. |
| Named volume for the ACME directory | It inherits the image's uid, not yours. Bind mount. |
| Run `artisan` with `CACHE_STORE=askr` in a sidecar | The shared memory isn't there. It clears nothing and says it worked. |
| Build the frontend before `.env` is right | `VITE_*` is baked in at build time. |
| Pipe a `docker compose` command to `/dev/null` | An orphaned container held a port through several "successful" recreates. |
| Trust a probe on `/api/status` | It's gated. Use `/healthz`. |
| Set `--queue-slots` without `--queue`/`--queue-script` | The ring accepts jobs nothing consumes. Queued mail then fails with no error, no log line and no mail. |
| Add `onQueue('new-name')` without adding it to `ASKR_QUEUE` | The worker polls a fixed list. Anything else ages in the ring. |
| Leave `--traffic-log` on forever | Append-only, no rotation, one line per request, body hashes included. |

---

## See also

- [UBUNTU.md](UBUNTU.md) — first install, systemd unit, firewall
- [DEPLOYMENT.md](DEPLOYMENT.md) — zero-downtime deploys, scaling, memory budget
- [WORKER_MODE.md](WORKER_MODE.md) — what must be reset per request, and the symptom index
- [ADMIN.md](ADMIN.md) — every admin endpoint
- [CACHE.md](CACHE.md) — BAN/PURGE, tags, `cache-report`
- [AUTOTLS.md](AUTOTLS.md) — ACME from flags or `[acme]`
- [UPGRADING.md](UPGRADING.md) — behaviour changes per version
