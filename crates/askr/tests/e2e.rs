//! End-to-end tests: start the real binary, serve real PHP, assert over real HTTP.
//!
//! Every unit test in this repo passed while five genuine bugs shipped into commits
//! during one afternoon — a shutdown hang, a canary that aborted but kept serving a
//! broken build, a rate-limit counter that always read zero from the master, a
//! `PURGE` that could never match, and a suggested metric that didn't exist. None of
//! them were reachable without starting the server and sending a request.
//!
//! So this file is the regression net for exactly those behaviours. It has no
//! dependencies: the HTTP client below is deliberately small and raw, which also
//! makes it easy to send `PURGE`/`BAN` and to lie about `Host`.
//!
//! These tests need the embedded `libphp` the binary links against, which CI builds
//! before `cargo test`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

// --- tiny HTTP client ----------------------------------------------------

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn cache_state(&self) -> &str {
        self.header("x-askr-cache").unwrap_or("")
    }
}

/// One request/response over a fresh connection. `Connection: close` keeps the
/// reply trivially parseable: everything after the blank line is the body.
fn request(port: u16, method: &str, path: &str, extra: &[(&str, &str)]) -> Resp {
    request_with_body(port, method, path, extra, "")
}

/// Same, with a request body. Sets Content-Length, since a PHP request that reads
/// `php://input` gets nothing without it.
fn request_with_body(
    port: u16,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> Resp {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("connect to {addr}: {e}"));
    sock.set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();

    let host = extra
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| format!("127.0.0.1:{port}"));
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in extra {
        if !k.eq_ignore_ascii_case("host") {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    sock.write_all(req.as_bytes()).unwrap();

    let mut reader = BufReader::new(sock);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line: {line:?}"));

    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap() == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut body = Vec::new();
    let _ = reader.read_to_end(&mut body);
    Resp {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn get(port: u16, path: &str) -> Resp {
    request(port, "GET", path, &[])
}

// --- server harness ------------------------------------------------------

/// A running `askr serve`, torn down on drop.
struct Server {
    child: Option<Child>,
    dir: PathBuf,
    port: u16,
    admin: u16,
    log: PathBuf,
}

/// An unused local port. Racy in principle, but each test gets its own and the
/// window between closing this listener and the server binding is tiny.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn unique_dir(name: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("askr-e2e-{name}-{n}"));
    std::fs::create_dir_all(dir.join("app")).unwrap();
    dir
}

impl Server {
    /// Write an app + config and start the server. `config` may use the
    /// placeholders `{PORT}`, `{ADMIN}` and `{ROOT}`.
    fn start(name: &str, files: &[(&str, &str)], config: &str) -> Server {
        let dir = unique_dir(name);
        Server::start_in(dir, files, config)
    }

    /// Same, but reusing a directory — for restart tests.
    ///
    /// Pass an empty `files` slice to restart without touching the app: rewriting
    /// the front controller changes its mtime, which Askr correctly reads as a
    /// deploy and refuses to restore a saved cache across.
    ///
    /// Retries on a lost port race. `free_port` has to release the port before the
    /// server can bind it, so with a dozen tests starting at once another one can
    /// take it in between — that's the harness being racy, not Askr, and it must not
    /// look like a product failure.
    fn start_in(dir: PathBuf, files: &[(&str, &str)], config: &str) -> Server {
        Server::start_in_with_env(dir, files, config, &[])
    }

    /// Same, with environment for the server process. Passed explicitly rather than set
    /// on the test process: tests share one process, so a global `set_var` would leak a
    /// token into whichever other test happened to start at the same moment.
    fn start_in_with_env(
        dir: PathBuf,
        files: &[(&str, &str)],
        config: &str,
        env: &[(&str, &str)],
    ) -> Server {
        for attempt in 1..=4 {
            match Server::try_start_in(dir.clone(), files, config, env) {
                Ok(s) => return s,
                Err(e) if e.contains("Address already in use") && attempt < 4 => {
                    std::thread::sleep(Duration::from_millis(200 * attempt));
                }
                Err(e) => panic!("could not start askr: {e}"),
            }
        }
        unreachable!()
    }

    fn try_start_in(
        dir: PathBuf,
        files: &[(&str, &str)],
        config: &str,
        env: &[(&str, &str)],
    ) -> Result<Server, String> {
        let app = dir.join("app");
        std::fs::create_dir_all(&app).unwrap();
        for (rel, contents) in files {
            let p = app.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, contents).unwrap();
        }
        let (port, admin) = (free_port(), free_port());
        let toml = config
            .replace("{PORT}", &port.to_string())
            .replace("{ADMIN}", &admin.to_string())
            .replace("{ROOT}", app.to_str().unwrap());
        let cfg = dir.join("askr.toml");
        std::fs::write(&cfg, toml).unwrap();

        let log = dir.join("askr.log");
        let out = std::fs::File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_askr"));
        cmd.args(["serve", "--config", cfg.to_str().unwrap()])
            .stdout(out)
            .stderr(err);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn askr");

        let mut s = Server {
            child: Some(child),
            dir,
            port,
            admin,
            log,
        };
        s.wait_ready()?;
        Ok(s)
    }

    /// Poll until the server answers, so tests never race the PHP boot.
    fn wait_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Some(c) = self.child.as_mut() {
                if let Ok(Some(status)) = c.try_wait() {
                    return Err(format!(
                        "askr exited early ({status}); log:\n{}",
                        self.log_contents()
                    ));
                }
            }
            let addr: SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
            if let Ok(mut sock) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
                // Connected: make sure PHP is actually answering, not just listening.
                let _ = sock.write_all(
                    format!(
                        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                        self.port
                    )
                    .as_bytes(),
                );
                let mut buf = [0u8; 16];
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                if let Ok(n) = sock.read(&mut buf) {
                    if n > 0 {
                        return Ok(());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err(format!(
            "askr did not become ready; log:\n{}",
            self.log_contents()
        ))
    }

    fn log_contents(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn pid(&self) -> i32 {
        self.child.as_ref().map(|c| c.id() as i32).unwrap_or(0)
    }

    fn signal(&self, sig: i32) {
        let pid = self.pid();
        if pid > 0 {
            // SAFETY: sending a signal to our own child.
            unsafe { libc::kill(pid, sig) };
        }
    }

    /// Graceful stop, waiting for the master to actually exit. A hang here is a
    /// real failure: the shutdown deadlock this suite exists for looked exactly
    /// like this.
    fn stop_gracefully(&mut self) {
        self.signal(libc::SIGTERM);
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "master did not exit within 30s of SIGTERM (shutdown hang); log:\n{}",
                        self.log_contents()
                    );
                }
            }
        }
    }

    fn admin_status(&self) -> String {
        request(self.admin, "GET", "/api/status", &[]).body
    }

    /// Block until the admin plane accepts connections.
    ///
    /// `wait_ready` waits for the *request* listener; the admin server binds on its own
    /// thread a moment later, so a test that goes straight at it races startup and fails
    /// with "Connection refused" perhaps one run in ten. A flaky test is worse than no
    /// test — it teaches you to re-run instead of read.
    fn wait_admin(&self) {
        let addr: SocketAddr = format!("127.0.0.1:{}", self.admin).parse().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "admin plane never came up on {addr}; log:\n{}",
            self.log_contents()
        );
    }

    fn log_has(&self, needle: &str) -> bool {
        self.log_contents().contains(needle)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SIGTERM first so workers drain; SIGKILL only if it won't go.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A page that is cacheable and prints something different every render, so a
/// cache hit is provable by comparing bodies.
const TOKEN_APP: &str = r#"<?php
header('Content-Type: text/plain');
$uri = strtok($_SERVER['REQUEST_URI'] ?? '/', '?');
if ($uri === '/nocache') { echo "nocache " . bin2hex(random_bytes(4)); exit; }
header('Askr-Cache: 300, tags=posts');
echo $uri . " " . bin2hex(random_bytes(4));
"#;

// --- tests ---------------------------------------------------------------

#[test]
fn serves_php_and_caches_responses() {
    let s = Server::start(
        "cache",
        &[("index.php", TOKEN_APP)],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "2"
[cache]
response_slots = 64
"#,
    );

    let first = get(s.port, "/page");
    assert_eq!(first.status, 200);
    assert_eq!(first.cache_state(), "MISS");

    let second = get(s.port, "/page");
    assert_eq!(second.cache_state(), "HIT");
    assert_eq!(
        first.body, second.body,
        "a HIT must return the stored bytes verbatim"
    );

    // A page that never opts in is never cached.
    let a = get(s.port, "/nocache");
    let b = get(s.port, "/nocache");
    assert_ne!(a.body, b.body, "an un-opted-in page must not be cached");
}

/// The bug: the cache key used the raw `Host` header (with port) while routing used
/// a normalised one, so `PURGE` could never match anything.
#[test]
fn purge_invalidates_one_url_and_ban_takes_a_glob() {
    let s = Server::start(
        "purge",
        &[("index.php", TOKEN_APP)],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "2"
[cache]
response_slots = 64
"#,
    );

    // Warm four URLs.
    for p in ["/posts/1", "/posts/12", "/cat/tech/rust", "/cat/food/pizza"] {
        get(s.port, p);
    }
    let keep = get(s.port, "/posts/12");
    assert_eq!(keep.cache_state(), "HIT");

    let purge = request(s.port, "PURGE", "/posts/1", &[]);
    assert_eq!(purge.status, 200, "PURGE from loopback is allowed");
    assert!(
        purge.body.contains("\"purged\":1"),
        "PURGE should report what it dropped, got {:?}",
        purge.body
    );
    assert_eq!(get(s.port, "/posts/1").cache_state(), "MISS");
    assert_eq!(
        get(s.port, "/posts/12").cache_state(),
        "HIT",
        "purging /posts/1 must not touch /posts/12"
    );

    let ban = request(s.port, "BAN", "/", &[("X-Ban-Url", "/cat/tech/*")]);
    assert_eq!(ban.status, 200);
    assert!(ban.body.contains("\"banned\":1"), "got {:?}", ban.body);
    assert_eq!(get(s.port, "/cat/tech/rust").cache_state(), "MISS");
    assert_eq!(
        get(s.port, "/cat/food/pizza").cache_state(),
        "HIT",
        "a glob must not reach outside itself"
    );

    // A regex-shaped pattern is refused loudly rather than matching nothing.
    let bad = request(s.port, "BAN", "/", &[("X-Ban-Url", "^/cat/.*")]);
    assert_eq!(bad.status, 400);
}

/// The bug class from 1.0.1/1.1.0: static serving handed out PHP sources and
/// dotfiles. This is the regression net for both.
#[test]
fn static_serving_never_leaks_sources_or_dotfiles() {
    let s = Server::start(
        "leak",
        &[
            ("index.php", "<?php echo \"app-ran\";"),
            (".env", "APP_KEY=base64:SECRET\nDB_PASSWORD=hunter2\n"),
            ("index.php.bak", "SECRET-BACKUP"),
            ("config.php~", "SECRET-TILDE"),
            (".git/config", "[core]\n"),
            (".well-known/security.txt", "contact: mailto:x@example.com"),
            ("build/app.js", "console.log(1)"),
        ],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "1"
"#,
    );

    for path in [
        "/.env",
        "/index.php",
        "/index.php.bak",
        "/config.php~",
        "/.git/config",
    ] {
        let r = get(s.port, path);
        assert!(
            !r.body.contains("SECRET") && !r.body.contains("APP_KEY") && !r.body.contains("[core]"),
            "{path} leaked: {:?}",
            r.body
        );
        assert!(
            r.body.contains("app-ran"),
            "{path} should fall through to the app, got {:?}",
            r.body
        );
    }

    // Legitimate files are still served.
    assert!(get(s.port, "/.well-known/security.txt")
        .body
        .contains("contact"));
    assert!(get(s.port, "/build/app.js").body.contains("console.log"));
}

/// The bug: the limit has to hold across the whole fleet. A per-process counter
/// would let 4 workers serve 4× the configured limit.
#[test]
fn rate_limit_holds_across_the_whole_fleet() {
    let s = Server::start(
        "ratelimit",
        &[("index.php", "<?php echo \"ok\";")],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "4"
[admin]
listen = "127.0.0.1:{ADMIN}"
[[ratelimit]]
path = "/login"
limit = 3
window = 60
"#,
    );

    let codes: Vec<u16> = (0..6).map(|_| get(s.port, "/login").status).collect();
    assert_eq!(
        codes,
        vec![200, 200, 200, 429, 429, 429],
        "3 per window means 3 in total, not 3 per worker"
    );

    let refused = get(s.port, "/login");
    assert!(
        refused.header("retry-after").is_some(),
        "a 429 must say when to come back"
    );
    assert_eq!(refused.header("x-ratelimit-limit"), Some("3"));

    // An unruled path is untouched.
    assert!((0..6).all(|_| get(s.port, "/").status == 200));

    // The counter lives in shared memory, so the master's /metrics must see it.
    // A process-local counter would report 0 here.
    let metrics = request(s.admin, "GET", "/metrics", &[]).body;
    let line = metrics
        .lines()
        .find(|l| l.starts_with("askr_ratelimit_blocked_total"))
        .unwrap_or("");
    let n: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        n >= 3,
        "master should see the fleet's refusals, got {line:?}"
    );
}

