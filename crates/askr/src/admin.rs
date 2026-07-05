//! Admin dashboard + API — the built-in "GUI" for maintaining/configuring a
//! running server. It runs in the master process (its own thread + tiny tokio
//! runtime) and exposes:
//!
//!   GET  /            a minimal HTML dashboard (auto-refreshing)
//!   GET  /api/status  supervisor status as JSON
//!   POST /api/reload  trigger a graceful rolling reload
//!
//! Bind it to localhost (default in examples) or reach it over a private
//! network / SSH tunnel. A future desktop control-center (Grove-style) can drive
//! several servers through this same API.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::thread;

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// Static server info shown on the dashboard.
#[derive(Clone)]
pub struct Info {
    pub server_listen: SocketAddr,
    pub mode: &'static str,
    pub record_dir: Option<std::path::PathBuf>,
}

/// Start the admin server on its own thread. Never blocks the caller.
pub fn spawn(addr: SocketAddr, info: Info) {
    thread::Builder::new()
        .name("askr-admin".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "admin: runtime");
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(error = %e, %addr, "admin: bind failed");
                        return;
                    }
                };
                tracing::info!(%addr, "admin dashboard listening");
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let io = TokioIo::new(stream);
                    let info = info.clone();
                    tokio::task::spawn(async move {
                        let service = service_fn(move |req| handle(req, info.clone()));
                        let _ = http1::Builder::new().serve_connection(io, service).await;
                    });
                }
            });
        })
        .ok();
}

async fn handle(
    req: Request<hyper::body::Incoming>,
    info: Info,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let resp = match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => html(DASHBOARD),
        (&Method::GET, "/api/status") => json(status_json(&info)),
        (&Method::GET, "/api/metrics") => json(metrics_json()),
        (&Method::GET, "/metrics") => prometheus(),
        (&Method::GET, "/api/errors") => json(errors_json(&info)),
        (&Method::POST, "/api/reload") => {
            crate::trigger_reload();
            json(r#"{"ok":true,"action":"reload"}"#.to_string())
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("not found")))
            .unwrap(),
    };
    Ok(resp)
}

fn status_json(info: &Info) -> String {
    let s = crate::status();
    let mut rss_total = 0u64;
    let workers = s
        .pids
        .iter()
        .map(|&p| {
            let rss = crate::metrics::rss_kb(p).unwrap_or(0);
            rss_total += rss;
            format!(r#"{{"pid":{p},"rss_kb":{rss}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    let pids = s
        .pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"version":"{ver}","listen":"{listen}","mode":"{mode}","uptime_secs":{up},"workers_configured":{wc},"workers_alive":{wa},"respawns":{rs},"rss_kb_total":{rss},"workers":[{workers}],"pids":[{pids}]}}"#,
        ver = env!("CARGO_PKG_VERSION"),
        listen = info.server_listen,
        mode = info.mode,
        up = s.uptime_secs,
        wc = s.workers_configured,
        wa = s.workers_alive,
        rs = s.respawns,
        rss = rss_total,
    )
}

