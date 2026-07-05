# Askr base image — the whole PHP application server in one layer.
#
# Built on ubuntu:24.04 to match the exact environment the release tarballs are
# built and CI-tested on (glibc 2.39): a binary linked against glibc 2.39 won't
# start on an older base (glibc is backward-, not forward-compatible). We do NOT
# recompile — we drop in the relocatable release package (binary + libphp with
# rpath $ORIGIN/lib), so this build is just fetch + package.
#
#   docker build --build-arg ASKR_VERSION=0.6.0 -t askr .
#
# Multi-arch: buildx sets TARGETARCH (amd64 / arm64); we fetch the matching
# tarball. See docs/DOCKER.md for the app-image and compose examples.

# ---- fetch the relocatable release tarball ----
FROM ubuntu:24.04 AS fetch
ARG ASKR_VERSION=0.6.0
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) A=x86_64 ;; \
      arm64) A=aarch64 ;; \
      *) echo "unsupported arch: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    # --retry-all-errors so a Docker build kicked off by the same tag push waits
    # for the release job to finish uploading the asset (404 → retry).
    curl -fsSL --retry 30 --retry-delay 20 --retry-all-errors \
      -o /tmp/askr.tgz \
      "https://github.com/kwhorne/askr/releases/download/v${ASKR_VERSION}/askr-${ASKR_VERSION}-linux-${A}.tar.gz"; \
    mkdir -p /opt/askr; \
    tar xzf /tmp/askr.tgz -C /opt/askr --strip-components=1

# ---- minimal runtime ----
FROM ubuntu:24.04 AS runtime
LABEL org.opencontainers.image.source="https://github.com/kwhorne/askr"
LABEL org.opencontainers.image.description="Askr — the whole PHP application server (embedded PHP, no FPM/nginx) in one container."
LABEL org.opencontainers.image.licenses="MIT"

# Runtime libraries the laravel-profile libphp links (openssl/libxml2/oniguruma/
# sqlite + icu/curl/gd stack/zip/pgsql), plus curl for the healthcheck and CA
# certs for outbound TLS.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libssl3 libxml2 libonig5 libsqlite3-0 \
      libicu74 libcurl4 libpng16-16 libjpeg-turbo8 libfreetype6 libwebp7 \
      libzip4 libpq5 zlib1g \
      ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=fetch /opt/askr /opt/askr
# System user (auto UID). ubuntu:24.04 already reserves UID 1000 for `ubuntu`, so
# we don't force one.
RUN useradd -r -m -d /home/askr askr \
    && mkdir -p /etc/askr /var/www/app \
    && chown -R askr:askr /var/www/app

USER askr
# 8000 = HTTP (map to 80/443 on the host — no root/setcap needed in-container);
# 9000 = admin/metrics (bind to 127.0.0.1 in your config).
EXPOSE 8000 9000

# Uses the built-in admin API. Enable the admin plane on 127.0.0.1:9000 in your
# config for this to work.
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s \
  CMD curl -sf http://127.0.0.1:9000/api/status || exit 1

# askr-run.sh wires up the libphp path + opcache (validate_timestamps=0).
# docker stop → SIGTERM → graceful drain; docker kill -s HUP → rolling reload.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/opt/askr/askr-run.sh"]
CMD ["serve", "--config", "/etc/askr/askr.toml"]