/// A rate limit must not be bypassable by inventing an `X-Forwarded-For`.
#[test]
fn forwarded_for_cannot_bypass_a_limit_without_trusted_proxies() {
    let s = Server::start(
        "xff",
        &[("index.php", "<?php echo \"ok\";")],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "2"
[[ratelimit]]
path = "/login"
limit = 3
window = 60
"#,
    );

    // Six requests, each claiming a different client address.
    let codes: Vec<u16> = (1..=6)
        .map(|i| {
            request(
                s.port,
                "GET",
                "/login",
                &[("X-Forwarded-For", &format!("9.9.9.{i}"))],
            )
            .status
        })
        .collect();
    assert_eq!(
        codes,
        vec![200, 200, 200, 429, 429, 429],
        "with no trusted_proxies the header must be ignored entirely"
    );
}

/// ESI: the shell is cached *with its tags*, so it can be a HIT while a fragment
/// is rendered per request.
#[test]
fn esi_assembles_fragments_per_request() {
    let app = r#"<?php
$uri = strtok($_SERVER['REQUEST_URI'] ?? '/', '?');
if ($uri === '/_esi/cart') { echo "[cart " . bin2hex(random_bytes(4)) . "]"; exit; }
if ($uri === '/_esi/broken') { http_response_code(500); echo "nope"; exit; }
header('Askr-Cache: 300');
header('Askr-ESI: on');
header('Content-Type: text/html');
echo "shell=" . bin2hex(random_bytes(4)) . ' ';
echo '<esi:include src="/_esi/cart"/>';
echo '<esi:remove><p>fallback</p></esi:remove>';
echo '<esi:include src="/_esi/broken"/>';
echo '<esi:include src="http://169.254.169.254/latest/meta-data/"/>';
"#;
    let s = Server::start(
        "esi",
        &[("index.php", app)],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "2"
[cache]
response_slots = 64
"#,
    );

    let a = get(s.port, "/");
    let b = get(s.port, "/");
    assert_eq!(b.cache_state(), "HIT", "the shell should be cached");

    let shell = |body: &str| body.split_whitespace().next().unwrap().to_string();
    assert_eq!(
        shell(&a.body),
        shell(&b.body),
        "the cached shell must be identical"
    );
    let cart = |body: &str| {
        let i = body.find("[cart ").expect("cart fragment missing");
        body[i..body[i..].find(']').unwrap() + i].to_string()
    };
    assert_ne!(
        cart(&a.body),
        cart(&b.body),
        "the uncached fragment must be re-rendered per request"
    );

    assert!(
        !b.body.contains("fallback"),
        "<esi:remove> must be stripped"
    );
    assert!(!b.body.contains("nope"), "a failing fragment leaves a hole");
    assert!(
        !b.body.contains("esi:include"),
        "no tag should survive into the response"
    );
    assert!(
        !b.body.contains("169.254"),
        "an off-origin src must be refused, not fetched"
    );
}

