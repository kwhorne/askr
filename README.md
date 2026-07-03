# Askr

**A share-nothing, thread-per-core PHP application server, in Rust.**

Askr embeds the PHP interpreter in-process (no FastCGI, no FPM pool), serves it
from a memory-safe Rust hot path, and is designed to reach zero per-request
bootstrap via a warm master + copy-on-write fork. It is the server engine behind
[`grove`](https://github.com/wirelabs/grove) — `grove serve` is just Askr in a
dev profile.

See [`docs/PRD.md`](docs/PRD.md) for the full product rationale.

> Status: **M0 — spike.** Proving the core assumption before anything else.

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
- **Output capture** via a SAPI `ub_write` override — the seam `serve_php` needs.
- **Bundled extensions** (json, spl, pcre, hash, …) work; `$_SERVER` is present.
- **Memory-safe boundary:** all `unsafe` is confined to the thin FFI in
  `askr-php`; the C shim (`csrc/shim.c`) is the only C we own.

### Reproduce from scratch

Requirements: Rust, a C toolchain (Xcode CLT / build-essential). No brew, no
autoconf/re2c/pkg-config needed — release tarballs ship a ready `configure`.

```bash
# 1. Build a minimal embed-enabled, non-ZTS libphp (~15s compile)
./scripts/build-libphp.sh

# 2. Build + run the spike
cargo run -p askr-php --example hello
cargo test -p askr-php
```

To point at a different PHP install, set `ASKR_PHP_CONFIG=/path/to/php-config`
(the install must be built with `--enable-embed` and non-ZTS).

---

## Layout

```
crates/
  askr-php/        embedded PHP (embed SAPI) — the M0 spike
    csrc/shim.c    thin C layer: boot Zend, capture output
    build.rs       compiles the shim, links libphp via php-config
    src/lib.rs     safe Rust wrapper (Interpreter / eval)
scripts/
  build-libphp.sh  reproducible minimal libphp build
vendor/php-build/  (gitignored) downloaded PHP source + install
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
