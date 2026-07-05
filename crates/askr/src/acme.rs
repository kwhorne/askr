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
        let _ = std::fs::write(&cred_path, json);
    }
    Ok(account)
}

async fn obtain(cfg: &AcmeConfig) -> anyhow::Result<()> {
    anyhow::ensure!(!cfg.domains.is_empty(), "acme: no domains");
    std::fs::create_dir_all(&cfg.cache_dir).context("acme: creating cache dir")?;

    let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
    let listener = TcpListener::bind(cfg.challenge_addr)
        .await
        .with_context(|| format!("acme: binding challenge server on {}", cfg.challenge_addr))?;
    let ch = challenges.clone();
    let server = tokio::spawn(async move { challenge_loop(listener, ch).await });

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

    std::fs::write(key_path(&cfg.cache_dir), key_pem).context("acme: writing key")?;
    std::fs::write(cert_path(&cfg.cache_dir), cert_pem).context("acme: writing cert")?;
    let renew_at = now_secs() + cfg.renew_after_days * 86_400;
    let _ = std::fs::write(cfg.cache_dir.join("renew_at"), renew_at.to_string());

    server.abort();
    tracing::info!(
        cert = %cert_path(&cfg.cache_dir).display(),
        "acme: certificate obtained"
    );
    Ok(())
}

/// Serve HTTP-01 challenge responses (and a redirect/OK for everything else).
async fn challenge_loop(listener: TcpListener, challenges: Challenges) {
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
                    let body = path
                        .strip_prefix("/.well-known/acme-challenge/")
                        .and_then(|token| challenges.lock().unwrap().get(token).cloned());
                    let resp = match body {
                        Some(key_auth) => Response::builder()
                            .status(StatusCode::OK)
                            .header(hyper::header::CONTENT_TYPE, "text/plain")
                            .body(Full::new(Bytes::from(key_auth)))
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

    #[tokio::test]
    async fn http01_challenge_serving() {
        let challenges: Challenges = Arc::new(Mutex::new(HashMap::new()));
        challenges
            .lock()
            .unwrap()
            .insert("tok123".to_string(), "tok123.keyauth".to_string());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(challenge_loop(listener, challenges));

        // Known token → 200 with the key authorization.
        let ok = http_get(addr, "/.well-known/acme-challenge/tok123").await;
        assert!(ok.contains(" 200 "), "{ok}");
        assert!(ok.contains("tok123.keyauth"), "{ok}");

        // Unknown token → 404.
        let miss = http_get(addr, "/.well-known/acme-challenge/nope").await;
        assert!(miss.contains(" 404 "), "{miss}");
    }
}
