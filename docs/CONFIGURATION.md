# Configuration

Askr can be configured with CLI flags (see [CLI](CLI.md)) or a typed
`askr.toml` file. The config file is the **declarative source of truth** — the
thing tooling and the admin GUI edit — and is recommended for production.

```bash
askr config-check askr.toml     # validate + print resolved settings
askr serve --config askr.toml   # run (the file is authoritative)
```

When `--config` is given, the file provides everything; other `serve` flags are
ignored.

## `askr.toml` reference

A complete, commented example lives at
[`examples/askr.toml`](../examples/askr.toml). Unknown keys are rejected, so
typos fail fast in `config-check`.

### `[server]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | string | `127.0.0.1:8000` | Address to bind. |
| `root` | path | `public` | Document root (the app's `public/`). |
| `front` | string | `index.php` | Front controller, relative to `root`. |
| `workers` | string | `auto` | Number of worker processes, or `auto` (= CPU cores). |
| `max_requests` | int | `0` | Recycle each worker after N requests (`0` = never). |
| `max_rss` | int | `0` | Recycle a worker gracefully once its RSS exceeds this many MB (`0` = never). Leak-aware; Linux only. |
| `shadow_to` | string | — | Mirror sampled safe requests to this upstream URL for deploy validation. |
| `shadow_sample` | int | `100` | Percent of eligible requests to mirror. |
| `max_body_size` | string | `16M` | Reject larger bodies with `413`. `K`/`M`/`G` or plain bytes. |
| `https` | bool | `false` | Force HTTPS in `$_SERVER` (e.g. behind a TLS terminator). Implied by TLS. |
| `workers_min` | int | = `workers` | CoW autoscaling floor (with `--cow`). |
| `workers_max` | int | = `workers` | CoW autoscaling ceiling (> min enables autoscaling). |
| `access_log` | path | — | JSON access log per request; `-` for stdout. Off if unset. |
| `sandbox` | bool | `false` | Linux hardening: seccomp no-exec. See [Sandbox](SANDBOX.md). |
| `sandbox_write` | path[] | `[]` | Landlock: writes allowed only under these paths (enables the FS restriction). |

### `[worker]`

Omit this whole section to run in per-request mode. Present it to enable
**worker mode** (boot once, serve many — see [Worker mode](WORKER_MODE.md)).

| Key | Type | Meaning |
| --- | --- | --- |
| `script` | path | Worker script that boots the app and loops. |
| `app_base` | path | Application base path, exported as `$ASKR_APP_BASE` for the worker script (inherited across `fork`). |
| `ini` | string | Extra php.ini lines (newline-separated), e.g. to load opcache. |
| `paranoid` | bool | Dev only: detect state bleed between requests (expensive). See [Worker mode](WORKER_MODE.md#is-my-app-worker-safe----paranoid). |

### `[tls]`

| Key | Type | Meaning |
| --- | --- | --- |
| `cert` | path | TLS certificate chain (PEM). Use with `key`. |
| `key` | path | TLS private key (PEM). |
| `self_signed` | bool | Generate a v3 self-signed cert on startup (dev). Mutually exclusive with `cert`/`key`. |

Enabling TLS negotiates HTTP/2 or HTTP/1.1 via ALPN and sets `HTTPS=on` in
`$_SERVER` (so Laravel emits `secure` cookies). Certs must be **X.509 v3**.

**Auto-TLS (ACME / Let's Encrypt)** is configured via flags (`--acme`,
`--acme-domain`, `--acme-email`, …) — combine with `--config`. See
[Auto-TLS](AUTOTLS.md).

### `[admin]`

| Key | Type | Meaning |
| --- | --- | --- |
| `listen` | string | Admin dashboard/API address (e.g. `127.0.0.1:9000`). Omit to disable. See [Admin](ADMIN.md). |

### `[queue]`

Run queue workers in the same binary, supervised alongside the web workers.

| Key | Type | Meaning |
| --- | --- | --- |
| `workers` | int | Number of queue-worker processes (`0` = off; floor when autoscaling). |
| `workers_max` | int | Autoscaling ceiling. When `> workers`, the pool scales on backlog (Horizon `balance=auto`, no extra daemon). Defaults to `workers`. |
| `script` | path | Queue runner script (e.g. `examples/askr-queue.php`). |
| `slots` | int | Shared-memory job queue slots (`0` = off; 32 KB each) — `askr_queue_*` + the `AskrQueue` driver. See [Cache](CACHE.md). |

### `[scheduler]`

Run the scheduler (built-in cron) in the same binary.

| Key | Type | Meaning |
| --- | --- | --- |
| `script` | path | Scheduler runner script (e.g. `examples/askr-scheduler.php`). Omit to disable. |

### `[[sidecar]]`

Supervise arbitrary external commands (array of tables; respawned if they die).
Run via `sh -c` in `$ASKR_APP_BASE`. Used for e.g. Inertia SSR — see [Docker](DOCKER.md).

```toml
[[sidecar]]
command = "node bootstrap/ssr/ssr.mjs"
```

### `[cache]`

Enable the shared-memory cache (`askr_cache_*`, and the Laravel driver). See
[Cache](CACHE.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `slots` | int | Small kv cache slots (`0` = disabled). ~4.3 KB each — counters, locks, small values. |
| `large_slots` | int | Large-value region slots (`0` = off). 64 KB each — Laravel sessions, cached fragments/collections. |
| `response_slots` | int | Response cache slots (`0` = off). ~140 KB each — full-response edge cache with tag invalidation. |

### `[broadcast]`

Enable `askr_broadcast()` and the SSE endpoint. See [Broadcasting](BROADCAST.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `enabled` | bool | Turn on the broadcast ring + `GET /askr/events`. |

### `[pusher]`

Pusher-compatible WebSocket + HTTP trigger (drop-in Reverb). Auto-enables the
broadcast ring.

| Key | Type | Meaning |
| --- | --- | --- |
| `enabled` | bool | Turn on the WS endpoint `/app/{key}` + trigger `/apps/{id}/events`. |
| `secret` | string | App secret to verify private/presence subscription auth (omit = accept, dev). |

### `[record]`

| Key | Type | Meaning |
| --- | --- | --- |
| `dir` | path | Record failing (5xx) requests here for `askr replay`. Captures bodies — sensitive. |

### `[reload]`

| Key | Type | Meaning |
| --- | --- | --- |
| `canary` | bool | Canary reload: roll one worker and health-check it before rolling the rest. |

### Example

```toml
[server]
listen = "0.0.0.0:8000"
root = "/var/www/app/public"
workers = "auto"
max_requests = 1000
max_body_size = "16M"

[worker]
script = "/opt/askr/examples/laravel-worker.php"
app_base = "/var/www/app"
ini = "zend_extension=/opt/askr/vendor/php-build/install/lib/php/extensions/no-debug-non-zts-20240924/opcache.so\nopcache.enable=1\nopcache.validate_timestamps=0"

[tls]
cert = "/etc/askr/cert.pem"
key = "/etc/askr/key.pem"

[admin]
listen = "127.0.0.1:9000"
```

## Environment variables

| Variable | Meaning |
| --- | --- |
| `ASKR_PHP_INI` | Extra php.ini lines, appended to the engine defaults. Overridden by `--ini` / `[worker] ini`. Commonly used to load opcache. |
| `ASKR_APP_BASE` | Application base path for the worker script (set automatically from `[worker] app_base`, or export it yourself in flag mode). |
| `ASKR_PHP_CONFIG` | Path to a `php-config` for a specific embed-enabled, non-ZTS PHP install (used at **build** time). |
| `RUST_LOG` | Log filter, e.g. `askr=debug`. Default `askr=info`. |
| `ASKR_CACHE_DB` / `ASKR_QUEUE_DB` / `ASKR_BROADCAST_DB` | Durable L2 backend paths (`--features sql-backend`; unset = L1 shared memory). See [Storage backends](STORAGE_BACKEND.md). |
| `ASKR_OBSERV_DSN` (+ `ASKR_OBSERV_SERVICE`/`HOST`/`BATCH`/`FLUSH_MS`/`QUEUE`) | Ship per-request logs to a MySQL-wire database (`--features observ`). See [Observability](OBSERVABILITY.md). |

### opcache

PHP 8.5 compiles OPcache into libphp and auto-registers it, so there is **no
`zend_extension` line** — just enable it (and JIT) in the INI:

```toml
[worker]
ini = "opcache.enable=1\nopcache.enable_cli=1\nopcache.validate_timestamps=0\nopcache.jit=tracing\nopcache.jit_buffer_size=128M"
```

`validate_timestamps=0` maximises throughput (no stat() per file); pair it with
a `SIGHUP` reload on deploy so fresh workers recompile the new code. `opcache.jit`
enables the JIT (on by default in this build). `askr-run.sh` sets sensible
defaults automatically.
