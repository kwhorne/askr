# Askr in Docker

Askr is unusually well-suited to containers: the release package is already
self-contained (the binary + `libphp` with an `$ORIGIN/lib` rpath), so the whole
PHP-FPM/nginx/supervisord stack people normally rig up disappears. **One
container is the whole environment** — because Askr supervises queue workers and
the scheduler, and the cache/broadcast live in shared memory in the same process
tree, this single service replaces what's usually five (app, nginx, redis, queue,
cron).

## The image

Published to GHCR for `linux/amd64` and `linux/arm64` on every release tag:

```
ghcr.io/kwhorne/askr:0.8        # latest 0.8.x
ghcr.io/kwhorne/askr:0.9.12      # exact
ghcr.io/kwhorne/askr:latest
```

### `-full` variant

The default image is the standard build. A **`-full`** variant is published with
the optional tiers compiled in — the **durable L2 SQL Anywhere backends**
(`ASKR_CACHE_DB` / `ASKR_QUEUE_DB` / `ASKR_BROADCAST_DB`) and the
**observability sink** (`ASKR_OBSERV_DSN`) — so you don't have to build from
source:

```
ghcr.io/kwhorne/askr:0.9.12-full
ghcr.io/kwhorne/askr:0.9-full
ghcr.io/kwhorne/askr:full
```

Same runtime, same size class; the extra features are inert until you set the
corresponding env vars. The `askr-*-linux-*-full.tar.gz` release tarball is the
non-Docker equivalent. See [Storage backends](STORAGE_BACKEND.md) and
[Observability](OBSERVABILITY.md).

It's built on **`ubuntu:24.04`** — deliberately, not Debian and not Alpine (see
[below](#why-ubuntu-2404-and-not-alpine)). It contains only the server; you layer
your app on top.

> **Ready-made scaffold:** [`examples/docker/`](../examples/docker/) has a
> drop-in `Dockerfile`, `askr.toml`, `docker-compose.yml` and `.dockerignore` —
> copy them to your Laravel project root and `docker compose up --build`.

## Your app image

```dockerfile
# 1. install PHP dependencies
FROM composer:2 AS deps
COPY . /app
RUN composer install --no-dev --optimize-autoloader

# 2. drop them onto the Askr runtime
FROM ghcr.io/kwhorne/askr:0.8 AS runtime
COPY --from=deps --chown=askr /app /var/www/app
ENV ASKR_APP_BASE=/var/www/app
CMD ["serve", \
     "--root", "/var/www/app/public", \
     "--worker-script", "/opt/askr/examples/laravel-worker.php", \
     "--workers", "4", \
     "--admin", "127.0.0.1:9000", \
     "--queue", "2", \
     "--scheduler-script", "/opt/askr/examples/askr-scheduler.php", \
     "--response-cache", "512", \
     "--access-log", "-"]
```

That one container runs the web workers, the queue, the scheduler, the shared
cache and broadcasting.

## docker-compose

```yaml
services:
  app:
    build: .
    ports:
      - "80:8000"          # host 80 → container 8000 (no root/setcap needed)
    read_only: true
    tmpfs:
      - /tmp               # multipart upload temp files land here
    volumes:
      - app-storage:/var/www/app/storage   # logs, file cache, sessions
    stop_grace_period: 30s # let the graceful drain finish
    environment:
      ASKR_APP_BASE: /var/www/app
volumes:
  app-storage:
```

## Inertia SSR (or any helper process)

Askr can supervise an arbitrary external command alongside the workers — spawned,
respawned if it dies, and stopped gracefully with the rest. This is how you run
**Inertia SSR** (a Node server Inertia renders against) in the same container:

```toml
# askr.toml
[[sidecar]]
command = "node bootstrap/ssr/ssr.mjs"
```

or `--sidecar "node bootstrap/ssr/ssr.mjs"`. For SSR the app image also needs
Node and the built SSR bundle (`npm run build` with an SSR entry, producing
`bootstrap/ssr/ssr.mjs`) — add a Node stage to your Dockerfile. Inertia talks to
the SSR server on `127.0.0.1:13714` inside the container. Without SSR, Inertia
renders client-side and no sidecar is needed.

The same mechanism runs any helper (a metrics exporter, a separate worker, etc.).

## The details that make it good

### Signals (PID 1)

The Askr master is already a supervisor that reaps its children, so it runs as
PID 1 without `tini`. The signal contract:

- `docker stop` → **SIGTERM** → graceful drain (in-flight requests finish). Set
  `stop_grace_period: 30s` so the drain window isn't cut short.
- `docker kill -s HUP <container>` → **zero-downtime rolling reload**; add
  `--canary` for a safe reload (a bad deploy takes down one worker, not all) —
  inside the container.

### Workers vs. cgroups

Askr's default worker count is **cgroup-aware** (0.4.2+): in a container it reads
the CPU limit (`cpu.max`) rather than the host's core count, so `cpus: 2` forks 2
workers, not 64. You can still pin it explicitly with `--workers N`, which is
recommended in production for predictability.

### Read-only filesystem

Run `read_only: true` and give Askr exactly what it needs to write:

- `tmpfs` on `/tmp` — upload temp files (streamed multipart) live here.
- a volume on `storage/` — logs, file cache, sessions (if using the file driver).

Everything else is immutable, which pairs perfectly with the
`opcache.validate_timestamps=0` that `askr-run.sh` sets: new code = new image =
new container, no reload semantics to reason about.

### Healthcheck

The image ships a `HEALTHCHECK` that hits the built-in admin API — enable the
admin plane on `127.0.0.1:9000` (`--admin 127.0.0.1:9000` or `[admin] listen`):

```
HEALTHCHECK CMD curl -sf http://127.0.0.1:9000/api/status || exit 1
```

Prometheus can scrape `http://<admin>/metrics`.

### TLS

- **Behind a load balancer** (the common Docker case): run plain HTTP and pass
  `--https` so `$_SERVER['HTTPS']` is correct for the app.
- **Publishing 443 directly**: mount a cert/key and use `--tls-cert`/`--tls-key`.
- **Non-root**: serve on 8000/8443 inside the container and let Docker map the
  host ports — no root or `setcap` needed in the container at all.

## Why `ubuntu:24.04` (and not Alpine)

**glibc match.** The release tarballs are built and CI-tested on `ubuntu-latest`
(24.04, glibc 2.39). A binary linked against glibc 2.39 won't start on Debian
bookworm (glibc 2.36) — glibc is backward-, not forward-compatible. `ubuntu:24.04`
is the exact build environment: zero surprises, and the same world the
[Ubuntu production guide](UBUNTU.md) documents.

**Not Alpine.** Alpine uses musl, which for this project is not cosmetic:

- Our binaries are glibc-linked; Alpine support would mean an entirely separate
  musl build pipeline (Rust + libphp + every C dependency). The `gcompat` shim is
  notoriously fragile for something as large as an embedded PHP interpreter.
- **PHP on musl has known production issues** — most seriously musl's small
  default thread-stack sizes cause segfaults in deeply recursive PHP workloads
  (large Blade trees, heavy regex), plus historical DNS/allocator quirks under
  load. Not a trade a "memory-safe, correct under load" server should make.
- The win is small: ~70 MB off the base, but libphp + opcache + app code are the
  same, so the total shrinks maybe 30–40%.

The whole image lands around 150–200 MB — fine for something that replaces app +
nginx + redis + cron. If you later want it smaller, the right path is a **chiseled
Ubuntu** or `gcr.io/distroless/cc` (glibc, no shell) `-slim` variant — Alpine's
size with glibc compatibility — not musl.
