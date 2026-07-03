# Security Policy

## Supported versions

Askr is pre-1.0. Security fixes target the latest released version on `main`.

| Version | Supported |
| --- | --- |
| 0.1.x | ✅ |
| < 0.1 | ❌ |

## Reporting a vulnerability

**Please do not report security issues in public GitHub issues.**

Use GitHub's private vulnerability reporting:
[Report a vulnerability](https://github.com/kwhorne/askr/security/advisories/new),
or email **security@kwhorne.com**.

Please include:

- a description of the issue and its impact,
- steps to reproduce (a proof of concept if possible),
- the Askr version (`askr --version`), your OS/kernel, and the PHP version.

You can expect an initial response within a few days. Once a fix is ready we'll
coordinate disclosure and credit you in the release notes if you wish.

## Security-sensitive areas

Askr's whole hot path is memory-safe Rust; a few areas warrant extra scrutiny:

- **The PHP FFI boundary** — PHP is the single `unsafe` frontier. The embed shim
  (`crates/askr-php/csrc/shim.c`) and the FFI in `crates/askr-php/src/lib.rs`
  are where memory-unsafe code lives. Sandboxing this boundary (seccomp/Landlock)
  is planned.
- **TLS termination** — certificate/key loading and the rustls configuration
  (`crates/askr/src/tls.rs`).
- **The admin plane** — the status/reload API has **no built-in authentication**;
  it must be bound to localhost / a private network (see
  [docs/ADMIN.md](docs/ADMIN.md)). Reports about it escaping that assumption are
  in scope.
- **Request handling** — the CGI `$_SERVER` mapping, request body limits, and
  path handling for static files (`cgi.rs`, `server.rs`).
- **The worker lifecycle** — `fork`/signal handling and per-request state reset
  in worker mode (state bleed between requests is a security concern).
