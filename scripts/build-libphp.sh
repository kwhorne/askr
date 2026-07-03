#!/usr/bin/env bash
#
# Build an embed-enabled (non-ZTS) libphp for Askr.
#
# Uses an official PHP *release tarball* (ships a ready `configure` and
# pre-generated lexers/parsers), so no autoconf/re2c/bison/pkg-config is needed.
#
# Two profiles:
#   PROFILE=minimal (default)  core only — fastest to build, proves embedding.
#   PROFILE=laravel            everything a real Laravel 12/13 app needs. The
#                              external-library extensions (oniguruma for
#                              mbregex, OpenSSL, libxml2 for dom/xml) are built
#                              from source as static libs and linked in — fully
#                              self-contained, no brew / pkg-config required.
#
# Output: vendor/php-build/install/{lib/libphp.dylib, include/php, bin/php-config}
#
#   PROFILE=laravel ./scripts/build-libphp.sh
set -euo pipefail

PHP_VERSION="${PHP_VERSION:-8.4.11}"
PROFILE="${PROFILE:-minimal}"
ONIG_VERSION="6.9.9"
OPENSSL_VERSION="3.3.2"
LIBXML2_VERSION="2.13.5"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/vendor/php-build"
SRC="$BUILD/php-$PHP_VERSION"
INSTALL="$BUILD/install"
JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)"

mkdir -p "$BUILD"
cd "$BUILD"

fetch() { # url outfile
    [ -f "$2" ] || { echo ">> downloading $2"; curl -fsSL -o "$2" "$1"; }
}

# --- dependency libs (laravel profile only) -------------------------------
DEP_FLAGS=()
if [ "$PROFILE" = "laravel" ]; then
    SDK="$(xcrun --show-sdk-path 2>/dev/null || echo /usr)"

    # oniguruma (mbstring multibyte regex: mb_split etc.)
    if [ ! -f "$BUILD/onig-install/lib/libonig.a" ]; then
        fetch "https://github.com/kkos/oniguruma/releases/download/v$ONIG_VERSION/onig-$ONIG_VERSION.tar.gz" onig.tar.gz
        tar xzf onig.tar.gz
        ( cd "onig-$ONIG_VERSION" && ./configure --prefix="$BUILD/onig-install" \
            --disable-shared --enable-static --disable-dependency-tracking >/dev/null &&
            make -j"$JOBS" >/dev/null && make install >/dev/null )
    fi

    # OpenSSL (Encrypter, cookie/session encryption)
    if [ ! -f "$BUILD/openssl-install/lib/libssl.a" ]; then
        fetch "https://github.com/openssl/openssl/releases/download/openssl-$OPENSSL_VERSION/openssl-$OPENSSL_VERSION.tar.gz" openssl.tar.gz
        tar xzf openssl.tar.gz
        ( cd "openssl-$OPENSSL_VERSION" && ./Configure darwin64-arm64-cc \
            no-shared no-tests no-docs --prefix="$BUILD/openssl-install" >/dev/null &&
            make -j"$JOBS" >/dev/null && make install_sw >/dev/null )
    fi

    # libxml2 (dom/xml/simplexml — Livewire's DOMDocument). The macOS SDK copy
    # is too old for PHP 8.4's ext/dom, so build a current one.
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

    DEP_FLAGS=(
        --enable-mbstring --enable-tokenizer --enable-ctype --enable-filter
        --enable-fileinfo --enable-session --enable-phar --enable-pcntl
        --enable-posix --enable-pdo --with-pdo-sqlite --with-sqlite3
        --enable-opcache --enable-bcmath --with-openssl --with-libxml
        --enable-dom --enable-xml --enable-simplexml
        --enable-xmlwriter --enable-xmlreader
    )
fi

# --- PHP ------------------------------------------------------------------
fetch "https://www.php.net/distributions/php-$PHP_VERSION.tar.gz" "php-$PHP_VERSION.tar.gz"
[ -d "$SRC" ] || tar xzf "php-$PHP_VERSION.tar.gz"
cd "$SRC"

echo ">> configure (embed shared, non-ZTS, profile=$PROFILE)"
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

echo
echo "libphp:     $INSTALL/lib/libphp.dylib"
echo "php-config: $INSTALL/bin/php-config"
echo "ZTS:        $(grep -c 'define ZTS 1' "$SRC/main/php_config.h" || true) (0 = non-ZTS, good)"
if [ "$PROFILE" = "laravel" ]; then
    echo "opcache:    $INSTALL/lib/php/extensions/*/opcache.so (load via zend_extension=)"
fi
