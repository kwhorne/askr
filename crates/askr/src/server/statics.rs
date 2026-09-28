//! Static files: path sanitising, what may never be served, ranges and MIME types.

use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};
use hyper::{Method, Response, StatusCode};
use tokio::io::AsyncRead;

use super::{full, text, ResBody};

/// A streaming file body — reads the file in 64 KB chunks so a large file never
/// buffers the whole thing in RAM (and reports an exact size so hyper sets
/// Content-Length and suppresses the body for HEAD).
pub(super) struct FileBody {
    file: tokio::fs::File,
    remaining: u64,
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }
        let want = this.remaining.min(64 * 1024) as usize;
        let mut buf = vec![0u8; want];
        let mut rb = tokio::io::ReadBuf::new(&mut buf);
        match Pin::new(&mut this.file).poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                let n = rb.filled().len();
                if n == 0 {
                    this.remaining = 0;
                    return Poll::Ready(None);
                }
                this.remaining -= n as u64;
                buf.truncate(n);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(buf)))))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// Serve a static file: streamed, with ETag + Cache-Control, conditional GET
/// (304) and single-range (206) support.
pub(super) async fn serve_static(
    path: &Path,
    meta: &std::fs::Metadata,
    method: &Method,
    headers: &hyper::HeaderMap,
) -> Response<ResBody> {
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("W/\"{len:x}-{mtime:x}\"");

    // Hashed build assets can be cached forever; everything else briefly.
    let cache_control = if path.components().any(|c| c.as_os_str() == "build") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };

    // Conditional GET (tolerate the -br/-gz suffix a compressed variant carries).
    if let Some(inm) = headers
        .get(hyper::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm.split(',').any(|t| {
            let t = t.trim().trim_end_matches("-br").trim_end_matches("-gz");
            t == etag
        }) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(hyper::header::ETAG, &etag)
                .header(hyper::header::CACHE_CONTROL, cache_control)
                .body(full(Bytes::new()))
                .unwrap();
        }
    }

    // Compress small, compressible, non-Range static files on the fly (JS/CSS/
    // JSON/SVG assets). Large files keep streaming uncompressed.
    let ct = mime_for(path);
    if !headers.contains_key(hyper::header::RANGE)
        && len <= crate::compress::MAX_STATIC
        && crate::compress::compressible(ct)
    {
        let accept = headers
            .get(hyper::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if let Some(enc) = crate::compress::negotiate(accept) {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if let Some(compressed) = crate::compress::compress(&bytes, enc) {
                    if compressed.len() < bytes.len() {
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header(hyper::header::CONTENT_TYPE, ct)
                            .header(hyper::header::ETAG, format!("{etag}{}", enc.etag_suffix()))
                            .header(hyper::header::CACHE_CONTROL, cache_control)
                            .header(hyper::header::CONTENT_ENCODING, enc.header())
                            .header(hyper::header::VARY, "Accept-Encoding")
                            .body(full(Bytes::from(compressed)))
                            .unwrap_or_else(|_| {
                                text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response")
                            });
                    }
                }
            }
        }
    }

    let (start, end) = parse_range(headers, len);
    let partial =
        headers.contains_key(hyper::header::RANGE) && (start != 0 || end != len.saturating_sub(1));
    // For an empty file end==0 and start==0, so end+1-start would be 1 — send 0.
    // (Empty static assets are common: a Vite CSS-only entry emits an empty .js.)
    let send_len = if len == 0 { 0 } else { end + 1 - start };

    let mut builder = Response::builder()
        .header(hyper::header::CONTENT_TYPE, mime_for(path))
        .header(hyper::header::ETAG, &etag)
        .header(hyper::header::CACHE_CONTROL, cache_control)
        .header(hyper::header::ACCEPT_RANGES, "bytes");
    builder = if partial {
        builder.status(StatusCode::PARTIAL_CONTENT).header(
            hyper::header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{len}"),
        )
    } else {
        builder.status(StatusCode::OK)
    };

    let _ = method; // hyper suppresses the body for HEAD (using FileBody's size_hint)

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return text(StatusCode::NOT_FOUND, "askr: file not found"),
    };
    if start > 0 {
        use tokio::io::AsyncSeekExt;
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return text(StatusCode::INTERNAL_SERVER_ERROR, "askr: seek failed");
        }
    }
    builder
        .body(
            FileBody {
                file,
                remaining: send_len,
            }
            .boxed(),
        )
        .unwrap_or_else(|_| text(StatusCode::INTERNAL_SERVER_ERROR, "askr: bad response"))
}

/// Parse a single HTTP Range header into an inclusive `(start, end)`. Falls back
/// to the whole file `(0, len-1)` for a missing/invalid/multi-range request.
pub(super) fn parse_range(headers: &hyper::HeaderMap, len: u64) -> (u64, u64) {
    let full = (0, len.saturating_sub(1));
    if len == 0 {
        return full;
    }
    let Some(spec) = headers
        .get(hyper::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
    else {
        return full;
    };
    // Single range only.
    let Some((s, e)) = spec.split(',').next().unwrap_or("").trim().split_once('-') else {
        return full;
    };
    let (start, end) = match (s.trim(), e.trim()) {
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(n) if n > 0 => (len.saturating_sub(n), len - 1),
            _ => return full,
        },
        (a, "") => match a.parse::<u64>() {
            Ok(start) => (start, len - 1),
            _ => return full,
        },
        (a, b) => match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(start), Ok(end)) => (start, end.min(len - 1)),
            _ => return full,
        },
    };
    if start > end || start >= len {
        return full;
    }
    (start, end)
}

