//! Automatic TLS via ACME (Let's Encrypt) — HTTP-01, master-coordinated.
//!
//! The prefork model makes ACME challenge routing tricky (a validation
//! connection hits a random worker). Askr sidesteps it: the **master** obtains
//! the certificate (running a tiny HTTP-01 challenge server on port 80) *before*
//! forking workers, and again on renewal — workers only ever serve HTTPS on 443
//! from the cached cert, so there's no port conflict and no cross-worker
//! challenge coordination. The completes the "single binary, no proxy" story.
//!
//! On obtain, `<cache>/cert.pem` + `<cache>/key.pem` are written (plus an account
//! and a `renew_at` marker); workers build their `TlsAcceptor` from those.
//!
//! The plain-HTTP listener (`spawn_front`) lives for the whole process, not just for an
//! issuance. That's what lets `force_https` actually redirect: a TLS listener never sees
//! a plain-HTTP request, so without something on port 80 a visitor typing `http://…`
//! got a connection failure. One listener now answers challenges *and* redirects, so
//! there's no second process fighting for the port.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use bytes::Bytes;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use instant_acme::{
    Account, AccountBuilder, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use tokio::net::TcpListener;

/// Settings for obtaining/renewing a certificate.
#[derive(Clone)]
pub struct AcmeConfig {
    pub domains: Vec<String>,
    pub email: String,
    pub cache_dir: PathBuf,
    pub directory_url: String,
    /// Where to answer HTTP-01 challenges (e.g. `0.0.0.0:80`).
    pub challenge_addr: SocketAddr,
    /// A custom CA root PEM to trust the ACME directory (for Pebble/testing).
    pub ca_root: Option<PathBuf>,
    /// Renew this many days before the marker (default handling in `needs_renewal`).
    pub renew_after_days: u64,
}

type Challenges = Arc<Mutex<HashMap<String, String>>>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn cert_path(dir: &Path) -> PathBuf {
    dir.join("cert.pem")
}
pub fn key_path(dir: &Path) -> PathBuf {
    dir.join("key.pem")
}

