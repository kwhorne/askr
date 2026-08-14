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

### A3. Use compose instead of a long command

For anything you'll start more than once, use
[`examples/docker/quickstart.yml`](../examples/docker/quickstart.yml). Copy it into your
project as `docker-compose.yml`:

```bash
curl -O https://raw.githubusercontent.com/kwhorne/askr/main/examples/docker/quickstart.yml
mv quickstart.yml docker-compose.yml
docker compose up          # -d to detach
docker compose down
```

It bind-mounts the directory you run it from, so editing code needs no rebuild, and it
adds the three things the bare `docker run` above leaves out: **worker mode** (boot
Laravel once — this is where the speed is), a **response cache** so page caching has
somewhere to go, and a 30-second `stop_grace_period` so `down` drains in-flight requests
instead of cutting them off.

Pointing it at a project elsewhere:

```bash
ASKR_APP_PATH=~/code/my-app docker compose -f quickstart.yml up
```

Handy once it's running:

```bash
curl localhost:9000/api/status                       # workers, mode, memory
docker compose exec askr /opt/askr/askr tune         # what config this app wants
docker compose logs -f                               # PHP diagnostics land here
```

### A4. For production

`quickstart.yml` is for development: it mounts your code. In production, bake the code
into an image so a deploy is a new image rather than a mutated directory — that's
[`examples/docker/docker-compose.yml`](../examples/docker/docker-compose.yml) with its
`Dockerfile`, read-only root filesystem and a volume for `storage/`. Pin an exact version
(`askr:1.4.14`), not `:1.4` or `:latest`. Full details: **[DOCKER.md](DOCKER.md)**.

