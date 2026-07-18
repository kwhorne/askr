//! Telemetry sink → ElyraSQL (observability store).
//!
//! Askr already *emits* signals (the structured access log in
//! [`crate::server`], the in-memory [`crate::metrics`]). This ships them to an
//! ElyraSQL database over the MySQL wire protocol, where they become an OLAP
//! workload for dashboards in Conductor. See `docs/OBSERVABILITY.md`.
//!
//! Design guarantees:
//!   * **Never blocks or fails a request.** The hot path only does a
//!     non-blocking `try_send` into a bounded queue; on backpressure the row is
//!     dropped and a counter incremented.
//!   * **Batched.** A single background task drains the queue and emits one
//!     multi-row `INSERT` per batch (or per flush interval) — the shape ElyraSQL
//!     group-commits efficiently.
//!   * **Off by default.** Enabled only when `ASKR_OBSERV_DSN` is set.
//!
//! Configuration (all env, all optional except the DSN):
//!   ASKR_OBSERV_DSN        mysql://user:pass@host:port/db   (enables the sink)
//!   ASKR_OBSERV_SERVICE    logical service name             (default "askr")
//!   ASKR_OBSERV_BATCH      rows per INSERT                  (default 1000)
//!   ASKR_OBSERV_FLUSH_MS   max buffering latency in ms      (default 1000)
//!   ASKR_OBSERV_QUEUE      bounded queue capacity           (default 65536)

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mysql_async::prelude::*;
use mysql_async::Value as MyValue;
use tokio::sync::mpsc;

/// One log event queued for insertion.
pub struct LogRow {
    pub ts_us: i64,
    pub level: &'static str,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub latency_ms: f64,
    pub ip: String,
}

struct Cfg {
    dsn: String,
    service: String,
    host: String,
    batch: usize,
    flush: Duration,
    queue: usize,
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A handle the request hot path uses to enqueue log rows.
pub struct TelemetrySink {
    tx: mpsc::Sender<LogRow>,
    dropped: AtomicU64,
}

impl TelemetrySink {
    /// Build and spawn the sink from the environment, or `None` if
    /// `ASKR_OBSERV_DSN` is unset (feature off). Call inside a Tokio runtime.
    pub fn from_env() -> Option<TelemetrySink> {
        let dsn = std::env::var("ASKR_OBSERV_DSN")
            .ok()
            .filter(|s| !s.is_empty())?;
        let cfg = Cfg {
            dsn,
            service: std::env::var("ASKR_OBSERV_SERVICE").unwrap_or_else(|_| "askr".into()),
            host: std::env::var("ASKR_OBSERV_HOST")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "unknown".into()),
            batch: env_usize("ASKR_OBSERV_BATCH", 1000).max(1),
            flush: Duration::from_millis(env_usize("ASKR_OBSERV_FLUSH_MS", 1000) as u64),
            queue: env_usize("ASKR_OBSERV_QUEUE", 65536).max(64),
        };
        // Metrics rollup: exactly one process snapshots the *shared* metrics into
        // the `metrics` table (all workers write the same global counters, so
        // duplicate writers would double-count). Elect a leader via a shared PID.
        if claim_metrics_leader() {
            let interval =
                Duration::from_millis(env_usize("ASKR_OBSERV_METRICS_MS", 10_000).max(1000) as u64);
            let mcfg = MetricsCfg {
                dsn: cfg.dsn.clone(),
                service: cfg.service.clone(),
                host: cfg.host.clone(),
                interval,
            };
            tokio::spawn(run_metrics_sink(mcfg));
            tracing::info!("observability: metrics rollup writer elected");
        }

        let (tx, rx) = mpsc::channel(cfg.queue);
        tokio::spawn(run_sink(cfg, rx));
        tracing::info!("observability: telemetry sink → ElyraSQL enabled");
        Some(TelemetrySink {
            tx,
            dropped: AtomicU64::new(0),
        })
    }

    /// Enqueue a row. Non-blocking; drops (counting) under backpressure so the
    /// request path is never stalled by telemetry.
    #[inline]
    pub fn log(&self, row: LogRow) {
        if self.tx.try_send(row).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(dropped = n, "observability: telemetry queue full, dropping");
            }
        }
    }
}

/// Background task: own the connection, batch, and bulk-insert.
async fn run_sink(cfg: Cfg, mut rx: mpsc::Receiver<LogRow>) {
    let mut conn: Option<mysql_async::Conn> = None;
    let mut buf: Vec<LogRow> = Vec::with_capacity(cfg.batch);
    loop {
        // Block for the first row, but wake on the flush interval to drain a
        // partial batch.
        match tokio::time::timeout(cfg.flush, rx.recv()).await {
            Ok(Some(first)) => {
                buf.push(first);
                while buf.len() < cfg.batch {
                    match rx.try_recv() {
                        Ok(r) => buf.push(r),
                        Err(_) => break,
                    }
                }
            }
            Ok(None) => {
                // Senders dropped: final drain and exit.
                flush(&cfg, &mut conn, &mut buf).await;
                break;
            }
            Err(_) => {} // timeout: flush whatever is buffered
        }
        if !buf.is_empty() {
            flush(&cfg, &mut conn, &mut buf).await;
        }
    }
}