/// `[[cache.rule]]`: policy without touching the app. The app here sets no
/// `Askr-Cache` header at all.
#[test]
fn cache_rules_apply_without_app_support() {
    let s = Server::start(
        "rules",
        &[(
            "index.php",
            "<?php echo ($_SERVER['REQUEST_URI'] ?? '/') . ' ' . bin2hex(random_bytes(4));",
        )],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "2"
[cache]
response_slots = 64
[[cache.rule]]
path = "/admin/*"
action = "pass"
[[cache.rule]]
path = "/*"
ttl = 300
"#,
    );

    let a = get(s.port, "/page");
    let b = get(s.port, "/page");
    assert_eq!(b.cache_state(), "HIT", "a rule TTL should cache the page");
    assert_eq!(a.body, b.body);

    let x = get(s.port, "/admin/users");
    let y = get(s.port, "/admin/users");
    assert_eq!(y.cache_state(), "PASS", "a pass rule must be visible");
    assert_ne!(x.body, y.body, "a passed path must never be cached");
}

/// Two things at once: the cache is restored after a restart, and — because
/// `stop_gracefully` waits for the master — that a graceful shutdown actually
/// completes. The shutdown deadlock that hid this feature's own dump would fail
/// here.
#[test]
fn cache_survives_a_graceful_restart() {
    let dir = unique_dir("persist");
    let state = dir.join("rcache.bin");
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{{PORT}}"
root = "{{ROOT}}"
workers = "2"
[cache]
response_slots = 64
persist = "{}"
"#,
        state.to_str().unwrap()
    );

    let mut s = Server::start_in(dir.clone(), &[("index.php", TOKEN_APP)], &config);
    let warm = get(s.port, "/page");
    assert_eq!(get(s.port, "/page").cache_state(), "HIT");
    s.stop_gracefully();
    assert!(
        state.is_file(),
        "a graceful shutdown should have written the cache; log:\n{}",
        s.log_contents()
    );

    let s2 = Server::start_in(dir, &[], &config);
    let after = get(s2.port, "/page");
    assert_eq!(
        after.cache_state(),
        "HIT",
        "the first request after a restart should be served from the restored cache"
    );
    assert_eq!(
        warm.body, after.body,
        "restored bytes must be identical to what was saved"
    );
}

