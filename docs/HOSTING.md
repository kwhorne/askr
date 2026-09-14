# Hosting multiple domains on one Askr

One Askr instance can serve **many domains** — each with its own document root and
app — and redirect between hostnames (e.g. `www.domene.no` → `domene.no`). No more
one Askr per site on a shared server, and no nginx/Apache vhost layer in front.

This guide covers three pieces that work together:

1. **Virtual hosts** (`[[site]]`) — route each domain to its own app.
2. **Redirects** (`[[redirect]]` + `force_https`) — `www`→apex, http→https.
3. **TLS for many domains** — one cert with several SANs, auto-reloaded on renewal.

> All of this is configured in `askr.toml`. See the field reference in
> [Configuration](CONFIGURATION.md); this page is the how-to.

---

## 1. Virtual hosts — a domain per app

Add a `[[site]]` block per domain. Askr routes each request to the site whose
`hosts` match the `Host` header; a request that matches none falls back to
`[server] root`.

```toml
[server]
listen = "0.0.0.0:443"
root   = "/var/www/default/public"   # fallback for unmatched hosts

[[site]]
hosts = ["domene.no", "*.domene.no"] # exact or *.suffix glob
root  = "/var/www/domene/public"
front = "index.php"                  # optional, defaults to index.php

[[site]]
hosts = ["kunde2.no"]
root  = "/var/www/kunde2/public"
```

- **Host matching:** exact (`domene.no`) or a suffix glob (`*.domene.no` matches
  `www.domene.no`, `app.domene.no`, and `domene.no` itself).
- **Static files** are served from the matching site's `root` in every mode.
- **Fallback:** an unmatched Host uses `[server] root` (so keep a sensible default,
  or a catch-all landing page).
- Each `root` must contain its `front` controller at startup, or Askr refuses to
  start with a clear error.

Run it:

```bash
askr serve --config askr.toml
```

### Per-request vs worker mode (important)

| Mode | Static files | Dynamic requests |
| --- | --- | --- |
| **Per-request** (default) | per-site ✅ | **per-site ✅** — each request runs that site's front controller |
| **Worker** (`[worker] script`, Octane) | per-site ✅ | single booted app ⚠️ |

**Full multi-app hosting works in per-request mode** — ideal for several small/medium
PHP apps (WordPress, small Laravel/Symfony sites) on one box. Each request boots the
matched site's front controller fresh.

**Worker mode** (a long-lived, booted Octane app) serves **one** app per instance:
statics are still routed per site, but every dynamic request hits the single booted
app. For multiple Octane apps today, either run one Askr instance per app, or route
by host inside your worker script. (Per-site worker pools are on the roadmap.)

---

## 2. Redirects — `www`→apex and http→https

### Host redirects

```toml
[[redirect]]
from = "www.domene.no"
to   = "https://domene.no"     # → https://domene.no/<path>?<query>

[[redirect]]
from   = "*.old.no"            # glob: any subdomain of old.no
to     = "https://ny.no"
status = 301                   # default is 308 (permanent, keeps the method)
```

- The request **path and query are preserved**: `www.domene.no/blog/1?ref=x` →
  `https://domene.no/blog/1?ref=x`.
- `from` matches the Host exactly or as `*.suffix`.
- `status` defaults to **308** (permanent, method-preserving); use `301` if you
  prefer the classic permanent redirect.
- Redirects are evaluated **before** any routing/PHP — they're cheap and never touch
  the app.

### Force HTTPS

```toml
[server]
force_https = true
```

or `askr serve … --force-https`. A plain-HTTP request is answered with a **308** to
the `https://` URL (same host + path + query). "Is this request secure?" is decided
from, in order: the connection's own TLS, `[server] https = true`, or an
`X-Forwarded-Proto: https` header (when Askr sits behind a TLS terminator). A request
that's already HTTPS is left untouched.

> `force_https` alone can't redirect port 80: a TLS listener never sees a plain-HTTP
> request, so something has to be listening on `:80` to redirect *from*. Add:
>
> ```toml
> [server]
> force_https = true
> http_redirect = "0.0.0.0:80"
> ```
>
> With [`--acme`](AUTOTLS.md) this is **automatic** on the ACME challenge address —
> one listener answers HTTP-01 challenges *and* redirects everything else, so there's
> no second process to fight over the port. A challenge always wins over the redirect,
> or issuing a first certificate would be impossible.

