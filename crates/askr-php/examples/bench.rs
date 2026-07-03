//! Warm-interpreter micro-benchmark: boot PHP once, then serve the same real
//! request N times against a warm interpreter (opcache stays hot across
//! requests). This is a taste of the warm-master model — though true zero
//! per-request bootstrap (Laravel's own boot) is the M2 CoW-fork work.
//!
//!   cargo run --release -p askr-php --example bench -- <public_dir> [n] [uri]

use std::path::Path;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let public = args.next().expect("usage: bench <public_dir> [n] [uri]");
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(50);
    let uri = args.next().unwrap_or_else(|| "/".to_string());

    let public = public.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1);
    let docroot = Path::new(&public);
    let script = docroot.join("index.php");
    assert!(script.is_file(), "no index.php in {}", docroot.display());
    let script_s = script.to_string_lossy().into_owned();

    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (uri.clone(), String::new()),
    };

    let make_req = || askr_php::Request {
        script_filename: script_s.clone(),
        method: "GET".into(),
        query_string: query.clone(),
        content_type: None,
        cookie: None,
        body: Vec::new(),
        server_vars: vec![
            ("REQUEST_METHOD".into(), "GET".into()),
            ("REQUEST_URI".into(), uri.clone()),
            ("PATH_INFO".into(), path.clone()),
            ("QUERY_STRING".into(), query.clone()),
            ("SCRIPT_NAME".into(), "/index.php".into()),
            ("SCRIPT_FILENAME".into(), script_s.clone()),
            (
                "DOCUMENT_ROOT".into(),
                docroot.to_string_lossy().into_owned(),
            ),
            ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
            ("SERVER_SOFTWARE".into(), "askr".into()),
            ("SERVER_NAME".into(), "localhost".into()),
            ("SERVER_PORT".into(), "443".into()),
            ("HTTP_HOST".into(), "localhost".into()),
            ("REMOTE_ADDR".into(), "127.0.0.1".into()),
            ("HTTPS".into(), "on".into()),
        ],
    };

    let mut php = askr_php::Interpreter::new().expect("php init");
    println!(
        "embedded PHP {}  —  {n} requests to {uri}\n",
        php.php_version()
    );

    let mut times = Vec::with_capacity(n);
    let mut last_status = 0;
    let mut last_len = 0;
    for i in 0..n {
        let req = make_req();
        let t = Instant::now();
        let resp = php.handle(&req).expect("handle");
        let us = t.elapsed().as_micros();
        times.push(us);
        last_status = resp.status;
        last_len = resp.body.len();
        if i == 0 {
            println!(
                "req #1  (cold opcache): {:>8.2} ms  -> {} ({} bytes)",
                us as f64 / 1000.0,
                resp.status,
                resp.body.len()
            );
        }
    }

    let warm = &times[1.min(times.len() - 1)..];
    let sum: u128 = warm.iter().sum();
    let avg = sum as f64 / warm.len() as f64;
    let min = *warm.iter().min().unwrap() as f64;
    let max = *warm.iter().max().unwrap() as f64;
    let mut sorted: Vec<u128> = warm.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[sorted.len() / 2] as f64;
    let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)] as f64;

    println!("\nwarm requests (#2..#{n}), status {last_status}, {last_len} bytes:");
    println!(
        "  avg {:.2} ms   p50 {:.2} ms   p99 {:.2} ms   min {:.2} ms   max {:.2} ms",
        avg / 1000.0,
        p50 / 1000.0,
        p99 / 1000.0,
        min / 1000.0,
        max / 1000.0
    );
    println!(
        "  ~{:.0} req/s (single core, single interpreter)",
        1_000_000.0 / avg
    );
}
