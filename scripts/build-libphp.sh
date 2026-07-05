#!/usr/bin/env bash
#
# Build an embed-enabled (non-ZTS) libphp for Askr.
#
# Uses an official PHP *release tarball* (ships a ready `configure` and
# pre-generated lexers/parsers), so no autoconf/re2c/bison is needed.
#
# Profiles:
#   PROFILE=minimal (default)  core only — fastest, proves embedding.
#   PROFILE=laravel            everything a real Laravel 12/13 app needs.
#
# Platforms:
#   Linux (production target): uses system dev libraries via pkg-config. The
#     laravel profile also builds intl/gd/curl/zip/pdo_mysql/pdo_pgsql so heavier
#     apps (e.g. Filament) run. Install the dev libs first:
#       sudo apt-get install -y build-essential pkg-config \
#         libssl-dev libxml2-dev libonig-dev libsqlite3-dev \
#         libicu-dev libcurl4-openssl-dev libpng-dev libjpeg-dev \
#         libfreetype-dev libwebp-dev libzip-dev zlib1g-dev libpq-dev
#     Produces  vendor/php-build/install/lib/libphp.so
#   macOS (dev): builds oniguruma/OpenSSL/libxml2 from source as static libs
#     (no brew / pkg-config needed). Produces …/libphp.dylib. The extra Linux
#     extensions (intl/gd/curl/…) are omitted here — the dev build is for the
#     test suite; Filament-class apps run on the Linux release/Docker image.
#
#   PROFILE=laravel ./scripts/build-libphp.sh
set -euo pipefail

PHP_VERSION="${PHP_VERSION:-8.4.11}"
PROFILE="${PROFILE:-minimal}"
OS="$(uname -s)"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/vendor/php-build"
SRC="$BUILD/php-$PHP_VERSION"
INSTALL="$BUILD/install"
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

mkdir -p "$BUILD"
cd "$BUILD"

fetch() { # url outfile
    [ -f "$2" ] || { echo ">> downloading $2"; curl -fsSL -o "$2" "$1"; }
}

# --- extension flags for the laravel profile ------------------------------
DEP_FLAGS=()
if [ "$PROFILE" = "laravel" ]; then
    DEP_FLAGS=(
        --enable-mbstring --enable-tokenizer --enable-ctype --enable-filter
        --enable-fileinfo --enable-session --enable-phar --enable-pcntl
        --enable-posix --enable-pdo --with-pdo-sqlite --with-sqlite3
        --enable-opcache --enable-bcmath --with-openssl --with-libxml
        --enable-dom --enable-xml --enable-simplexml
        --enable-xmlwriter --enable-xmlreader
    )
fi

# --- platform-specific dependency setup -----------------------------------
if [ "$OS" = "Darwin" ] && [ "$PROFILE" = "laravel" ]; then
    # macOS has no pkg-config / dev libs; build static deps from source.
    ONIG_VERSION="6.9.9"; OPENSSL_VERSION="3.3.2"; LIBXML2_VERSION="2.13.5"
    SDK="$(xcrun --show-sdk-path 2>/dev/null || echo /usr)"

    if [ ! -f "$BUILD/onig-install/lib/libonig.a" ]; then
        fetch "https://github.com/kkos/oniguruma/releases/download/v$ONIG_VERSION/onig-$ONIG_VERSION.tar.gz" onig.tar.gz
        tar xzf onig.tar.gz
        ( cd "onig-$ONIG_VERSION" && ./configure --prefix="$BUILD/onig-install" \
            --disable-shared --enable-static --disable-dependency-tracking >/dev/null &&
            make -j"$JOBS" >/dev/null && make install >/dev/null )
    fi
    if [ ! -f "$BUILD/openssl-install/lib/libssl.a" ]; then
        fetch "https://github.com/openssl/openssl/releases/download/openssl-$OPENSSL_VERSION/openssl-$OPENSSL_VERSION.tar.gz" openssl.tar.gz
        tar xzf openssl.tar.gz
        ( cd "openssl-$OPENSSL_VERSION" && ./Configure darwin64-arm64-cc \
            no-shared no-tests no-docs --prefix="$BUILD/openssl-install" >/dev/null &&
            make -j"$JOBS" >/dev/null && make install_sw >/dev/null )
    fi
    if [ ! -f "$BUILD/libxml2-install/lib/libxml2.a" ]; then
        fetch "https://download.gnome.org/sources/libxml2/${LIBXML2_VERSION%.*}/libxml2-$LIBXML2_VERSION.tar.xz" libxml2.tar.xz
        tar xf libxml2.tar.xz
        ( cd "libxml2-$LIBXML2_VERSION" && ./configure --prefix="$BUILD/libxml2-install" \
            --disable-shared --enable-static --without-python --without-lzma \
            --with-zlib --without-http --disable-dependency-tracking >/dev/null &&
            make -j"$JOBS" >/dev/null && make install >/dev/null )
    fi
    export SQLITE_CFLAGS="-I$SDK/usr/include"
    export SQLITE_LIBS="-L$SDK/usr/lib -lsqlite3"
    export ONIG_CFLAGS="-I$BUILD/onig-install/include"
    export ONIG_LIBS="-L$BUILD/onig-install/lib -lonig"
    export OPENSSL_CFLAGS="-I$BUILD/openssl-install/include"
    export OPENSSL_LIBS="-L$BUILD/openssl-install/lib -lssl -lcrypto"
    export LIBXML_CFLAGS="$("$BUILD/libxml2-install/bin/xml2-config" --cflags)"
    export LIBXML_LIBS="$("$BUILD/libxml2-install/bin/xml2-config" --libs)"
