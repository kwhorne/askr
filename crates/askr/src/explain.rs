//! `askr why <url>` — the admin side and the command line.
//!
//! The admin plane runs in the master, which has no PHP, so it cannot answer the question
//! itself: it sends the request to this server's own listener, marked with the secret
//! the workers share ([`crate::server::why`]), and turns the trace that comes back into a
//! [`Report`]. The command line asks the admin plane for one and prints it.
//!
//! The probe is a real request — it runs PHP, it can fill the cache, and it counts against
//! a rate limit — so it is GET only.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;

use crate::server::why::{self, Step};

/// What `GET /api/why` answers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: u16,
    pub elapsed_ms: f64,
    pub bytes: u64,
    #[serde(default)]
    pub content_type: Option<String>,
    /// `X-Askr-Cache` on the response: HIT, MISS, STALE, PASS…
    #[serde(default)]
    pub cache: Option<String>,
    pub steps: Vec<Step>,
}

/// What to ask about: a URL (absolute, or a path), extra request headers, and the
/// peer to pretend to be.
#[derive(Debug, Default)]
pub struct Question {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub peer: Option<IpAddr>,
}

impl Question {
    /// From the admin query string: `url=…&h=Name:%20value&peer=10.0.0.4`.
    pub fn from_query(q: &str) -> Result<Self, String> {
        let mut out = Question::default();
        for pair in q.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v)?;
            match k {
                "url" => out.url = v,
                "h" => {
                    let (n, val) = v
                        .split_once(':')
                        .ok_or_else(|| format!("header {v:?} is not `Name: value`"))?;
                    out.headers
                        .push((n.trim().to_string(), val.trim().to_string()));
                }
                "peer" => {
                    out.peer = Some(
                        v.parse()
                            .map_err(|_| format!("peer {v:?} is not an IP address"))?,
                    )
                }
                _ => return Err(format!("unknown parameter {k:?}")),
            }
        }
        if out.url.is_empty() {
            return Err("url is required".to_string());
        }
        Ok(out)
    }

    /// The query string [`from_query`](Self::from_query) reads.
    pub fn to_query(&self) -> String {
        let mut q = format!("url={}", percent_encode(&self.url));
        for (n, v) in &self.headers {
            q.push_str(&format!("&h={}", percent_encode(&format!("{n}: {v}"))));
        }
        if let Some(p) = self.peer {
            q.push_str(&format!("&peer={p}"));
        }
        q
    }

    /// Host and path-and-query. A bare path is asked of `localhost`.
    fn split(&self) -> Result<(String, String), String> {
        let u = self.url.trim();
        if u.starts_with('/') {
            return Ok(("localhost".to_string(), u.to_string()));
        }
        let rest = u
            .strip_prefix("http://")
            .or_else(|| u.strip_prefix("https://"))
            .ok_or_else(|| format!("{u:?} is neither a path nor an http(s) URL"))?;
        let (host, pq) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if host.is_empty() {
            return Err(format!("{u:?} has no host"));
        }
        Ok((host.to_string(), pq.to_string()))
    }
}

/// Ask this server's own listener, and report what it decided.
///
/// `listen` is the server's configured address (an unspecified address is reached over
/// loopback); `tls` says whether that listener speaks TLS.
pub async fn probe(listen: SocketAddr, tls: bool, q: &Question) -> Result<Report, String> {
    let secret = why::secret().ok_or("explaining is unavailable: no secret this boot")?;
    let (host, pq) = q.split()?;
    let target = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), listen.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), listen.port())
        }
        _ => listen,
    };
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(&pq)
        .header(hyper::header::HOST, &host)
        .header(why::REQUEST_HEADER, secret);
    if let Some(p) = q.peer {
        req = req.header(why::PEER_HEADER, p.to_string());
    }
    for (n, v) in &q.headers {
        req = req.header(n.as_str(), v.as_str());
    }
    let req = req
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("bad request: {e}"))?;

    let started = Instant::now();
    let fut = async {
        let tcp = tokio::net::TcpStream::connect(target)
            .await
            .map_err(|e| format!("connecting to {target}: {e}"))?;
        if tls {
            let name = rustls::pki_types::ServerName::try_from(
                crate::cgi::host_without_port(&host).to_string(),
            )
            .map_err(|e| format!("{host:?} as a TLS name: {e}"))?;
            let stream = tokio_rustls::TlsConnector::from(own_listener_tls())
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS to {target}: {e}"))?;
            send(stream, req).await
        } else {
            send(tcp, req).await
        }
    };
    let (status, headers, bytes) = tokio::time::timeout(Duration::from_secs(60), fut)
        .await
        .map_err(|_| "no answer within 60 seconds".to_string())??;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    let header = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let steps = match header(why::TRACE_HEADER) {
        Some(t) => serde_json::from_str(&t).map_err(|e| format!("unreadable trace: {e}"))?,
        None => {
            return Err(
                "the server answered without a trace — was it started by this \
                        master (a stale worker from before a restart, or another server on \
                        this port)?"
                    .to_string(),
            )
        }
    };
    Ok(Report {
        method: "GET".to_string(),
        host,
        path: pq,
        status,
        elapsed_ms,
        bytes,
        content_type: header("content-type"),
        cache: header("x-askr-cache"),
        steps,
    })
}