/// A dump must not be restored on top of different code.
#[test]
fn a_changed_app_invalidates_the_saved_cache() {
    let dir = unique_dir("persist-changed");
    let state = dir.join("rcache.bin");
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{{PORT}}"
root = "{{ROOT}}"
workers = "1"
[cache]
response_slots = 64
persist = "{}"
"#,
        state.to_str().unwrap()
    );

    let mut s = Server::start_in(dir.clone(), &[("index.php", TOKEN_APP)], &config);
    get(s.port, "/page");
    s.stop_gracefully();
    assert!(state.is_file());

    // "Deploy": change the front controller.
    let changed = format!("{TOKEN_APP}\n// deployed\n");
    let s2 = Server::start_in(dir, &[("index.php", &changed)], &config);
    assert_eq!(
        get(s2.port, "/page").cache_state(),
        "MISS",
        "a cache saved before a deploy must not be served after it"
    );
}

/// Canary reload on a healthy deploy: the rollout completes and the fleet stays up.
///
/// One worker on purpose. With several workers the kernel decides which one serves a
/// request, so the canary may legitimately not reach `canary_min_requests` and the
/// verdict becomes `inconclusive` — correct behaviour, but not something a test can
/// assert against. With a single worker every request is the canary's.
#[test]
fn canary_rollout_completes_on_a_healthy_deploy() {
    let s = Server::start(
        "canary-ok",
        &[("index.php", "<?php echo \"ok\";")],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "1"
[admin]
listen = "127.0.0.1:{ADMIN}"
[reload]
canary = true
canary_window = 2
canary_min_requests = 3
"#,
    );

    s.signal(libc::SIGHUP);
    // Serve enough traffic for the gate to have something to judge.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ok = 0;
    while Instant::now() < deadline {
        if get(s.port, "/").status == 200 {
            ok += 1;
        }
        if s.admin_status().contains("\"rollout\":\"ok\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ok > 3, "the site must keep serving during a rollout");
    assert!(
        s.admin_status().contains("\"rollout\":\"ok\""),
        "a healthy canary should roll the fleet; status: {}, log:\n{}",
        s.admin_status(),
        s.log_contents()
    );
}

/// Canary reload on a broken deploy: the rollout aborts instead of spreading.
///
/// One worker is used so the canary is the only thing serving the new code, which
/// makes the comparison against the (idle) fleet deterministic.
#[test]
fn canary_rollout_aborts_on_a_broken_deploy() {
    let app = r#"<?php
if (file_exists(__DIR__ . '/broken')) { http_response_code(500); echo "broken"; exit; }
echo "ok";
"#;
    let s = Server::start(
        "canary-bad",
        &[("index.php", app)],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "1"
[admin]
listen = "127.0.0.1:{ADMIN}"
[reload]
canary = true
canary_window = 2
canary_min_requests = 3
canary_max_error_rate = 2.0
"#,
    );

    // "Deploy" a build that fails, then reload into it.
    std::fs::write(s.dir.join("app/broken"), "1").unwrap();
    s.signal(libc::SIGHUP);

    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        let _ = get(s.port, "/");
        if s.admin_status().contains("\"rollout\":\"aborted\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        s.admin_status().contains("\"rollout\":\"aborted\""),
        "a canary serving 5xx must abort the rollout; status: {}, log:\n{}",
        s.admin_status(),
        s.log_contents()
    );
    assert!(
        s.log_has("canary UNHEALTHY"),
        "the abort should be loud in the log:\n{}",
        s.log_contents()
    );
}

/// A canary with no traffic must not be reported as healthy. The rollout continues
/// (a deploy shouldn't be blocked by an absence of evidence) but says `inconclusive`,
/// because a silent pass is indistinguishable from a real one.
#[test]
fn canary_with_no_traffic_is_inconclusive_not_healthy() {
    let s = Server::start(
        "canary-quiet",
        &[("index.php", "<?php echo \"ok\";")],
        r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
workers = "1"
[admin]
listen = "127.0.0.1:{ADMIN}"
[reload]
canary = true
canary_window = 2
canary_min_requests = 25
"#,
    );

    // Reload, then send far less traffic than the gate needs to judge.
    s.signal(libc::SIGHUP);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if !s.admin_status().contains("\"rollout\":\"rolling\"")
            && !s.admin_status().contains("\"rollout\":\"idle\"")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let status = s.admin_status();
    assert!(
        status.contains("\"rollout\":\"inconclusive\""),
        "a canary with no traffic must be inconclusive, not ok; status: {status}, log:\n{}",
        s.log_contents()
    );
    // And the site is still up: an inconclusive verdict rolls on rather than stalling.
    assert_eq!(get(s.port, "/").status, 200);
}

/// The cache oracle records real traffic and tells you what caching would buy — and,
/// crucially, refuses to recommend a page that isn't identical for every visitor.
#[test]
fn the_cache_oracle_separates_safe_pages_from_personalised_ones() {
    let app = r#"<?php
$uri = strtok($_SERVER['REQUEST_URI'] ?? '/', '?');
// Identical for everyone.
if ($uri === '/about') { echo "<h1>About us</h1>"; exit; }
// Looks cacheable, renders per visitor — the trap.
if ($uri === '/dashboard') { echo "user " . bin2hex(random_bytes(4)); exit; }
echo "home";
"#;
    let dir = unique_dir("oracle");
    let log = dir.join("traffic.jsonl");
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{{PORT}}"
root = "{{ROOT}}"
workers = "1"
traffic_log = "{}"
"#,
        log.to_str().unwrap()
    );
    let mut s = Server::start_in(dir, &[("index.php", app)], &config);

    for _ in 0..6 {
        assert_eq!(get(s.port, "/about").status, 200);
        assert_eq!(get(s.port, "/dashboard").status, 200);
    }
    s.stop_gracefully();

    let recorded = std::fs::read_to_string(&log).expect("traffic log should exist");
    assert!(
        recorded.lines().count() >= 12,
        "expected a line per PHP-served request, got {}",
        recorded.lines().count()
    );

    // Now the analysis, through the real subcommand.
    let out = Command::new(env!("CARGO_BIN_EXE_askr"))
        .args(["cache-report", log.to_str().unwrap()])
        .output()
        .expect("run cache-report");
    assert!(out.status.success(), "cache-report failed");
    let report = String::from_utf8_lossy(&out.stdout);

    let line = |needle: &str| {
        report
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line for {needle} in:\n{report}"))
            .to_string()
    };
    assert!(
        line("/about").contains("identical for every visitor"),
        "a shared page should be recommended: {}",
        line("/about")
    );
    assert!(
        line("/dashboard").contains("unsafe"),
        "a per-visitor page must be flagged, not recommended: {}",
        line("/dashboard")
    );
    // And it must not appear in the config the operator is invited to paste.
    let paste = report
        .split("Suggested askr.toml")
        .nth(1)
        .unwrap_or("")
        .to_string();
    assert!(
        !paste.contains("/dashboard"),
        "an unsafe page leaked into the suggested config:\n{paste}"
    );
}

