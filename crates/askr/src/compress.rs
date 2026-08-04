//! Response compression (`Content-Encoding`).
//!
//! Askr served everything uncompressed — for HTML/JSON/JS/CSS that's often 5–10×
//! more bytes on the wire than needed. This compresses compressible responses in
//! the Rust hot path, negotiating `br` (preferred) or `gzip` from the client's
//! `Accept-Encoding`. Both encoders are pure Rust (no system libs), so the
//! self-contained build is unaffected.

/// A negotiated content encoding.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Encoding {
    Br,
    Gzip,
}

impl Encoding {
    pub fn header(self) -> &'static str {
        match self {
            Encoding::Br => "br",
            Encoding::Gzip => "gzip",
        }
    }
    /// A short ETag suffix so a compressed variant can't collide with the plain
    /// one in a downstream cache.
    pub fn etag_suffix(self) -> &'static str {
        match self {
            Encoding::Br => "-br",
            Encoding::Gzip => "-gz",
        }
    }
}

/// Don't bother compressing responses smaller than this.
pub const MIN_SIZE: usize = 1024;
/// Cap on-the-fly static-file compression (larger files stream uncompressed).
pub const MAX_STATIC: u64 = 4 * 1024 * 1024;

/// Pick the best supported encoding from an `Accept-Encoding` value (prefer br).
pub fn negotiate(accept_encoding: &str) -> Option<Encoding> {
    let a = accept_encoding.to_ascii_lowercase();
    // Crude but effective: honour presence, not q-values (br beats gzip).
    if a.split(',').any(|e| e.trim().starts_with("br")) {
        Some(Encoding::Br)
    } else if a.split(',').any(|e| e.trim().starts_with("gzip")) {
        Some(Encoding::Gzip)
    } else {
        None
    }
}

/// Is a content type worth compressing (text-ish, already-compressed formats no)?
pub fn compressible(content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or("").trim();
    ct.starts_with("text/")
        || matches!(
            ct,
            "application/json"
                | "application/javascript"
                | "application/xml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/manifest+json"
                | "application/ld+json"
                | "application/wasm"
                | "image/svg+xml"
        )
}

/// Compress `body`. Returns None on encoder error (caller falls back to plain).
pub fn compress(body: &[u8], enc: Encoding) -> Option<Vec<u8>> {
    match enc {
        Encoding::Gzip => {
            use flate2::write::GzEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut e = GzEncoder::new(Vec::with_capacity(body.len() / 2), Compression::new(5));
            e.write_all(body).ok()?;
            e.finish().ok()
        }
        Encoding::Br => {
            let mut out = Vec::with_capacity(body.len() / 2);
            let params = brotli::enc::BrotliEncoderParams {
                quality: 5,
                ..Default::default()
            };
            let mut input = body;
            brotli::BrotliCompress(&mut input, &mut out, &params).ok()?;
            Some(out)
        }
    }
}

/// Decide whether to compress and do it. Returns `(encoding, compressed)` when
/// worthwhile — i.e. the client accepts it, the type is compressible, the body
/// is big enough, and compression actually shrank it.
pub fn maybe(
    body: &[u8],
    content_type: &str,
    accept_encoding: &str,
) -> Option<(Encoding, Vec<u8>)> {
    if body.len() < MIN_SIZE || !compressible(content_type) {
        return None;
    }
    let enc = negotiate(accept_encoding)?;
    let out = compress(body, enc)?;
    if out.len() < body.len() {
        Some((enc, out))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_prefers_brotli_then_gzip() {
        assert_eq!(negotiate("br, gzip"), Some(Encoding::Br));
        assert_eq!(negotiate("gzip, deflate, br"), Some(Encoding::Br));
        assert_eq!(negotiate("gzip, deflate"), Some(Encoding::Gzip));
        assert_eq!(negotiate("GZIP"), Some(Encoding::Gzip), "case-insensitive");
        assert_eq!(negotiate("gzip;q=1.0"), Some(Encoding::Gzip));
    }

    #[test]
    fn no_acceptable_encoding_means_identity() {
        assert_eq!(negotiate(""), None);
        assert_eq!(negotiate("identity"), None);
        assert_eq!(negotiate("deflate"), None, "deflate alone isn't offered");
        // A client asking for something unknown gets uncompressed bytes, not a guess.
        assert_eq!(negotiate("zstd"), None);
    }

    #[test]
    fn only_text_like_types_are_compressed() {
        assert!(compressible("text/html; charset=utf-8"));
        assert!(compressible("application/json"));
        assert!(compressible("text/css"));
        assert!(compressible("application/javascript"));
        // Already-compressed payloads would only get bigger.
        assert!(!compressible("image/png"));
        assert!(!compressible("image/jpeg"));
        assert!(!compressible("video/mp4"));
        assert!(!compressible("application/zip"));
        assert!(!compressible(""));
    }

    #[test]
    fn compressing_actually_shrinks_repetitive_text() {
        let body = "hello world ".repeat(500).into_bytes();
        let out = maybe(&body, "text/html", "gzip");
        let (enc, bytes) = out.expect("a large text body should be compressed");
        assert_eq!(enc, Encoding::Gzip);
        assert!(
            bytes.len() < body.len() / 4,
            "expected real compression, got {} from {}",
            bytes.len(),
            body.len()
        );
    }

    #[test]
    fn tiny_or_binary_bodies_are_left_alone() {
        assert!(maybe(b"hi", "text/html", "gzip").is_none(), "not worth it");
        let big = vec![0u8; 40_000];
        assert!(
            maybe(&big, "image/png", "gzip").is_none(),
            "binary types are skipped"
        );
        assert!(
            maybe(&big, "text/html", "").is_none(),
            "no encoding negotiated"
        );
    }
}
