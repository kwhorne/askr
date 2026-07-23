# Stability & compatibility

This document defines what Askr promises not to break, and how changes are rolled
out. It is the compatibility contract you can build tooling, deployments, and apps
against.

## Status: pre-1.0

Askr is currently in the `0.x` series. Per SemVer, **`0.x` makes no stability
promise** — a minor bump (`0.9` → `0.10`) may change any of the surfaces below.
In practice we already treat the surfaces as stable and avoid gratuitous breakage,
but the hard guarantee below begins at **1.0**.

The list here *is* the freeze list: it's what 1.0 will lock down. If something you
depend on isn't listed, ask — it either belongs here or is intentionally internal.

## What SemVer covers (from 1.0)

Within a major version (`1.x`), the following are **stable**. A breaking change to
any of them requires a major bump (`2.0`) and a deprecation period (below).

### 1. CLI — subcommands and flags

- Subcommands: `serve`, `test`, `replay`, `doctor`, `config-check`, `upgrade`,
  `status`.
- Every documented `--flag` on those commands: its name, whether it takes a value,
  and its meaning. Default values may be tuned across minor versions when they are
  purely performance defaults (e.g. the auto worker count) — never when a default
  changes behaviour you'd notice.
- Flags print in `askr <cmd> --help`; that output is the source of truth. See
  [CLI.md](CLI.md).

*Not* covered: the internal sidecar subcommands the supervisor spawns for itself
(`web`, `queue`, `scheduler`, `command`). These are an implementation detail of the
master↔worker protocol and may change at any time — don't invoke them directly.

### 2. Configuration file (`askr.toml`)

- Every documented key under `[server]`, `[cache]`, `[tls]`, `[acme]`, `[sidecars]`,
  etc.: its name, type, and meaning. See [CONFIGURATION.md](CONFIGURATION.md).
- Unknown keys are ignored, not rejected — so a config written for a newer Askr
  still loads on an older one (forward-compatible), and vice-versa.

### 3. Environment variables

The `ASKR_*` variables that select runtime backends and telemetry:

| Variable | Purpose |
| --- | --- |
| `ASKR_APP_BASE` | App root passed to the runner scripts. |
| `ASKR_PHP_INI` | Extra `php.ini` path. |
| `ASKR_PUSHER_SECRET` | Pusher/Reverb auth secret. |
| `ASKR_ADMIN_TOKEN` | Bearer token required for the admin reload + data endpoints (unset = open). |
| `ASKR_CACHE_DB` / `ASKR_QUEUE_DB` / `ASKR_BROADCAST_DB` | Select the durable **L2** backend (feature `sql-backend`). |
| `ASKR_OBSERV_DSN` and `ASKR_OBSERV_{SERVICE,HOST,BATCH,FLUSH_MS,QUEUE,METRICS_MS,TLS}` | Observability sink (feature `observ`). |
| `ASKR_OTEL_ENDPOINT` / `ASKR_OTEL_SERVICE` | OpenTelemetry trace export (feature `otel`). |

When an env var and a flag/config key set the same thing, precedence is documented
per option and is itself stable.

### 4. The PHP bridge (`askr_*` functions)

The native functions Askr injects into PHP are a stable ABI for app and package
authors. Their names and call signatures won't change within a major version:

| Function | Purpose |
| --- | --- |
| `askr_handle_request($handler)` | Worker request loop (Octane-style). |
| `askr_defer($callable)` | Run work after the response is sent. |
| `askr_cow_ready()` | Signal the CoW template is warm. |
| `askr_cache_get/set/add/delete/increment/flush/touch/forget_tag(...)` | Shared cache. |
| `askr_queue_push/pop/delete/release/size(...)` | Shared job queue. |
| `askr_broadcast($channel, $payload)` | Publish a broadcast event. |

New functions may be **added** in a minor version; existing ones won't change or
disappear without a deprecation cycle. The official
[`kwhorne/askr-laravel`](LARAVEL.md) package wraps these — depend on it rather than
calling the bridge directly if you want the smoothest ride.

### 5. Reserved HTTP surface

- Endpoints Askr serves itself: `GET /askr/events` (SSE), `/app/{key}` (Pusher
  WebSocket), `POST /apps/{id}/events` (Pusher trigger), and the admin `/metrics`
  (Prometheus text). Paths under `/askr/` are reserved for Askr — don't route them
  in your app.
- Response contract headers: the app-supplied **`Askr-Cache`** request-to-cache
  header (consumed, never forwarded) and the **`X-Askr-Cache: HIT|MISS|STALE`**
  diagnostic header. When HTTP/3 is on, TCP responses carry **`Alt-Svc: h3=…`**.

### 6. Build features

The optional Cargo features are part of the surface: `sql-backend`, `observ`,
`otel`, `http3`. A feature won't be removed or have its meaning changed within a
major version; the default build stays feature-free. The published `-full`
artifact compiles them all in.

## What is explicitly *not* stable

These may change in any release, including patch releases. Don't build automation
against them:

- The internal Rust crates (`askr`, `askr-php`) as a library API. Askr is a binary,
  not a published crate.
- The on-disk / in-memory **shared-memory layout** (cache, queue, broadcast, metrics
  regions). It's private to a single `askr` process tree and is not a wire format —
  never share it between different Askr versions.
- Exact **log line wording** and human-readable `tracing` output. The *structured*
  access-log JSON fields are stable; prose is not.
- The `/var/lib/askr` (or `--dir`) internal layout used by `askr upgrade`.
- Timing, ordering, and internal span *structure* of traces (span **names** and
  documented **attributes** are stable; their nesting is best-effort).

## Deprecation policy (from 1.0)

When we need to change a stable surface:

1. **Add**, don't mutate. The new name/flag/key/function ships first and works.
2. **Alias + warn.** The old name keeps working as a hidden alias for at least one
   full minor version, emitting a one-line deprecation warning on use.
3. **Remove** only at the next major version, and only if it was warning for a full
   minor series.

Example already in place: `--acme-directory` was renamed to the clearer
`--acme-directory-url` (to stop it being confused with `--acme-dir`, the local
cert-cache directory). The old spelling still works as an alias.

## Reporting a break

If an upgrade within a major version breaks one of the stable surfaces above, that's
a bug — please open an issue with the before/after and the version pair. Pre-1.0,
breaking changes are called out in [CHANGELOG.md](../CHANGELOG.md).