/// Virtual hosts route by `Host`, and one host's cache can't be served to another.
#[test]
fn virtual_hosts_are_isolated() {
    let dir = unique_dir("vhost");
    for site in ["a", "b"] {
        let d = dir.join(format!("site-{site}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("index.php"),
            format!("<?php echo \"site-{site} \" . bin2hex(random_bytes(4));"),
        )
        .unwrap();
    }
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{{PORT}}"
root = "{}"

[[site]]
hosts = ["a.test"]
root = "{}"

[[site]]
hosts = ["b.test"]
root = "{}"
"#,
        dir.join("site-a").to_str().unwrap(),
        dir.join("site-a").to_str().unwrap(),
        dir.join("site-b").to_str().unwrap(),
    );
    // `start_in` writes into <dir>/app, which this test doesn't use as a root.
    let s = Server::start_in(dir, &[("index.php", "<?php echo 'unused';")], &config);

    let a = request(s.port, "GET", "/", &[("Host", "a.test")]);
    let b = request(s.port, "GET", "/", &[("Host", "b.test")]);
    assert!(a.body.contains("site-a"), "got {:?}", a.body);
    assert!(b.body.contains("site-b"), "got {:?}", b.body);
}

/// PHP diagnostics must reach the operator's log, never the visitor's browser.
///
/// Askr's built-in defaults were `display_errors=1` + `log_errors=0`, so a notice was
/// written into the response body — absolute filesystem paths and all — and nowhere
/// else. The published 1.4.0 image demonstrably served
/// `Deprecated: … in /app/vendor/laravel/framework/config/database.php` to anyone who
/// asked for the homepage, in both worker and per-request mode, because a framework
/// masks this only once its own error handler is installed and config files are parsed
/// before that. In worker mode the output also preceded the headers and corrupted the
/// response entirely.
#[test]
fn php_diagnostics_are_logged_not_served() {
    let dir = unique_dir("diagnostics");
    let app = "<?php trigger_error('path /etc/askr/secret.php', E_USER_DEPRECATED); echo 'OK';";
    let config = r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
"#;
    let s = Server::start_in(dir, &[("index.php", app)], config);

    let r = get(s.port, "/");
    assert_eq!(r.status, 200, "log:\n{}", s.log_contents());

    // The visitor sees the page and nothing about our filesystem.
    assert_eq!(r.body.trim(), "OK", "diagnostics leaked into the response");
    assert!(
        !r.body.contains("Deprecated") && !r.body.contains("secret.php"),
        "diagnostics leaked into the response: {:?}",
        r.body
    );

    // The operator sees the diagnostic. Suppressing it in the body is only correct if
    // it lands somewhere — silently dropping it would trade one failure for another.
    assert!(
        s.log_has("secret.php"),
        "diagnostic reached neither the body nor the log:\n{}",
        s.log_contents()
    );
}

