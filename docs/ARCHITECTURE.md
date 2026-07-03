# Architecture

Askr embeds PHP in-process and serves it from a memory-safe Rust hot path. This
document explains the moving parts and the reasoning behind them.

## The big picture

```
                      askr (master process)
        binds one listening socket · supervises workers · admin API
                               │  fork()  (one per core)
        ┌───────────────┬──────┴────────┬───────────────┐
   ┌────▼─────┐    ┌────▼─────┐    ┌────▼─────┐    ┌────▼─────┐
   │ worker 0 │    │ worker 1 │    │ worker 2 │    │ worker N │
   │ tokio I/O│    │ tokio I/O│    │ tokio I/O│    │ tokio I/O│
   │ + 1 PHP  │    │ + 1 PHP  │    │ + 1 PHP  │    │ + 1 PHP  │
   │ interp.  │    │ interp.  │    │ interp.  │    │ interp.  │
   └──────────┘    └──────────┘    └──────────┘    └──────────┘
        └───────────────┴───────────────┴───────────────┘
                 all accept() on the shared socket
                   (the kernel balances connections)
```

Each worker is a fully independent process: its own PHP interpreter, its own
I/O, no shared mutable state, no locks on the hot path. This is the
share-nothing model — scaled by **processes**, for the reason below.

## Why processes, not threads

Askr runs a **non-ZTS** (non-thread-safe) build of PHP. This is a deliberate
choice: the ZTS/TSRM machinery has a persistent runtime
cost and is a source of complexity, and the memory sharing it exists to provide
we get from the OS for free.

The consequence is architectural: a non-ZTS interpreter is a **per-process
singleton** — you cannot run two interpreters in one process. So Askr scales
across cores by running one worker **process** per core, not one thread per core.
That *is* the share-nothing / thread-per-core philosophy, realised with
processes.

In Rust terms, `askr_php::Interpreter` is `!Send`/`!Sync`: it lives and dies on
the one thread that created it.

## Embedding PHP

`askr-php` links PHP's **embed SAPI** (`libphp`) via FFI. A thin C shim
(`crates/askr-php/csrc/shim.c`) is the only C we own; it:

- boots the Zend engine once (module startup),
- overrides the SAPI `ub_write` hook to capture output into a buffer,
- overrides `send_header` to capture response headers,
- feeds the request body to `php://input` via `read_post`,
- injects `$_SERVER` via `register_server_variables`.

All `unsafe` is confined to this FFI boundary; the rest of the server is safe
Rust. PHP is the single `unsafe` frontier (and the target for future
seccomp/Landlock sandboxing).

## Two serving modes

### Per-request mode (default)

Each request runs the front controller (`index.php`) from scratch — a full
framework bootstrap every time, exactly like PHP-FPM. Correct and simple, but
pays the bootstrap cost (~110 ms for a typical Laravel app) on every request.

### Worker mode (`--worker-script`)

A long-lived worker script boots the application **once** and then loops,
serving every request against the already-booted app — the Octane model, but
entirely in-process (no IPC). The shim registers a PHP function
`askr_handle_request($handler)`:

```php
$app = /* boot once */;
while (askr_handle_request(function (array $request) use ($app) {
    // handle $request against the warm $app; echo output / header()
})) {}
```

`askr_handle_request` blocks until Rust delivers a request, runs the handler,
and ships the captured status/headers/body back. On a real Laravel app this
drops per-request latency from ~110 ms to ~9 ms. See [Worker mode](WORKER_MODE.md).

## Request lifecycle

```
client → tokio accept → [TLS handshake] → hyper (HTTP/1.1 or HTTP/2)
      → build CGI $_SERVER (cgi.rs) + read body (bounded)
      → hand to the PHP interpreter thread over a channel
      → PHP runs (per-request front controller, or the warm worker)
      → capture status + headers + body
      → hyper writes the response
```

The interpreter runs on a **dedicated OS thread** per worker process (because
it's `!Send`); tokio owns the sockets and hands requests to it over a channel.
One interpreter serves one request at a time — like an FPM worker — and
cross-core concurrency comes from having many worker processes.

The seam between the I/O layer and PHP is `Php::handle`. The tokio/hyper I/O
layer is pragmatic; the share-nothing endgame swaps it for a per-core
**io_uring** loop behind this same seam — the biggest remaining
efficiency step, and Linux-only.

## The master (supervisor)

The master process:

- binds **one** listening socket (SO_REUSEADDR) and `fork()`s the workers, which
  inherit the socket fd and all `accept()` on it (classic prefork). This
  distributes load on Linux *and* macOS, unlike `SO_REUSEPORT` whose kernel
  balancing is Linux-only.
- **reaps** exited workers and respawns replacements → recycling + crash
  resilience.
- handles signals: `SIGINT`/`SIGTERM` drain and shut down; `SIGHUP` triggers a
  graceful **rolling reload**.
- optionally runs the **admin** server (status + reload) in its own thread.

### Graceful recycling & rolling reload

A worker recycles after `--max-requests` (staggered per worker so they don't all
recycle at once): it stops accepting, drains in-flight requests, exits, and the
master respawns a fresh one.

`SIGHUP` restarts workers **one at a time**, waiting for each fresh worker to
boot before rolling the next, so there are always live workers accepting and the
listen socket stays open throughout — new PHP code with no downtime. See
[Deployment](DEPLOYMENT.md).

## Static files & TLS

Rust serves existing static files directly (a `try_files`-style check against the
document root) and only invokes PHP for dynamic routes. TLS is terminated by
rustls (ring provider — no OpenSSL, no C toolchain) with ALPN negotiating
HTTP/2 or HTTP/1.1, so Askr is a complete single binary with no proxy in front.

## Crate layout

| Crate | Responsibility |
| --- | --- |
| `askr-php` | Embeds PHP (embed SAPI) via FFI; the C shim, the safe `Interpreter`/`Request`/`Response` API, and the worker-loop bridge. |
| `askr` | The server binary: CLI, config, the master/supervisor, per-worker runtime, HTTP front (TLS, static, dispatch), the admin plane, and `doctor`. |

```
crates/askr/src/
  main.rs      CLI + master/supervisor (fork, signals, reaping, reload)
  worker.rs    one worker: shared listener + tokio runtime + interpreter
  php.rs       interpreter pinned to a thread; per-request + worker modes
  server.rs    hyper front: TLS, HTTP/1.1+2, static files, dispatch, drain
  cgi.rs       HTTP request → CGI $_SERVER mapping
  tls.rs       rustls acceptor (PEM or self-signed)
  config.rs    typed askr.toml + validation
  admin.rs     admin dashboard + status/reload API
  doctor.rs    pre-flight checks
```