fn metrics_json() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(m) = crate::metrics::Metrics::get() else {
        return "{}".to_string();
    };
    let req = m.requests.load(Relaxed);
    let php = m.php_us.load(Relaxed);
    let total = m.total_us.load(Relaxed);
    let (avg_total_ms, avg_php_ms) = if req > 0 {
        (
            total as f64 / req as f64 / 1000.0,
            php as f64 / req as f64 / 1000.0,
        )
    } else {
        (0.0, 0.0)
    };
    let php_pct = php.saturating_mul(100).checked_div(total).unwrap_or(0);
    let st: Vec<u64> = (0..5).map(|i| m.status[i].load(Relaxed)).collect();
    let buckets = m.bucket_counts();
    let bounds = crate::metrics::BUCKET_BOUNDS_MS;
    let bounds_s = bounds
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let counts_s = buckets
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let (chits, cmisses, ccoalesced) = crate::rcache::stats();
    let ctotal = chits + cmisses;
    let chit_pct = chits.saturating_mul(100).checked_div(ctotal).unwrap_or(0);
    format!(
        r#"{{"requests":{req},"errors":{err},"bytes_out":{bytes},"avg_total_ms":{att:.2},"avg_php_ms":{aph:.2},"php_pct":{php_pct},"io_pct":{io_pct},"slowest_ms":{slow:.2},"cache":{{"hits":{chits},"misses":{cmisses},"coalesced":{ccoalesced},"hit_pct":{chit_pct}}},"status":{{"1xx":{s1},"2xx":{s2},"3xx":{s3},"4xx":{s4},"5xx":{s5}}},"histogram":{{"bounds_ms":[{bounds_s}],"counts":[{counts_s}]}}}}"#,
        req = req,
        err = m.errors.load(Relaxed),
        bytes = m.bytes_out.load(Relaxed),
        att = avg_total_ms,
        aph = avg_php_ms,
        php_pct = php_pct,
        io_pct = 100 - php_pct,
        slow = m.slowest_us.load(Relaxed) as f64 / 1000.0,
        s1 = st[0],
        s2 = st[1],
        s3 = st[2],
        s4 = st[3],
        s5 = st[4],
    )
}

