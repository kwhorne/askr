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
> private network. The admin plane has no built-in authentication in 0.1.0 —
> don't expose it publicly. Put it behind your own auth/proxy if remote access is
> needed.

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
  "version": "0.3.0",
  "listen": "0.0.0.0:8000",
  "mode": "worker",
  "uptime_secs": 3600,
  "workers_configured": 8,
  "workers_alive": 8,
  "respawns": 3,
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
| `pids` | Live worker PIDs. |

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
