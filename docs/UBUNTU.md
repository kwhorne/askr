# Running Askr on Ubuntu

Askr's production target is Linux. On Ubuntu the build is simpler than on macOS:
the embed `libphp` is built against system dev libraries via `pkg-config` (no
from-source dependency builds), producing `libphp.so`.

## 1. Prerequisites

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Build toolchain + PHP dependency dev libraries
sudo apt-get update
sudo apt-get install -y \
  build-essential pkg-config curl git \
  libssl-dev libxml2-dev libonig-dev libsqlite3-dev
```

`build-essential` provides gcc/make; the `*-dev` packages provide OpenSSL,
libxml2 (dom/xml), oniguruma (mbstring/mbregex) and SQLite (pdo_sqlite) with
their `pkg-config` files, which PHP's `configure` discovers automatically.

## 2. Build the embedded PHP

```bash
git clone git@github.com:kwhorne/askr.git
cd askr

# Build a non-ZTS, embed-enabled libphp.so with everything Laravel needs.
PROFILE=laravel ./scripts/build-libphp.sh
# -> vendor/php-build/install/lib/libphp.so
#    vendor/php-build/install/bin/php-config
```

## 3. Build Askr

```bash
cargo build --release
# binary: target/release/askr
```

The build links `libphp.so` and bakes in an rpath to
`vendor/php-build/install/lib`, so the binary finds it at runtime. To relocate,
set an rpath/`LD_LIBRARY_PATH` to wherever `libphp.so` lives, or point
`ASKR_PHP_CONFIG` at another embed-enabled, non-ZTS `php-config` before building.

## 4. Pre-flight check

```bash
OPCACHE=$(ls vendor/php-build/install/lib/php/extensions/*/opcache.so)
export ASKR_PHP_INI=$'zend_extension='"$OPCACHE"$'\nopcache.enable=1'

./target/release/askr doctor
# verifies: non-ZTS build, all required extensions, kernel >= 5.1 (io_uring)
```

## 5. Serve a Laravel app

```bash
export ASKR_PHP_INI=$'zend_extension='"$OPCACHE"$'\nopcache.enable=1\nopcache.validate_timestamps=0'

ASKR_APP_BASE=/var/www/app \
./target/release/askr serve \
  --root /var/www/app/public \
  --worker-script /path/to/askr/examples/laravel-worker.php \
  --listen 0.0.0.0:8000 \
  --workers "$(nproc)" \
  --max-requests 1000
```

Add TLS with `--tls-cert cert.pem --tls-key key.pem` (or `--tls-self-signed`
for testing). Reload code with zero downtime: `kill -HUP <master-pid>`.

## 6. systemd unit (production)

`/etc/systemd/system/askr.service`:

```ini
[Unit]
Description=Askr PHP application server
After=network.target

[Service]
Type=simple
User=www-data
WorkingDirectory=/opt/askr
Environment=ASKR_APP_BASE=/var/www/app
Environment=ASKR_PHP_INI=zend_extension=/opt/askr/vendor/php-build/install/lib/php/extensions/no-debug-non-zts-20240924/opcache.so
ExecStart=/opt/askr/target/release/askr serve \
  --root /var/www/app/public \
  --worker-script /opt/askr/examples/laravel-worker.php \
  --listen 0.0.0.0:8000 --workers 8 --max-requests 1000
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
KillSignal=SIGTERM

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now askr
sudo systemctl reload askr   # graceful rolling reload (SIGHUP)
```

`systemctl reload` triggers the rolling reload (new PHP code, no downtime);
`systemctl stop` sends SIGTERM, which drains all workers before exit.

## Notes

- **io_uring:** the current I/O layer is tokio/epoll. The per-core io_uring core
  is the next architectural step and is Linux-only — this is where the
  biggest efficiency gains land. `askr doctor` reports whether the kernel
  supports it (≥ 5.1; 5.10+ recommended).
- **PHP version:** override with `PHP_VERSION=8.4.x ./scripts/build-libphp.sh`.
- **opcache path:** the `no-debug-non-zts-YYYYMMDD` directory name encodes the
  PHP API version; adjust the `zend_extension=` path to match your build.