/// A container healthcheck must not need a credential.
///
/// The image polled `/api/status`, which returns PIDs and memory figures and is therefore
/// gated by `ASKR_ADMIN_TOKEN` — so switching that token on made Docker report a healthy
/// container as unhealthy and restart it. `/healthz` is open by design; everything else on
/// the admin plane is now denied by default, so an endpoint added later is protected
/// without anyone remembering to protect it.
#[test]
fn healthz_needs_no_token_while_the_rest_of_the_admin_plane_does() {
    let dir = unique_dir("healthz");
    let config = r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
[admin]
listen = "127.0.0.1:{ADMIN}"
"#;
    let s = Server::start_in_with_env(
        dir,
        &[("index.php", "<?php echo 'ok';")],
        config,
        &[("ASKR_ADMIN_TOKEN", "s3cret")],
    );

    s.wait_admin();

    let health = get(s.admin, "/healthz");
    assert_eq!(
        health.status,
        200,
        "healthz must answer without a token; log:\n{}",
        s.log_contents()
    );
    assert_eq!(health.body.trim(), "ok");

    let status = get(s.admin, "/api/status");
    assert_eq!(
        status.status, 401,
        "status must be gated, got {:?}",
        status.body
    );

    let metrics = get(s.admin, "/metrics");
    assert_eq!(metrics.status, 401, "metrics must be gated");

    // Deny by default: a path nobody has whitelisted is refused, not served.
    let future = get(s.admin, "/api/something-added-later");
    assert_eq!(
        future.status, 401,
        "unknown admin paths must be denied, not 404'd"
    );

    // With the token, the gated endpoints work — the gate isn't just breaking things.
    let authed = request(
        s.admin,
        "GET",
        "/api/status",
        &[("Authorization", "Bearer s3cret")],
    );
    assert_eq!(
        authed.status, 200,
        "token should open it: {:?}",
        authed.body
    );
}

