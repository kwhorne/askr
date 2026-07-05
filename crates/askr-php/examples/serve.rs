//! Run a real PHP front controller through the embedded interpreter — once, as a
//! single request — and print the captured HTTP response.
//!
//!   cargo run -p askr-php --example serve -- <public_dir> [uri]
//!
//! e.g. cargo run -p askr-php --example serve -- ~/code/larafast-tall/public /

use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let public = args.next().expect("usage: serve <public_dir> [uri]");
    let uri = args.next().unwrap_or_else(|| "/".to_string());

    let public = shellexpand(&public);
    let docroot = Path::new(&public);
    let script = docroot.join("index.php");
    assert!(script.is_file(), "no index.php in {}", docroot.display());

    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (uri.clone(), String::new()),
    };

    let server_vars = vec![
        ("REQUEST_METHOD".into(), "GET".into()),
        ("REQUEST_URI".into(), uri.clone()),
        ("PATH_INFO".into(), path.clone()),
        ("QUERY_STRING".into(), query.clone()),
        ("SCRIPT_NAME".into(), "/index.php".into()),
        (
            "SCRIPT_FILENAME".into(),
            script.to_string_lossy().into_owned(),
        ),
        (
            "DOCUMENT_ROOT".into(),
            docroot.to_string_lossy().into_owned(),
        ),
        ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(), "askr".into()),
        ("SERVER_NAME".into(), "localhost".into()),
        ("SERVER_PORT".into(), "443".into()),
        ("HTTP_HOST".into(), "localhost".into()),
        ("REMOTE_ADDR".into(), "127.0.0.1".into()),
        ("HTTPS".into(), "on".into()),
    ];

    let req = askr_php::Request {
        script_filename: script.to_string_lossy().into_owned(),
        method: "GET".into(),
        query_string: query,
        content_type: None,
        cookie: None,
        body: Vec::new(),
        server_vars,
        ..Default::default()
    };

    let mut php = askr_php::Interpreter::new().expect("php init");
    println!("== embedded PHP {} ==", php.php_version());
    println!("GET {uri}  (docroot: {})\n", docroot.display());

    let resp = php.handle(&req).expect("handle");

    println!("HTTP {} (php_status={})", resp.status, resp.php_status);
    for (k, v) in &resp.headers {
        println!("  {k}: {v}");
    }
    println!("\n---- body ({} bytes) ----", resp.body.len());
    let body = String::from_utf8_lossy(&resp.body);
    let preview: String = body.chars().take(1600).collect();
    println!("{preview}");
    if body.chars().count() > 1600 {
        println!("... [truncated]");
    }
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}