elif [ "$PROFILE" = "laravel" ]; then
    # Linux: rely on system dev libs via pkg-config. Verify they're present.
    command -v pkg-config >/dev/null || { echo "ERROR: pkg-config not found. Install: sudo apt-get install -y pkg-config"; exit 1; }
    APT_HINT="sudo apt-get install -y build-essential pkg-config \\
      libssl-dev libxml2-dev libonig-dev libsqlite3-dev \\
      libicu-dev libcurl4-openssl-dev libpng-dev libjpeg-dev \\
      libfreetype-dev libwebp-dev libzip-dev zlib1g-dev libpq-dev"
    missing=""
    for pc in openssl libxml-2.0 oniguruma sqlite3 icu-uc libcurl libpng freetype2 libwebp libzip; do
        pkg-config --exists "$pc" || missing="$missing $pc"
    done
    if [ -n "$missing" ]; then
        echo "ERROR: missing dev libraries for:$missing"
        echo "Install: $APT_HINT"
        exit 1
    fi
    # Heavier extensions for full-featured apps (Filament needs intl; gd for
    # images; curl for the HTTP client; pdo_mysql/pgsql for real databases).
    # mysqlnd is bundled (no external lib).
    DEP_FLAGS+=(
        --enable-intl
        --with-curl
        --enable-gd --with-jpeg --with-freetype --with-webp
        --enable-exif
        --with-zip
        --with-zlib
        --with-pdo-mysql=mysqlnd --with-pdo-pgsql
    )
fi

# --- PHP ------------------------------------------------------------------
fetch "https://www.php.net/distributions/php-$PHP_VERSION.tar.gz" "php-$PHP_VERSION.tar.gz"
[ -d "$SRC" ] || tar xzf "php-$PHP_VERSION.tar.gz"
cd "$SRC"

echo ">> configure (embed shared, non-ZTS, profile=$PROFILE, os=$OS)"
make distclean >/dev/null 2>&1 || true
./configure \
    --prefix="$INSTALL" \
    --enable-embed=shared \
    --disable-all \
    "${DEP_FLAGS[@]}" \
    --disable-cgi --disable-cli --disable-fpm --disable-phpdbg \
    --without-iconv

echo ">> make (-j$JOBS)"
make -j"$JOBS"
echo ">> install"
make install

# libphp.so on Linux, libphp.dylib on macOS.
LIB="$(ls "$INSTALL"/lib/libphp.* 2>/dev/null | head -1)"
echo
echo "libphp:     $LIB"
echo "php-config: $INSTALL/bin/php-config"
echo "ZTS:        $(grep -c 'define ZTS 1' "$SRC/main/php_config.h" || true) (0 = non-ZTS, good)"
if [ "$PROFILE" = "laravel" ]; then
    echo "opcache:    $INSTALL/lib/php/extensions/*/opcache.so (load via zend_extension=)"
fi
