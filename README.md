# Askr

**A share-nothing, thread-per-core PHP application server, in Rust.**

Askr embeds the PHP interpreter in-process (no FastCGI, no FPM pool), serves it
from a memory-safe Rust hot path, and is designed to reach zero per-request
bootstrap via a warm master + copy-on-write fork. It is the server engine behind
[`grove`](https://github.com/wirelabs/grove) — `grove serve` is just Askr in a
dev profile.

See [`docs/PRD.md`](docs/PRD.md) for the full product rationale.

> Status: **M0 — spike (done ✅).** The core assumption holds: real Laravel 12
> renders in-process, no FastCGI.

---

## Headline result

A standalone `askr serve` binary serving a real Laravel 12 + Livewire app over
HTTP — **HTTP 200**, encrypted cookies, session, CSRF, Blade, Livewire — with
the PHP running entirely in-process. No FastCGI, no FPM, no nginx.

```
$ askr serve --root ~/code/app/public --listen 127.0.0.1:8000
 INFO askr::php: embedded PHP ready version=8.4.11
 INFO askr::server: askr serving listen=127.0.0.1:8000 docroot=.../public

$ curl -sI http://127.0.0.1:8000/
HTTP/1.1 200 OK
x-powered-by: PHP/8.4.11
set-cookie: XSRF-TOKEN=...; secure; samesite=lax
set-cookie: laravel_session=...; secure; httponly; samesite=lax
```

In **worker mode** the app boots once and every request reuses it — real
Laravel 12 per-request latency drops from ~110 ms to **~9 ms**, and throughput on
8 workers goes from 37 to **347 req/s (9.4×)**, verified correct under load. See
[Worker mode](#askr--the-standalone-server-a1-) below.

Raw per-request overhead of the embedding layer is negligible — a trivial
`index.php` runs at **~56,000 req/s on a single core / single interpreter**
(~0.02 ms/req warm). A full Laravel request is ~110 ms, and that cost is
*entirely Laravel's per-request bootstrap* — precisely what the M2 warm-master +
CoW-fork model exists to eliminate. The bench makes the thesis concrete.

```
cargo run --release -p askr-php --example bench -- <public_dir> [n] [uri]
```

---

## M0 — the embedding spike (done ✅)

The entire project hinges on one question: *can we run PHP in-process from Rust,
cheaply, and capture its output?* The answer is yes.

`crates/askr-php` boots PHP's **embed SAPI** via FFI, evaluates PHP, and captures
everything the script writes back into a Rust `String` — the exact seam that today
costs grove a FastCGI round-trip (`grove-proxy::serve_php`).

```
$ cargo run -p askr-php --example hello
embedded PHP version: 8.4.11
hello from PHP 8.4.11
{
    "stack": "TALL",
    "server": "askr",
    "n": 55
}
[ok=true status=0]
```

Key properties validated by the spike:

- **In-process, no FastCGI.** The Zend engine runs inside the Rust process.
- **non-ZTS** (`thread safety ... no`) — one interpreter per thread; memory is to
  be shared later via CoW fork, not ZTS/TSRM (PRD §6.1).
- **Full request contract**, not just eval: `$_SERVER` injection, request body
  via `php://input`, and captured HTTP **status + headers + body** — the exact
  seam grove's `serve_php()` pays a FastCGI round-trip for today.
- **Real frameworks run:** Laravel 12 + Livewire boots, routes, runs the
  middleware pipeline (encryption/session/CSRF), compiles Blade and renders.
- **Memory-safe boundary:** all `unsafe` is confined to the thin FFI in
  `askr-php`; the C shim (`csrc/shim.c`) is the only C we own.

### The extension matrix (PRD §6.5), discovered empirically

Booting a real app surfaced exactly which extensions Laravel needs, in order —
each one built from source as a **static lib**, fully self-contained (no brew,
no pkg-config):

| Blocker hit | Extension | How it's satisfied |
| --- | --- | --- |
| `mb_split()` undefined | mbstring (mbregex) | oniguruma, static |
| `openssl_cipher_iv_length()` | openssl | OpenSSL 3.3, static |
| `Class "DOMDocument" not found` | dom/xml | libxml2 2.13, static (SDK's is too old) |
| + pdo_sqlite, tokenizer, session, bcmath, … | bundled | `--enable-*` |

### Reproduce from scratch

Requirements: Rust, a C toolchain (Xcode CLT / build-essential). No brew, no
autoconf/re2c/pkg-config needed — release tarballs ship a ready `configure`.

```bash
# 1a. Minimal libphp (core only, ~15s) — enough for the hello/test spike
./scripts/build-libphp.sh

# 1b. …or the full Laravel profile (builds oniguruma+openssl+libxml2 statically)
PROFILE=laravel ./scripts/build-libphp.sh

# 2. Build + run
cargo run -p askr-php --example hello        # hello world in-process
cargo test  -p askr-php                       # eval + full request contract
cargo run   -p askr-php --example exts        # list loaded extensions
cargo run   -p askr-php --example serve -- <public_dir> /   # serve a real app

# opcache (a zend_extension) is loaded via $ASKR_PHP_INI, e.g.:
export ASKR_PHP_INI=$'zend_extension=/abs/path/opcache.so\nopcache.enable=1\nopcache.enable_cli=1'
```

To point at a different PHP install, set `ASKR_PHP_CONFIG=/path/to/php-config`
(the install must be built with `--enable-embed` and non-ZTS).

---

## Askr — the standalone server (A1 ✅)

Askr is a **standalone production PHP application server**, not a dev tool
(that's [`grove`](https://github.com/wirelabs/grove), which stays separate). The
ambition: the smartest, most efficient way to run PHP at scale.

Because the interpreter is non-ZTS, one process = one interpreter. Scaling
across cores is therefore **process-per-core (fork)**, not threads — which *is*
the share-nothing model: a warm master forks one worker per core, each with its
own interpreter and a CoW-shared heap.

| Step | Goal | Status |
| --- | --- | --- |
| **A1** | `askr serve` runs a real app over HTTP (one process/interpreter) | ✅ |
| **A3** | Multi-core: fork one worker process per core, shared listener | ✅ |
| **A4a** | Persistent worker loop: boot the app once, serve many (in-process) | ✅ |
| **A4b** | Real Laravel 12 through the worker loop — zero per-request bootstrap | ✅ |
| A5 | `askr-laravel`: production-grade state reset; TLS, HTTP/2·3, graceful reload | next |
| A2 | Prod-grade static serving + `$_SERVER`/body/header edge cases | |

```
askr serve --root ./public --listen 0.0.0.0:8000 --workers 8 [--https]
```

**Scaling model.** non-ZTS ⇒ one interpreter per process, so Askr scales by
*processes*, not threads. The master binds one listening socket and forks N
workers that all `accept()` on the inherited fd (classic prefork). This
distributes load on Linux *and* macOS — unlike `SO_REUSEPORT`, whose kernel
balancing is Linux-only. Measured on a heavy Livewire app (client-bound
load-gen on the same box):

| workers | req/s | speedup |
| --- | --- | --- |
| 1 | 8.8 | 1.0× |
| 4 | 23.3 | 2.6× |
| 8 | 37.0 | 4.2× |

tokio/hyper is the pragmatic I/O layer; the share-nothing endgame swaps it for a
per-core io_uring loop behind the same seam (`Php::handle`).

**Worker mode (A4a) — the big win.** With `--worker-script`, each worker boots
the application *once* and then loops, serving every request against the
already-booted app (the Octane model, entirely in-process — no IPC). A registered
PHP function `askr_handle_request($handler)` blocks until Rust delivers a
request, runs the handler against the warm app, and ships the captured
status/headers/body back. Same app, an 8 ms boot, 4 workers:

| mode | req/s |
| --- | --- |
| per-request (boots every request) | 346 |
| worker (boots once) | 1024 (**3×**) |

**Real Laravel 12, in worker mode (A4b).** A real Livewire app
(`examples/laravel-worker.php` boots it once and loops). Instead of refreshing
PHP superglobals between requests, the worker builds a fresh
`Illuminate\Http\Request` from the data Askr hands it — clean, no Zend surgery.

Warm per-request latency collapses as the framework bootstrap disappears:

| request | latency |
| --- | --- |
| #1 (cold boot) | 303 ms |
| #2 (warm) | 9.9 ms |
| #3 (warm) | 8.9 ms |

Throughput, 8 workers, real Laravel 12:

| mode | req/s | ms/req |
| --- | --- | --- |
| per-request (the FPM model) | 37 | 26.9 |
| **worker (boot once)** | **347** | **2.9** |

**9.4× throughput**, and verified correct under load: 300/300 requests `200`,
each worker booted exactly once, zero application errors — no state bleed.

```
ASKR_APP_BASE=/path/to/app askr serve \
  --root /path/to/app/public \
  --worker-script examples/laravel-worker.php --workers 8 --https
```

Production-grade state reset between requests (full Octane-level: scoped
instances, rebound singletons, auth/session isolation across every flow) is the
`askr-laravel` package — that's A5.

## Layout

```
crates/
  askr/              the standalone server binary (A1 + A3)
    src/main.rs      CLI + fork-per-core supervisor (signal forwarding, reaping)
    src/worker.rs    one worker: shared listener + runtime + interpreter
    src/php.rs       interpreter on a dedicated thread; per-request + worker modes
    src/cgi.rs       HTTP request -> CGI $_SERVER mapping
    src/server.rs    hyper front: static files + dispatch to PHP
  askr-php/          embedded PHP (embed SAPI) — the M0 spike
    csrc/shim.c      thin C layer: boot Zend, per-request cycle, capture I/O
    build.rs         compiles the shim, links libphp via php-config
    src/lib.rs       safe Rust wrapper (Interpreter / eval / Request / Response)
    examples/
      hello.rs       hello world in-process
      exts.rs        list loaded extensions
      serve.rs       serve a real front controller (index.php) once
      bench.rs       warm-interpreter throughput micro-benchmark
scripts/
  build-libphp.sh    reproducible libphp build (minimal | laravel profile)
vendor/php-build/    (gitignored) downloaded PHP source + static deps + install
```

## Roadmap (from the PRD)

| Phase | Goal |
| --- | --- |
| **M0** | Prove embedding works. ✅ |
| M1 | `grove serve` runs real Laravel 13 on embedded PHP (reuse grove's io/TLS/proxy). |
| M2 | Warm master + CoW fork → zero per-request bootstrap; `askr-laravel` state hooks. |
| M3 | Prod hardening: HTTP/3, extension matrix, graceful reload, WAF, OTel, seccomp/Landlock. |
| M4 | Polish + release. |

## License

MIT © Wirelabs AS
