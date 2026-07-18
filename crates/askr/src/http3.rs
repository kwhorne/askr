//! HTTP/3 (QUIC) listener via quinn + h3, sharing the rustls (ring) certificate.
//!
//! Runs alongside the TCP HTTP/1.1+HTTP/2 listener: the same request handler seam
//! serves both. Clients discover it via the `Alt-Svc: h3=":443"` header the TCP
//! path advertises. Requires TLS (`--tls-cert`/`--tls-key`). Behind `--features
//! http3`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::Request;

use crate::server::Runtime;

/// Build a QUIC server endpoint from a PEM cert + key, bound to `addr`, ALPN `h3`.
pub fn endpoint(
    cert_path: &Path,
    key_path: &Path,
    addr: SocketAddr,
) -> anyhow::Result<quinn::Endpoint> {
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_slice()).collect::<Result<_, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())?
        .ok_or_else(|| anyhow::anyhow!("no private key in {}", key_path.display()))?;

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic));

    // SO_REUSEPORT so every prefork worker binds the same UDP port; the kernel
    // steers each QUIC connection (4-tuple) to one worker consistently.
    let sock = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    let udp: std::net::UdpSocket = sock.into();

    Ok(quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        udp,
        Arc::new(quinn::TokioRuntime),
    )?)
}

/// Accept QUIC connections and serve HTTP/3, dispatching each request through the
/// shared request handler (`crate::server::handle`).
pub async fn serve(endpoint: quinn::Endpoint, rt: Arc<Runtime>) {
    while let Some(incoming) = endpoint.accept().await {
        let rt = rt.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(error = %e, "h3: connection failed");
                    return;
                }
            };
            let peer = conn.remote_address();
            let mut h3 = match h3::server::Connection::new(h3_quinn::Connection::new(conn)).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::debug!(error = %e, "h3: handshake failed");
                    return;
                }
            };
            loop {
                match h3.accept().await {
                    Ok(Some(resolver)) => {
                        let rt = rt.clone();
                        tokio::spawn(async move {
                            if let Ok((req, stream)) = resolver.resolve_request().await {
                                if let Err(e) = respond(req, stream, rt, peer).await {
                                    tracing::debug!(error = %e, "h3: request failed");
                                }
                            }
                        });
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, "h3: accept ended");
                        break;
                    }
                }
            }
        });
    }
}

/// Read the request body, dispatch through `handle`, and write the response back
/// over the h3 stream.
async fn respond<S>(
    req: Request<()>,
    mut stream: h3::server::RequestStream<S, Bytes>,
    rt: Arc<Runtime>,
    peer: SocketAddr,
) -> anyhow::Result<()>
where
    S: h3::quic::BidiStream<Bytes>,
{
    let mut body = BytesMut::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
    }

    let (parts, _) = req.into_parts();
    let hyper_req = Request::from_parts(parts, Full::new(body.freeze()));

    let resp = crate::server::handle(hyper_req, rt, peer)
        .await
        .map_err(|e| anyhow::anyhow!("handle: {e:?}"))?;

    let (parts, resp_body) = resp.into_parts();
    stream
        .send_response(hyper::Response::from_parts(parts, ()))
        .await?;
    let bytes = resp_body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    if !bytes.is_empty() {
        stream.send_data(bytes).await?;
    }
    stream.finish().await?;
    Ok(())
}
