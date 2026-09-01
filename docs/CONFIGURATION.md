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

> **A config file is the whole configuration.** `--config` is not merged with the other
> flags — it replaces them. Askr refuses to start if you pass both, naming the flags that
> would have been ignored, because the alternative is a server that runs with settings you
> didn't choose and says nothing. Everything a flag can do has a key here.

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
| `tls_handshake_timeout` | int | `10` | Seconds a client may take to finish the TLS handshake (slowloris guard). |
| `header_read_timeout` | int | `15` | Seconds a client may take to send request headers (slowloris guard). |
| `https` | bool | `false` | Force HTTPS in `$_SERVER` (e.g. behind a TLS terminator). Implied by TLS. |
| `force_https` | bool | `false` | Redirect plain HTTP to HTTPS (308), using the connection's TLS state / `https` / `X-Forwarded-Proto`. |
| `http_redirect` | — | Answer plain HTTP here and 308 it to HTTPS, e.g. `"0.0.0.0:80"`. Needs `force_https`. Automatic on the ACME challenge address with `--acme`. |
| `traffic_log` | path | Record one JSON line per PHP-served request for [`askr cache-report`](CLI.md#askr-cache-report). A diagnostic — turn it on for an hour, then off. |
| `trusted_proxies` | list | `[]` | Proxies whose `X-Forwarded-For` may be believed, as IPs or CIDRs (`"10.0.0.0/8"`). Required for correct client identity in [`[[ratelimit]]`](#ratelimit) behind a load balancer. |
| `workers_min` | int | = `workers` | CoW autoscaling floor (with `--cow`). |
| `workers_max` | int | = `workers` | CoW autoscaling ceiling (> min enables autoscaling). |
| `access_log` | path | — | JSON access log per request; `-` for stdout. Off if unset. |
| `http3` | bool | `false` | Serve HTTP/3 (QUIC) on the TLS port (requires TLS; build with `--features http3`). |
| `sandbox` | bool | `false` | Linux hardening: seccomp no-exec. See [Sandbox](SANDBOX.md). |
| `sandbox_write` | path[] | `[]` | Landlock: writes allowed only under these paths (enables the FS restriction). |
| `sandbox_required` | bool | `false` | Refuse to serve if the sandbox doesn't fully apply. Needs `sandbox_write`. |

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

**Auto-TLS (ACME / Let's Encrypt)** has its own section — see [`[acme]`](#acme). Do not
set both: `[tls]` is a certificate you supply, `[acme]` is one Askr fetches.

### `[acme]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Obtain and renew a certificate over HTTP-01. |
| `domains` | list | `[]` | Hostnames to certify. At least one required. Bare hostnames — no scheme, port or wildcard. |
| `email` | string | `admin@<first domain>` | Contact address for the ACME account. |
| `dir` | path | `/var/lib/askr/acme` | Where the account key and certificate are cached. Must survive restarts, or you will hit Let's Encrypt's rate limits. |
| `staging` | bool | `false` | Use Let's Encrypt staging: untrusted certs, far higher limits. **Do this first.** |
| `directory_url` | string | Let's Encrypt | Custom ACME directory (a Pebble test server). Distinct from `dir`. |
| `http` | address | `0.0.0.0:80` | Where HTTP-01 challenges are answered — and, with `force_https`, where plain HTTP is redirected from. |
| `ca_root` | path | – | Extra CA root to trust for the directory. Testing only. |

```toml
[server]
listen = "0.0.0.0:443"
root = "/var/www/example.com/public"
force_https = true
trusted_proxies = ["172.17.0.1"]   # file-only, which is why [acme] had to exist

[acme]
enabled = true
domains = ["example.com", "www.example.com"]
email = "admin@example.com"
dir = "/var/lib/askr/acme"
```

One long-lived listener on `http` answers challenges *and* 308s everything else, so a
certificate can be issued and HTTP redirected without the two fighting over port 80.

Askr refuses to start rather than let a mistake here become a site quietly serving plain
HTTP: `domains` without `enabled`, `enabled` without `domains`, `[acme]` alongside
`[tls]`, and a wildcard domain (HTTP-01 cannot validate one) are all errors.

Until 1.4.10 ACME was flags-only. Since `--config` is the whole configuration rather than a
set of defaults, that made auto-TLS and a config file mutually exclusive — and combinations
like "auto-TLS behind a proxy" unreachable, because `trusted_proxies` has never had a flag.

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

**`slots` is required when `workers` is set**, and Askr refuses to start without it. The
ring is only mapped when slots are configured; without it every push returns 0, Laravel does
not check that, and queued jobs are discarded silently — which is how a live site stopped
sending mail with nothing in any log.

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

### `[[redirect]]`

Declarative host redirects (array of tables), evaluated before any dispatch. `from`
matches the `Host` header exactly or as a `*.suffix` glob; the request path + query
are preserved; `status` defaults to 308 (permanent, method-preserving).

```toml
[[redirect]]
from = "www.domene.no"
to   = "https://domene.no"     # → https://domene.no/<path>?<query>

[[redirect]]
from   = "*.old.no"
to     = "https://ny.no"
status = 301
```

For plain-HTTP→HTTPS across all hosts, use `[server] force_https = true` instead.

### `[[site]]`

Virtual hosts — serve several domains/apps from **one** Askr instance, routed by the
`Host` header. Each site has its own document root + front controller; `hosts` match
exactly or as a `*.suffix` glob. A request whose Host matches no site falls back to
`[server] root`.

```toml
[server]
listen = "0.0.0.0:443"
root   = "/var/www/default/public"   # fallback

[[site]]
hosts = ["domene.no", "*.domene.no"]
root  = "/var/www/domene/public"

[[site]]
hosts = ["annet.no"]
root  = "/var/www/annet/public"
front = "index.php"
```

Static files are served from the matching site's root in any mode. **Full dynamic
dispatch (a different app per host) works in per-request mode** — each request runs
that site's front controller fresh. In **worker mode** the single booted app is
fixed, so give each app its own instance (or route by host inside the worker script)
until per-site worker pools land. Combine with `[[redirect]]` for per-host www→apex.

### `[cache]`

Enable the shared-memory cache (`askr_cache_*`, and the Laravel driver). See
[Cache](CACHE.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `slots` | int | Small kv cache slots (`0` = disabled). ~4.3 KB each — counters, locks, small values. |
| `large_slots` | int | Large-value region slots (`0` = off). 64 KB each — Laravel sessions, cached fragments/collections. |
| `response_slots` | int | Response cache slots (`0` = off). ~140 KB each — full-response edge cache with tag invalidation. |
| `strip_query_params` | list | Query parameters ignored when building the response-cache key. Trailing `*` globs (`"utm_*"`). PHP still receives the full query. |
| `ignore_cookies` | list | Cookies that don't make a request non-cacheable (analytics: `"_ga"`, `"_gid"`, `"_fbp"`). Trailing `*` globs. Default: any cookie defeats caching. |
| `vary_user_agent` | bool | Split the response-cache key on mobile vs desktop `User-Agent` (also sets `Vary: User-Agent`). Default `false`. |
| `persist` | path | Save the response cache here on graceful shutdown and load it at boot, so a restart doesn't start cold. Unset = off. |
| `persist_key` | string | Release identifier. When set, a saved cache only loads if it matches — set it to your release SHA so a deploy can't resurrect pre-deploy HTML. |
| `saint_seconds` | int | Saint mode: seconds to treat PHP as unhealthy after a `5xx`, during which requests holding a `stale-if-error` entry skip PHP entirely. `0` = off (default). |

### `[[ratelimit]]`

Rate limits enforced in the Rust layer before PHP is woken, with token buckets in
shared memory (so a limit spans the whole worker fleet). **First match wins.** See
[Features](FEATURES.md#rate-limiting-before-php-wakes-up).

| Key | Type | Meaning |
| --- | --- | --- |
| `path` | string | Path glob (`*`, `?`), must start with `/`. Globs, not regexes. |
| `limit` | int | Requests allowed per `window`. Must be > 0. |
| `window` | int | Window length in seconds. Default `60`. |
| `by` | string | Identity counted: `ip` (default), `header:<Name>`, `cookie:<name>`. |
| `burst` | int | Extra tokens a bursty client may accumulate on top of `limit`. |

```toml
[[ratelimit]]
path = "/login"
limit = 5
window = 300
```

Refused requests get `429` with `Retry-After`. Reserved `/askr/*` endpoints are exempt.
Set `[server] trusted_proxies` when running behind a load balancer, or `X-Forwarded-For`
is ignored and every client shares one bucket.

#### `[[cache.rule]]`

Per-path cache policy, applied without touching the app. **First match wins** — put
specific rules above the catch-all. See [Features](FEATURES.md#cache-rules--policy-without-touching-the-app).

| Key | Type | Meaning |
| --- | --- | --- |
| `path` | string | Path glob (`*`, `?`), must start with `/`. Globs, not regexes — a regex-shaped pattern is rejected at load. |
| `action` | string | `"pass"` = never cache this path, even if the app sent `Askr-Cache`. Responses carry `X-Askr-Cache: PASS`. |
| `ttl` | int | Fresh seconds. Caches a path the app never opted in to; overrides the app's TTL (the app's tags are kept). |
| `swr` | int | Stale-while-revalidate window, seconds past `ttl`. |
| `stale_if_error` | int | `stale-if-error` window, seconds past `ttl`. |
| `force` | bool | Cache **even when the request carries cookies**. Dangerous on anything user-specific — one visitor's page is then served to everyone. |

```toml
[[cache.rule]]
path = "/admin/*"
action = "pass"

[[cache.rule]]
path = "/static/*"
ttl = 86400
force = true
```

Cache-key normalisation example — tracking parameters and analytics cookies stop
fragmenting the cache:

```toml
[cache]
response_slots = 512
strip_query_params = ["utm_*", "gclid", "fbclid", "_ga"]
ignore_cookies = ["_ga", "_gid", "_fbp"]
vary_user_agent = false
```

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
| `dir` | path | Record failing (5xx) requests here for `askr replay`. Captures bodies — sensitive; created `0700`, files `0600`, credentials redacted. |

### `[reload]`

| Key | Type | Meaning |
| --- | --- | --- |
| `canary` | bool | Canary reload: roll one worker and health-check it before rolling the rest. |
| `canary_window` | int | Seconds to watch the canary. Default `5`. |
| `canary_min_requests` | int | Requests the canary must serve for a verdict; below this the rollout is `inconclusive` and continues with a warning. Default `20`. |
| `canary_max_error_rate` | float | Percentage points of error rate the canary may exceed the fleet by. Default `2.0`. |
| `canary_max_latency_factor` | float | Mean-latency factor vs the fleet. Default `3.0`. |

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
| `ASKR_OTEL_ENDPOINT` (+ `ASKR_OTEL_SERVICE`) | Export OpenTelemetry traces (root `http.request` + child `php.execute`) over OTLP/gRPC (`--features otel`). See [Observability](OBSERVABILITY.md#traces-opentelemetry). |

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
