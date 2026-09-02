<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-full-dark.svg">
    <img src="assets/logo-full.svg" alt="Askr — the real server for Laravel & PHP" width="440">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/kwhorne/askr/actions/workflows/ci.yml"><img src="https://github.com/kwhorne/askr/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  &nbsp;·&nbsp; <strong>v1.5.1</strong> &nbsp;·&nbsp; MIT
</p>

<p align="center">
  <a href="https://elyracode.com/docs/askr/"><strong>Documentation</strong></a>
  &nbsp;·&nbsp;
  <a href="https://elyracode.com/askr">Product</a>
  &nbsp;·&nbsp;
  <a href="https://github.com/kwhorne/askr/releases">Releases</a>
  &nbsp;·&nbsp;
  <a href="CHANGELOG.md">Changelog</a>
</p>

---

**A standalone, memory-safe PHP application server, written in Rust.**

Askr embeds the PHP interpreter in-process — no FastCGI, no FPM — and serves it from a
memory-safe Rust hot path. In worker mode it boots your application **once** and serves
many requests against it, removing per-request framework bootstrap entirely.

One binary replaces the usual stack: TLS and HTTP/2, static files, a response cache,
worker supervision, queue workers, a scheduler, and an admin dashboard. No proxy in
front, no Redis beside it.

## Performance

Real Laravel + Livewire, served entirely in-process:

| | Per-request (the FPM model) | Worker mode (boot once) |
| --- | --- | --- |
| Latency per request | ~110 ms | **~9 ms** |
| Throughput (8 workers) | 37 req/s | **347 req/s** |

Roughly **9×**, verified correct under load: 300/300 `200`, each worker booted exactly
once, no state bleed. Raw embedding overhead is ~0.02 ms per request (~56k req/s
single-core for a trivial script) — the framework bootstrap is the cost, and worker mode
removes it.

Reproducible method and comparisons against FrankenPHP, FPM+nginx and RoadRunner:
[docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Install

Self-contained releases for Linux (x86_64 and arm64) carry the binary, the embedded PHP,
OPcache and the examples in one tarball:

```bash
VER=v1.5.1; ARCH=$(uname -m)
BASE=https://github.com/kwhorne/askr/releases/download/$VER
TARBALL=askr-${VER#v}-linux-$ARCH.tar.gz

curl -fsSLO $BASE/$TARBALL
curl -fsSLO $BASE/$TARBALL.minisig
curl -fsSL https://raw.githubusercontent.com/kwhorne/askr/$VER/keys/release.pub -o askr.pub
minisign -V -p askr.pub -m $TARBALL          # verify before unpacking

tar xzf $TARBALL && cd askr-${VER#v}-linux-$ARCH

./askr-run.sh doctor
ASKR_APP_BASE=/var/www/app ./askr-run.sh serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers "$(nproc)" --tls-self-signed --admin 127.0.0.1:9000
```

Releases are signed with [minisign](https://jedisct1.github.io/minisign/) and carry a
SLSA build-provenance attestation; `askr upgrade` verifies the signature itself and
refuses a release it cannot verify. See [SECURITY.md](SECURITY.md).

Runtime libraries are usually already present:
`sudo apt-get install -y libssl3 libxml2 libonig5 libsqlite3-0`.

Docker, tarball and source installs are covered step by step in
[docs/INSTALL.md](docs/INSTALL.md); production setup with systemd, TLS and hardening in
[docs/UBUNTU.md](docs/UBUNTU.md).

## Capabilities

| | |
| --- | --- |
| **Runtime** | Embedded PHP 8.5 (non-ZTS, OPcache + JIT), Laravel's full extension set, one worker process per core on a shared listener, worker mode with per-request state reset |
| **HTTP** | TLS (rustls) with HTTP/2 and optional HTTP/3, auto-TLS via ACME, virtual hosts, redirects, compression, streamed static files and uploads |
| **Caching** | Response cache with tag invalidation, ESI fragment assembly, `PURGE`/`BAN`, declarative per-path rules, request coalescing, `stale-if-error`, survives restarts |
| **Without Redis** | Shared-memory cache, sessions, atomic locks, counters, job queue and broadcasting — with Laravel drivers in [`packages/laravel`](packages/laravel) |
| **Operations** | Zero-downtime rolling reload with canary judging, supervised queue workers and scheduler, fleet-wide rate limiting, leak-aware recycling, admin dashboard and Prometheus metrics |
| **Security** | Signed and verified self-update, seccomp + Landlock sandbox, `unsafe` confined to the PHP FFI boundary |

Every capability is documented at
[elyracode.com/docs/askr](https://elyracode.com/docs/askr/).

## Documentation

**Full documentation: [elyracode.com/docs/askr](https://elyracode.com/docs/askr/)**

The same pages live in [`docs/`](docs/README.md). The ones most people need first:

- [**Laravel setup**](docs/LARAVEL.md) — the recommended guide: `composer require kwhorne/askr-laravel`, `.env`, queue, scheduler, broadcasting
- [**Ubuntu setup**](docs/UBUNTU.md) — the recommended production install
- [Maintenance](docs/MAINTENANCE.md) — deploys, caches, certificates, backups, log rotation
- [Configuration](docs/CONFIGURATION.md) and [CLI reference](docs/CLI.md)
- [Upgrading](docs/UPGRADING.md) — what to adopt per version, and what can bite you
- [Stability & compatibility](docs/STABILITY.md) — the 1.0 contract and deprecation policy

## Project layout

```
crates/
  askr/            the standalone server binary
  askr-php/        embeds PHP (embed SAPI) via FFI
packages/laravel/  the askr-laravel composer package
scripts/           libphp build, packaging, release tooling
examples/          worker-mode templates and an example askr.toml
docs/              full documentation
keys/release.pub   the release signing key
```

## Contributing

Contributions are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers building (an embed
`libphp` first) and the checks CI runs. Please follow the
[Code of Conduct](CODE_OF_CONDUCT.md), and report security issues privately per the
[Security Policy](SECURITY.md) rather than in public issues.

## Links

- **Documentation:** [elyracode.com/docs/askr](https://elyracode.com/docs/askr/)
- **Product:** [elyracode.com/askr](https://elyracode.com/askr)
- **Source & issues:** [github.com/kwhorne/askr](https://github.com/kwhorne/askr)
- **Container images:** [ghcr.io/kwhorne/askr](https://github.com/kwhorne/askr/pkgs/container/askr)
- **Laravel package:** [`kwhorne/askr-laravel`](https://packagist.org/packages/kwhorne/askr-laravel)

## License

MIT © Wirelabs AS — see [LICENSE](LICENSE).