/// Ensure a live connection (with the schema migrated), then emit one multi-row
/// INSERT for the buffered rows. On error the connection is dropped (reconnect
/// next round); rows are dropped rather than retried indefinitely, to keep memory
/// bounded — telemetry favours availability over completeness.
async fn flush(cfg: &Cfg, conn: &mut Option<mysql_async::Conn>, buf: &mut Vec<LogRow>) {
    if conn.is_none() {
        match connect(cfg).await {
            Ok(c) => *conn = Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "observability: connect failed; dropping batch");
                buf.clear();
                return;
            }
        }
    }
    let c = conn.as_mut().unwrap();
    if let Err(e) = insert_batch(cfg, c, buf).await {
        tracing::warn!(error = %e, "observability: insert failed; will reconnect");
        *conn = None; // force reconnect next round
    }
    buf.clear();
}

/// Parse the DSN into connection options. When `ASKR_OBSERV_TLS` is set (or the
/// DSN has `?tls=1`), enable TLS — required by MySQL 8 / MariaDB 11 defaults
/// (`caching_sha2_password`), which can't complete auth over a plain socket.
/// Invalid/self-signed certs are accepted (telemetry, not a trust boundary).
fn dsn_opts(dsn: &str) -> Result<mysql_async::Opts, mysql_async::Error> {
    let tls = std::env::var("ASKR_OBSERV_TLS")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
        || dsn.contains("tls=1");
    let clean = dsn.split("?tls=1").next().unwrap_or(dsn);
    let opts = mysql_async::Opts::from_url(clean)?;
    if !tls {
        return Ok(opts);
    }
    let ssl = mysql_async::SslOpts::default()
        .with_danger_accept_invalid_certs(true)
        .with_danger_skip_domain_validation(true);
    Ok(mysql_async::OptsBuilder::from_opts(opts)
        .ssl_opts(ssl)
        .into())
}

async fn connect(cfg: &Cfg) -> Result<mysql_async::Conn, mysql_async::Error> {
    let opts = dsn_opts(&cfg.dsn)?;
    let mut conn = mysql_async::Conn::new(opts).await?;
    // Idempotent schema setup (day-partition + retention handled out of band).
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS logs (\
         id BIGINT UNSIGNED PRIMARY KEY AUTO_INCREMENT, \
         ts DATETIME(6) NOT NULL, \
         service VARCHAR(64) NOT NULL, \
         host VARCHAR(64) NOT NULL, \
         level VARCHAR(8) NOT NULL, \
         method VARCHAR(8), path VARCHAR(255), \
         status SMALLINT, latency_ms INT, \
         message TEXT, attrs JSON)",
    )
    .await?;
    Ok(conn)
}

async fn insert_batch(
    cfg: &Cfg,
    conn: &mut mysql_async::Conn,
    buf: &[LogRow],
) -> Result<(), mysql_async::Error> {
    const COLS: usize = 10;
    let mut sql = String::from(
        "INSERT INTO logs (ts, service, host, level, method, path, status, latency_ms, message, attrs) VALUES ",
    );
    for i in 0..buf.len() {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str("(?,?,?,?,?,?,?,?,?,?)");
    }
    let mut params: Vec<MyValue> = Vec::with_capacity(buf.len() * COLS);
    for r in buf {
        // ElyraSQL accepts a 'YYYY-MM-DD HH:MM:SS.ffffff' string for DATETIME(6).
        params.push(MyValue::from(fmt_ts(r.ts_us)));
        params.push(MyValue::from(cfg.service.as_str()));
        params.push(MyValue::from(cfg.host.as_str()));
        params.push(MyValue::from(r.level));
        params.push(MyValue::from(r.method.as_str()));
        params.push(MyValue::from(r.path.as_str()));
        params.push(MyValue::from(r.status));
        params.push(MyValue::from(r.latency_ms));
        params.push(MyValue::from(format!("{} {}", r.method, r.path)));
        params.push(MyValue::from(format!(
            "{{\"ip\":\"{}\"}}",
            r.ip.replace('"', "")
        )));
    }
    conn.exec_drop(sql, params).await
}

/// Microseconds-since-epoch → `YYYY-MM-DD HH:MM:SS.ffffff` (UTC).
fn fmt_ts(us: i64) -> String {
    let secs = us.div_euclid(1_000_000);
    let micros = us.rem_euclid(1_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{micros:06}")
}

/// days since 1970-01-01 → (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (y + i64::from(m <= 2), m, d)
}