Then skip to [step 4: the Laravel side](#4-the-laravel-side).

---

## Path B — Release tarball (recommended for production)

Self-contained: the binary, `libphp`, OPcache and the example scripts, in one archive.
Nothing is installed system-wide and no system PHP is touched.

### B1. Download and unpack

```bash
VER=v1.4.14; ARCH=$(uname -m)
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
  see [B6](#b6-put-the-site-on-https).
- `--admin 127.0.0.1:9000` — status/metrics/reload API, bound to loopback so it isn't
  exposed.

### B5. Check it

```bash
curl -ik https://localhost:8443/
curl -s http://127.0.0.1:9000/api/status
```

### B6. Put the site on HTTPS

Askr terminates TLS itself — there's no nginx in front to configure. Pick the route that
matches where you are.

#### Route 1: Let's Encrypt, automatic (what most people want)

Askr obtains **and renews** the certificate itself. No certbot, no cron job, no reload:
a renewed certificate is picked up by running workers without dropping a connection.

**Test with staging first.** Let's Encrypt's production rate limits are strict and
hitting them means waiting hours; staging is generous but issues untrusted certs, so
browsers will warn — that's expected:

```bash
sudo ./askr-run.sh serve \
  --root /var/www/app/public \
  --listen 0.0.0.0:443 \
  --acme --acme-staging \
  --acme-domain example.com \
  --acme-email you@example.com \
  --acme-dir /var/lib/askr/acme
```

Watch the log for the certificate being issued, then drop `--acme-staging` and delete
the staging cache so a real certificate is fetched:

```bash
sudo rm -rf /var/lib/askr/acme && sudo ./askr-run.sh serve … --acme …   # without --acme-staging
```

Requirements, all three of which trip people up:

- **`example.com` must already resolve to this server.** ACME proves you control the
  domain by fetching a file over HTTP — DNS first, certificate second.
- **Port 80 must be reachable from the internet**, because that's where the HTTP-01
  challenge is answered (`--acme-http`, default `0.0.0.0:80`). You can keep serving
  traffic on it; Askr only intercepts the `/.well-known/acme-challenge/` path.
- **`--acme-dir` must persist across restarts.** It holds the account key, `cert.pem`,
  `key.pem` and the renewal deadline. Losing it means re-issuing every boot, which is
  how you hit rate limits. In Docker, make it a volume.

Several domains on one certificate (SAN) — repeat the flag:

```bash
--acme-domain example.com --acme-domain www.example.com --acme-domain api.example.com
```

Full reference, including private CAs and testing against
[Pebble](https://github.com/letsencrypt/pebble): **[AUTOTLS.md](AUTOTLS.md)**.

#### Route 2: a certificate you already have

From a corporate CA, a wildcard you manage, or `mkcert` locally:

```bash
./askr-run.sh serve --root /var/www/app/public --listen 0.0.0.0:443 \
  --tls-cert /etc/ssl/certs/example.com.fullchain.pem \
  --tls-key  /etc/ssl/private/example.com.key
```

Use the **full chain** (leaf + intermediates), not just the leaf — a lone leaf works in
browsers that happen to have the intermediate cached and fails everywhere else, which is
a miserable thing to debug. In `askr.toml`:

```toml
[tls]
cert = "/etc/ssl/certs/example.com.fullchain.pem"
key  = "/etc/ssl/private/example.com.key"
```

Replacing those files is enough — Askr notices and reloads the certificate without a
restart, so renewals from your own tooling need no orchestration.

#### Route 3: self-signed, for development

```bash
--tls-self-signed
```

Generated at startup, never written to disk. Browsers will warn; `curl` needs `-k`.
Never in production.

#### Then send everyone to HTTPS

Serving HTTPS doesn't stop people arriving on `http://`. Add:

```toml
[server]
force_https = true
```

(or `--force-https`). A request that isn't secure is answered with a **308** to the same
host, path and query. "Is this request secure?" is decided from the connection's own TLS,
then `[server] https = true`, then `X-Forwarded-Proto: https`.

**One thing to know:** `force_https` can only redirect a request that *reaches* Askr over
plain HTTP, and a TLS listener never sees one. So something has to listen on port 80.

- **Route 1 (ACME): already done.** The challenge listener stays up for the whole process
  and redirects everything that isn't a challenge, so `--force-https` is all you add. A
  challenge always wins over the redirect — otherwise a domain could never get its first
  certificate.
- **Route 2 (your own certificate):** name the plain port yourself:

  ```toml
  [server]
  force_https = true
  http_redirect = "0.0.0.0:80"
  ```

  (or `--http-redirect 0.0.0.0:80`). Verified: `308 → https://example.com/pricing?ref=x`,
  path and query preserved.
- **Behind a load balancer or CDN:** the balancer redirects, and `force_https` covers
  anything that slips through via `X-Forwarded-Proto` — set
  [`trusted_proxies`](CONFIGURATION.md) so that header is believed only from your
  balancer.

Binding `:80` needs privileges just like `:443`, and if it fails Askr warns and keeps
serving HTTPS rather than refusing to start.

If you also want `www` → apex, that's a `[[redirect]]` rule:
**[HOSTING.md](HOSTING.md#2-redirects--wwwapex-and-httphttps)**.

#### Check it

```bash
curl -sI https://example.com/ | head -1
curl -sI http://example.com/  | head -2          # expect 308 + Location: https://…
echo | openssl s_client -connect example.com:443 -servername example.com 2>/dev/null \
  | openssl x509 -noout -subject -issuer -dates
```

The last line is the one worth keeping: it tells you which certificate is actually being
served, who issued it, and when it expires — the three things a broken TLS setup lies
about.

#### Binding to port 443

Ports below 1024 need privileges. Either run the service as root and let Askr drop
privileges, or grant the capability once so it doesn't need root at all:

```bash
sudo setcap 'cap_net_bind_service=+ep' /opt/askr/askr
```

The systemd unit in [UBUNTU.md](UBUNTU.md) handles this for you.

#### Many domains, many certificates

One certificate with several SANs (above) covers most cases. For genuinely separate
sites with separate certificates, see virtual hosts in
**[HOSTING.md](HOSTING.md)**.

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

**Every PHP route returns 500 but static files work.** On Linux, the container can't write
`storage/` — the image runs as uid 999 and a bind mount keeps the host's ownership. Add
`user: "1000:1000"` (the uid that owns the project). See
[DOCKER.md](DOCKER.md#bind-mounting-an-app-on-linux-file-ownership). macOS hides this, so a
compose file that works on your laptop can fail on the server.

**Flags seem to be ignored.** If you pass `--config`, it *is* the configuration — the other
flags aren't merged in. Askr now refuses to start and names them; before 1.4.6 it ignored
them silently. Move them into the file: [CONFIGURATION.md](CONFIGURATION.md).

**`Driver [askr] not supported`.** `SESSION_DRIVER=askr` needs the Laravel package:
`composer require kwhorne/askr-laravel`.

**`unrecognized subcommand 'askr'` from Docker.** Drop the leading `askr` — the
image's entrypoint is the launcher already. See [A1](#a1-run-it).

**A feature in the docs isn't in your binary.** You have the plain build, not `-full`.
Compare `askr --version` output and re-read [B1](#b1-download-and-unpack).

**ACME never issues a certificate.** Almost always one of: the domain doesn't resolve
to this machine yet, port 80 isn't reachable from the internet, or you're rate-limited
after retrying with production instead of `--acme-staging`. The log says which.

**HTTPS works in `curl` but browsers complain about the chain.** You gave `--tls-cert`
the leaf only. Use the full chain.

**`Permission denied` binding `:443`.** Ports under 1024 need privileges — see
[binding to port 443](#binding-to-port-443).

**Sessions or cache empty out at random.** With `SESSION_DRIVER=askr` the regions live
in shared memory sized at startup; a too-small region evicts. `askr tune` suggests
sizes, and [CACHE.md](CACHE.md) explains the trade-off.

Still stuck? `askr doctor` first, then the logs, then
[open an issue](https://github.com/kwhorne/askr/issues) — the output of `askr doctor`
and `askr --version` in the report saves a round trip.