/// Strip the leading slash and reject any `..`/absolute traversal.
pub(super) fn sanitize(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in Path::new(path.trim_start_matches('/')).components() {
        if let Component::Normal(c) = comp {
            out.push(c);
        }
    }
    out
}

/// Paths that must never be served as static bytes:
///
/// - **PHP sources.** Serving `/index.php` returned the file's source instead of
///   running it — source disclosure, and any other `.php` under the docroot leaked
///   too (installers, legacy scripts, files holding credentials).
/// - **Dotfiles and dot-directories** — `.env`, `.git/*`, `.htaccess`. A docroot
///   pointed at an app root (a common misconfiguration) otherwise served secrets
///   verbatim.
///
/// `.well-known/` stays allowed: ACME HTTP-01, `security.txt` and friends live
/// there legitimately.
///
/// Blocked paths fall through to the front controller, so the app answers (a 404
/// from the framework) instead of Askr handing out bytes. Note that Askr only ever
/// *executes* the configured front controller — never an arbitrary `.php` found on
/// disk — so an uploaded script can't be run through this path either.
pub(super) fn static_forbidden(rel: &Path) -> bool {
    let dotted = rel.components().any(|c| match c {
        Component::Normal(s) => {
            let s = s.to_string_lossy();
            s.starts_with('.') && s != ".well-known"
        }
        _ => false,
    });
    if dotted {
        return true;
    }
    // Editor and deploy leftovers: `index.php.bak`, `config.php~`, `db.php.save`.
    // A `.php.<anything>` file is still PHP source, and nobody serves `~`/`.bak`
    // on purpose. nginx and Apache hand these out by default — Askr ships with no
    // config to add rules to, so it refuses them itself.
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains(".php.") || name.ends_with('~') {
        return true;
    }
    const LEFTOVER: [&str; 8] = [
        ".bak", ".orig", ".save", ".swp", ".swo", ".old", ".rej", ".tmp",
    ];
    if LEFTOVER.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    matches!(
        rel.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "php" | "php3" | "php4" | "php5" | "php7" | "php8" | "phps" | "pht" | "phtml" | "phar"
        )
    )
}

pub(super) fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_traversal() {
        assert_eq!(sanitize("/build/app.js"), PathBuf::from("build/app.js"));
        // path traversal and absolute components are dropped
        assert_eq!(sanitize("/../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(sanitize("/a/../b/./c"), PathBuf::from("a/b/c"));
        assert!(sanitize("/").as_os_str().is_empty());
    }

    #[test]
    fn static_serving_refuses_sources_and_dotfiles() {
        // PHP sources are never handed out as bytes (and never executed from disk).
        assert!(static_forbidden(Path::new("index.php")));
        assert!(static_forbidden(Path::new("legacy/install.PHP")));
        assert!(static_forbidden(Path::new("a.phtml")));
        assert!(static_forbidden(Path::new("a.phar")));
        assert!(static_forbidden(Path::new("a.php5")));
        // Dotfiles and dot-directories: secrets and VCS metadata.
        assert!(static_forbidden(Path::new(".env")));
        assert!(static_forbidden(Path::new(".env.production")));
        assert!(static_forbidden(Path::new(".git/config")));
        assert!(static_forbidden(Path::new("sub/.htaccess")));
        // `.well-known` is legitimate (ACME HTTP-01, security.txt).
        assert!(!static_forbidden(Path::new(".well-known/security.txt")));
        assert!(!static_forbidden(Path::new(
            ".well-known/acme-challenge/tok123"
        )));
        // Editor / deploy leftovers — `.php.bak` is still PHP source.
        assert!(static_forbidden(Path::new("index.php.bak")));
        assert!(static_forbidden(Path::new("index.php.orig")));
        assert!(static_forbidden(Path::new("db.php.save")));
        assert!(static_forbidden(Path::new("index.PHP.BAK")));
        assert!(static_forbidden(Path::new("config.php~")));
        assert!(static_forbidden(Path::new("notes.txt~")));
        assert!(static_forbidden(Path::new("sub/logo.png.bak")));
        // Normal assets are unaffected — including ones that merely *contain* a
        // leftover-looking word.
        assert!(!static_forbidden(Path::new("img/photo.old.png")));
        assert!(!static_forbidden(Path::new("build/vendor.bak.js")));
        assert!(!static_forbidden(Path::new("build/app.js")));
        assert!(!static_forbidden(Path::new("img/logo.png")));
        assert!(!static_forbidden(Path::new("phpinfo.txt")));
        assert!(!static_forbidden(Path::new("graphql")));
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_for(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(
            mime_for(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_for(Path::new("a.woff2")), "font/woff2");
        assert_eq!(mime_for(Path::new("a.unknown")), "application/octet-stream");
        assert_eq!(mime_for(Path::new("noext")), "application/octet-stream");
    }
}
