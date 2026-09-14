# Admin dashboard & API

Askr ships a small **control plane** — a status/reload API and a web dashboard —
served by the master process. It's the server-appropriate "GUI" for maintaining
a live server: no desktop app, no install, reachable over SSH or a private
network.

Enable it with `--admin <ADDR>` or `[admin] listen` in `askr.toml`:

```bash
askr serve --root ./public --worker-script examples/laravel-worker.php \
  --workers 8 --admin 127.0.0.1:9000
```

```toml
[admin]
listen = "127.0.0.1:9000"
```

Then open <http://127.0.0.1:9000/>.

> **Bind to localhost** (the examples do) and reach it over an SSH tunnel or a
> private network. Binding to a non-loopback address **requires `ASKR_ADMIN_TOKEN`**
> — Askr refuses to start without it (1.5.1). An open admin plane on a network is a
> public reload trigger; on loopback it is reachable only by local processes, which is
> the documented model and still allowed.
>
> **`ASKR_ADMIN_TOKEN`** — set this to require `Authorization: Bearer <token>` on the
> reload trigger (`POST /api/reload`) and the data endpoints (`/api/status`,
> `/api/metrics`, `/metrics`, `/api/errors`). When unset, the plane is open (rely on
> loopback/network isolation). The dashboard shell (`GET /`) is always open but
> carries no data itself.
>
> The same token also gates **`PURGE`/`BAN`** cache invalidation on the *public*
> listener (see [Features](FEATURES.md#purge--ban-over-http)); without a token those
> are accepted from loopback only — unless `trusted_proxies` is set, in which case a
> loopback peer is the proxy rather than a local operator and a token is required.
>
> **Two checks apply to the gated endpoints whether or not a token is set**, because
> both attacks work fine against a plane that never had one:
>
> - **`Host` must name this listener** when the admin plane is bound to loopback.
>   Otherwise a page on the attacker's domain can re-resolve its own hostname to
>   127.0.0.1 (DNS rebinding), at which point the browser treats
>   `http://evil.test:9000/api/status` as same-origin and hands the response to the
>   attacker's script. `localhost` and any loopback literal are accepted; set
>   **`ASKR_ADMIN_HOSTS`** (comma-separated hostnames) if a proxy in front forwards
>   its own `Host`. A non-loopback bind is reached by name on purpose, so the check
>   does not apply there.
> - **Requests a browser reports as cross-site are refused** (`Sec-Fetch-Site`, or an
>   `Origin` that doesn't match `Host`). `POST /api/reload` is a CORS "simple
>   request": no preflight runs, so without this any web page could roll the fleet.
>   `curl` and deploy scripts send neither header and are unaffected.

```bash
ASKR_ADMIN_TOKEN=$(openssl rand -hex 32) askr serve … --admin 0.0.0.0:9000
curl -H "Authorization: Bearer $ASKR_ADMIN_TOKEN" http://host:9000/api/status
```

## Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/` | HTML dashboard (auto-refreshing) with a reload button. |
| `GET` | `/api/status` | Supervisor status as JSON (incl. per-worker RSS). |
| `GET` | `/api/metrics` | Traffic metrics as JSON (throughput, latency, PHP vs I/O). |
| `POST` | `/api/reload` | Trigger a graceful rolling reload. |

### `GET /api/status`

```json
{
  "version": "1.7.0",
  "listen": "0.0.0.0:8000",
  "mode": "worker",
  "uptime_secs": 3600,
  "workers_configured": 8,
  "workers_alive": 8,
  "respawns": 3,
  "queues": [
    {"queue": "default", "app": "9f2c1a77b3e04d61", "pending": 2, "delayed": 0, "reserved": 1,
     "oldest_pending_secs": 3, "last_polled_secs": 0, "last_drained_secs": 1}
  ],
  "queues_idle": [
    {"queue": "mail", "app": "9f2c1a77b3e04d61", "last_polled_secs": 1, "last_drained_secs": 46}
  ],
  "warnings": [],
  "pids": [43509, 43510, 43511, 43512, 43513, 43514, 43515, 43516]
}
```

| Field | Meaning |
| --- | --- |
| `version` | Askr version. |
| `listen` | The application server's listen address. |
| `mode` | `worker` or `per-request`. |
| `uptime_secs` | Seconds since the master started. |
| `workers_configured` | Target worker count. |
| `workers_alive` | Workers currently running. |
| `respawns` | Total worker respawns (recycles + crashes + reloads). |
| `rss_kb_total` | Total resident memory across workers (KB). |
| `workers` | Per-worker `{pid, rss_kb}` (the leak signal — watch RSS vs recycling). |
| `queues` | Per-queue backlog — `{queue, app, pending, delayed, reserved, oldest_pending_secs}` — each entry also carrying `last_polled_secs` and `last_drained_secs`. |
| `queues_idle` | Queues a worker polls that hold no jobs right now: `{queue, app, last_polled_secs, last_drained_secs}`. |
| `warnings` | Lanes that are in trouble, named, with the numbers behind the call. Empty when nothing is wrong — see below. |
| `pids` | Live worker PIDs. |

#### Queue liveness and `warnings`

A production queue lane went three days with nothing draining it. Askr diagnosed it
correctly every ten seconds for the whole three days — it named the queue and suggested
the cause — but only in its own log. No failed jobs, no admin warning, no health signal;
the app's `/up` answered 200 throughout. A log line a product never reads is not an alert.
The queue depth was already on this endpoint; what was missing was Askr saying *this is
wrong*, so a dashboard would have had to hardcode Askr's threshold to reach a conclusion
Askr had already reached.

Every queue-related entry carries `app`: the application whose jobs these are, or whose
worker polls this lane, as the 16-hex-digit namespace derived from that application's
docroot — `null` where there is no namespace. Shared memory is partitioned per application
(see [Hosting](HOSTING.md#what-sites-share-and-what-they-dont)), and until 1.7.0 this
endpoint stripped the namespace off and reported jobs under the bare queue name: two
applications' `mail` lanes were one entry, so a lane being polled briskly by one
application hid another application's jobs that nothing could reach. They are now distinct
entries, in `queues`, in `queues_idle` and in `warnings`.

Two fields carry the liveness of a lane. `last_polled_secs` is seconds since a queue
worker last **asked** that queue for a job — whether or not it got one; `last_drained_secs`
is seconds since one last **reserved** a job from it. Both are `null` when it has never
happened. `null` means *never*, which is not "a long time ago", and must not be rendered as
a duration.

A lane is remembered once polled, which is what `queues_idle` reports: a queue that is
polled and currently empty stays visible, and that is what tells "nobody is listening to
`mail`" apart from "there are no queue workers at all". Up to 64 distinct queue names are
tracked; beyond that a lane simply has no liveness signal, and is never reported as faulty
on that basis.

The poll/drain pair, matched per application, separates faults that job age alone cannot
tell apart and whose remedies are opposite. That is what `warnings` reports:

```json
"warnings": [
  {"kind": "queue_unattended", "queue": "mail", "app": "9f2c1a77b3e04d61", "polled_by": [],
   "pending": 812, "oldest_pending_secs": 259181, "last_polled_secs": null, "last_drained_secs": null,
   "detail": "no worker is asking this queue for jobs — check the queue name a worker polls (ASKR_QUEUE) against the one the app dispatches to"},
  {"kind": "queue_wrong_application", "queue": "broadcasts", "app": "4b81de0c5a7f2390",
   "polled_by": ["9f2c1a77b3e04d61"], "pending": 40, "oldest_pending_secs": 512400,
   "last_polled_secs": null, "last_drained_secs": null,
   "detail": "these jobs were pushed by one application and the only workers polling this queue name belong to another, so no worker can ever see them — set [queue] root (and [scheduler] root) to the docroot of the application that dispatches them. Adding workers cannot help"}
]
```

- **`queue_unattended`** — jobs are waiting and nothing is polling the lane. Almost always
  a queue-name mismatch: the app dispatches to `onQueue('mail')` and the worker polls
  `default`. Adding workers does nothing.
- **`queue_wrong_application`** — the jobs are waiting under one application and the only
  workers polling that queue name belong to another, which `polled_by` names: an array of
  the application namespaces that *are* polling it. The queue name is already right, and
  the jobs are unreachable rather than behind — `askr_queue_pop` matches the namespaced
  key, so no number of extra workers can ever pop them. The fix is `[queue] root` /
  `[scheduler] root`, which say which application a sidecar serves — see
  [Hosting](HOSTING.md#queue-and-scheduler-sidecars-serve-one-application). `polled_by` is
  present on every warning and is an empty array for the other kinds.
- **`queue_not_draining`** — workers are polling and the backlog still grows. The lane is
  saturated, or jobs keep being released back. More workers, or look at what is failing.

`kind` is a stable machine-readable tag — switch on it. `detail` is prose for the person
reading it and is **not** stable; never match on it.

`warnings` is the field a dashboard renders directly, and that is the point: a product
that renders it does not reimplement Askr's thresholds and then drift from them when
either side changes. How long a job may sit ready and unclaimed before a lane counts as
stalled is [`[queue] stall_secs`](CONFIGURATION.md#queue) (default 30 s) — the same number
behind the watchdog log line and the per-queue `/metrics` series.

Two details worth knowing before you alert on this. "Nothing is polling the lane" means
**no poll in the last 60 seconds, or none ever** — not a single missed tick. A Laravel
queue worker polls continuously while jobs flow and on its `--sleep` interval (3 s by
default) when idle, so a minute of silence is many missed polls. A worker configured with
a `--sleep` near or above 60 s can produce a brief `queue_unattended` for jobs that arrive
just after a poll; the next evaluation clears it. That window is not configurable.

And `askr_queue_unattended` is emitted only for lanes that currently hold jobs, so an
empty lane has no series rather than a `0`. Alerting on `== 1` is correct; alerting on
`== 0` is not a health check.

### `GET /api/metrics`

Traffic metrics, aggregated across all workers via shared memory (no IPC). The
standout is the **PHP vs I/O split** — because Askr runs PHP in-process, it can
measure how much of each request is PHP execution vs TLS/I/O, which a
FastCGI/proxy setup can't see cleanly.

```json
{
  "requests": 201, "errors": 0, "bytes_out": 31940459,
  "avg_total_ms": 72.01, "avg_php_ms": 71.98,
  "php_pct": 99, "io_pct": 1,
  "slowest_ms": 222.59,
  "status": {"1xx":0,"2xx":200,"3xx":0,"4xx":1,"5xx":0},
  "histogram": {"bounds_ms":[1,2,5,10,25,50,100,250,500,1000,2500,5000],
                "counts":[0,0,0,3,25,53,79,41,0,0,0,0,0]}
}
```

Counters are cumulative; the dashboard derives a live req/s from successive
polls. The latency `histogram.counts` has one more entry than `bounds_ms` (the
final `> last bound` overflow bucket).

### `POST /api/reload`

```bash
curl -X POST http://127.0.0.1:9000/api/reload
# {"ok":true,"action":"reload"}
```

Triggers the same graceful **rolling reload** as `SIGHUP`: workers are restarted
one at a time, so there's no downtime. Use this to pick up new PHP code after a
deploy.

## The dashboard

`GET /` serves a single self-contained HTML page that polls `/api/status` and
`/api/metrics` every 2 s and shows uptime, workers alive/configured, respawns,
per-worker memory, live throughput, average latency, the **PHP vs I/O** split, a
latency histogram, status-code breakdown, and a **Graceful reload** button. No
build step, no assets — it's embedded in the binary.

## Scripting

The API is trivial to script for CI/CD or monitoring:

```bash
# health gate in a deploy script
alive=$(curl -s http://127.0.0.1:9000/api/status | jq .workers_alive)
[ "$alive" -gt 0 ] || { echo "askr has no live workers"; exit 1; }

# deploy new code, then reload
rsync -a build/ /var/www/app/
curl -fsS -X POST http://127.0.0.1:9000/api/reload
```

## Roadmap

A future desktop **control center** (Grove-style, Tauri) can manage a *fleet* of
Askr servers through this same API. An OpenTelemetry/Prometheus export and
per-route timing (PHP vs I/O split per route) build on the same shared-memory
metrics.
