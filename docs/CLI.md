# CLI reference

```
askr <command> [options]
```

Commands: [`serve`](#askr-serve), [`doctor`](#askr-doctor),
[`config-check`](#askr-config-check). Run `askr <command> --help` for the
built-in help.

Global: `-V`/`--version`, `-h`/`--help`. Logging verbosity is controlled by
`RUST_LOG` (e.g. `RUST_LOG=askr=debug`); the default is `askr=info`.

---

## `askr serve`

Serve a PHP application over HTTP.

```bash
askr serve --root ./public --worker-script examples/laravel-worker.php \
  --workers 8 --tls-self-signed --admin 127.0.0.1:9000
```

You can pass options as flags, **or** put everything in a config file and run
`askr serve --config askr.toml` (the file is then the single source of truth and
the other flags are ignored). See [Configuration](CONFIGURATION.md).

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config <FILE>` | — | Load all settings from `askr.toml`; other flags ignored. |
| `--root <DIR>` | `./public` or `.` | Document root (the app's `public/`). |
| `--front <REL>` | `index.php` | Front controller, relative to `--root`. |
| `--listen <ADDR>` | `127.0.0.1:8000` | Address to bind. |
| `--workers <N>` | CPU cores | Worker processes (each is an independent process with its own interpreter). |
| `--worker-script <FILE>` | — | Enable **worker mode**: boot the app once and serve many. Omit for per-request mode. |
| `--max-requests <N>` | `0` | Recycle each worker after N requests (`0` = never). |
| `--max-body-size <SIZE>` | `16M` | Reject larger request bodies with `413`. Accepts `512K`/`16M`/`2G` or plain bytes. |
| `--https` | off | Mark requests as HTTPS in `$_SERVER` (e.g. behind a TLS terminator). Implied by TLS. |
| `--tls-cert <FILE>` | — | TLS certificate chain (PEM). Requires `--tls-key`. Enables HTTPS + HTTP/2. |
| `--tls-key <FILE>` | — | TLS private key (PEM). Requires `--tls-cert`. |
| `--tls-self-signed` | off | Generate a v3 self-signed cert on startup (dev/testing). Conflicts with `--tls-cert`. |
| `--admin <ADDR>` | — | Admin dashboard/API listen address (e.g. `127.0.0.1:9000`). Off if unset. See [Admin](ADMIN.md). |
| `--ini <LINES>` | `$ASKR_PHP_INI` | Extra php.ini lines (e.g. to load opcache). |

Notes:

- A single process is used only when `--workers 1`, no `--max-requests`, and no
  `--admin`; otherwise the multi-process supervisor runs (needed for recycling,
  reload, and the admin plane).
- TLS certificates must be **X.509 v3** (rustls rejects v1). With `openssl req`,
  add `-addext "subjectAltName=DNS:example.com"` to produce a v3 cert, or use
  `--tls-self-signed`.

### Signals

| Signal | Effect |
| --- | --- |
| `SIGHUP` | Graceful **rolling reload** — restart workers one at a time (new PHP code, zero downtime). |
| `SIGTERM` / `SIGINT` | Graceful shutdown — drain all workers, then exit. |

---

## `askr doctor`

Pre-flight checks before deploying. Exits non-zero if a critical check fails, so
it can gate a deploy.

```bash
askr doctor --ini "zend_extension=/path/opcache.so"$'\n'"opcache.enable=1"
```

Checks:

- embedded PHP version,
- **non-ZTS** build (required),
- every Laravel-required extension is present,
- on Linux, the kernel supports **io_uring** (≥ 5.1).

```
askr doctor
  ✓ embedded PHP 8.4.11
  ✓ thread safety: non-ZTS (NTS)
  ✓ ext-ctype … ext-openssl … ext-dom   (all required)
  · 30 extensions loaded
  platform: linux
  ✓ kernel 6.x (io_uring needs ≥ 5.1)
  ✓ all critical checks passed
```

---

## `askr config-check`

Validate a config file and print the resolved settings, without starting the
server. Useful in CI/CD and before a deploy.

```bash
askr config-check askr.toml
```

```
✓ config OK: askr.toml
  listen:        0.0.0.0:8000
  root:          /var/www/app/public
  front:         index.php
  workers:       8
  mode:          worker (boot once)
  worker script: /opt/askr/examples/laravel-worker.php
  max_requests:  1000
  max_body_size: 16777216 bytes
  tls:           self-signed
  admin:         127.0.0.1:9000
```

It fails (non-zero) if the listen address is invalid, the document root or front
controller is missing, the worker script or app base doesn't exist, or TLS
cert/key paths are wrong.