/// Worker mode must hand the raw request body *and* its content type to the worker
/// script, because that's the only way a urlencoded form post can be reconstructed.
///
/// Askr parses multipart bodies itself (it has to, to stream files to disk) but passes
/// `application/x-www-form-urlencoded` through untouched — and Symfony's
/// `Request::create()` fills the POST bag from its `$parameters` argument only, never
/// from the body. `examples/laravel-worker.php` therefore has to parse it, and it didn't:
/// every classic HTML form post in a Laravel app lost its fields, which surfaced as a 419
/// on submit because `_token` was missing. Found against a real Laravel 13 app.
///
/// This pins the contract the fix depends on. It runs in per-request mode, where the same
/// data reaches PHP as `$_SERVER` + `php://input`, so it needs no Laravel.
#[test]
fn a_urlencoded_post_body_reaches_php_intact_with_its_content_type() {
    let dir = unique_dir("urlencoded");
    let app = r#"<?php
$raw = file_get_contents('php://input');
$type = $_SERVER['CONTENT_TYPE'] ?? '(none)';
parse_str($raw, $parsed);
echo "type=$type|raw=$raw|token=" . ($parsed['_token'] ?? '(missing)');
"#;
    let config = r#"
[server]
listen = "127.0.0.1:{PORT}"
root = "{ROOT}"
"#;
    let s = Server::start_in(dir, &[("index.php", app)], config);

    let r = request_with_body(
        s.port,
        "POST",
        "/",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        "_token=abc123&email=a%40b.c",
    );
    assert_eq!(r.status, 200, "log:\n{}", s.log_contents());
    assert!(
        r.body.contains("type=application/x-www-form-urlencoded"),
        "content type must survive: {:?}",
        r.body
    );
    assert!(
        r.body.contains("raw=_token=abc123&email=a%40b.c"),
        "body must arrive byte-for-byte (a webhook signature depends on it): {:?}",
        r.body
    );
    assert!(
        r.body.contains("token=abc123"),
        "the body must be parseable into fields: {:?}",
        r.body
    );
}
