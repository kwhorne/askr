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

> **Status:** request logs ship today. A metrics-rollup table and trace/span
> export are on the [roadmap](#roadmap) below.

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

Shipped now: **request logs**. Planned:

- **Metrics table** — a periodic snapshot of `metrics::Metrics` (requests, errors,
  bytes, p50/p95/p99, inflight) as one row per interval, so rate/latency panels
  don't have to scan raw `logs`.
- **Trace/span export** — OpenTelemetry-style spans (`php_cpu` vs `cache` vs
  `compress` vs `coalesce`), which Askr can split because it owns the boundary.
- **Richer `attrs`** — request id, authenticated user, trace id.

---

## Honest limits

This targets SMB / self-hosted / the Elyra ecosystem's own observability: one
database, plain SQL, easy retention. It is **not** a hyperscale TSDB replacement.
For very high volumes, raise `ASKR_OBSERV_BATCH`, sample at the app layer, or front
it with a dedicated telemetry store.

See also: [Admin dashboard](ADMIN.md) (`/metrics`) · [Configuration](CONFIGURATION.md) ·
[Storage backends](STORAGE_BACKEND.md).
