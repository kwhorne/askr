# CLI reference

```
askr <command> [options]
```

Commands: [`serve`](#askr-serve), [`test`](#askr-test), [`replay`](#askr-replay),
[`doctor`](#askr-doctor), [`config-check`](#askr-config-check),
[`upgrade`](#askr-upgrade). Run `askr <command> --help` for the built-in help.

Global: `-V`/`--version`, `-h`/`--help`. Logging verbosity is `RUST_LOG` (e.g.
`RUST_LOG=askr=debug`); default `askr=info`.

---

## `askr serve`

Serve a PHP application over HTTP(S).

```bash
askr serve --root ./public --worker-script examples/laravel-worker.php \
  --workers 8 --tls-self-signed --admin 127.0.0.1:9000
```

Pass options as flags, **or** put everything in a config file and run
`askr serve --config askr.toml` (the file is then the single source of truth and
the other flags are ignored). See [Configuration](CONFIGURATION.md).

### Core

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config <FILE>` | — | Load all settings from `askr.toml` (other flags ignored). |
| `--root <DIR>` | `./public` | Document root. |
| `--front <FILE>` | `index.php` | Front controller, relative to root. |
| `--listen <ADDR>` | `127.0.0.1:8000` | Address to bind. |
| `--https` | off | Mark requests as HTTPS in `$_SERVER` (behind a TLS terminator). |
| `--ini <LINES>` | — | Extra `php.ini` lines (e.g. opcache). Overrides `$ASKR_PHP_INI`. |
| `--max-body-size <SIZE>` | `16M` | Reject larger bodies with `413` (`K`/`M`/`G`). |

### Workers & scaling

| Flag | Default | Meaning |
| --- | --- | --- |
| `--workers <N>` | CPU cores (cgroup-aware) | Worker processes (one interpreter each). |
| `--worker-script <FILE>` | — | Boot the app once, serve many (Octane model). Omit for per-request. |
| `--max-requests <N>` | `0` | Recycle each worker after N requests (`0` = never). |
| `--max-rss <MB>` | `0` | Recycle a worker gracefully once its RSS exceeds N MB (`0` = never). Leak-aware: drains before PHP hits `memory_limit` and OOMs. Linux only. |
| `--workers-min <N>` | `--workers` | CoW autoscaling floor. |
| `--workers-max <N>` | `--workers` | CoW autoscaling ceiling (> min enables autoscaling). |
| `--cow` | off | CoW template: boot once, fork warm workers (~ms respawn). Needs `--worker-script`. |
| `--paranoid` | off | Dev: detect state bleed between requests (worker mode; expensive). |

### TLS

| Flag | Default | Meaning |
| --- | --- | --- |
| `--tls-cert <PEM>` / `--tls-key <PEM>` | — | Serve HTTPS from a cert + key (ALPN h2/http1.1). |
| `--tls-self-signed` | off | Generate a self-signed cert on startup (dev). |
| `--acme` | off | Auto-TLS via ACME/Let's Encrypt (HTTP-01). See [AUTOTLS](AUTOTLS.md). |
| `--acme-domain <D>` | — | Domain(s) (repeatable). Required with `--acme`. |
| `--acme-email <E>` | — | ACME account contact. |
| `--acme-dir <DIR>` | `/var/lib/askr/acme` | Account + cert cache. |
| `--acme-staging` | off | Let's Encrypt staging. |
| `--acme-http <ADDR>` | `0.0.0.0:80` | Where to answer HTTP-01 challenges. |
| `--acme-directory <URL>` / `--acme-ca-root <PEM>` | — | Custom ACME directory / CA (Pebble, private CA). |

### Sidecars (same process tree)

| Flag | Default | Meaning |
| --- | --- | --- |
| `--queue <N>` + `--queue-script <FILE>` | `0` | Supervised queue-worker processes. |
| `--scheduler-script <FILE>` | — | Run the built-in scheduler (cron). |
| `--sidecar "<cmd>"` | — | Supervise an arbitrary command (repeatable), e.g. Inertia SSR. |

### In-binary services (no Redis/Reverb)

| Flag | Default | Meaning |
| --- | --- | --- |
| `--cache-slots <N>` | `0` | Shared kv cache (`askr_cache_*`; ~4.3 KB/slot). |
| `--cache-large-slots <N>` | `0` | Large-value region (64 KB/slot) — sessions, fragments. |
| `--response-cache <N>` | `0` | Full-response cache + tag invalidation (~140 KB/slot). See [CACHE](CACHE.md). |
| `--queue-slots <N>` | `0` | Shared-memory job queue (`askr_queue_*`; 32 KB/slot). |
| `--broadcast` | off | `askr_broadcast()` + SSE at `/askr/events`. See [BROADCAST](BROADCAST.md). |
| `--pusher` | off | Pusher-compatible WebSocket + trigger (drop-in Reverb; auto-enables broadcast). |
| `--pusher-secret <S>` | `$ASKR_PUSHER_SECRET` | Verify private/presence subscription auth. |

### Operations & hardening

| Flag | Default | Meaning |
| --- | --- | --- |
| `--admin <ADDR>` | off | Admin dashboard/API + Prometheus `/metrics`. Bind to localhost. See [ADMIN](ADMIN.md). |
| `--access-log <PATH\|->` | off | JSON access log per request (`-` = stdout). |
| `--canary` | off | Canary reload: roll one worker + health-check before the rest. |
| `--record-errors <DIR>` | off | Persist 5xx requests for `askr replay`. Sensitive. |
| `--sandbox` | off | Linux hardening: seccomp no-exec. See [SANDBOX](SANDBOX.md). |
| `--sandbox-write <DIR>` | — | Landlock: writes allowed only here (repeatable). |

Signals: **SIGHUP** = graceful rolling reload; **SIGTERM/SIGINT** = graceful drain + shutdown.

---

## `askr test`

Run tests by forking a fresh, warm process per file (boot once, perfect isolation).

```bash
askr test --root . --runner examples/askr-test.php tests/
```

| Flag | Meaning |
| --- | --- |
| `[paths…]` | Test files/dirs (dirs scanned for `*Test.php`); default `./tests`. |
| `--root <DIR>` | App base (`$ASKR_APP_BASE`). |
| `--runner <FILE>` | Runner invoked per file (PHPUnit/Pest); omit to run files directly. |
| `--parallel <N>` | Concurrent files (default CPU cores). |
| `--ini <LINES>` | Extra `php.ini`. |

## `askr replay`

Replay a recorded failing request (see `serve --record-errors`).

```bash
askr replay /var/lib/askr/errors/<id>.json
```

## `askr doctor`

Pre-flight checks: PHP build, extensions (required + recommended), platform
(io_uring probe on Linux). `--ini <LINES>` to load opcache. Exit non-zero on
critical failure.

## `askr config-check`

`askr config-check askr.toml` — validate a config file and print the resolved
settings without starting the server.

## `askr upgrade`

Self-update the release install in place. Downloads the matching Linux tarball
from GitHub, verifies its `sha256`, and swaps the whole prefix (binary + bundled
libphp) atomically — the previous version is kept at `<prefix>/../askr.old` for
rollback. Does **not** restart the running server unless you pass `--restart`.

```bash
askr upgrade --check              # is a newer release available?
sudo askr upgrade                 # install the latest (then restart manually)
sudo askr upgrade --restart       # install + systemctl restart askr
sudo askr upgrade --version 0.8.0 # pin a version (also to roll back)
```

| Flag | Meaning |
| --- | --- |
| `--check` | Report whether a newer release exists; install nothing. |
| `--version <X.Y.Z>` | Install a specific version instead of the latest (rollback). |
| `--restart` | Run `systemctl restart askr` after a successful swap. |

Needs write access to the install directory (run with `sudo` for `/opt/askr`).
Refuses to run **inside a container** — upgrade those by pulling a new
`ghcr.io/kwhorne/askr` tag. Linux release only; elsewhere build from source.