// --- metrics rollup (elected single writer) -------------------------------

struct MetricsCfg {
    dsn: String,
    service: String,
    host: String,
    interval: Duration,
}

#[derive(Default, Clone, Copy)]
struct Snap {
    requests: u64,
    errors: u64,
    bytes: u64,
    buckets: [u64; 13],
}

/// Become the metrics-rollup writer iff the slot is free or the recorded leader
/// PID is dead — one winner per box (the shared metrics are global).
fn claim_metrics_leader() -> bool {
    use std::sync::atomic::Ordering;
    let Some(m) = crate::metrics::Metrics::get() else {
        return false;
    };
    let me = (unsafe { libc::getpid() }) as u32;
    loop {
        let cur = m.metrics_leader.load(Ordering::Acquire);
        if cur == me {
            return true;
        }
        if cur == 0 || !pid_alive(cur) {
            if m.metrics_leader
                .compare_exchange(cur, me, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
            continue;
        }
        return false;
    }
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Snapshot the shared metrics into the `metrics` table every `interval`, as
/// per-window deltas + windowed latency percentiles.
async fn run_metrics_sink(cfg: MetricsCfg) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut conn: Option<mysql_async::Conn> = None;
    let mut prev = Snap::default();
    let mut first = true;
    let mut tick = tokio::time::interval(cfg.interval);
    loop {
        tick.tick().await;
        let Some(m) = crate::metrics::Metrics::get() else {
            continue;
        };
        let cur = Snap {
            requests: m.requests.load(Relaxed),
            errors: m.errors.load(Relaxed),
            bytes: m.bytes_out.load(Relaxed),
            buckets: m.bucket_counts(),
        };
        let inflight = m.inflight.load(Relaxed);
        if first {
            first = false;
            prev = cur;
            continue; // baseline before the first delta
        }
        let d_req = cur.requests.wrapping_sub(prev.requests);
        let d_err = cur.errors.wrapping_sub(prev.errors);
        let d_bytes = cur.bytes.wrapping_sub(prev.bytes);
        let mut d_buckets = [0u64; 13];
        for (d, (c, p)) in d_buckets
            .iter_mut()
            .zip(cur.buckets.iter().zip(prev.buckets.iter()))
        {
            *d = c.wrapping_sub(*p);
        }
        prev = cur;
        let total: u64 = d_buckets.iter().sum();

        if conn.is_none() {
            match connect_metrics(&cfg.dsn).await {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "observability: metrics connect failed");
                    continue;
                }
            }
        }
        let c = conn.as_mut().unwrap();
        let params: Vec<MyValue> = vec![
            MyValue::from(fmt_ts(now_us())),
            MyValue::from(cfg.service.as_str()),
            MyValue::from(cfg.host.as_str()),
            MyValue::from(d_req),
            MyValue::from(d_err),
            MyValue::from(d_bytes),
            MyValue::from(percentile(&d_buckets, total, 0.50)),
            MyValue::from(percentile(&d_buckets, total, 0.95)),
            MyValue::from(percentile(&d_buckets, total, 0.99)),
            MyValue::from(inflight),
        ];
        let sql = "INSERT INTO metrics (ts,service,host,requests,errors,bytes_out,p50_ms,p95_ms,p99_ms,inflight) \
                   VALUES (?,?,?,?,?,?,?,?,?,?)";
        if let Err(e) = c.exec_drop(sql, params).await {
            tracing::warn!(error = %e, "observability: metrics insert failed; reconnecting");
            conn = None;
        }
    }
}

async fn connect_metrics(dsn: &str) -> Result<mysql_async::Conn, mysql_async::Error> {
    let opts = dsn_opts(dsn)?;
    let mut conn = mysql_async::Conn::new(opts).await?;
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS metrics (\
         id BIGINT UNSIGNED PRIMARY KEY AUTO_INCREMENT, \
         ts DATETIME(6) NOT NULL, \
         service VARCHAR(64) NOT NULL, \
         host VARCHAR(64) NOT NULL, \
         requests BIGINT, errors BIGINT, bytes_out BIGINT, \
         p50_ms DOUBLE, p95_ms DOUBLE, p99_ms DOUBLE, \
         inflight INT)",
    )
    .await?;
    Ok(conn)
}

/// Windowed percentile from the latency histogram deltas: the upper bound (ms) of
/// the bucket where the cumulative count crosses `q`.
fn percentile(buckets: &[u64; 13], total: u64, q: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let bounds = &crate::metrics::BUCKET_BOUNDS_MS;
    let overflow = (bounds[bounds.len() - 1] as f64) * 2.0;
    let target = (q * total as f64).ceil() as u64;
    let mut cum = 0u64;
    for (i, &c) in buckets.iter().enumerate() {
        cum += c;
        if cum >= target {
            return bounds.get(i).map(|&b| b as f64).unwrap_or(overflow);
        }
    }
    overflow
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
