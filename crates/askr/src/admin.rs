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
    let pids = s
        .pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"version":"{ver}","listen":"{listen}","mode":"{mode}","uptime_secs":{up},"workers_configured":{wc},"workers_alive":{wa},"respawns":{rs},"pids":[{pids}]}}"#,
        ver = env!("CARGO_PKG_VERSION"),
        listen = info.server_listen,
        mode = info.mode,
        up = s.uptime_secs,
        wc = s.workers_configured,
        wa = s.workers_alive,
        rs = s.respawns,
    )
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
    <tr><td>Worker PIDs</td><td id="pids">—</td></tr>
  </table>
  <button onclick="reload()">Graceful reload</button>
  <span id="msg"></span>
<script>
async function refresh() {
  try {
    const s = await (await fetch('/api/status')).json();
    ver.textContent = 'v' + s.version;
    listen.textContent = s.listen;
    mode.innerHTML = '<span class="pill">' + s.mode + '</span>';
    const h = Math.floor(s.uptime_secs/3600), m = Math.floor(s.uptime_secs%3600/60), sec = s.uptime_secs%60;
    uptime.textContent = h + 'h ' + m + 'm ' + sec + 's';
    workers.textContent = s.workers_alive + ' / ' + s.workers_configured + ' alive';
    respawns.textContent = s.respawns;
    pids.textContent = s.pids.join(', ');
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
