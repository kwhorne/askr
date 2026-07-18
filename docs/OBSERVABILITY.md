# Observability — ship Askr's telemetry to ElyraSQL

Store Askr's own request logs in **ElyraSQL** (or any MySQL-wire database) and
query them with plain SQL — in **Conductor**, a BI tool, or `mysql` on the command
line. No new agent, no sidecar collector: Askr already builds a structured record
for every request; this feature streams that record to a database over the MySQL
wire protocol.

```
  Askr worker(s) ──emit──▶ batching sink ──bulk INSERT (MySQL wire)──▶ ElyraSQL ──SQL──▶ Conductor / any client
  (per-request log_access)   (bounded queue,                           (OLAP store,        (dashboards, search)
                              background flush)                         retention)
```

Many Askr workers on a box share one background sink task; point them all at one
central telemetry database. It is **opt-in twice over** — compiled only with
`--features observ`, and inert at runtime unless `ASKR_OBSERV_DSN` is set — so the
default build, its behaviour, and CI are unaffected.

> **Status:** request **logs** (below), a **metrics rollup**
> ([Metrics rollup](#metrics-rollup)) and OpenTelemetry **traces**
> ([Traces](#traces-opentelemetry)) all ship today.
>
> **Target:** [ElyraSQL](https://github.com/kwhorne/sql-anywhere) and other
> MySQL-wire databases that use `mysql_native_password`. Servers whose *default*
> is `caching_sha2_password` (MySQL 8+, MariaDB 11+) need `ASKR_OBSERV_TLS=1`.

---

## Enable it

**1. Get a build that includes it.** Easiest is the published **`-full`** image or
tarball (durable L2 + observability compiled in):

```bash
docker pull ghcr.io/kwhorne/askr:0.9-full        # or the -full release tarball
```

Or build it yourself:

```bash
cargo build --release --features observ
```

(The *default* release/Docker build does not include it — use `-full` or your own.)

**2. Point it at a database:**

```bash
export ASKR_OBSERV_DSN='mysql://user:pass@telemetry-host:3306/askr_logs'
askr serve --root public --worker-script askr/worker.php --workers auto …
```

On start you'll see `observability: telemetry sink → ElyraSQL enabled`, and the
`logs` table is created automatically if it doesn't exist. That's it — every
request now lands in the database.

---

## Configuration

All via environment variables (unset `ASKR_OBSERV_DSN` = feature off):

| Variable | Default | Meaning |
| --- | --- | --- |
| `ASKR_OBSERV_DSN` | — | `mysql://user:pass@host:port/db`. **Enables the sink.** |
| `ASKR_OBSERV_SERVICE` | `askr` | Logical service name written to each row. |
| `ASKR_OBSERV_HOST` | `$HOSTNAME` / `unknown` | Host label written to each row. |
| `ASKR_OBSERV_BATCH` | `1000` | Rows per `INSERT`. |
| `ASKR_OBSERV_FLUSH_MS` | `1000` | Max buffering latency before a partial batch is flushed. |
| `ASKR_OBSERV_QUEUE` | `65536` | Bounded in-memory queue capacity (per worker). |
| `ASKR_OBSERV_METRICS_MS` | `10000` | Metrics-rollup interval (see [Metrics rollup](#metrics-rollup)). |
| `ASKR_OBSERV_TLS` | off | Connect over TLS (required by `caching_sha2_password` servers). Also `?tls=1` in the DSN. |

Higher `BATCH`/`FLUSH_MS` = fewer, larger inserts (more throughput, more latency
before rows appear). Larger `QUEUE` tolerates longer database stalls before the
sink starts dropping.

---

## What gets written

One row per request, built from Askr's existing access record. The table is
created for you:

```sql
CREATE TABLE IF NOT EXISTS logs (
  id         BIGINT UNSIGNED PRIMARY KEY AUTO_INCREMENT,
  ts         DATETIME(6) NOT NULL,     -- request completion time (µs, UTC)
  service    VARCHAR(64) NOT NULL,     -- ASKR_OBSERV_SERVICE
  host       VARCHAR(64) NOT NULL,     -- ASKR_OBSERV_HOST
  level      VARCHAR(8)  NOT NULL,     -- info (2xx/3xx) · warn (4xx) · error (5xx)
  method     VARCHAR(8),
  path       VARCHAR(255),
  status     SMALLINT,
  latency_ms INT,
  message    TEXT,                     -- "GET /orders/42"
  attrs      JSON                      -- {"ip":"203.0.113.7"}
);
```

`level` is derived from the status class (`≥500 → error`, `≥400 → warn`, else
`info`), so you can filter by severity without parsing status ranges. `attrs` is
JSON so it can grow (request id, user id, trace id) without a schema change.

---

## Guarantees (why it's safe on the hot path)

- **Never blocks a request.** The request thread does a single non-blocking
  `try_send` into a bounded channel and moves on. It runs inside the existing
  access-log hook, independent of the file access log.
- **Drops, never stalls, under backpressure.** If the database is slow or down and
  the queue fills, rows are dropped (with a rate-limited `telemetry queue full`
  warning). Telemetry favours availability over completeness — it will never grow
  memory unbounded or fail a request.
- **Batched + self-healing.** A background task drains the queue and emits one
  multi-row `INSERT` per batch (or per `FLUSH_MS`); on any database error it drops
  the batch and reconnects on the next round. On shutdown it drains what's buffered.

---

## Querying

Anything that speaks the MySQL wire works. These lean on ElyraSQL's OLAP path
(`FACET()`, `PERCENTILE()`, time-range zone-map skipping); on plain MySQL, use the
standard equivalents.

```sql
-- Error rate per service, 1-minute buckets
SELECT service,
       DATE_FORMAT(ts, '%Y-%m-%d %H:%i:00') AS minute,
       COUNT(*)            AS reqs,
       SUM(status >= 500)  AS errors
FROM logs
WHERE ts >= NOW() - INTERVAL 1 HOUR
GROUP BY service, minute
ORDER BY minute;
```

```sql
-- Slowest endpoints, latency percentiles (ElyraSQL PERCENTILE aggregate)
SELECT path,
       PERCENTILE(latency_ms, 0.50) AS p50,
       PERCENTILE(latency_ms, 0.95) AS p95,
       PERCENTILE(latency_ms, 0.99) AS p99
FROM logs
WHERE service = 'askr' AND ts >= NOW() - INTERVAL 15 MINUTE
GROUP BY path
ORDER BY p95 DESC
LIMIT 20;
```

```sql
-- Faceted log explorer in a single pass (ElyraSQL FACET aggregate)
SELECT FACET(service) AS services,
       FACET(level)   AS levels,
       FACET(status)  AS statuses,
       COUNT(*)       AS total
FROM logs
WHERE MATCH(message) AGAINST('orders')
  AND ts >= NOW() - INTERVAL 1 HOUR;
```

> This is also `/metrics` (Prometheus) territory — the two are complementary. Use
> `/metrics` for live gauges and alerting ([Admin](ADMIN.md)); use the `logs` table
> for per-request forensics, ad-hoc search, and long retention.

---

## Metrics rollup

Alongside the raw `logs`, Askr writes a periodic rollup to a `metrics` table, so
rate/latency dashboards don't have to scan every request. One row every
`ASKR_OBSERV_METRICS_MS` (default 10 s):

```sql
CREATE TABLE IF NOT EXISTS metrics (
  id         BIGINT UNSIGNED PRIMARY KEY AUTO_INCREMENT,
  ts         DATETIME(6) NOT NULL,
  service    VARCHAR(64) NOT NULL,
  host       VARCHAR(64) NOT NULL,
  requests   BIGINT,   errors BIGINT,   bytes_out BIGINT,   -- deltas over the window
  p50_ms     DOUBLE,   p95_ms DOUBLE,   p99_ms DOUBLE,      -- windowed latency percentiles
  inflight   INT
);
```

`requests`/`errors`/`bytes_out` are **per-window deltas**; the percentiles come
from the windowed latency histogram. Because the shared metrics are global across
all workers on a box, **exactly one process** writes the rollup — elected via a
shared-memory PID (re-elected if it dies), so there's no double-counting.

## Traces (OpenTelemetry)

Askr owns the whole request boundary, so it can export a trace that splits the
time PHP-FPM and Octane are blind to. Build with `--features otel` (it's in the
[`-full`](DOCKER.md#-full-variant) image/tarball) and point it at an OTLP/gRPC
collector — Jaeger, Tempo, Grafana Agent, the OTel Collector:

```dotenv
ASKR_OTEL_ENDPOINT=http://127.0.0.1:4317   # enables trace export (OTLP/gRPC)
ASKR_OTEL_SERVICE=askr                      # service.name (default "askr")
```

Each PHP request becomes a root span with a child span per phase:

```
http.request     ── GET /orders/42 · status=200 · cache=MISS · 15.4 ms
├─ php.execute   ── 15.2 ms                    ← the PHP-vs-everything split
└─ response.build ── 0.2 ms                    ← where the rest went (compress)
```

The root `http.request` span carries `http.request.method`, `url.path`,
`http.response.status_code`, `http.response.body.size` and `askr.cache`
(`HIT`/`MISS`/`STALE`); the child spans are the exact wall-clock windows of each
phase. That nesting makes the "PHP is ~99.5 % of the request" reality visible per
request — *and* shows where the remaining fraction went — something a FastCGI
split can't show, because it never sees the PHP boundary.

Try it in 30 seconds:

```bash
docker run -d --name jaeger -e COLLECTOR_OTLP_ENABLED=true \
  -p 16686:16686 -p 4317:4317 jaegertracing/all-in-one
ASKR_OTEL_ENDPOINT=http://127.0.0.1:4317 askr serve --root public …
# then open http://localhost:16686 and pick service "askr"
```

> Spans are exported on a background batch processor, so they never touch request
> latency. v1 traces PHP requests; cache-HIT / static fast paths (sub-ms) show on
> `/metrics` instead.

## Retention

Keep the table bounded out of band — a scheduled job is enough. If you partition
`logs` by day, dropping old data is an O(1) `DROP PARTITION` instead of a row scan:

```sql
-- e.g. nightly, keep 30 days
ALTER TABLE logs DROP PARTITION p_2026_06_16;
```

For dashboards over long windows, roll raw rows up into per-minute summaries on a
schedule (`INSERT … SELECT … GROUP BY service, <minute bucket>`).

---

## Roadmap

Shipped now: **request logs**, **metrics rollup**, and **OpenTelemetry traces**
(root + `php.execute` + `response.build`). Planned:

- **Finer spans** — child spans for `cache` and `coalesce` alongside
  `php.execute`/`response.build`, plus trace/request ids threaded into `attrs`.
- **Trace the fast paths** — cache-HIT / static responses (v1 traces PHP requests
  only).
- **caching_sha2 over plain sockets** — currently needs `ASKR_OBSERV_TLS=1`.

---

## Honest limits

This targets SMB / self-hosted / the Elyra ecosystem's own observability: one
database, plain SQL, easy retention. It is **not** a hyperscale TSDB replacement.
For very high volumes, raise `ASKR_OBSERV_BATCH`, sample at the app layer, or front
it with a dedicated telemetry store.

See also: [Admin dashboard](ADMIN.md) (`/metrics`) · [Configuration](CONFIGURATION.md) ·
[Storage backends](STORAGE_BACKEND.md).
