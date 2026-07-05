# Build a Laravel app into an Askr container — the whole environment in one image
# (web workers + queue + scheduler + cache + broadcasting), no nginx/FPM/redis/cron.
#
# Copy this whole examples/docker/ folder to your Laravel project root, then:
#   docker build -t my-app .
#   docker run -p 80:8000 my-app
# or use the bundled docker-compose.yml:  docker compose up --build
#
# Pin the Askr version to a specific tag in production (e.g. :0.4.2).

# ---- 1. PHP dependencies (cached on composer.json/lock) ----
FROM composer:2 AS deps
WORKDIR /app
COPY composer.json composer.lock ./
# --ignore-platform-reqs: the build PHP differs from Askr's runtime PHP, which
# provides the extensions (intl/gd/curl/…). Askr's libphp is the source of truth.
RUN composer install --no-dev --no-scripts --prefer-dist --no-interaction --no-progress --ignore-platform-reqs
COPY . .
RUN composer dump-autoload --optimize --no-dev

# ---- 2. (optional) front-end assets ----
# Uncomment to build Vite assets (required for Filament's theme); then uncomment
# the COPY in the runtime stage. If npm hits a peer-dependency conflict, swap
# `npm ci` for `npm install --legacy-peer-deps`.
# FROM node:22-slim AS assets
# WORKDIR /app
# COPY package.json package-lock.json ./
# RUN npm ci
# COPY . .
# RUN npm run build

# ---- 3. Askr runtime ----
FROM ghcr.io/kwhorne/askr:0.6 AS runtime
COPY --from=deps --chown=askr /app /var/www/app
# COPY --from=assets --chown=askr /app/public/build /var/www/app/public/build
COPY --chown=askr askr.toml /etc/askr/askr.toml
ENV ASKR_APP_BASE=/var/www/app
# The base image's ENTRYPOINT (askr-run.sh, wires up libphp + opcache) and CMD
# (serve --config /etc/askr/askr.toml) take it from here.
