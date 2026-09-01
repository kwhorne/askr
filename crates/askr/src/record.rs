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
//! <dir>`) and the directory should be treated as sensitive: a 5xx on a login form
//! writes that form's body — the password — to disk. The directory is created `0700`
//! and each file `0600`, and credential-bearing headers (`Cookie`, `Authorization`,
//! `Proxy-Authorization`) are replaced with a marker before the envelope is written.
//! A replay of an auth-dependent failure therefore runs unauthenticated; that is the
//! trade, and it is the right one for a file that outlives the incident.

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

/// What a credential is replaced with in a recorded envelope. Kept as a value rather
/// than dropping the key, so a replay still knows the header was there.
pub const REDACTED: &str = "[redacted]";

/// `$_SERVER` keys whose values are credentials.
const REDACT_KEYS: [&str; 3] = [
    "HTTP_COOKIE",
    "HTTP_AUTHORIZATION",
    "HTTP_PROXY_AUTHORIZATION",
];

/// Replace credentials in the `$_SERVER` map with [`REDACTED`].
fn redact(server_vars: &[(String, String)]) -> Vec<(String, String)> {
    server_vars
        .iter()
        .map(|(k, v)| {
            if REDACT_KEYS.iter().any(|r| k.eq_ignore_ascii_case(r)) {
                (k.clone(), REDACTED.to_string())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

/// Create a file readable by this user only, refusing to follow a symlink or reuse a
/// name. The recorder writes into a directory an operator named; the file must not be
/// something another user planted there.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)?.write_all(bytes)
}

/// Persist a failing request. Best-effort: any I/O error is logged and ignored.
pub fn record_failure(dir: &Path, req: &Request, status: u16) {
    let mkdir = {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)
    };
    if let Err(e) = mkdir {
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
        cookie: req.cookie.as_ref().map(|_| REDACTED.to_string()),
        server_vars: redact(&req.server_vars),
        body_len: req.body.len(),
    };
    let json = match serde_json::to_vec_pretty(&env) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "record: serialize failed");
            return;
        }
    };
    // The body is kept as sent — it is what makes a replay a replay — which is why it,
    // too, is 0600 and why the directory is documented as sensitive.
    let _ = write_private(&dir.join(format!("{id}.bin")), &req.body);
    if let Err(e) = write_private(&dir.join(format!("{id}.json")), &json) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A 5xx on a login form used to write the session cookie, the bearer token and
    /// the form body — the password — world-readable under a 022 umask. The body is
    /// still kept (it is the replay), so the files must be private and the credentials
    /// in the envelope must not be there at all.
    #[cfg(unix)]
    #[test]
    fn a_recorded_failure_is_private_and_carries_no_credentials() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("askr-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let req = Request {
            script_filename: "/srv/index.php".into(),
            method: "POST".into(),
            query_string: String::new(),
            content_type: Some("application/x-www-form-urlencoded".into()),
            cookie: Some("laravel_session=abc".into()),
            body: b"email=a%40b.c&password=hunter2".to_vec(),
            server_vars: vec![
                ("HTTP_COOKIE".into(), "laravel_session=abc".into()),
                ("HTTP_AUTHORIZATION".into(), "Bearer eyJ".into()),
                ("HTTP_USER_AGENT".into(), "curl".into()),
            ],
            post_fields: Vec::new(),
            files: Vec::new(),
        };
        record_failure(&dir, &req, 500);

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "the directory is private");

        let mut jsons: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        assert_eq!(jsons.len(), 1);
        let json = jsons.pop().unwrap();
        assert_eq!(mode(&json), 0o600, "the envelope is private");
        assert_eq!(
            mode(&json.with_extension("bin")),
            0o600,
            "the body is private"
        );

        let text = std::fs::read_to_string(&json).unwrap();
        assert!(
            !text.contains("laravel_session=abc"),
            "no cookie value: {text}"
        );
        assert!(!text.contains("Bearer eyJ"), "no bearer token: {text}");
        assert!(
            text.contains(REDACTED),
            "the marker says a credential was there"
        );
        assert!(
            text.contains("\"HTTP_USER_AGENT\""),
            "ordinary headers survive"
        );

        // Replay still reconstructs the request — unauthenticated, by design.
        let back = load(&json).unwrap();
        assert_eq!(back.cookie.as_deref(), Some(REDACTED));
        assert_eq!(
            back.body, req.body,
            "the body is what makes a replay a replay"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
