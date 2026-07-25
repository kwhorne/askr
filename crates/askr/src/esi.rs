//! Edge-Side Includes: assemble one HTML page from independently cached fragments.
//!
//! A page opts in with a response header (`Askr-ESI: on`) and marks its dynamic
//! holes with tags:
//!
//! ```html
//! <html><body>
//!   <esi:include src="/_esi/header"/>
//!   <main>Article body, cached for hours…</main>
//!   <esi:include src="/_esi/cart"/>
//! </body></html>
//! ```
//!
//! The point is *where* the expansion happens: Askr caches the page **with the tags
//! still in it** and expands on the way out, so the shell can sit in cache for a day
//! while the cart fragment has a 0-second TTL and is rendered per request. Each
//! fragment is an ordinary request with its own `Askr-Cache` header, so every hole
//! gets its own TTL, tags and invalidation.
//!
//! This module is the pure part: turning a body into a plan of literals and includes.
//! Fetching the fragments lives in `server.rs`, where the cache and PHP are.

/// One piece of a planned response.
#[derive(Debug, PartialEq, Eq)]
pub enum Segment {
    /// Bytes copied through verbatim (a byte range into the original body).
    Literal(usize, usize),
    /// An `<esi:include>` to expand, with the `src` it asked for.
    Include(String),
}

/// Does this body contain anything for us to do? A cheap pre-check so a page that
/// opted in but has no tags costs one substring search.
pub fn has_tags(body: &[u8]) -> bool {
    find(body, 0, b"<esi:").is_some()
}

/// Split a body into literals and includes.
///
/// Supports `<esi:include src="…"/>` (single or double quotes, any attribute order)
/// and `<esi:remove>…</esi:remove>`, whose contents are dropped — that block is the
/// fallback markup for caches that don't speak ESI, so an ESI-aware server must
/// remove it. Anything else starting with `<esi:` is passed through untouched rather
/// than guessed at.
pub fn plan(body: &[u8]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut lit_start = 0usize;
    let mut i = 0usize;

    while let Some(open) = find(body, i, b"<esi:") {
        // `<esi:remove>` … `</esi:remove>` — drop the whole block.
        if starts_with(body, open, b"<esi:remove") {
            let Some(gt) = find(body, open, b">") else {
                break;
            };
            let Some(close) = find(body, gt, b"</esi:remove>") else {
                // Unclosed remove: leave the rest alone rather than eating the page.
                i = gt + 1;
                continue;
            };
            push_literal(&mut out, lit_start, open);
            let end = close + b"</esi:remove>".len();
            lit_start = end;
            i = end;
            continue;
        }

        if starts_with(body, open, b"<esi:include") {
            // The tag ends at the first `>`; `/>` and `>` are both accepted.
            let Some(gt) = find(body, open, b">") else {
                break;
            };
            let tag = &body[open..gt];
            // No `src`: not something we can act on, so leave the tag in place.
            if let Some(src) = attr(tag, b"src") {
                push_literal(&mut out, lit_start, open);
                out.push(Segment::Include(src));
                lit_start = gt + 1;
            }
            i = gt + 1;
            continue;
        }

        // Some other esi: tag — pass through.
        i = open + b"<esi:".len();
    }

    push_literal(&mut out, lit_start, body.len());
    out
}

fn push_literal(out: &mut Vec<Segment>, from: usize, to: usize) {
    if to > from {
        out.push(Segment::Literal(from, to));
    }
}

/// Read `name="value"` (or `name='value'`) out of a tag.
fn attr(tag: &[u8], name: &[u8]) -> Option<String> {
    let mut i = 0;
    while let Some(pos) = find(tag, i, name) {
        // Must be preceded by whitespace, so `data-src` doesn't match `src`.
        let boundary = pos == 0 || tag[pos - 1].is_ascii_whitespace();
        let mut j = pos + name.len();
        while j < tag.len() && tag[j].is_ascii_whitespace() {
            j += 1;
        }
        if !boundary || j >= tag.len() || tag[j] != b'=' {
            i = pos + name.len();
            continue;
        }
        j += 1;
        while j < tag.len() && tag[j].is_ascii_whitespace() {
            j += 1;
        }
        let quote = *tag.get(j)?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        j += 1;
        let start = j;
        while j < tag.len() && tag[j] != quote {
            j += 1;
        }
        return std::str::from_utf8(&tag[start..j]).ok().map(str::to_owned);
    }
    None
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| p + from)
}