fn errors_json(info: &Info) -> String {
    let Some(dir) = &info.record_dir else {
        return r#"{"enabled":false,"errors":[]}"#.to_string();
    };
    let items = crate::record::list(dir)
        .into_iter()
        .take(20)
        .map(|(id, status)| format!(r#"{{"id":"{id}","status":{status}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"enabled":true,"errors":[{items}]}}"#)
}

fn push_counter(s: &mut String, name: &str, help: &str, val: &str) {
    use std::fmt::Write;
    let _ = write!(
        s,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n"
    );
}

/// Prometheus text-format exposition of the shared metrics (`GET /metrics`).
fn prometheus() -> Response<Full<Bytes>> {
    use std::fmt::Write;
    use std::sync::atomic::Ordering::Relaxed;
    let mut s = String::new();
    let Some(m) = crate::metrics::Metrics::get() else {
        return text_plain(s);
    };

    push_counter(
        &mut s,
        "askr_requests_total",
        "Total HTTP requests served.",
        &m.requests.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_errors_total",
        "Requests that failed at the server layer.",
        &m.errors.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_bytes_out_total",
        "Response bytes sent.",
        &m.bytes_out.load(Relaxed).to_string(),
    );
    push_counter(
        &mut s,
        "askr_php_seconds_total",
        "Cumulative time spent in PHP.",
        &format!("{:.6}", m.php_us.load(Relaxed) as f64 / 1e6),
    );
    push_counter(
        &mut s,
        "askr_request_seconds_total",
        "Cumulative total request time.",
        &format!("{:.6}", m.total_us.load(Relaxed) as f64 / 1e6),
    );
    push_counter(
        &mut s,
        "askr_cache_evictions_total",
        "KV cache entries evicted under pressure.",
        &m.cache_evictions.load(Relaxed).to_string(),
    );

    // Response status classes.
    let _ = write!(
        s,
        "# HELP askr_responses_total Responses by status class.\n# TYPE askr_responses_total counter\n"
    );
    for (i, class) in ["1xx", "2xx", "3xx", "4xx", "5xx"].iter().enumerate() {
        let _ = writeln!(
            s,
            "askr_responses_total{{class=\"{class}\"}} {}",
            m.status[i].load(Relaxed)
        );
    }

    // Response cache.
    let (hits, misses, coalesced) = crate::rcache::stats();
    push_counter(
        &mut s,
        "askr_cache_hits_total",
        "Response cache hits.",
        &hits.to_string(),
    );
    push_counter(
        &mut s,
        "askr_cache_misses_total",
        "Response cache misses.",
        &misses.to_string(),
    );
    push_counter(
        &mut s,
        "askr_cache_coalesced_total",
        "Requests served by coalescing onto a leader.",
        &coalesced.to_string(),
    );

    // Gauges.
    let _ = write!(
        s,
        "# HELP askr_inflight Requests currently executing in PHP.\n# TYPE askr_inflight gauge\naskr_inflight {}\n",
        m.inflight.load(Relaxed)
    );
    let st = crate::status();
    let _ = write!(
        s,
        "# HELP askr_workers_alive Live worker processes.\n# TYPE askr_workers_alive gauge\naskr_workers_alive {}\n",
        st.workers_alive
    );

    // Latency histogram (cumulative buckets, seconds).
    let buckets = m.bucket_counts();
    let bounds = crate::metrics::BUCKET_BOUNDS_MS;
    let _ = write!(
        s,
        "# HELP askr_request_duration_seconds Request latency.\n# TYPE askr_request_duration_seconds histogram\n"
    );
    let mut cum = 0u64;
    for (i, &bound) in bounds.iter().enumerate() {
        cum += buckets[i];
        let _ = writeln!(
            s,
            "askr_request_duration_seconds_bucket{{le=\"{:.3}\"}} {cum}",
            bound as f64 / 1000.0
        );
    }
    cum += buckets[bounds.len()]; // overflow bucket
    let _ = write!(
        s,
        "askr_request_duration_seconds_bucket{{le=\"+Inf\"}} {cum}\naskr_request_duration_seconds_count {cum}\naskr_request_duration_seconds_sum {:.6}\n",
        m.total_us.load(Relaxed) as f64 / 1e6
    );

    text_plain(s)
}

fn text_plain(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            hyper::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn json(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn html(body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap()
}

const DASHBOARD: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Askr admin</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root { color-scheme: light dark; }
  body { font: 15px/1.5 system-ui, sans-serif; max-width: 720px; margin: 3rem auto; padding: 0 1rem; }
  h1 { font-size: 1.4rem; } h1 small { color: #888; font-weight: 400; font-size: .7em; }
  table { border-collapse: collapse; width: 100%; margin: 1rem 0; }
  td { padding: .4rem .6rem; border-bottom: 1px solid #8883; }
  td:first-child { color: #888; width: 40%; }
  .pill { display:inline-block; padding:.1rem .5rem; border-radius:1rem; background:#3a7afe22; color:#3a7afe; }
  button { font: inherit; padding: .5rem 1rem; border: 0; border-radius: .4rem; background: #3a7afe; color: #fff; cursor: pointer; }
  button:active { transform: translateY(1px); }
  #msg { margin-left: 1rem; color: #2a2; }
</style></head>
<body>
  <h1>🌳 Askr <small id="ver"></small></h1>
  <table>
    <tr><td>Listening</td><td id="listen">—</td></tr>
    <tr><td>Mode</td><td id="mode">—</td></tr>
    <tr><td>Uptime</td><td id="uptime">—</td></tr>
    <tr><td>Workers</td><td id="workers">—</td></tr>
    <tr><td>Respawns</td><td id="respawns">—</td></tr>
    <tr><td>Memory (RSS)</td><td id="rss">—</td></tr>
    <tr><td>Worker PIDs</td><td id="pids">—</td></tr>
  </table>

  <h2 style="font-size:1.1rem;margin-top:2rem">Traffic</h2>
  <table>
    <tr><td>Throughput</td><td id="rps">—</td></tr>
    <tr><td>Requests</td><td id="requests">—</td></tr>
    <tr><td>Avg latency</td><td id="avglat">—</td></tr>
    <tr><td>PHP vs I/O</td><td id="split">—</td></tr>
    <tr><td>Response cache</td><td id="cache">—</td></tr>
    <tr><td>Slowest</td><td id="slowest">—</td></tr>
    <tr><td>Status</td><td id="status">—</td></tr>
    <tr><td>Latency</td><td id="hist" style="font:12px/1.4 ui-monospace,monospace">—</td></tr>
  </table>

  <h2 style="font-size:1.1rem;margin-top:2rem">Recorded failures <small style="color:#888;font-weight:400">— <code>askr replay &lt;id&gt;.json</code></small></h2>
  <div id="errors" style="font:13px/1.6 ui-monospace,monospace;color:#888">—</div>

  <button onclick="reload()">Graceful reload</button>
  <span id="msg"></span>
<script>
let last = null;
function bar(pct){ pct=Math.max(0,Math.min(100,pct)); return '<span style="display:inline-block;height:.8em;width:'+pct+'%;background:#3a7afe;border-radius:2px"></span>'; }
async function refresh() {
  try {
    const s = await (await fetch('/api/status')).json();
    ver.textContent = 'v' + s.version;
    listen.textContent = s.listen;
    mode.innerHTML = '<span class="pill">' + s.mode + '</span>';
    const h = Math.floor(s.uptime_secs/3600), mn = Math.floor(s.uptime_secs%3600/60), sec = s.uptime_secs%60;
    uptime.textContent = h + 'h ' + mn + 'm ' + sec + 's';
    workers.textContent = s.workers_alive + ' / ' + s.workers_configured + ' alive';
    respawns.textContent = s.respawns;
    rss.textContent = (s.rss_kb_total/1024).toFixed(0) + ' MB' +
      (s.workers && s.workers.length ? '  (' + s.workers.map(w => (w.rss_kb/1024).toFixed(0)).join(', ') + ' MB)' : '');
    pids.textContent = s.pids.join(', ');

    const m = await (await fetch('/api/metrics')).json();
    const now = performance.now();
    if (last && m.requests >= last.requests) {
      const dr = m.requests - last.requests, dt = (now - last.t) / 1000;
      rps.textContent = dt > 0 ? (dr/dt).toFixed(0) + ' req/s' : '—';
    }
    last = { requests: m.requests, t: now };
    requests.textContent = m.requests + (m.errors ? '  (' + m.errors + ' errors)' : '');
    avglat.textContent = (m.avg_total_ms||0).toFixed(1) + ' ms';
    split.innerHTML = 'PHP ' + m.php_pct + '%  ' + bar(m.php_pct) + '  I/O ' + m.io_pct + '%';
    const c = m.cache || {hits:0,misses:0,coalesced:0,hit_pct:0};
    cache.textContent = (c.hits+c.misses) ? (c.hit_pct + '% hit  (' + c.hits + ' hits, ' + c.misses + ' misses, ' + (c.coalesced||0) + ' coalesced)') : 'no lookups';
    slowest.textContent = (m.slowest_ms||0).toFixed(1) + ' ms';
    const st = m.status || {};
    status.textContent = ['2xx','3xx','4xx','5xx'].map(k => k+':'+(st[k]||0)).join('  ');
    const b = m.histogram || {bounds_ms:[],counts:[]};
    const max = Math.max(1, ...b.counts);
    const labels = b.bounds_ms.map(x => '≤'+x+'ms').concat(['>'+b.bounds_ms[b.bounds_ms.length-1]+'ms']);
    hist.innerHTML = b.counts.map((c,i) =>
      labels[i].padStart(8) + ' ' + '█'.repeat(Math.round(c/max*24)) + ' ' + c
    ).filter((_,i)=> b.counts[i]>0).join('<br>') || '(no traffic yet)';

    const er = await (await fetch('/api/errors')).json();
    if (!er.enabled) { errors.textContent = 'disabled (start with --record-errors <dir>)'; }
    else if (!er.errors.length) { errors.textContent = 'none recorded 🎉'; }
    else { errors.innerHTML = er.errors.map(e => 'HTTP ' + e.status + '  ' + e.id + '.json').join('<br>'); }
  } catch (e) {}
}
async function reload() {
  msg.textContent = 'reloading…';
  await fetch('/api/reload', { method: 'POST' });
  msg.textContent = 'rolling reload triggered';
  setTimeout(() => { msg.textContent = ''; refresh(); }, 2000);
}
refresh(); setInterval(refresh, 2000);
</script>
</body></html>"#;
