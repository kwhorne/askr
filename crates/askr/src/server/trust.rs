//! Who the client is: trusted proxies and `X-Forwarded-For`.
//!
//! One definition, used by the rate limiter and by the `$_SERVER` builder, so the address
//! a limit counts and the `REMOTE_ADDR` PHP sees can never disagree.

use std::net::SocketAddr;

use hyper::Request;

/// A trusted-proxy entry: a network address plus prefix length.
pub type Cidr = (std::net::IpAddr, u8);

/// Parse `10.0.0.0/8`, `192.168.1.5` or `::1` into a network + prefix length.
/// A bare address becomes a full-length prefix (a single host).
pub fn parse_cidr(s: &str) -> Option<Cidr> {
    let s = s.trim();
    let (addr, bits) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (s, None),
    };
    let ip: std::net::IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits = bits.unwrap_or(max);
    (bits <= max).then_some((ip, bits))
}

/// Is `ip` inside the network `(net, bits)`?
pub(super) fn cidr_contains((net, bits): &Cidr, ip: &std::net::IpAddr) -> bool {
    fn masked(bytes: &[u8], bits: u8, out: &mut [u8]) {
        let bits = bits as usize;
        for (i, b) in bytes.iter().enumerate() {
            let keep = bits.saturating_sub(i * 8).min(8);
            out[i] = if keep == 0 {
                0
            } else {
                b & (0xFFu16 << (8 - keep)) as u8
            };
        }
    }
    match (net, ip) {
        (std::net::IpAddr::V4(n), std::net::IpAddr::V4(a)) => {
            let (mut x, mut y) = ([0u8; 4], [0u8; 4]);
            masked(&n.octets(), *bits, &mut x);
            masked(&a.octets(), *bits, &mut y);
            x == y
        }
        (std::net::IpAddr::V6(n), std::net::IpAddr::V6(a)) => {
            let (mut x, mut y) = ([0u8; 16], [0u8; 16]);
            masked(&n.octets(), *bits, &mut x);
            masked(&a.octets(), *bits, &mut y);
            x == y
        }
        _ => false,
    }
}

/// The client's address, honouring `X-Forwarded-For` **only** through trusted
/// proxies.
///
/// Walks the forwarded chain right-to-left and returns the first address that
/// isn't itself a trusted proxy — the standard approach. With no trusted proxies
/// configured the header is ignored entirely, because believing it would let any
/// client rotate a fake address and walk straight past a rate limit.
pub fn client_ip_from(
    headers: &hyper::HeaderMap,
    peer: SocketAddr,
    trusted: &[Cidr],
) -> std::net::IpAddr {
    let peer_ip = peer.ip();
    if !peer_is_trusted(peer_ip, trusted) {
        return peer_ip;
    }
    let chain: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for hop in chain.iter().rev() {
        // An entry may carry a port (`1.2.3.4:5678`); tolerate both forms.
        let parsed = hop
            .parse::<std::net::IpAddr>()
            .ok()
            .or_else(|| hop.parse::<SocketAddr>().ok().map(|s| s.ip()));
        if let Some(ip) = parsed {
            if !trusted.iter().any(|c| cidr_contains(c, &ip)) {
                return ip;
            }
        }
    }
    peer_ip
}

/// Is `peer` one of the proxies the operator has vouched for in `trusted_proxies`?
///
/// The same test `client_ip_from` applies before it will read `X-Forwarded-For` at all,
/// exposed so the `$_SERVER` builder can decide what PHP gets to see with exactly the
/// same answer.
pub fn peer_is_trusted(peer: std::net::IpAddr, trusted: &[Cidr]) -> bool {
    !trusted.is_empty() && trusted.iter().any(|c| cidr_contains(c, &peer))
}

/// The client's address for a `hyper::Request`. Thin wrapper over
/// [`client_ip_from`], which is the one definition both this and the `$_SERVER`
/// builder use.
pub(super) fn client_ip<B>(
    req: &Request<B>,
    peer: SocketAddr,
    trusted: &[Cidr],
) -> std::net::IpAddr {
    client_ip_from(req.headers(), peer, trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parsing_and_matching() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        // Bare address = single host.
        let c = parse_cidr("192.168.1.5").unwrap();
        assert!(cidr_contains(&c, &ip("192.168.1.5")));
        assert!(!cidr_contains(&c, &ip("192.168.1.6")));
        // v4 prefix.
        let c = parse_cidr("10.0.0.0/8").unwrap();
        assert!(cidr_contains(&c, &ip("10.255.3.1")));
        assert!(!cidr_contains(&c, &ip("11.0.0.1")));
        let c = parse_cidr("192.168.1.0/24").unwrap();
        assert!(cidr_contains(&c, &ip("192.168.1.200")));
        assert!(!cidr_contains(&c, &ip("192.168.2.1")));
        // /0 matches everything of the same family, but not across families.
        let c = parse_cidr("0.0.0.0/0").unwrap();
        assert!(cidr_contains(&c, &ip("8.8.8.8")));
        assert!(!cidr_contains(&c, &ip("::1")));
        // v6.
        let c = parse_cidr("fd00::/8").unwrap();
        assert!(cidr_contains(&c, &ip("fd12::1")));
        assert!(!cidr_contains(&c, &ip("fe80::1")));
        assert!(cidr_contains(&parse_cidr("::1").unwrap(), &ip("::1")));
        // Rejected input.
        assert!(parse_cidr("not-an-ip").is_none());
        assert!(parse_cidr("10.0.0.0/99").is_none());
        assert!(parse_cidr("").is_none());
    }

    #[test]
    fn forwarded_for_is_only_believed_through_trusted_proxies() {
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let req = |xff: &str| {
            hyper::Request::builder()
                .uri("/")
                .header("x-forwarded-for", xff)
                .body(())
                .unwrap()
        };
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();

        // No trusted proxies: the header is ignored entirely, so a spoofed address
        // can't hand the client a fresh rate-limit bucket.
        assert_eq!(client_ip(&req("9.9.9.9"), peer, &[]), ip("127.0.0.1"));

        // Peer is a trusted proxy: believe the chain.
        let trusted = vec![parse_cidr("127.0.0.1").unwrap()];
        assert_eq!(client_ip(&req("9.9.9.9"), peer, &trusted), ip("9.9.9.9"));

        // Multi-hop: take the rightmost address that isn't itself a trusted proxy.
        let trusted = vec![
            parse_cidr("127.0.0.1").unwrap(),
            parse_cidr("10.0.0.0/8").unwrap(),
        ];
        assert_eq!(
            client_ip(&req("9.9.9.9, 10.1.1.1, 10.1.1.2"), peer, &trusted),
            ip("9.9.9.9"),
            "trusted hops are skipped from the right"
        );
        // A client-supplied fake in front of the real one doesn't win.
        assert_eq!(
            client_ip(&req("1.1.1.1, 9.9.9.9"), peer, &trusted),
            ip("9.9.9.9")
        );
        // Garbage in the chain falls back to the peer.
        assert_eq!(client_ip(&req("nonsense"), peer, &trusted), ip("127.0.0.1"));
        // An entry with a port is tolerated.
        assert_eq!(
            client_ip(&req("9.9.9.9:5678"), peer, &trusted),
            ip("9.9.9.9")
        );
    }
}
