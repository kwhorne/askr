#!/usr/bin/env bash
#
# Assemble a self-contained Askr distribution: the binary, the embedded libphp,
# opcache, examples, docs and a launcher — with the rpath fixed to $ORIGIN/lib so
# it runs from anywhere.
#
# Prereqs: a release build (`cargo build --release`) and a built libphp
# (`PROFILE=laravel ./scripts/build-libphp.sh`). On Linux, `patchelf`.
#
# Output: dist/askr-<version>-<os>-<arch>.tar.gz
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALL="$ROOT/vendor/php-build/install"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH="$(uname -m)"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
# Optional variant suffix (e.g. SUFFIX=-full for the sql-backend + observ build).
NAME="askr-$VERSION-$OS-$ARCH${SUFFIX:-}"
DIST="$ROOT/dist/$NAME"

[ -x "$ROOT/target/release/askr" ] || { echo "build first: cargo build --release"; exit 1; }
[ -d "$INSTALL/lib" ] || { echo "build libphp first: PROFILE=laravel ./scripts/build-libphp.sh"; exit 1; }

echo ">> assembling $NAME"
rm -rf "$DIST"
mkdir -p "$DIST/lib"

# Binary
cp "$ROOT/target/release/askr" "$DIST/askr"

# libphp (.so on Linux, .dylib on macOS)
LIB="$(ls "$INSTALL"/lib/libphp.* | head -1)"
cp "$LIB" "$DIST/lib/"

# PHP 8.5 compiles OPcache statically into libphp (no opcache.so to bundle) and
# auto-registers it — enabled at runtime via opcache.enable=1 (see askr-run.sh).

# Relocate: make the loader find lib/ next to the binary.
if [ "$OS" = "linux" ]; then
    patchelf --set-rpath '$ORIGIN/lib' "$DIST/askr"
else
    install_name_tool -add_rpath '@loader_path/lib' "$DIST/askr" 2>/dev/null || true
fi

# Payload
cp -r "$ROOT/examples" "$DIST/examples"
cp -r "$ROOT/docs" "$DIST/docs"
cp "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/CHANGELOG.md" "$DIST/"

# Launcher: sets up opcache and runs the binary from anywhere.
cat > "$DIST/askr-run.sh" <<'EOF'
#!/usr/bin/env bash
# Convenience launcher: enables opcache (+ JIT), then runs askr from this dir.
# PHP 8.5 has OPcache built into libphp, so we just switch it on via the INI.
HERE="$(cd "$(dirname "$0")" && pwd)"
if [ -z "${ASKR_PHP_INI:-}" ]; then
    export ASKR_PHP_INI="opcache.enable=1
opcache.enable_cli=1
opcache.validate_timestamps=0
opcache.jit=tracing
opcache.jit_buffer_size=128M"
fi
exec "$HERE/askr" "$@"
EOF
chmod +x "$DIST/askr-run.sh"

# Package install notes
cat > "$DIST/INSTALL.txt" <<EOF
Askr $VERSION — self-contained package ($OS/$ARCH)

Contents:
  askr            the server binary (rpath -> ./lib)
  askr-run.sh     launcher (enables opcache, then runs askr)
  lib/            libphp + opcache
  examples/       worker/queue/scheduler scripts, AskrCacheStore.php, askr.toml
  docs/           full documentation

Runtime dependencies (Linux): the embedded PHP links a few system libraries.
On Ubuntu they're normally already present; if not:

  sudo apt-get install -y libssl3 libxml2 libonig5 libsqlite3-0 \\
    libicu74 libcurl4 libpng16-16 libjpeg-turbo8 libfreetype6 libwebp7 \\
    libzip4 libpq5 zlib1g

Quick start:

  ./askr-run.sh doctor
  ASKR_APP_BASE=/var/www/app ./askr-run.sh serve \\
    --root /var/www/app/public \\
    --worker-script examples/laravel-worker.php \\
    --workers \$(nproc) --tls-self-signed

See docs/ for everything else.
EOF

# Tarball + checksum
cd "$ROOT/dist"
tar czf "$NAME.tar.gz" "$NAME"
if command -v sha256sum >/dev/null; then
    sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
elif command -v shasum >/dev/null; then
    shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
fi

echo ">> dist/$NAME.tar.gz"
