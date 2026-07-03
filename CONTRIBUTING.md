# Contributing to Askr

Thanks for your interest in improving Askr! This document covers how to get a
dev build running and the conventions the project follows.

## Getting started

Askr embeds PHP, so you build an embed-enabled `libphp` once, then build the
Rust binary.

```bash
# Requirements: Rust 1.80+, a C toolchain.
# Ubuntu also needs: build-essential pkg-config libssl-dev libxml2-dev libonig-dev libsqlite3-dev
git clone https://github.com/kwhorne/askr
cd askr

# 1. Build a non-ZTS, embed-enabled libphp (minimal is enough for tests)
./scripts/build-libphp.sh
#    …or PROFILE=laravel ./scripts/build-libphp.sh to serve a real app

# 2. Build Askr
cargo build
```

See [docs/BUILDING.md](docs/BUILDING.md) for details and the extension matrix,
and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and crate layout.

## Before you open a pull request

Run the same checks CI does:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- **Format** with `cargo fmt` and keep `clippy` clean (no warnings).
- **Add tests** for new behaviour where it makes sense (the pure helpers have
  unit tests; the embedding path has an integration test in `askr-php`).
- Confine `unsafe` to the PHP FFI boundary and document it with `// SAFETY:`.
- Keep commits focused; conventional-commit-style messages
  (`feat:`, `fix:`, `docs:`, `ci:` …) are appreciated.
- Update `CHANGELOG.md` for user-facing changes.

## Scope

Askr is a **production PHP application server**. Local development tooling is the
domain of [`grove`](https://github.com/kwhorne/grove) — please keep proposals
aligned with the server's goals. Big architectural changes (the io_uring core,
HTTP/3, the `askr-laravel` package) are best discussed in an issue first.

## Reporting bugs / requesting features

Use the issue templates. For security issues, **do not** open a public issue —
see [SECURITY.md](SECURITY.md).

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).
