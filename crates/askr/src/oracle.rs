//! The cache oracle: measure what caching *would* do, before caching anything.
//!
//! The reason full-page caching is rare in PHP isn't performance, it's uncertainty —
//! nobody knows how much a rule would win, or whether it would serve one visitor's
//! page to everyone. Askr sees every request and every response body, so it can
//! answer both questions from real traffic without changing a single byte of what it
//! serves.
//!
//! Collection is a JSONL line per request (`--traffic-log`), analysis is a separate
//! command (`askr cache-report`). Keeping them apart means the hot path stays a single
//! `write`, and the analysis can be as thorough as it likes.
//!
//! The interesting part is the safety verdict, not the hit rate. For each candidate
//! rule the oracle checks whether **the same cache key ever produced a different
//! response body inside the TTL window**. If it did, that page is personalised and
//! caching it would be a bug — and this is the only place that can know, because it
//! has both the key and the bytes.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

/// One observed request. Short keys: this is written once per request.
#[derive(Debug, Clone)]
pub struct Sample {
    pub ts_ms: u64,
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: String,
    /// The request carried cookies that aren't on the ignore list.
    pub cookie: bool,
    /// The response set a cookie — a session started, so it can't be shared.
    pub set_cookie: bool,
    pub status: u16,
    pub bytes: u64,
    pub php_us: u64,
    pub body_hash: u64,
    /// The app already asked for this to be cached (`Askr-Cache`).
    pub opted_in: bool,
}

impl Sample {
    /// Serialise as one JSON line.
    pub fn to_line(&self) -> String {
        format!(
            r#"{{"t":{},"m":"{}","h":"{}","p":"{}","q":"{}","c":{},"sc":{},"s":{},"b":{},"u":{},"x":{},"o":{}}}"#,
            self.ts_ms,
            esc(&self.method),
            esc(&self.host),
            esc(&self.path),
            esc(&self.query),
            self.cookie,
            self.set_cookie,
            self.status,
            self.bytes,
            self.php_us,
            self.body_hash,
            self.opted_in,
        )
    }

    fn from_line(line: &str) -> Option<Sample> {
        // A deliberately small hand-rolled reader: the writer above is the only
        // producer, so there's no need to pull in a JSON parser for the analysis
        // path. Anything unparseable is skipped rather than aborting a report.
        Some(Sample {
            ts_ms: num(line, "\"t\":")?,
            method: string(line, "\"m\":")?,
            host: string(line, "\"h\":")?,
            path: string(line, "\"p\":")?,
            query: string(line, "\"q\":").unwrap_or_default(),
            cookie: boolean(line, "\"c\":")?,
            set_cookie: boolean(line, "\"sc\":")?,
            status: num(line, "\"s\":")? as u16,
            bytes: num(line, "\"b\":")?,
            php_us: num(line, "\"u\":")?,
            body_hash: num(line, "\"x\":")?,
            opted_in: boolean(line, "\"o\":")?,
        })
    }
}

fn esc(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let i = line.find(key)? + key.len();
    Some(&line[i..])
}

fn num(line: &str, key: &str) -> Option<u64> {
    let rest = field(line, key)?;
    let end = rest.find([',', '}'])?;
    rest[..end].trim().parse().ok()
}

fn boolean(line: &str, key: &str) -> Option<bool> {
    let rest = field(line, key)?;
    Some(rest.starts_with("true"))
}