/// True if there's no cached cert or the renewal marker has passed.
/// Write a file only the owner can read.
///
/// `std::fs::write` uses the process umask, which is typically 022 — so a TLS private key
/// and the ACME account credentials landed as 0644, readable by every local user. The
/// mode is set at creation rather than chmod'ed afterwards, so there's no window where
/// the key exists with wider permissions.
/// Write `bytes` to a sibling temporary file and `rename()` it into place, so a
/// reader either sees the whole previous file or the whole new one and never a
/// half-written or mode-0644-for-an-instant version of either.
///
/// The temp file carries the final mode from creation — creating it 0644 and
/// tightening afterwards would expose a private key for the interval between.
fn stage_and_rename(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;

    // A leftover temp from a killed process must not fail the renewal forever.
    let _ = std::fs::remove_file(&tmp);
    let mut f = opts.open(&tmp)?;
    let write = f.write_all(bytes).and_then(|()| f.sync_all());
    if let Err(e) = write {
        drop(f);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    // An existing file keeps its old mode, so tighten it too (an upgrade from a version
    // that wrote 0644 must not leave the key readable).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn needs_renewal(dir: &Path) -> bool {
    if !cert_path(dir).exists() || !key_path(dir).exists() {
        return true;
    }
    match std::fs::read_to_string(dir.join("renew_at"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(renew_at) => now_secs() >= renew_at,
        None => true,
    }
}

/// Obtain (or renew) the certificate. Blocking wrapper — runs its own runtime
/// and a temporary HTTP-01 challenge server.
pub fn obtain_blocking(cfg: &AcmeConfig) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    rt.block_on(obtain(cfg))
}

fn builder(cfg: &AcmeConfig) -> anyhow::Result<AccountBuilder> {
    Ok(match &cfg.ca_root {
        Some(p) => Account::builder_with_root(p).context("acme: reading CA root")?,
        None => Account::builder().context("acme: builder")?,
    })
}

async fn load_or_create_account(cfg: &AcmeConfig) -> anyhow::Result<Account> {
    let cred_path = cfg.cache_dir.join("account.json");
    if let Ok(data) = std::fs::read(&cred_path) {
        if let Ok(creds) = serde_json::from_slice::<AccountCredentials>(&data) {
            if let Ok(acct) = builder(cfg)?.from_credentials(creds).await {
                return Ok(acct);
            }
            tracing::warn!("acme: cached account rejected; creating a new one");
        }
    }
    let contact = format!("mailto:{}", cfg.email);
    let (account, creds) = builder(cfg)?
        .create(
            &NewAccount {
                contact: &[contact.as_str()],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            cfg.directory_url.clone(),
            None,
        )
        .await
        .context("acme: creating account")?;
    if let Ok(json) = serde_json::to_vec(&creds) {
        let _ = write_private(&cred_path, &json);
    }
    Ok(account)
}

async fn obtain(cfg: &AcmeConfig) -> anyhow::Result<()> {
    anyhow::ensure!(!cfg.domains.is_empty(), "acme: no domains");
    std::fs::create_dir_all(&cfg.cache_dir).context("acme: creating cache dir")?;

    // Prefer the long-lived front: it already owns the challenge port, so binding a
    // second listener here would simply fail. Only fall back to a temporary one when no
    // front is running (a bare `obtain` with no server, e.g. in tests).
    let (challenges, server) = match FRONT.get() {
        Some(front) => (front.clone(), None),
        None => {
            let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
            let listener = TcpListener::bind(cfg.challenge_addr)
                .await
                .with_context(|| {
                    format!("acme: binding challenge server on {}", cfg.challenge_addr)
                })?;
            let ch = challenges.clone();
            let task = tokio::spawn(async move { challenge_loop(listener, ch, false).await });
            (challenges, Some(task))
        }
    };

    tracing::info!(domains = ?cfg.domains, "acme: obtaining certificate");
    let account = load_or_create_account(cfg).await?;

    let identifiers: Vec<Identifier> = cfg.domains.iter().cloned().map(Identifier::Dns).collect();
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .context("acme: new order")?;

    let mut authzs = order.authorizations();
    while let Some(authz) = authzs.next().await {
        let mut authz = authz?;
        if authz.status == AuthorizationStatus::Valid {
            continue;
        }
        let mut challenge = authz
            .challenge(ChallengeType::Http01)
            .ok_or_else(|| anyhow::anyhow!("acme: server offered no http-01 challenge"))?;
        let token = challenge.token.clone();
        let key_auth = challenge.key_authorization().as_str().to_string();
        challenges.lock().unwrap().insert(token, key_auth);
        challenge
            .set_ready()
            .await
            .context("acme: set challenge ready")?;
    }

    let status = order
        .poll_ready(&RetryPolicy::default())
        .await
        .context("acme: polling order")?;
    anyhow::ensure!(
        status == OrderStatus::Ready,
        "acme: order did not become ready ({status:?})"
    );

    let key_pem = order.finalize().await.context("acme: finalize")?;
    let cert_pem = order
        .poll_certificate(&RetryPolicy::default())
        .await
        .context("acme: fetching certificate")?;

    // Both files are staged and then renamed, key first, cert last. Written in
    // place, a worker spawning or reloading mid-write reads a new key against the
    // old certificate and fails to start; the window was the length of two file
    // writes. It is now one syscall, and the order is deliberate: the cert-mtime
    // watcher in supervisor.rs keys on the *certificate*, so by the time anything
    // notices a change the matching key is already in place.
    stage_and_rename(&key_path(&cfg.cache_dir), key_pem.as_bytes(), 0o600)
        .context("acme: writing key")?;
    stage_and_rename(&cert_path(&cfg.cache_dir), cert_pem.as_bytes(), 0o644)
        .context("acme: writing cert")?;
    let renew_at = now_secs() + cfg.renew_after_days * 86_400;
    let _ = std::fs::write(cfg.cache_dir.join("renew_at"), renew_at.to_string());

    // Only tear down a listener we created. The front outlives every issuance.
    if let Some(task) = server {
        task.abort();
    }
    // Challenge tokens are single-use; leaving them served forever is pointless surface.
    if let Ok(mut m) = challenges.lock() {
        m.clear();
    }
    tracing::info!(
        cert = %cert_path(&cfg.cache_dir).display(),
        "acme: certificate obtained"
    );
    Ok(())
}

/// The long-lived plain-HTTP listener's challenge map, if one is running.
///
/// `obtain` publishes tokens here instead of binding its own listener, so an issuance
/// never has to fight the front for port 80 — and the front can therefore stay up
/// between renewals, which is the whole point.
static FRONT: std::sync::OnceLock<Challenges> = std::sync::OnceLock::new();

/// Start the plain-HTTP front on `addr` for the lifetime of the process.
///
/// Answers ACME HTTP-01 challenges, and when `redirect` is set sends everything else to
/// `https://<host><path>` with a 308. Idempotent: calling it twice is a no-op.
///
/// Binding port 80 can legitimately fail (no privileges, something else already there),
/// and that must not stop a server from serving HTTPS — so failure is a warning, not an
/// error.
pub fn spawn_front(addr: SocketAddr, redirect: bool) {
    if FRONT.get().is_some() {
        return;
    }
    let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
    if FRONT.set(challenges.clone()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("askr-http-front".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!(error = %e, "http front: runtime");
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!(
                            %addr, error = %e,
                            "http front: could not bind (privileges? port in use?) — \
                             plain-HTTP requests will not be answered or redirected"
                        );
                        return;
                    }
                };
                tracing::info!(%addr, redirect, "http front listening");
                challenge_loop(listener, challenges, redirect).await;
            });
        })
        .ok();
}

