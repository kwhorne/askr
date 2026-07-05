# Automatic TLS (ACME / Let's Encrypt)

Askr can obtain and renew its own TLS certificate — the last piece of the
"single binary, no proxy" story. No certbot, no cron, no nginx in front.

```bash
askr serve \
  --root /var/www/app/public \
  --worker-script /opt/askr/examples/laravel-worker.php \
  --listen 0.0.0.0:443 \
  --acme --acme-domain example.com --acme-email you@example.com \
  --acme-dir /var/lib/askr/acme
```

That's it: on first start Askr obtains a certificate for `example.com` from
Let's Encrypt, serves HTTPS on `:443`, and renews automatically before expiry.

## How it works (and why it's prefork-safe)

The prefork model (one worker process per core, all accepting on the shared
socket) makes ACME challenge routing tricky — a validation connection would hit a
random worker. Askr sidesteps it:

- The **master** runs a tiny **HTTP-01** challenge server on `--acme-http`
  (default `0.0.0.0:80`) and obtains the certificate **before forking** workers.
- **Workers** only ever serve HTTPS on `--listen` from the cached cert — no port
  conflict, no cross-worker coordination.
- A background thread in the master **renews** before expiry and then triggers a
  graceful rolling reload so workers pick up the new cert with zero downtime.

`--acme-dir` holds the ACME account, `cert.pem`, `key.pem` and a `renew_at`
marker. Keep it on a persistent volume.

## Requirements

- Port **80** reachable from the internet at your domain (HTTP-01 validation).
  Askr serves the challenge there; set `--listen` to your HTTPS port (`:443`).
- The domain's DNS must resolve to this host.
- Run on Linux in production. Binding `:80`/`:443` as non-root needs
  `CAP_NET_BIND_SERVICE` (see [UBUNTU.md](UBUNTU.md)) or a port mapping (Docker).

## Options

| Flag | Meaning |
| --- | --- |
| `--acme` | Enable auto-TLS |
| `--acme-domain <d>` | Domain (repeat for SAN certs) |
| `--acme-email <e>` | ACME account contact |
| `--acme-dir <path>` | Account + cert cache (persist this) |
| `--acme-staging` | Use Let's Encrypt **staging** (high rate limits; untrusted) — use while testing |
| `--acme-http <addr>` | Where to answer HTTP-01 challenges (default `0.0.0.0:80`) |
| `--acme-directory <url>` | Custom ACME directory (e.g. a private CA / Pebble) |
| `--acme-ca-root <pem>` | Trust this CA root for the ACME directory (testing) |

> Start with `--acme-staging` to avoid Let's Encrypt's strict production rate
> limits, confirm it works, then drop the flag for a trusted cert.

## Testing locally with Pebble

[Pebble](https://github.com/letsencrypt/pebble) is Let's Encrypt's test ACME
server. The full flow (account → order → finalize → certificate) is exercised
against it in development:

```bash
# 1. run Pebble (VA_ALWAYS_VALID skips the inbound HTTP fetch; the challenge
#    server itself is covered by a unit test)
docker run -d --name pebble -p 14000:14000 -p 15000:15000 \
  -e PEBBLE_VA_ALWAYS_VALID=1 ghcr.io/letsencrypt/pebble:latest
docker cp pebble:/test /tmp/pebble-test   # grab /tmp/pebble-test/certs/pebble.minica.pem

# 2. obtain against Pebble
askr serve --root ./public --listen 127.0.0.1:8443 --workers 1 \
  --acme --acme-domain askr.test --acme-email me@askr.test \
  --acme-dir /tmp/acme --acme-directory https://localhost:14000/dir \
  --acme-ca-root /tmp/pebble-test/certs/pebble.minica.pem --acme-http 127.0.0.1:5002

# → /tmp/acme/cert.pem issued by "Pebble Intermediate CA", HTTPS served with it
```

For a full end-to-end test *including* the inbound HTTP-01 fetch, run Pebble
without `PEBBLE_VA_ALWAYS_VALID` plus `pebble-challtestsrv` (to point the domain
at the challenge server on port 80) — that needs port 80 and a Linux host.
