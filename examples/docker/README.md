# Dockerising a Laravel app with Askr

A ready-made scaffold: **one container** that runs the web workers, the queue,
the scheduler, the shared cache and broadcasting — replacing the usual
app + nginx + redis + queue + cron stack.

## Use it

Copy the four files here to your Laravel project **root**:

```
Dockerfile
askr.toml
docker-compose.yml
.dockerignore
```

Then:

```bash
docker compose up --build
# → http://localhost
```

or without compose:

```bash
docker build -t my-app .
docker run -p 80:8000 -v app-storage:/var/www/app/storage my-app
```

## What you get in that one container

| Normally a separate service | Here |
| --- | --- |
| nginx / Apache | Askr serves HTTP directly (+ gzip/br compression) |
| php-fpm | embedded PHP, booted once (worker mode) |
| redis (cache) | shared-memory cache (`[cache]`) — add the Laravel driver |
| queue worker | `[queue] workers = 2`, supervised |
| cron | `[scheduler]`, built-in |
| reverb (websockets) | `[broadcast]` + `--pusher` for Echo |

## Configuration

Everything is in **`askr.toml`** (mounted at `/etc/askr/askr.toml`). It's tuned
for a container: `workers = "auto"` reads the container's CPU limit (cgroup), the
worker/queue/scheduler runner scripts are the ones bundled in the base image, and
the admin plane (health + Prometheus `/metrics`) binds to `127.0.0.1:9000`.

Raise `max_body_size` for large uploads — they're streamed to `/tmp` (a tmpfs),
so memory stays flat regardless of file size.

## Filament / front-end assets

Askr's `libphp` (0.5.0+) bundles `intl`, `gd`, `curl`, `zip`, `pdo_mysql`/`pgsql`,
so **Filament apps run**. Two app-side steps for a Filament build (not Askr
issues — the same on any server):

- **Build the Vite assets** so the panel's theme is present. Uncomment the
  `assets` stage in the `Dockerfile` (`npm ci && npm run build`). If npm hits a
  peer-dependency conflict, use `npm install --legacy-peer-deps`.
- **Don't ship a stale package cache.** `composer install --no-dev` drops dev
  providers; if a committed `bootstrap/cache/packages.php` still references one
  you'll get a "Class … not found" 500. The compose file mounts
  `bootstrap/cache` as tmpfs (regenerated at boot) so this can't happen; if you
  run without it, `rm -f bootstrap/cache/*.php` in the build.

## The `.env` / secrets

`.env` is intentionally **not** copied into the image. Provide it at runtime:

- `env_file: .env` in `docker-compose.yml`, or
- individual `environment:` entries, or
- your platform's secret store.

## Writable paths (read-only image)

The image is `read_only: true`. Laravel needs two writable spots, both provided
by the compose file:

- **`/var/www/app/storage`** → a named volume (logs, file cache, sessions, views).
- **`/var/www/app/bootstrap/cache`** → a tmpfs (compiled config/route/package
  cache, regenerated on boot).

Everything else is immutable — which pairs perfectly with
`opcache.validate_timestamps=0`: new code = new image = new container.

## Operating it

- **Zero-downtime reload:** `docker kill -s HUP <container>` (rolls workers one at
  a time; `[reload] canary = true` health-checks the first).
- **Graceful stop:** `docker stop` sends SIGTERM → in-flight requests drain
  (compose sets `stop_grace_period: 30s`).
- **Health:** the built-in `HEALTHCHECK` hits the admin API; `docker ps` shows
  `healthy`.
- **Metrics:** scrape `http://127.0.0.1:9000/metrics` (Prometheus) via a
  port-forward or an internal network.

## TLS

Behind a load balancer, run plain HTTP and add `https = true` to `[server]` so
`$_SERVER['HTTPS']` is correct. To terminate TLS in the container, set
`[tls] cert`/`key` and publish 8443. See [../../docs/DOCKER.md](../../docs/DOCKER.md).