/// Where an HTTP request on the front should be sent, or `None` to answer it here.
///
/// Pure so the redirect can be tested without a socket. Keeps the request's host (so
/// virtual hosts survive) and its path + query, and refuses a missing or malformed host
/// rather than emitting `https:///path`.
pub(crate) fn redirect_location(host: Option<&str>, path_and_query: &str) -> Option<String> {
    let host = host?.trim();
    if host.is_empty() || host.contains('/') || host.contains(char::is_whitespace) {
        return None;
    }
    // Strip a plain-HTTP port so we don't redirect to https://example.com:80/.
    let host = host.strip_suffix(":80").unwrap_or(host);
    Some(format!("https://{host}{path_and_query}"))
}

/// Serve HTTP-01 challenge responses; redirect or 404 for everything else.
async fn challenge_loop(listener: TcpListener, challenges: Challenges, redirect: bool) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let challenges = challenges.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                let challenges = challenges.clone();
                async move {
                    let path = req.uri().path();
                    // A challenge always wins over the redirect: sending the ACME
                    // validator to HTTPS would break issuance for a domain whose
                    // certificate doesn't exist yet.
                    let body =
                        path.strip_prefix("/.well-known/acme-challenge/")
                            .and_then(|token| {
                                challenges
                                    .lock()
                                    .map(|m| m.get(token).cloned())
                                    .unwrap_or(None)
                            });
                    if let Some(key_auth) = body {
                        return Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header(hyper::header::CONTENT_TYPE, "text/plain")
                                .body(Full::new(Bytes::from(key_auth)))
                                .unwrap(),
                        );
                    }
                    let pq = req
                        .uri()
                        .path_and_query()
                        .map(|p| p.as_str())
                        .unwrap_or("/")
                        .to_string();
                    let host = req
                        .headers()
                        .get(hyper::header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let location = redirect
                        .then(|| redirect_location(host.as_deref(), &pq))
                        .flatten();
                    let resp = match location {
                        Some(to) => Response::builder()
                            .status(StatusCode::PERMANENT_REDIRECT)
                            .header(hyper::header::LOCATION, to)
                            .header(hyper::header::CONTENT_LENGTH, "0")
                            .body(Full::new(Bytes::new()))
                            .unwrap(),
                        None => Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Full::new(Bytes::from("askr: not found")))
                            .unwrap(),
                    };
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn http_get(addr: SocketAddr, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn redirect_location_keeps_host_path_and_query() {
        assert_eq!(
            redirect_location(Some("example.com"), "/a/b?c=1"),
            Some("https://example.com/a/b?c=1".to_string())
        );
        // A plain-HTTP port must not survive into the https URL.
        assert_eq!(
            redirect_location(Some("example.com:80"), "/"),
            Some("https://example.com/".to_string())
        );
        // No host, or a host that would let someone steer the Location header
        // somewhere else entirely, is answered here instead of redirected.
        assert_eq!(redirect_location(None, "/"), None);
        assert_eq!(redirect_location(Some(""), "/"), None);
        assert_eq!(redirect_location(Some("evil.com/x"), "/"), None);
        assert_eq!(redirect_location(Some("a b"), "/"), None);
    }

    /// A challenge must win over the redirect: sending the ACME validator to HTTPS
    /// would break issuance for a domain that has no certificate yet — the exact
    /// situation ACME exists to resolve.
    #[tokio::test]
    async fn challenge_wins_over_redirect_and_the_rest_is_redirected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
        challenges
            .lock()
            .unwrap()
            .insert("tok123".to_string(), "tok123.keyauth".to_string());
        tokio::spawn(challenge_loop(listener, challenges, true));

        let ch = http_get(addr, "/.well-known/acme-challenge/tok123").await;
        assert!(ch.contains(" 200 "), "{ch}");
        assert!(ch.contains("tok123.keyauth"), "{ch}");

        let other = http_get(addr, "/pricing?ref=x").await;
        assert!(other.contains(" 308 "), "{other}");
        assert!(other.contains("https://x/pricing?ref=x"), "{other}");
    }

    /// With redirects off (no `force_https`) the front stays a challenge server, so
    /// enabling ACME can't silently start bouncing traffic somebody didn't ask to bounce.
    #[tokio::test]
    async fn without_redirect_enabled_the_front_only_serves_challenges() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(challenge_loop(listener, challenges, false));
        let r = http_get(addr, "/pricing").await;
        assert!(r.contains(" 404 "), "{r}");
    }

    #[tokio::test]
    async fn http01_challenge_serving() {
        let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
        challenges
            .lock()
            .unwrap()
            .insert("tok123".to_string(), "tok123.keyauth".to_string());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(challenge_loop(listener, challenges, false));

        // Known token → 200 with the key authorization.
        let ok = http_get(addr, "/.well-known/acme-challenge/tok123").await;
        assert!(ok.contains(" 200 "), "{ok}");
        assert!(ok.contains("tok123.keyauth"), "{ok}");

        // Unknown token → 404.
        let miss = http_get(addr, "/.well-known/acme-challenge/nope").await;
        assert!(miss.contains(" 404 "), "{miss}");
    }
}
