//! TLS termination with rustls (ring provider — no OpenSSL, no C toolchain).
//!
//! Askr terminates TLS itself so it can be a complete single binary (no nginx /
//! stunnel in front). A single certificate chain + key is loaded from PEM files;
//! ALPN offers HTTP/2 and HTTP/1.1.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Build a `TlsAcceptor` from a PEM certificate chain and private key on disk.
pub fn acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<TlsAcceptor> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("reading TLS cert {}", cert_path.display()))?;
    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("reading TLS key {}", key_path.display()))?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<_, _>>()
        .context("parsing TLS certificate chain")?;
    anyhow::ensure!(
        !certs.is_empty(),
        "no certificates found in {}",
        cert_path.display()
    );

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("parsing TLS private key")?
        .with_context(|| format!("no private key found in {}", key_path.display()))?;

    from_parts(certs, key)
}

/// Build a `TlsAcceptor` with a freshly generated self-signed v3 certificate for
/// the given hostnames (for dev / testing — browsers will warn).
pub fn self_signed(hosts: &[String]) -> anyhow::Result<TlsAcceptor> {
    let cert = rcgen::generate_simple_self_signed(hosts.to_vec())
        .context("generating self-signed certificate")?;

    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    // rcgen 0.14 renamed `CertifiedKey::key_pair` to `signing_key`.
    let key_der = PrivateKeyDer::try_from(cert.signing_key.serialize_der())
        .map_err(|e| anyhow::anyhow!("self-signed key: {e}"))?;

    from_parts(vec![cert_der], key_der)
}

fn from_parts(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<TlsAcceptor> {
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls server config")?;

    // Offer HTTP/2 then HTTP/1.1 via ALPN.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}
