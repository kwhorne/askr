# Upgrading Askr

The short version: **within `1.x`, an upgrade is a drop-in.** Replace the binary (or
the image tag), reload, done. You don't need to touch `askr.toml`.

That's a promise, not a hope — the surfaces that make it true are listed in
[STABILITY.md](STABILITY.md), and every release since 1.0 has kept it. New features
arrive as new config keys that default to off, so a config written for 1.0.0 still
means exactly the same thing on the newest 1.x.

- [How to upgrade](#how-to-upgrade)
- [Zero-downtime upgrades](#zero-downtime-upgrades)
- [Rolling back](#rolling-back)
- [Version-by-version notes](#version-by-version-notes)
- [What can actually bite you](#what-can-actually-bite-you)

## How to upgrade

### Release tarball (systemd install)

```bash
askr upgrade                 # downloads, verifies signature + sha256, swaps the prefix
sudo systemctl reload askr   # graceful; see below
```

`askr upgrade` replaces the whole prefix (binary + bundled `libphp`) atomically and keeps
the previous version at `<prefix>/../askr.old`. It does **not** restart the server unless
you pass `--restart`.

From 1.5.0 it verifies a [minisign](https://jedisct1.github.io/minisign/) signature
against a public key compiled into the binary, and **refuses** a release it cannot verify
— a missing signature included. The `.sha256` is still checked, for what it is: proof the
download arrived intact, not proof of who produced it.

Verify it yourself if you'd rather not trust the updater:

```bash
VER=v1.5.1; ARCH=$(uname -m)
BASE=https://github.com/kwhorne/askr/releases/download/$VER
TARBALL=askr-${VER#v}-linux-$ARCH.tar.gz

curl -fLO $BASE/$TARBALL
curl -fLO $BASE/$TARBALL.minisig
curl -fsSL https://raw.githubusercontent.com/kwhorne/askr/$VER/keys/release.pub -o askr.pub
minisign -V -p askr.pub -m $TARBALL

# And the provenance attestation, which binds it to the workflow and commit that built it
gh attestation verify $TARBALL --repo kwhorne/askr
```

### Docker

```bash
docker pull ghcr.io/kwhorne/askr:1.5.1     # or :1.5 to follow patches
```

Pin the **exact** version in production and bump it deliberately. `:1.5` follows
patch releases, `:latest` follows everything — convenient for a laptop, surprising
on a server at 3am.

The `-full` tags (`1.5.1-full`) are the same server built with the optional features
compiled in: `sql-backend`, `observ`, `otel`, `http3`. If you use any of those, stay
on `-full`.

### The Laravel package

```bash
composer update kwhorne/askr-laravel
```

The package and the server are versioned independently; any recent package works with
any `1.x` server. Upgrade it when you want a new PHP-side helper.

## Zero-downtime upgrades

A reload replaces the workers without dropping a connection:

```bash
kill -HUP $(pidof askr)      # or: systemctl reload askr, or POST /api/reload
```

Workers finish their in-flight requests, then are replaced one at a time — there is
always a live worker accepting. **A reload does not pick up a new Askr binary**: the
master process is the old one. For a new binary you need a restart, which means a
brief gap unless something in front of you retries.

If you can afford one more moving part, the sturdiest sequence is:

1. `askr upgrade` (new binary on disk, old one kept)
2. `askr config-check askr.toml` — catches a config that the new version rejects
   *before* you stop anything
3. **drain the queue** — the shared-memory ring does not survive a restart, and jobs
   still in it are lost with no error anywhere the application can see
   ([Maintenance](MAINTENANCE.md#drain-the-ring-before-a-restart-or-an-upgrade))
4. restart

Turn on the canary so a bad **application** deploy can't take the fleet with it —
worth having in place before you start upgrading things:

```toml
[reload]
canary = true
canary_window = 5
canary_min_requests = 20
```

See [Deployment](DEPLOYMENT.md#canary-reload-zero-bad-deploy).

## Rolling back

- **Tarball:** the previous prefix is at `<prefix>/../askr.old`. Swap it back and
  restart.
- **Docker:** run the previous tag. This is why pinning matters.
- **Config:** a config written for an older 1.x is still valid, so rolling back the
  binary never requires rolling back `askr.toml`.

Rolling back is a supported operation, not an emergency improvisation. If a downgrade
ever fails on a config that the newer version accepted, that's a bug worth reporting —
it means we added something that isn't additive.

## Version-by-version notes

Nothing here is required. These are the things worth *adopting* after each upgrade.

### To 1.5.1

**One change can stop a start.** A non-loopback admin bind now **requires**
`ASKR_ADMIN_TOKEN`; without it the server refuses to start rather than exposing an open
reload trigger and a public dump of PIDs and memory. If you run `--admin 0.0.0.0:…` (or
any non-loopback `[admin] listen`) with no token, set one, or bind the admin plane to
`127.0.0.1` and reach it over SSH. A loopback bind is unchanged.

**One change is visible to code that reads job ids.** Queue jobs are now leased: a job's
id, as the driver hands it back, changes on each retry, because it identifies the
*reservation* rather than the row — that is what stops a worker whose lease lapsed from
acknowledging a job another worker has since taken. If you correlate log lines across a
job's attempts, key on the payload's uuid (what Laravel's `failed_jobs` uses), not the
id.

The rest need no action, but are worth knowing:

- **Multi-site instances are partitioned.** With `[[site]]`, the shared cache, sessions,
  locks, counters and job queue are now keyed per application (by docroot), so two sites
  in one instance no longer share them — two domains on one docroot still do. Queue and
  scheduler sidecars belong to the application at the top-level `root`; a second
  application in the same instance has no workers of its own. Broadcasting stays
  instance-wide (one Pusher secret per instance). See
  [Hosting](HOSTING.md#what-sites-share-and-what-they-dont).
- **`Vary` responses are cached again.** In 1.5.0 a response carrying its own `Vary`
  (e.g. `Accept-Language`) was not cached; it is now stored as one variant per value, so
  the hit rate returns on localised pages without serving one visitor's language to
  another.
- **The Pusher HTTP trigger requires a signature.** `POST /apps/{id}/events` must carry
  Pusher's `auth_signature` (and `body_md5`, `auth_timestamp`). `pusher-php-server` —
  and therefore Laravel's broadcaster — sends it on every call, so a correctly configured
  app needs no change; a bespoke trigger caller must sign, or it gets a `401`.

New, opt-in: **`[queue] persist = "<name>"`** keeps the job ring in a named shared-memory
object so pending jobs survive a restart (`askr upgrade` included). In a container raise
`--shm-size`; see [Docker](DOCKER.md#shared-memory-size).

### To 1.5.0

**Nothing is required.** Two things are worth adopting deliberately, because upgrading
alone will not turn them on:

- **`--sandbox-required` / `[server] sandbox_required`.** The sandbox used to warn and
  serve unhardened when a kernel feature was missing, which looks identical to success.
  Required mode refuses to serve instead. It needs `sandbox_write`, and refuses to start
  without it — seccomp alone does not stop PHP writing a webshell, because Askr
  interprets PHP in-process and no process creation is involved. See
  [Sandbox](SANDBOX.md#fail-closed---sandbox-required).
- **`ASKR_ADMIN_TOKEN`, if you run behind a local reverse proxy.** `PURGE`/`BAN` used to
  be accepted from any loopback peer with no token — and behind nginx or Caddy on
  127.0.0.1, *every* request is a loopback peer. Setting `trusted_proxies` now makes the
  token mandatory for those methods, so if you have `trusted_proxies` configured and no
  token, cache invalidation will start answering `403`. That is the fix working; set the
  token.

Three behaviour changes to be aware of, none of them configurable:

- **A response carrying its own `Vary` is no longer cached.** The cache key cannot
  express an arbitrary `Vary`, and the header used to be dropped rather than honoured —
  so a localised app answering `Vary: Accept-Language` had one visitor's language served
  to everyone. Correctness costs hit rate on exactly those responses.
- **Scheme is part of the cache key.** With `force_https` off, http and https no longer
  share an entry. Expect a one-time dip in hit rate.
- **Underscored header names are dropped.** `X_Forwarded_For` no longer becomes
  `HTTP_X_FORWARDED_FOR` in `$_SERVER`, because it collided with the dashed spelling and
  bypassed anything filtering it. This is the same default nginx ships. If an app of
  yours genuinely reads an underscored header, rename it to use dashes.

Releases are now **signed**, and `askr upgrade` refuses one whose signature does not
verify against the key compiled into the binary. Upgrading *to* 1.5.0 from an older
build still uses the old checksum-only path — the verification lives in the new binary,
so it protects the upgrade *after* this one.

### To 1.4.14

Nothing to do. If you use `askr doctor --app`, two things behave better:

- It now resolves configuration the way the application does — real environment variables
  beat `.env` — and names the source of each value. Run it **inside the container** for this
  to be meaningful.
- Its scheduler check no longer fires on `Artisan::command()`, which defines a command
  rather than scheduling one.

### To 1.4.13

**If you run queue workers, make sure `[queue] slots` is set** — `8192` is a reasonable
start. Askr now refuses to start without it when queue workers are configured, so a
misconfiguration is an error at boot rather than mail that quietly never sends.

```toml
[queue]
slots = 8192          # required alongside workers
workers = 4
script = "/opt/askr/examples/askr-queue.php"
```

The equivalent on the command line: `--queue-script` now requires `--queue-slots`.

### To 1.4.12

Take this one if your app draws QR codes — Laravel Fortify's two-factor setup, most likely.
Before it, `iconv` was not compiled in, and the page answered 500 with
`Call to undefined function iconv()`. Nothing to change beyond the version.

If you build from source on macOS, `PROFILE=minimal` also works again there.

### To 1.4.11

Nothing to do; everything is additive. Two things you may now see for the first time:

- **`WARN queue backlog is not being consumed`** in the log, naming a queue. It is telling you
  the truth: jobs on that queue are not being taken. Either no queue worker is running
  (`--queue` with `--queue-script`) or it doesn't poll that name (`ASKR_QUEUE`,
  comma-separated). This was previously invisible.
- **`/api/status` has a `queues` array.** `queue_ready` and friends are unchanged.

Worth running once against a running deployment:

```bash
askr doctor --app /var/www/example.com        # from inside the container, ideally
./scripts/smoke.sh https://example.com http://127.0.0.1:9000 "$ASKR_ADMIN_TOKEN"
```

Both exit non-zero on a failure, so they can gate a deploy. The first found a real
misconfiguration on the deployment it was written against; the second found a real
production fault on its first run.

### To 1.4.10

Nothing to do — both changes are additive.

If you use auto-TLS **and** wanted a config file, you can now have both: move the flags
into an `[acme]` section (see [CONFIGURATION.md](CONFIGURATION.md#acme)). Note that
`[acme]` and `[tls]` are mutually exclusive, and that a `[acme]` section with `domains` but
no `enabled = true` is now an error rather than a silent no-op.

If you monitor queue depth, `delayedSize()`, `reservedSize()` and
`creationTimeOfOldestPendingJob()` now report real numbers instead of 0/null — a
threshold that has never fired may start firing, because it was reading zeros.

### To 1.4.9

**Package-only fix — `composer update kwhorne/askr-laravel`.** On Laravel 13 the queue
driver was a fatal error at class-load time (missing contract methods), so anything that
resolved the queue — sending mail, dispatching a job — killed the worker and answered 502
with `askr: php worker died mid-request`. No server upgrade needed.

Note that `delayedSize()` and `reservedSize()` report 0 and
`creationTimeOfOldestPendingJob()` reports `null` on this driver: Askr's queue knows those
values internally but doesn't expose them to PHP yet. If you rely on `queue:monitor`
thresholds, use `pendingSize()`/`size()`, which are accurate.

### To 1.4.8

**Upgrade if you run Livewire, Flux or anything needing Alpine in worker mode**, and take
the worker script with it — the fix is in `examples/laravel-worker.php`, so a new binary
alone changes nothing if you copied the old script into your project:

```bash
docker pull ghcr.io/kwhorne/askr:1.4.8
docker run --rm ghcr.io/kwhorne/askr:1.4.8 \
  sh -lc 'cat /opt/askr/examples/laravel-worker.php' > storage/askr-worker.php   # if you keep your own copy
```

Symptoms this fixes: interactivity that works for the first page load or two and then
stops, `wire:` and `x-` attributes doing nothing, a Flux appearance toggle showing both
icons — all with an empty console, because the script tag was missing rather than broken.

### To 1.4.7

**Upgrade if you serve HTTPS.** Over HTTP/2 — which ALPN picks by default over TLS — Askr
lost the host the request was addressed to. Nothing to configure; the effects were:

- generated URLs and redirects came out as `https://localhost/…` (Laravel builds them from
  the request, not from `APP_URL`);
- **virtual hosts fell through to the default site**, so with `[[site]]` an h2 request for
  one domain could be served another's app — check your access logs if you host several;
- response-cache entries could be shared between domains. If you cache and host more than
  one domain, flush the cache after upgrading: `curl -X BAN -H 'X-Ban-Url: /*' …` or simply
  restart without `[cache] persist`.

### To 1.4.6

**If you use `--config` together with other flags, Askr will now refuse to start** and tell
you which flags it would have ignored. That's the fix: they were never applied. Move them
into the config file. Nothing else changes.

Worth reading if you deploy with Docker on Linux: the new
[bind-mount ownership](DOCKER.md#bind-mounting-an-app-on-linux-file-ownership) and
[behind-nginx](HOSTING.md#behind-nginx-or-any-other-proxy) sections cover the traps that
make a laptop-tested compose file fail on a server.

### To 1.4.5

**Upgrade — this closes Askr-46.** If you serve a Laravel app with Flux/Livewire in worker
mode, file responses (`flux.js`, downloads, streamed exports) were killing workers; that
is gone, with the standard asset setup and no workarounds. Also: an `exit()` or an
escaping exception in the app now costs that one request instead of the worker. The
`docs/WORKER_MODE.md` known-issue section is obsolete as of this release.

### To 1.4.4

Nothing to do. No behaviour changes for a healthy app — this release is about what happens
when one isn't:

- a failed `accept()` no longer takes down a worker that is serving other requests;
- a worker that dies mid-request answers **502** instead of a complete-looking **200 with
  an empty body** (if you have monitoring that only checks status codes, it will start
  seeing these — they were always failures, just invisible ones);
- the log says what actually ended a worker instead of guessing "fatal/OOM?". If you have
  alerts matching that string, they won't fire any more. The replacements name the case:
  the request channel closing, or the worker script leaving its loop.

### To 1.4.3

**Upgrade if you run Laravel in worker mode**, and take the new worker script with it —
the fixes are in `examples/laravel-worker.php`, so a new binary alone changes nothing if
you copied the old script into your project:

```bash
docker pull ghcr.io/kwhorne/askr:1.4.3
composer update kwhorne/askr-laravel        # for the cache/queue config fix
# using your own copy of the worker script? re-copy it:
docker run --rm ghcr.io/kwhorne/askr:1.4.3 \
  sh -lc 'cat /opt/askr/examples/laravel-worker.php' > storage/askr-worker.php
```

What changes for you:

- **Authenticated requests no longer leak between visitors.** If you ran worker mode with
  sessions before 1.4.3, this was happening — quietly, and only on workers that had
  served a login.
- **HTML form posts work.** If you saw unexplained 419s on submit and worked around them
  with a header or by disabling CSRF for a route, undo that.
- **File downloads and streamed responses have bodies.** Anything you thought was a Flux,
  Livewire or download bug is worth re-testing.
- **`CACHE_STORE=askr` works without editing `config/cache.php`.** The manual entry the
  package README documented is now optional; keeping it changes nothing.

### To 1.4.2

Two things are worth acting on.

**If you set `ASKR_ADMIN_TOKEN` and run the Docker image**, your containers were reporting
`unhealthy` — the healthcheck polled the gated `/api/status`. Nothing to configure; the
image now polls `/healthz`. If you wrote your own probe, point it at `/healthz` too.

**If you want `http://` visitors redirected**, you can now have it:

```toml
[server]
force_https = true
http_redirect = "0.0.0.0:80"      # not needed with --acme; automatic there
```

Also worth knowing:

- **The admin plane denies by default.** If you drive it with a script that hits some path
  other than `/`, `/favicon.ico` or `/healthz`, that path now needs the bearer token. The
  documented endpoints are unchanged.
- **ACME keys are re-written 0600.** If your tooling read `key.pem` as a non-owner user, it
  will stop. That it worked before was the bug.

### To 1.4.1

**Upgrade if you serve with the Docker image or the release tarball.** Before 1.4.1, a
PHP notice, warning or deprecation was written into the HTTP response body — including
absolute filesystem paths — and *not* logged. The published 1.4.0 image served this to
anyone requesting the homepage of a stock Laravel 12 app on PHP 8.5:

```
Deprecated: Constant PDO::MYSQL_ATTR_SSL_CA is deprecated since 8.5 …
in /app/vendor/laravel/framework/config/database.php on line 62
```

A framework masks this once its own error handler is installed, but config files are
parsed before that, so boot-time diagnostics went straight to the client. In worker mode
it also preceded the headers and truncated the page.

Nothing to configure — diagnostics now go to the log. Two things to know:

- **If you relied on seeing PHP errors in the browser during development**, opt back in:
  `ASKR_PHP_INI="display_errors=1"`.
- **Check your log volume after upgrading.** Notices that were previously discarded
  (`log_errors` was off) are now written. If a busy app emits one per request, that's
  real output — and a good prompt to fix the notice, since it was always there.

`error_reporting` is unchanged (`E_ALL`), so nothing new is hidden; it changed
destination, not visibility.

### To 1.4.0

Worth doing, in this order:

```bash
# 1. Find out what's actually worth caching — and what only looks cacheable
askr serve --traffic-log /tmp/traffic.jsonl    # leave it for an hour of normal traffic
askr cache-report /tmp/traffic.jsonl
```

```php
// 2. Cache the routes it called safe. No tag list to maintain: the page is tagged
//    with the models it read, so a save() clears exactly the pages that showed them.
Route::get('/products/{product}', ProductController::class)
    ->middleware('askr.cache:300');
```

Requires `composer update kwhorne/askr-laravel` for the middleware.

**One behaviour change to know about.** A response carrying more cache tags than an
entry can hold (8) is now **refused** rather than cached. Before, the surplus tags were
silently dropped — which meant `askr_cache_forget_tag()` could never reach them, and
the page stayed stale until its TTL expired.

If you hand-write long tag lists you may therefore see a page stop being cached, and
`askr_cache_tag_overflow_total` count up on `/metrics`. That page was already broken;
it just failed quietly. Tag by class (`posts`) instead of per instance (`posts:3`), or
cache a smaller fragment with [ESI](FEATURES.md#esi--one-page-many-ttls).

`--traffic-log` is a diagnostic: it writes a line per PHP-served request, so turn it
off again when you have your answer.

### To 1.3.0

No action needed. Internally this is a dependency refresh (including four major
bumps) plus a much larger test suite; no user-visible behaviour changed.

Two things to know:

- **A persisted response cache may be dropped once.** `[cache] persist` files are
  tied to the entry layout, and a new Askr build can invalidate them. The first boot
  after an upgrade then starts with a cold cache and logs
  `response cache dump ignored (different build or cache size)`. That's the guard
  working — a cache is never reinterpreted across layouts.
- **`ASKR_*_DB` SQLite files are opened by a newer bundled SQLite** (rusqlite 0.31 →
  0.40). SQLite is backwards compatible with older files, so nothing to do, but take
  your usual backup first if those hold queue jobs you can't lose.

### To 1.2.0

Worth adopting:

```toml
# Refuse abusive traffic before PHP wakes up — enforced across the whole fleet
[server]
trusted_proxies = ["10.0.0.0/8"]     # required behind a load balancer

[[ratelimit]]
path = "/login"
limit = 5
window = 300

# Keep the cache across restarts
[cache]
persist = "/var/lib/askr/rcache.bin"
persist_key = "your-release-sha"
```

- If you configure rate limits **behind a proxy and forget `trusted_proxies`**, every
  client shares one bucket and you'll rate-limit your whole site. Askr warns at
  startup; take the warning seriously.
- Run `askr tune --root public` for measured starting values for `workers` and
  `max_rss_mb`.
- The canary gate got much better in this release (it compares the new worker against
  the rest of the fleet instead of an absolute error count). If you'd tried `canary`
  before and found it aborted deploys for no reason, try it again.

### To 1.1.0

Worth adopting: [ESI](FEATURES.md#esi--one-page-many-ttls) if a single live widget is
what keeps a page uncacheable, `PURGE`/`BAN` for URL-targeted invalidation, and
`[[cache.rule]]` if you need cache policy for an app you can't edit.

`PURGE`/`BAN` are gated on `ASKR_ADMIN_TOKEN`, or restricted to loopback when no token
is set. Set the token if you want to invalidate from a deploy script on another host.

### To 1.0.1

**Upgrade if you're on 1.0.0.** Static file serving could disclose PHP sources and
dotfiles: `GET /index.php` returned source, and with a document root pointed at an app
root, `GET /.env` returned `APP_KEY` and database credentials. Fixed in 1.0.1, with the
suffixed variants (`index.php.bak`, `config.php~`) fixed in 1.1.0.

While you're there, check that your document root is a dedicated `public/` directory
and not the application root.

### From 0.9.x to 1.0

1.0 added no features — it froze the surface. If your `0.9.12` setup worked, 1.0 works
identically.

One deprecation carried over: `--acme-directory` became `--acme-directory-url` in
0.9.7 (it was too easily confused with `--acme-dir`, the local certificate cache). The
old spelling still works as a hidden alias.

## What can actually bite you

Honest list, in rough order of how often it happens:

1. **A cold cache after a restart.** The response cache lives in shared memory. Unless
   `[cache] persist` is set — and the dump is still valid — the first requests after
   any restart hit PHP. Coalescing stops it becoming a stampede, but a big site
   restarting at peak will feel it. Reload rather than restart when you can.
2. **A config the new version rejects.** Validation gets stricter as it gets better
   (glob patterns that look like regexes, rules with no effect, unknown keys). This is
   deliberate — a silently ignored rule is worse — but it means
   `askr config-check askr.toml` belongs in your deploy script, before the restart.
3. **`libphp` and the binary are a matched pair.** A release tarball contains both.
   Don't mix a new `askr` with an old bundled `libphp`; `askr upgrade` and the Docker
   images handle this for you.
4. **A PHP security release means a new Askr release.** PHP is compiled into the
   distribution, so you can't patch it independently. Watch
   [PHP's releases](https://www.php.net/ChangeLog-8.php) as well as ours.
5. **Optional features live in the `-full` build.** Upgrading from a `-full` tag to a
   plain one silently drops `sql-backend`, `observ`, `otel` and `http3`. The server
   will start and your `ASKR_OBSERV_DSN` will simply do nothing.

## After upgrading

```bash
askr doctor                    # PHP build, extensions, platform probes
askr config-check askr.toml    # config still valid and resolving as you expect
curl -s localhost:9000/api/status   # workers alive, version, rollout state
```

If something looks wrong, the [admin API](ADMIN.md) and the
[observability guide](OBSERVABILITY.md) are the fastest ways to see what the server
thinks is happening.
