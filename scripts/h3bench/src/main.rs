//! Minimal, honest h2/h3 load client.
//!
//! Opens ONE connection (TCP+TLS for h2, QUIC for h3) and drives `concurrency`
//! multiplexed streams, each issuing `requests` sequential GETs. Validates every
//! response (status 200 + non-empty body), counts non-2xx, and reports throughput
//! and tail latency. The single-connection design is deliberate: it's what exposes
//! transport head-of-line blocking under packet loss (h2/TCP) vs per-stream QUIC.
//!
//! usage: h3bench <h2|h3> <https://host:port/path> <concurrency> <requests-per-stream>

use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::Buf;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};

type R<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _e: &rustls::pki_types::CertificateDer<'_>,
        _i: &[rustls::pki_types::CertificateDer<'_>],
        _s: &rustls::pki_types::ServerName<'_>,
        _o: &[u8],
        _n: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ED25519,
        ]
    }
}

fn client_tls(alpn: &[&[u8]]) -> rustls::ClientConfig {
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    cfg
}

#[tokio::main]
async fn main() -> R<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: h3bench <h2|h3> <url> <concurrency> <requests-per-stream>");
        std::process::exit(2);
    }
    let proto = args[1].clone();
    let url = args[2].clone();
    let conc: usize = args[3].parse()?;
    let per: usize = args[4].parse()?;

    let uri: http::Uri = url.parse()?;
    let host = uri.host().ok_or("no host in url")?.to_string();
    let port = uri.port_u16().unwrap_or(443);
    let addr: SocketAddr = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .next()
        .ok_or("dns: no address")?;

    let t0 = Instant::now();
    let (lat, ok, err) = match proto.as_str() {
        "h3" => run_h3(&uri, &host, addr, conc, per).await?,
        "h2" => run_h2(&uri, &host, addr, conc, per).await?,
        other => return Err(format!("unknown proto {other}").into()),
    };
    let elapsed = t0.elapsed().as_secs_f64();

    let mut l = lat;
    l.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| -> f64 {
        if l.is_empty() {
            return 0.0;
        }
        let i = ((l.len() as f64 - 1.0) * p).round() as usize;
        l[i]
    };
    let rps = ok as f64 / elapsed;
    println!(
        "proto={proto} conc={conc} reqs={} ok={ok} err={err} secs={elapsed:.2} rps={rps:.0} \
         p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
        ok + err,
        pct(0.50),
        pct(0.95),
        pct(0.99),
        l.last().copied().unwrap_or(0.0),
    );
    Ok(())
}

async fn run_h3(
    uri: &http::Uri,
    host: &str,
    addr: SocketAddr,
    conc: usize,
    per: usize,
) -> R<(Vec<f64>, u64, u64)> {
    let tls = client_tls(&[b"h3"]);
    let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(qcc)));
    let conn = ep.connect(addr, host)?.await?;
    let (mut driver, send_request) = h3::client::new(h3_quinn::Connection::new(conn)).await?;
    tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let mut tasks = Vec::new();
    for _ in 0..conc {
        let mut sr = send_request.clone();
        let uri = uri.clone();
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::with_capacity(per);
            let (mut ok, mut err) = (0u64, 0u64);
            for _ in 0..per {
                let t = Instant::now();
                match one_h3(&mut sr, &uri).await {
                    Ok(true) => {
                        ok += 1;
                        lat.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    _ => err += 1,
                }
            }
            (lat, ok, err)
        }));
    }
    merge(tasks).await
}

async fn one_h3<T>(sr: &mut h3::client::SendRequest<T, bytes::Bytes>, uri: &http::Uri) -> R<bool>
where
    T: h3::quic::OpenStreams<bytes::Bytes>,
{
    let req = http::Request::get(uri.clone()).body(())?;
    let mut stream = sr.send_request(req).await?;
    stream.finish().await?;
    let resp = stream.recv_response().await?;
    let mut len = 0usize;
    while let Some(mut chunk) = stream.recv_data().await? {
        len += chunk.remaining();
        chunk.advance(chunk.remaining());
    }
    Ok(resp.status() == 200 && len > 0)
}

async fn run_h2(
    uri: &http::Uri,
    host: &str,
    addr: SocketAddr,
    conc: usize,
    per: usize,
) -> R<(Vec<f64>, u64, u64)> {
    let tls = client_tls(&[b"h2"]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())?;
    let tls_stream = connector.connect(server_name, tcp).await?;
    let io = TokioIo::new(tls_stream);
    let (send, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), io).await?;
    tokio::spawn(conn);

    let mut tasks = Vec::new();
    for _ in 0..conc {
        let mut s = send.clone();
        let uri = uri.clone();
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::with_capacity(per);
            let (mut ok, mut err) = (0u64, 0u64);
            for _ in 0..per {
                let t = Instant::now();
                match one_h2(&mut s, &uri).await {
                    Ok(true) => {
                        ok += 1;
                        lat.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    _ => err += 1,
                }
            }
            (lat, ok, err)
        }));
    }
    merge(tasks).await
}

async fn one_h2(
    s: &mut hyper::client::conn::http2::SendRequest<Empty<bytes::Bytes>>,
    uri: &http::Uri,
) -> R<bool> {
    let req = http::Request::get(uri.clone()).body(Empty::<bytes::Bytes>::new())?;
    let resp = s.send_request(req).await?;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    Ok(status == 200 && !body.is_empty())
}

async fn merge(
    tasks: Vec<tokio::task::JoinHandle<(Vec<f64>, u64, u64)>>,
) -> R<(Vec<f64>, u64, u64)> {
    let mut lat = Vec::new();
    let (mut ok, mut err) = (0u64, 0u64);
    for t in tasks {
        let (l, o, e) = t.await?;
        lat.extend(l);
        ok += o;
        err += e;
    }
    Ok((lat, ok, err))
}
