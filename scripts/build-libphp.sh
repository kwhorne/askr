#!/usr/bin/env bash
#
# Build a minimal, embed-enabled (non-ZTS) libphp for the Askr M0 spike.
#
# Uses an official PHP *release tarball*, which ships a pre-generated `configure`
# and pre-generated lexers/parsers — so no autoconf/re2c/bison/pkg-config are
# required. With `--disable-all` there are effectively no external library deps.
#
# Output: vendor/php-build/install/{lib/libphp.dylib, include/php, bin/php-config}
#
# Override the version with: PHP_VERSION=8.4.23 ./scripts/build-libphp.sh
set -euo pipefail

PHP_VERSION="${PHP_VERSION:-8.4.11}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/vendor/php-build"
SRC="$BUILD/php-$PHP_VERSION"
INSTALL="$BUILD/install"

mkdir -p "$BUILD"
cd "$BUILD"

if [ ! -f "php-$PHP_VERSION.tar.gz" ]; then
    echo ">> downloading PHP $PHP_VERSION"
    curl -fsSL -o "php-$PHP_VERSION.tar.gz" \
        "https://www.php.net/distributions/php-$PHP_VERSION.tar.gz"
fi

if [ ! -d "$SRC" ]; then
    echo ">> extracting"
    tar xzf "php-$PHP_VERSION.tar.gz"
fi

cd "$SRC"

if [ ! -f Makefile ]; then
    echo ">> configure (embed shared, non-ZTS, minimal)"
    ./configure \
        --prefix="$INSTALL" \
        --enable-embed=shared \
        --disable-all \
        --disable-cgi \
        --disable-cli \
        --disable-fpm \
        --disable-phpdbg \
        --without-pcre-jit \
        --without-iconv \
        --disable-opcache
fi

echo ">> make"
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)"

echo ">> install"
make install

echo
echo "libphp:     $INSTALL/lib/libphp.dylib"
echo "php-config: $INSTALL/bin/php-config"
echo "ZTS:        $(grep -c 'define ZTS 1' "$SRC/main/php_config.h" || true) (0 = non-ZTS, good)"
