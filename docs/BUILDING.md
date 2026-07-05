# Building

Askr has two build steps:

1. Build an embed-enabled, **non-ZTS** `libphp` (once).
2. Build the `askr` binary with `cargo` (links `libphp`).

## Requirements

- **Rust** (stable; `rustup`).
- A **C toolchain** (`build-essential` on Ubuntu, Xcode Command Line Tools on
  macOS).
- On **Ubuntu**: `pkg-config` + PHP dependency dev libraries (below).
- On **macOS**: nothing extra — the script builds the PHP dependencies from
  source as static libraries.

No `autoconf`/`re2c`/`bison` are needed: official PHP *release tarballs* ship a
ready `configure` and pre-generated lexers/parsers.

## Step 1 — build `libphp`

`scripts/build-libphp.sh` downloads a PHP release, configures a non-ZTS embed
build, and installs it under `vendor/php-build/install/` (git-ignored).

### Profiles

| Profile | Extensions | Use |
| --- | --- | --- |
| `minimal` (default) | core only | the embedding spike / running tests |
| `laravel` | everything a real Laravel 12/13 app needs | serving real apps |

```bash
# Minimal (fast, ~15s) — enough for `cargo test`
./scripts/build-libphp.sh

# Full Laravel profile
PROFILE=laravel ./scripts/build-libphp.sh
```

Override the PHP version with `PHP_VERSION=8.4.x ./scripts/build-libphp.sh`.

### Ubuntu

Install the dependency dev libraries first — PHP's `configure` finds them via
`pkg-config`:

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config curl \
  libssl-dev libxml2-dev libonig-dev libsqlite3-dev
PROFILE=laravel ./scripts/build-libphp.sh
# -> vendor/php-build/install/lib/libphp.so
```

### macOS

No system libraries needed — the script builds oniguruma, OpenSSL and libxml2
from source as static libs and links them in (fully self-contained, no brew, no
pkg-config):

```bash
PROFILE=laravel ./scripts/build-libphp.sh
# -> vendor/php-build/install/lib/libphp.dylib
```

## Step 2 — build Askr

```bash
cargo build --release   # target/release/askr
```

`askr-php`'s `build.rs` discovers PHP via `vendor/php-build/install/bin/php-config`,
compiles the shim, links `libphp`, and bakes in an **rpath** to the install's
`lib/` so the binary finds `libphp.{so,dylib}` at runtime.

To use a different PHP install, point `ASKR_PHP_CONFIG` at another
embed-enabled, non-ZTS `php-config` before building:

```bash
ASKR_PHP_CONFIG=/opt/php/bin/php-config cargo build --release
```

## The extension matrix

Booting a real Laravel app surfaces, in order, which extensions it needs. The
`laravel` profile provides them:

| Symptom without it | Extension | Provided by |
| --- | --- | --- |
| `Call to undefined function mb_split()` | mbstring (mbregex) | oniguruma |
| `Call to undefined function openssl_*` | openssl | OpenSSL |
| `Class "DOMDocument" not found` | dom / xml / simplexml | libxml2 |
| — | pdo_sqlite, tokenizer, session, ctype, filter, fileinfo, phar, bcmath, pcntl, posix, opcache | bundled / built-in |

On Ubuntu these come from the `-dev` packages via pkg-config. On macOS the
script builds oniguruma/OpenSSL/libxml2 statically (the SDK's libxml2 is too old
for PHP 8.5's `ext/dom`).

## opcache

PHP 8.5 compiles OPcache statically into libphp and auto-registers it — there is
no `opcache.so` and no `zend_extension` line. Just switch it on (and turn on JIT)
via the INI (see [Configuration](CONFIGURATION.md)):

```bash
export ASKR_PHP_INI=$'opcache.enable=1\nopcache.enable_cli=1\nopcache.validate_timestamps=0\nopcache.jit=tracing\nopcache.jit_buffer_size=128M'
```

Confirm your build with `askr doctor` (see [CLI](CLI.md)).

## Continuous integration

`.github/workflows/ci.yml` builds a minimal `libphp` on Ubuntu, then runs
`rustfmt --check`, `clippy -D warnings`, the tests, and a release build; a second
job compiles the full `laravel`-profile `libphp.so` against apt dev libraries and
asserts the build is non-ZTS.