fn starts_with(hay: &[u8], at: usize, needle: &[u8]) -> bool {
    hay.len() >= at + needle.len() && hay[at..at + needle.len()].eq_ignore_ascii_case(needle)
}

/// Is a fragment `src` safe to fetch? Only same-origin absolute paths.
///
/// Askr must never turn an ESI tag into an outbound fetch: a page (or a template
/// injection into one) could otherwise make the server request arbitrary URLs —
/// classic SSRF, from inside the trust boundary.
pub fn safe_src(src: &str) -> bool {
    !src.is_empty()
        && src.starts_with('/')
        && !src.starts_with("//")
        && !src.contains("://")
        && !src.contains('\\')
        // `..` can't escape the docroot (paths are sanitised downstream) but it can
        // confuse cache keys, so refuse it outright.
        && !src.split('/').any(|s| s == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(body: &str, frag: &str) -> String {
        let mut out = Vec::new();
        for seg in plan(body.as_bytes()) {
            match seg {
                Segment::Literal(a, b) => out.extend_from_slice(&body.as_bytes()[a..b]),
                Segment::Include(_) => out.extend_from_slice(frag.as_bytes()),
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn pre_check_is_cheap_and_correct() {
        assert!(!has_tags(b"<html><body>no tags</body></html>"));
        assert!(has_tags(b"<esi:include src=\"/x\"/>"));
    }

    #[test]
    fn expands_includes_in_place() {
        assert_eq!(expand("<a><esi:include src=\"/f\"/></a>", "F"), "<a>F</a>");
        // Self-closing and plain `>` both work, as do single quotes and spacing.
        assert_eq!(expand("x<esi:include src='/f'>y", "F"), "xFy");
        assert_eq!(expand("x<esi:include   src = \"/f\" />y", "F"), "xFy");
        // Several holes in one page.
        assert_eq!(
            expand("<esi:include src=\"/a\"/>|<esi:include src=\"/b\"/>", "F"),
            "F|F"
        );
        // Case-insensitive tag name (HTML in the wild).
        assert_eq!(expand("<ESI:Include SRC=\"/f\"/>", "F"), "F");
    }

    #[test]
    fn collects_srcs_in_order() {
        let body = b"<esi:include src=\"/one\"/>mid<esi:include src=\"/two\"/>";
        let srcs: Vec<String> = plan(body)
            .into_iter()
            .filter_map(|s| match s {
                Segment::Include(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(srcs, vec!["/one".to_string(), "/two".to_string()]);
    }

    #[test]
    fn removes_fallback_blocks() {
        // <esi:remove> holds markup for caches that don't speak ESI.
        assert_eq!(expand("a<esi:remove><p>plain</p></esi:remove>b", "F"), "ab");
        // An unclosed remove must not eat the rest of the page.
        let body = "a<esi:remove>tail";
        assert_eq!(expand(body, "F"), body);
    }

    #[test]
    fn leaves_unknown_and_malformed_tags_alone() {
        // Unknown esi: verb — passed through, not guessed at.
        let body = "a<esi:vars>$(HTTP_HOST)</esi:vars>b";
        assert_eq!(expand(body, "F"), body);
        // include without src — left in place rather than dropped silently.
        let body = "a<esi:include/>b";
        assert_eq!(expand(body, "F"), body);
        // `data-src` must not be mistaken for `src`.
        let body = "a<esi:include data-src=\"/f\"/>b";
        assert_eq!(expand(body, "F"), body);
        // Unterminated tag.
        let body = "a<esi:include src=\"/f\"";
        assert_eq!(expand(body, "F"), body);
    }

    #[test]
    fn empty_and_tagless_bodies() {
        assert_eq!(plan(b""), vec![]);
        assert_eq!(plan(b"plain"), vec![Segment::Literal(0, 5)]);
    }

    #[test]
    fn src_must_be_same_origin() {
        assert!(safe_src("/_esi/cart"));
        assert!(safe_src("/a/b?c=1"));
        // SSRF vectors: absolute URLs, protocol-relative, schemes, backslashes.
        assert!(!safe_src("http://evil.example/x"));
        assert!(!safe_src("//evil.example/x"));
        assert!(!safe_src("https://169.254.169.254/latest/meta-data/"));
        assert!(!safe_src("file:///etc/passwd"));
        assert!(!safe_src("\\\\evil\\share"));
        // Relative and traversal.
        assert!(!safe_src("cart"));
        assert!(!safe_src("/a/../../etc/passwd"));
        assert!(!safe_src(""));
    }
}