---

## 3. TLS for several domains

One certificate can cover several domains via Subject Alternative Names. With
built-in ACME:

```bash
askr serve --config askr.toml \
  --acme --acme-domain domene.no --acme-domain www.domene.no \
         --acme-domain kunde2.no --acme-email you@domene.no
```

All listed domains end up in one cert; ACME renews it in memory automatically. See
[Automatic TLS](AUTOTLS.md).

Using an **external** cert (e.g. certbot)? Point Askr at the files and it will
**hot-reload** them when they change on disk — no restart, no dropped connections:

```toml
[tls]
cert = "/etc/letsencrypt/live/domene.no/fullchain.pem"
key  = "/etc/letsencrypt/live/domene.no/privkey.pem"
```

A background watcher notices the renewed cert (mtime) and triggers a graceful rolling
reload; respawned workers read the new cert. (Certs must be **X.509 v3** with a SAN —
a v1 cert is rejected by rustls, and Askr fails fast rather than crash-looping.)

---

## A complete example

```toml
[server]
listen      = "0.0.0.0:443"
root        = "/var/www/default/public"
force_https = true                     # http→https everywhere

[tls]
cert = "/etc/letsencrypt/live/domene.no/fullchain.pem"
key  = "/etc/letsencrypt/live/domene.no/privkey.pem"

# domene.no + its app
[[site]]
hosts = ["domene.no", "*.domene.no"]
root  = "/var/www/domene/public"

# a second, independent app
[[site]]
hosts = ["kunde2.no", "www.kunde2.no"]
root  = "/var/www/kunde2/public"

# www → apex for the primary domain
[[redirect]]
from = "www.domene.no"
to   = "https://domene.no"
```

This one instance serves two apps, forces HTTPS, redirects `www.domene.no` to the
apex, and hot-reloads the certificate when certbot renews it.

## Behind nginx (or any other proxy)

Askr replaces nginx, but on a host where nginx already owns 80/443 and fronts other
sites, the pragmatic setup is Askr on loopback with nginx proxying in. Two settings
matter, and both are easy to get subtly wrong:

```toml
[server]
listen = "0.0.0.0:8080"
# The proxy terminated TLS: without this, Laravel builds http:// URLs and you get
# redirect loops.
https = true
# The gateway of the Docker network, NOT 127.0.0.1 — that's the address the container
# sees the proxy arrive from. Point it at loopback and X-Forwarded-For is ignored, so
# every visitor looks like one IP and rate limiting lumps them together.
trusted_proxies = ["172.18.0.0/16"]
```

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Host              $host;
    proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_buffering off;                      # don't buffer large downloads or SSE
    proxy_set_header Upgrade    $http_upgrade; # broadcasting / Livewire streams
    proxy_set_header Connection $connection_upgrade;
}
```

**Pass everything through.** No `root`, no `try_files`, no `fastcgi_pass`: Askr already
serves static files, negotiates gzip and brotli, sets `immutable` on hashed build assets,
and refuses dotfiles and `.php` sources. Duplicating that in nginx only creates two places
for the rules to disagree.

Two things worth knowing:

- `client_max_body_size` in nginx must match `max_body_size` in Askr, or one of them
  returns 413 while the other would have accepted the upload.
- `$connection_upgrade` isn't defined by default on every distribution. Without it
  `nginx -t` fails with "unknown variable"; add the usual `map $http_upgrade
  $connection_upgrade { default upgrade; '' close; }` at the top of the vhost.

Askr sets no security headers of its own. Behind nginx you may already have them at the
edge; serving directly, they're the application's job.

---

## Notes & limits

- Multi-site is a **per-request-mode** feature for full dynamic dispatch; worker mode
  serves one booted app (statics per-site). Per-site worker pools are future work.
- Redirects and `force_https` run for **all** requests, before routing.
- Host matching is case-insensitive and ignores the port.

### What sites share, and what they don't

Everything in shared memory is one region per *instance*. Since 1.5.1 it is
partitioned by **application** — a namespace derived from the site's docroot — so two
sites with different docroots are two applications and cannot see each other's data,
while two domains serving one docroot are one application and share, as they should.

| | Isolated per application | Shared across the instance |
| --- | --- | --- |
| Response cache entries | ✓ keyed on `Host` | |
| Response cache **tags** (`askr_cache_forget_tag`) | ✓ | |
| KV cache, sessions, locks, counters (`askr_cache_*`) | ✓ | |
| `askr_cache_flush()` | ✓ flushes only the caller's application | response cache is flushed whole |
| Job queue (`askr_queue_*`) | ✓ names, pops and acks | |
| Broadcasting (SSE, Pusher) | | ✓ one secret per instance, so one application |
| Rate limiting, metrics, admin plane | | ✓ operational, per instance |

Two consequences worth knowing:

- **Queue and scheduler sidecars serve exactly one application, and the configuration has
  to say which.** A second application's jobs land in its own namespace, and a sidecar
  namespaced to another application cannot pop them at all. Since 1.7.0 that is
  [`[queue] root` / `[scheduler] root`](#queue-and-scheduler-sidecars-serve-one-application),
  and Askr refuses to start without it.
- **Broadcasting is not partitioned.** Channel names are instance-wide, and the Pusher
  secret is one per instance. Treat realtime as belonging to one application per
  instance.

### Queue and scheduler sidecars serve one application

A queue or scheduler sidecar is one process with one namespace for its whole life, so it
consumes exactly one application's jobs. On an instance with `[[site]]` there is more than
one application, and which one the sidecar belongs to is a question only the configuration
can answer.

The mechanism is the namespace above. A pushed job is stored under a key carrying the
namespace of the application that pushed it, and `askr_queue_pop` matches on that
*namespaced* key. A sidecar namespaced to one application therefore never sees another's
jobs — not slowly, never: the key it asks for does not exist. Until 1.7.0 sidecars took the
namespace of the top-level `[server] root`, so wherever a `[[site]]` application dispatched
the jobs, every job was accepted, stored and never read. One instance ran that way for six
days: mail, webhooks and broadcasts all stopped, nothing failed, and the admin API showed
the jobs sitting there with `reserved: 0` on every lane — nothing had ever even been
claimed. It was noticed when a person could not reset their password.

Two keys say which application:

```toml
[server]
root = "/var/www/default/public"

[[site]]
hosts = ["domene.no", "*.domene.no"]
root  = "/var/www/domene/public"        # this application dispatches the jobs

[queue]
workers = 2
slots   = 1024
script  = "/opt/askr/examples/askr-queue.php"
root    = "/var/www/domene/public"      # ...so its workers run in that namespace

[scheduler]
script = "/opt/askr/examples/askr-scheduler.php"
# no root here: it defaults to [queue] root
```

`[scheduler] root` defaults to `[queue] root`, and that to `[server] root`, so a scheduler
serving the same application as the queue workers needs no second line. Set it when the
scheduler belongs to a different application than the queue workers, or when an instance
runs the scheduler without queue workers — the two are resolved independently, so
`[queue] root` never overrides it. If the application that queues the jobs *is* the
top-level one, set `[queue] root` to the same path as `[server] root` — that is a valid and
expected answer; it just has to be an answer.

**Askr refuses to start** when `[[site]]` is configured together with a queue or scheduler
sidecar and neither key is set. The error says it cannot tell which application the sidecar
serves and names the key to set. An upgrade to 1.7.0 on a multi-site instance that runs a
sidecar will hit this on the first start. It is deliberate: guessing cost six days of
silently discarded work, and refusing costs one line of config.

Pointing the sidecar does not make it serve two applications. Jobs pushed by any *other*
application on the instance still have no workers — but that is now visible instead of
silent. `GET /api/status` carries the application (`app`) on every queue entry, so two
applications' `mail` lanes are two entries rather than one, and a lane whose jobs only
another application's workers poll is reported as `kind: "queue_wrong_application"` with
`polled_by` naming those applications — see
[Admin](ADMIN.md#queue-liveness-and-warnings). The same judgement is
`askr_queue_unreachable` on `/metrics`
([Observability](OBSERVABILITY.md#queue-health-on-metrics)), and the backlog watchdog logs
it. Applications that each need their own workers want their own instance.