fn string(line: &str, key: &str) -> Option<String> {
    let rest = field(line, key)?.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

/// Collapse a concrete path into the pattern a cache rule would use:
/// `/products/1421/reviews` → `/products/*/reviews`.
///
/// Segments that are clearly identifiers (numbers, UUIDs, long hex) become `*`, so
/// thousands of distinct URLs collapse into the handful of shapes an operator would
/// actually write a rule for.
pub fn path_pattern(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        out.push('/');
        if is_identifier(seg) {
            out.push('*');
        } else {
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn is_identifier(seg: &str) -> bool {
    let all_digits = seg.chars().all(|c| c.is_ascii_digit());
    if all_digits && !seg.is_empty() {
        return true;
    }
    // UUID-ish, or a long hex/base-ish token: an id, not a route name.
    let hexish = seg.len() >= 16
        && seg
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '_');
    let mixed_long = seg.len() >= 24 && seg.chars().any(|c| c.is_ascii_digit());
    hexish || mixed_long
}

/// What a candidate rule would have achieved.
pub struct Verdict {
    pub pattern: String,
    pub ttl: u64,
    pub requests: usize,
    pub eligible: usize,
    pub hits: usize,
    pub php_us_saved: u64,
    /// Requests to this pattern that carried cookies.
    pub with_cookies: usize,
    /// Responses that set a cookie — never shareable.
    pub set_cookie: usize,
    /// Times the *same* key produced a different body inside the window. Any of
    /// these means the page is personalised and must not be cached as-is.
    pub divergent: usize,
    pub already_opted_in: bool,
}

impl Verdict {
    pub fn hit_rate(&self) -> f64 {
        if self.eligible == 0 {
            0.0
        } else {
            self.hits as f64 * 100.0 / self.eligible as f64
        }
    }

    /// A short, honest safety note — the reason to trust or reject the number above.
    pub fn risk(&self) -> String {
        if self.divergent > 0 {
            return format!(
                "\u{2717} unsafe: {} responses differed for the same URL",
                self.divergent
            );
        }
        if self.set_cookie > 0 {
            return format!(
                "\u{2717} unsafe: {} responses set a cookie",
                self.set_cookie
            );
        }
        if self.with_cookies > 0 {
            let pct = self.with_cookies as f64 * 100.0 / self.requests.max(1) as f64;
            return format!("\u{26a0} {pct:.0}% carried cookies (needs ignore_cookies or force)");
        }
        "\u{2713} identical for every visitor".to_string()
    }
}

/// Simulate one candidate rule over the samples.
///
/// Only requests that actually ran PHP are in the log, so a hit here means "this
/// response could have come from cache instead of costing PHP".
pub fn simulate(samples: &[Sample], pattern: &str, ttl: u64) -> Verdict {
    let mut v = Verdict {
        pattern: pattern.to_string(),
        ttl,
        requests: 0,
        eligible: 0,
        hits: 0,
        php_us_saved: 0,
        with_cookies: 0,
        set_cookie: 0,
        divergent: 0,
        already_opted_in: false,
    };
    // key → (stored_at_ms, body_hash)
    let mut store: HashMap<String, (u64, u64)> = HashMap::new();

    for s in samples {
        if path_pattern(&s.path) != pattern {
            continue;
        }
        v.requests += 1;
        if s.cookie {
            v.with_cookies += 1;
        }
        if s.set_cookie {
            v.set_cookie += 1;
        }
        if s.opted_in {
            v.already_opted_in = true;
        }
        // Only anonymous, successful, read-only requests can be served from cache.
        let cacheable = matches!(s.method.as_str(), "GET" | "HEAD") && s.status == 200;
        if !cacheable {
            continue;
        }
        v.eligible += 1;

        let key = format!("{} {} {}?{}", s.method, s.host, s.path, s.query);
        match store.get(&key) {
            Some(&(stored_at, hash)) if s.ts_ms.saturating_sub(stored_at) <= ttl * 1000 => {
                v.hits += 1;
                v.php_us_saved += s.php_us;
                // The decisive check: had we served the stored copy, would the
                // visitor have got the right bytes?
                if hash != s.body_hash {
                    v.divergent += 1;
                }
            }
            _ => {
                store.insert(key, (s.ts_ms, s.body_hash));
            }
        }
    }
    v
}

/// Read a traffic log written by `--traffic-log`.
pub fn load(path: &Path) -> Result<Vec<Sample>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading traffic log {}", path.display()))?;
    let mut out: Vec<Sample> = text.lines().filter_map(Sample::from_line).collect();
    out.sort_by_key(|s| s.ts_ms);
    Ok(out)
}

/// Analyse a traffic log and print what caching would buy.
pub fn report(path: &Path, ttls: &[u64], top: usize) -> Result<()> {
    let samples = load(path)?;
    anyhow::ensure!(
        !samples.is_empty(),
        "no usable samples in {} — is the server running with --traffic-log?",
        path.display()
    );

    let span_ms = samples
        .last()
        .map(|s| s.ts_ms)
        .unwrap_or(0)
        .saturating_sub(samples.first().map(|s| s.ts_ms).unwrap_or(0));
    let total_php_us: u64 = samples.iter().map(|s| s.php_us).sum();

    // Below this, a "per minute" figure says more about the sample than the site.
    const SHORT_SAMPLE_MS: u64 = 60_000;
    let short = span_ms < SHORT_SAMPLE_MS;

    println!("askr cache-report\n");
    println!("  samples        {}", samples.len());
    println!("  window         {}", human_ms(span_ms));
    println!("  PHP time       {} total", human_us(total_php_us));
    if short {
        println!(
            "\n  \u{26a0} This sample covers {} \u{2014} too short to extrapolate. The rates below are\n    scaled to a minute from far less than a minute of traffic; treat them as a\n    direction, not a number. Run with --traffic-log through a normal hour.",
            human_ms(span_ms)
        );
    }
    println!(
        "\n  These are requests that ran PHP. Anything already served from cache never\n  reached here, so this is the work that is *still* being done.\n"
    );

    // One verdict per (pattern, ttl); keep the best TTL per pattern by time saved.
    let mut patterns: Vec<String> = samples.iter().map(|s| path_pattern(&s.path)).collect();
    patterns.sort();
    patterns.dedup();

    let mut best: Vec<Verdict> = Vec::new();
    for p in &patterns {
        let mut candidates: Vec<Verdict> = ttls.iter().map(|&t| simulate(&samples, p, t)).collect();
        candidates.sort_by_key(|v| std::cmp::Reverse(v.php_us_saved));
        if let Some(v) = candidates.into_iter().next() {
            if v.eligible > 0 {
                best.push(v);
            }
        }
    }
    best.sort_by_key(|v| std::cmp::Reverse(v.php_us_saved));

    println!(
        "  {:<28} {:>5} {:>6} {:>9}  safety",
        "pattern", "ttl", "hit", "PHP saved"
    );
    println!("  {}", "-".repeat(94));
    let mut safe_saving = 0u64;
    for v in best.iter().take(top) {
        println!(
            "  {:<28} {:>4}s {:>5.0}% {:>7.2} s/m  {}",
            truncate(&v.pattern, 28),
            v.ttl,
            v.hit_rate(),
            per_minute(v.php_us_saved, span_ms),
            v.risk()
        );
        if v.divergent == 0 && v.set_cookie == 0 && v.with_cookies == 0 {
            safe_saving += v.php_us_saved;
        }
    }

    let pct = if total_php_us > 0 {
        safe_saving as f64 * 100.0 / total_php_us as f64
    } else {
        0.0
    };
    println!(
        "\n  Safe rules alone would have removed {pct:.0}% of the PHP time above ({} of {}).",
        human_us(safe_saving),
        human_us(total_php_us)
    );
    if !short {
        println!(
            "  That's {:.2} CPU-s per minute back.",
            per_minute(safe_saving, span_ms)
        );
    }

    // Turn the safe ones into config the operator can paste.
    let paste: Vec<&Verdict> = best
        .iter()
        .filter(|v| v.divergent == 0 && v.set_cookie == 0 && v.hits > 0 && !v.already_opted_in)
        .take(top)
        .collect();
    if !paste.is_empty() {
        println!("\n  Suggested askr.toml:\n");
        println!("    [cache]");
        println!("    response_slots = 512");
        for v in &paste {
            println!();
            if v.with_cookies > 0 {
                println!(
                    "    # {} of {} requests carried cookies \u{2014} check they're only analytics",
                    v.with_cookies, v.requests
                );
                println!("    # before relying on `force`, or list them in ignore_cookies.");
            }
            println!("    [[cache.rule]]");
            println!("    path = \"{}\"", v.pattern);
            println!("    ttl = {}", v.ttl);
        }
    }

    println!("\n  What this does and doesn't know:\n");
    println!("    The safety column compares the actual response bytes for the same URL inside");
    println!(
        "    the window. '\u{2713} identical' means every visitor really did get the same page"
    );
    println!("    during the sample \u{2014} not that they always will.");
    println!(
        "    Cookies are counted as they arrived: a page marked \u{26a0} may still be perfectly"
    );
    println!("    cacheable if those cookies are analytics only (see [cache] ignore_cookies).");
    println!("    A short sample flatters long TTLs. A rule can't beat the traffic it saw.");
    Ok(())
}

fn per_minute(us: u64, span_ms: u64) -> f64 {
    if span_ms == 0 {
        return 0.0;
    }
    (us as f64 / 1e6) / (span_ms as f64 / 60_000.0)
}

/// Microseconds at a precision that doesn't round a real measurement to `0.0 s`.
fn human_us(us: u64) -> String {
    if us < 1_000_000 {
        format!("{:.0} ms", us as f64 / 1000.0)
    } else {
        format!("{:.1} s", us as f64 / 1e6)
    }
}

fn human_ms(ms: u64) -> String {
    let s = ms / 1000;
    if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else if s < 120 {
        format!("{s} s")
    } else if s < 7200 {
        format!("{:.0} min", s as f64 / 60.0)
    } else {
        format!("{:.1} h", s as f64 / 3600.0)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let keep: String = s.chars().take(n - 1).collect();
        format!("{keep}\u{2026}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts_ms: u64, path: &str, php_us: u64, body_hash: u64) -> Sample {
        Sample {
            ts_ms,
            method: "GET".into(),
            host: "example.com".into(),
            path: path.into(),
            query: String::new(),
            cookie: false,
            set_cookie: false,
            status: 200,
            bytes: 100,
            php_us,
            body_hash,
            opted_in: false,
        }
    }

    #[test]
    fn identifiers_collapse_into_patterns() {
        assert_eq!(path_pattern("/products/1421"), "/products/*");
        assert_eq!(
            path_pattern("/products/1421/reviews"),
            "/products/*/reviews"
        );
        assert_eq!(path_pattern("/"), "/");
        assert_eq!(path_pattern("/about"), "/about");
        // UUIDs and long hex tokens are ids too.
        assert_eq!(
            path_pattern("/orders/550e8400-e29b-41d4-a716-446655440000"),
            "/orders/*"
        );
        assert_eq!(path_pattern("/f/0123456789abcdef0123"), "/f/*");
        // Real route names survive, even longish ones.
        assert_eq!(
            path_pattern("/newsletter-subscribe"),
            "/newsletter-subscribe"
        );
        assert_eq!(path_pattern("/api/v1/health"), "/api/v1/health");
    }

    #[test]
    fn repeated_requests_inside_the_ttl_are_hits() {
        let s = vec![
            sample(0, "/about", 10_000, 1),
            sample(1_000, "/about", 10_000, 1),
            sample(2_000, "/about", 10_000, 1),
        ];
        let v = simulate(&s, "/about", 60);
        assert_eq!(
            (v.eligible, v.hits),
            (3, 2),
            "first is a miss, then two hits"
        );
        assert_eq!(v.php_us_saved, 20_000);
        assert!(v.risk().starts_with('\u{2713}'), "got {}", v.risk());
    }

    #[test]
    fn a_request_after_the_ttl_is_a_miss_again() {
        let s = vec![
            sample(0, "/about", 10_000, 1),
            sample(61_000, "/about", 10_000, 1),
        ];
        let v = simulate(&s, "/about", 60);
        assert_eq!(v.hits, 0, "61s later the entry has expired");
    }

    /// The whole point of the oracle: catching a page that *looks* cacheable but
    /// renders differently per visitor. Hit rate alone would recommend caching it.
    #[test]
    fn differing_bodies_for_one_url_are_reported_as_unsafe() {
        let s = vec![
            sample(0, "/dashboard", 20_000, 111),
            sample(1_000, "/dashboard", 20_000, 222), // a different visitor's page
            sample(2_000, "/dashboard", 20_000, 333),
        ];
        let v = simulate(&s, "/dashboard", 60);
        assert_eq!(v.hits, 2, "it would have hit…");
        assert_eq!(v.divergent, 2, "…and served the wrong bytes both times");
        assert!(v.risk().contains("unsafe"), "got {}", v.risk());
    }

    #[test]
    fn a_response_that_sets_a_cookie_is_never_safe() {
        let mut a = sample(0, "/login", 5_000, 1);
        a.set_cookie = true;
        let mut b = sample(500, "/login", 5_000, 1);
        b.set_cookie = true;
        let v = simulate(&[a, b], "/login", 60);
        assert!(v.risk().contains("set a cookie"), "got {}", v.risk());
    }

    #[test]
    fn cookies_on_the_request_are_a_warning_not_a_refusal() {
        let mut a = sample(0, "/news", 5_000, 7);
        a.cookie = true;
        let b = sample(500, "/news", 5_000, 7);
        let v = simulate(&[a, b], "/news", 60);
        assert_eq!(v.divergent, 0);
        assert!(v.risk().starts_with('\u{26a0}'), "got {}", v.risk());
        assert!(
            v.risk().contains("50%"),
            "half of them carried cookies: {}",
            v.risk()
        );
    }

    #[test]
    fn non_get_and_error_responses_are_not_eligible() {
        let mut post = sample(0, "/orders", 9_000, 1);
        post.method = "POST".into();
        let mut err = sample(100, "/orders", 9_000, 1);
        err.status = 500;
        let ok = sample(200, "/orders", 9_000, 1);
        let v = simulate(&[post, err, ok], "/orders", 60);
        assert_eq!(v.requests, 3);
        assert_eq!(v.eligible, 1, "only the successful GET could be cached");
        assert_eq!(v.hits, 0);
    }

    #[test]
    fn a_sample_survives_a_round_trip_through_the_log_format() {
        let mut s = sample(1234, "/a/b", 42, 99);
        s.query = "page=2&q=\"quoted\"".into();
        s.cookie = true;
        s.opted_in = true;
        s.status = 200;
        let line = s.to_line();
        let back = Sample::from_line(&line).expect("should parse its own output");
        assert_eq!(back.ts_ms, 1234);
        assert_eq!(back.path, "/a/b");
        assert_eq!(back.query, "page=2&q=\"quoted\"");
        assert!(back.cookie && back.opted_in);
        assert_eq!((back.php_us, back.body_hash), (42, 99));
    }

    #[test]
    fn small_measurements_are_not_rounded_away() {
        // A real 33ms of PHP must not print as "0.0 s" — that reads as "nothing".
        assert_eq!(human_us(33_000), "33 ms");
        assert_eq!(human_us(999_000), "999 ms");
        assert_eq!(human_us(1_500_000), "1.5 s");
        // A sub-second window must not print as "0 s" either.
        assert_eq!(human_ms(340), "0.3 s");
        assert_eq!(human_ms(9_900), "9.9 s");
        assert_eq!(human_ms(45_000), "45 s");
        assert_eq!(human_ms(600_000), "10 min");
    }

    #[test]
    fn garbage_lines_are_skipped_not_fatal() {
        assert!(Sample::from_line("not json").is_none());
        assert!(Sample::from_line("").is_none());
    }
}
