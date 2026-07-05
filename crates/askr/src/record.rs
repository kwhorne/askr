//! Record & replay of failing requests (#5).
//!
//! When a request ends in a 5xx, Askr writes the whole CGI envelope (method,
//! URI, the full `$_SERVER` map, and the raw body) to a directory — one
//! `<id>.json` (metadata) plus a `<id>.bin` (raw body) per failure. Later,
//! `askr replay <id.json>` reconstructs the *exact* request and runs it against
//! a fresh interpreter, so debugging a production 5xx goes from "try to
//! reproduce" to "replay it".
//!
//! Because it captures request bodies, recording is opt-in (`--record-errors
//! <dir>`) and the directory should be treated as sensitive.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use askr_php::Request;
use serde::{Deserialize, Serialize};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The serialized request envelope (body is stored alongside as `<id>.bin`).
#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub status: u16,
    pub recorded_at: u64,
    pub script_filename: String,
    pub method: String,
    pub query_string: String,
    pub content_type: Option<String>,
    pub cookie: Option<String>,
    pub server_vars: Vec<(String, String)>,
    pub body_len: usize,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist a failing request. Best-effort: any I/O error is logged and ignored.
pub fn record_failure(dir: &Path, req: &Request, status: u16) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, "record: mkdir failed");
        return;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("{}-{}-{}", now_secs(), std::process::id(), seq);
    let env = Envelope {
        id: id.clone(),
        status,
        recorded_at: now_secs(),
        script_filename: req.script_filename.clone(),
        method: req.method.clone(),
        query_string: req.query_string.clone(),
        content_type: req.content_type.clone(),
        cookie: req.cookie.clone(),
        server_vars: req.server_vars.clone(),
        body_len: req.body.len(),
    };
    let json = match serde_json::to_vec_pretty(&env) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "record: serialize failed");
            return;
        }
    };
    let _ = std::fs::write(dir.join(format!("{id}.bin")), &req.body);
    if let Err(e) = std::fs::write(dir.join(format!("{id}.json")), json) {
        tracing::warn!(error = %e, "record: write failed");
    } else {
        tracing::info!(id, status, "recorded failing request for replay");
    }
}

/// Load an envelope + its body back into a [`Request`], given the `.json` path.
pub fn load(json_path: &Path) -> anyhow::Result<Request> {
    let text = std::fs::read(json_path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", json_path.display()))?;
    let env: Envelope = serde_json::from_slice(&text)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", json_path.display()))?;
    let body = std::fs::read(json_path.with_extension("bin")).unwrap_or_default();
    Ok(Request {
        script_filename: env.script_filename,
        method: env.method,
        query_string: env.query_string,
        content_type: env.content_type,
        cookie: env.cookie,
        body,
        server_vars: env.server_vars,
        post_fields: Vec::new(),
        files: Vec::new(),
    })
}

/// List recorded failures in a directory (most recent first), as `(id, status)`.
pub fn list(dir: &Path) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) == Some("json") {
            if let Ok(text) = std::fs::read(&path) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&text) {
                    out.push((env.id, env.status));
                }
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}
