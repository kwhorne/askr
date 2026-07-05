# The io_uring core (plan)

> Status: **planned**, Linux-only. This document is the design and de-risking
> plan so the implementation on Linux is a focused, low-risk step rather than a
> speculative rewrite. It is deliberately written *before* the code, because
> io_uring cannot be built or run on the macOS dev box — it is a Linux kernel
> interface.

## Why

Askr's thesis was "boot the app once." That collapsed the ~110 ms per-request
framework bootstrap to ~9 ms and gave ~9× the FPM model. With bootstrap gone,
the remaining cost on the hot path is **the I/O layer itself**: accepting
connections, TLS, and moving bytes.

Today that layer is tokio + hyper on epoll — a pragmatic, correct default. The
last big efficiency step is a per-core **io_uring** I/O core: fewer syscalls
(batched submission/completion), no readiness→read two-step, and true
thread-per-core with no cross-core work stealing. This is the piece that turns
"the most efficient PHP server" from a claim into a measured number.

It is Linux-only, so it must be **additive**: the epoll/tokio path stays the
default and the fallback (macOS dev, older kernels, io_uring disabled by sysctl).

## The seam

The whole design has always pointed at one seam:

```
crate::php::Php::handle(req) -> Response      // interpreter, unchanged
```

Everything above it — accept loop, TLS, HTTP framing, the request→CGI mapping —
is replaceable I/O plumbing. The io_uring core swaps *only* that plumbing; PHP,
the shared-memory substrate, the response cache, broadcasting, and the supervisor
are untouched. The share-nothing model is already in place: one worker **process**
per core, each with its own interpreter, all accepting on a shared socket.
io_uring makes each of those processes' I/O loop completion-based.

## Options considered

| Approach | Notes |
| --- | --- |
| **`tokio-uring`** | io_uring on top of tokio's structure; completion-based `TcpStream`. Familiar runtime, but its buffered/owned-buffer API doesn't implement hyper's poll-based `AsyncRead`/`AsyncWrite`, so hyper needs a buffering adapter. |
| **`monoio`** | Thread-per-core io_uring runtime (from the ByteDance folks) with a `monoio-compat` layer for poll-based I/O and an HTTP stack. Closest to the endgame model; means running hyper under a compat shim or using monoio's HTTP. |
| **raw `io-uring`** | Maximum control, maximum surface to get wrong. Not worth it before we have a benchmark proving the win. |

**Plan:** start with **monoio** (thread-per-core is the actual goal), behind a
Cargo feature and a Linux+capability gate, feeding the existing `Php::handle`
seam. Keep hyper/tokio as the default path.

## Gating & fallback

- Cargo: `io_uring` feature, and io_uring deps only under
  `[target.'cfg(target_os = "linux")'.dependencies]` so macOS builds are
  unaffected.
- Runtime: select the backend at startup —
  1. `--io-backend io_uring` requested, **and**
  2. `target_os = "linux"`, **and**
  3. the `io_uring_setup(2)` probe succeeds (`askr doctor` already reports this).

  Otherwise fall back to the tokio/epoll path with a log line. No configuration
  should ever *fail* because io_uring is missing.

## Phased implementation (on Linux)

1. **Probe** — `askr doctor` verifies io_uring at runtime (done: real
   `io_uring_setup` probe, not just a kernel-version guess).
2. **Backend trait** — extract the per-worker accept/serve loop behind a small
   trait with the tokio implementation as `TokioBackend` (pure refactor, no
   behaviour change; verifiable on macOS).
3. **monoio backend** — `UringBackend` (Linux, feature-gated) that runs the
   accept loop and connection I/O on io_uring, calling the same `Php::handle`.
   HTTP/1 first; TLS and HTTP/2 after.
4. **Benchmark** — `scripts/bench.sh` on identical hardware/app: tokio vs
   io_uring, and vs FrankenPHP and nginx+php-fpm. Gate the default flip on a
   real, reproducible win.

## Benchmark methodology

Use `scripts/bench.sh` (auto-detects `oha`/`wrk`/`hey`/`ab`). Measure the same
app and route across:

- Askr, worker mode, tokio path
- Askr, worker mode, io_uring path
- Askr, worker mode + response cache (the anonymous-traffic ceiling)
- FrankenPHP / Laravel Octane
- nginx + php-fpm

Report requests/sec and latency p50/p99 at a fixed concurrency, plus the admin
`/api/metrics` **PHP-vs-I/O split** — the split is what tells us how much of the
remaining time is I/O (i.e. how much io_uring can even address). If the split is
already 99% PHP / 1% I/O on a given workload, io_uring won't move that workload —
and that's a useful, honest result to publish too.

## Non-goals (for the first cut)

- Replacing the interpreter threading model (non-ZTS → process-per-core stays).
- io_uring for the PHP side (blocking DB/file I/O inside PHP is out of scope).
- Windows / macOS (fallback path only).
