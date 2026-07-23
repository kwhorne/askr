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

> To redirect port 80 → 443, run a small plain-HTTP Askr on `:80` with
> `force_https = true` alongside your TLS instance on `:443`, or terminate `:80` at
> your load balancer.

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

---

## Notes & limits

- Multi-site is a **per-request-mode** feature for full dynamic dispatch; worker mode
  serves one booted app (statics per-site). Per-site worker pools are future work.
- Redirects and `force_https` run for **all** requests, before routing.
- The response cache keys on the `Host`, so cached responses never leak across
  domains.
- Host matching is case-insensitive and ignores the port.
