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

Real Laravel 12 + Livewire (a TALL app), served entirely in-process through the
embedded interpreter — **HTTP 200**, encrypted cookies, session, CSRF, Blade,
Livewire components — no FastCGI, no FPM:

```
$ cargo run -p askr-php --example serve -- ~/code/app/public /
== embedded PHP 8.4.11 ==
HTTP 200 (php_status=0)
  Set-Cookie: XSRF-TOKEN=...; secure; samesite=lax
  Set-Cookie: laravel_session=...; secure; httponly; samesite=lax
---- body (159669 bytes) ----
<!DOCTYPE html> <html ...> <title>Laravel</title> ...
```

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

## Layout

```
crates/
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