async fn send<S>(
    stream: S,
    req: Request<Empty<Bytes>>,
) -> Result<(u16, hyper::HeaderMap, u64), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| format!("HTTP handshake: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("request: {e}"))?;
    let status = resp.status().as_u16();
    let (parts, body) = resp.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|e| format!("reading the body: {e}"))?
        .to_bytes();
    Ok((status, parts.headers, body.len() as u64))
}

/// TLS to our own listener, on loopback: the certificate is not checked, because the
/// thing on the other end is this process's own children and the name in the URL is
/// usually not one the certificate (self-signed, or for the public name) would match.
/// The secret makes the request count; nothing secret comes back that the admin plane
/// could not read anyway.
fn own_listener_tls() -> Arc<rustls::ClientConfig> {
    #[derive(Debug)]
    struct OwnListener(Arc<rustls::crypto::CryptoProvider>);
    impl rustls::client::danger::ServerCertVerifier for OwnListener {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(OwnListener(provider)))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// The report as an operator reads it.
pub fn render(r: &Report) -> String {
    let mut out = format!(
        "{} http://{}{}  →  {}{} in {:.0} ms, {} bytes\n\n",
        r.method,
        r.host,
        r.path,
        r.status,
        r.cache
            .as_deref()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default(),
        r.elapsed_ms,
        r.bytes
    );
    let width = r.steps.iter().map(|s| s.stage.len()).max().unwrap_or(0);
    for s in &r.steps {
        out.push_str(&format!("  {:width$}  {}\n", s.stage, s.outcome));
        if let Some(w) = &s.why {
            for line in wrap(w, 76usize.saturating_sub(width)) {
                out.push_str(&format!("  {:width$}    {line}\n", ""));
            }
        }
    }
    out
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width.max(20) {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `askr why`: ask the admin plane at `admin` and print the answer.
pub fn run(admin: &str, q: Question, json: bool) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let body = rt.block_on(async {
        let addr: SocketAddr = admin.parse().map_err(|_| {
            anyhow::anyhow!("--admin {admin:?} is not an address like 127.0.0.1:9000")
        })?;
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
            anyhow::anyhow!("connecting to the admin plane at {addr}: {e} — is [admin] listen set?")
        })?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/why?{}", q.to_query()))
            .header(hyper::header::HOST, addr.to_string());
        if let Some(t) = std::env::var("ASKR_ADMIN_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        {
            req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = sender
            .send_request(req.body(Full::new(Bytes::new()))?)
            .await?;
        let status = resp.status();
        let body = resp.into_body().collect().await?.to_bytes();
        let text = String::from_utf8_lossy(&body).into_owned();
        anyhow::ensure!(
            status.is_success(),
            "the admin plane answered {status}: {text}"
        );
        Ok::<_, anyhow::Error>(text)
    })?;
    if json {
        println!("{body}");
    } else {
        let report: Report = serde_json::from_str(&body)?;
        print!("{}", render(&report));
    }
    Ok(())
}

fn percent_decode(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| format!("bad escape in {s:?}"))?;
                out.push(hex);
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| format!("{s:?} is not UTF-8"))
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_survives_the_query_string() {
        let q = Question {
            url: "https://shop.test/products?utm_source=x&a=1".to_string(),
            headers: vec![("Cookie".into(), "laravel_session=abc; _ga=1".into())],
            peer: Some("10.0.0.4".parse().unwrap()),
        };
        let back = Question::from_query(&q.to_query()).unwrap();
        assert_eq!(back.url, q.url);
        assert_eq!(back.headers, q.headers);
        assert_eq!(back.peer, q.peer);
        assert_eq!(
            back.split().unwrap(),
            (
                "shop.test".to_string(),
                "/products?utm_source=x&a=1".to_string()
            )
        );
        assert_eq!(
            Question {
                url: "/x".into(),
                ..Default::default()
            }
            .split()
            .unwrap(),
            ("localhost".to_string(), "/x".to_string())
        );
        assert!(Question::from_query("h=nocolon&url=/").is_err());
        assert!(Question::from_query("peer=nope&url=/").is_err());
        assert!(Question::from_query("").is_err());
        assert!(Question {
            url: "ftp://x/".into(),
            ..Default::default()
        }
        .split()
        .is_err());
    }

    #[test]
    fn the_report_reads_as_a_list_of_decisions() {
        let r = Report {
            method: "GET".into(),
            host: "shop.test".into(),
            path: "/products".into(),
            status: 200,
            elapsed_ms: 31.6,
            bytes: 5120,
            content_type: Some("text/html".into()),
            cache: Some("MISS".into()),
            steps: vec![
                Step::new("site", "/srv/shop/public (index.php)").because("[server] root"),
                Step::new("store", "not stored").because("the response has no Askr-Cache header"),
            ],
        };
        let text = render(&r);
        assert!(
            text.starts_with("GET http://shop.test/products  →  200 (MISS) in 32 ms, 5120 bytes"),
            "{text}"
        );
        assert!(
            text.contains("  site   /srv/shop/public (index.php)\n"),
            "{text}"
        );
        assert!(
            text.contains("         the response has no Askr-Cache header\n"),
            "{text}"
        );
    }
}
