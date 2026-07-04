//! `askr-php` — embedded PHP for the Askr application server.
//!
//! M0 spike: prove that PHP's embed SAPI can be booted in-process from Rust and
//! that we can run PHP code and capture its output — with no FastCGI hop.
//!
//! The interpreter is non-ZTS (single interpreter per process/thread; memory is
//! shared later via CoW fork). This type is therefore **not**
//! `Send`/`Sync`: an [`Interpreter`] must live and die on the thread that
//! created it. Later milestones pin one interpreter per core.

use std::ffi::{c_char, c_int, CStr, CString};
use std::marker::PhantomData;

extern "C" {
    fn askr_php_startup() -> c_int;
    fn askr_php_shutdown();
    fn askr_php_eval(code: *const c_char, out: *mut *mut c_char, out_len: *mut usize) -> c_int;
    fn askr_php_free(p: *mut c_char);
    fn askr_php_run_script(script: *const c_char) -> c_int;

    #[allow(clippy::too_many_arguments)]
    fn askr_php_handle(
        script_filename: *const c_char,
        method: *const c_char,
        query_string: *const c_char,
        content_type: *const c_char,
        content_length: usize,
        body: *const c_char,
        body_len: usize,
        var_names: *const *const c_char,
        var_values: *const *const c_char,
        nvars: c_int,
        cookie: *const c_char,
        out_body: *mut *mut c_char,
        out_body_len: *mut usize,
        out_headers: *mut *mut c_char,
        out_headers_len: *mut usize,
        out_status: *mut c_int,
    ) -> c_int;
}

/// An in-process PHP interpreter. Boot once, evaluate many times, drop to shut
/// down. Only one may exist per thread at a time.
pub struct Interpreter {
    // Not Send/Sync: the Zend engine is thread-local (non-ZTS).
    _not_send: PhantomData<*const ()>,
}

/// The result of evaluating PHP code.
#[derive(Debug)]
pub struct Eval {
    /// Everything the script wrote (echo/print/output buffering flushes).
    pub output: String,
    /// Raw shim status: 0 ok, -1 eval FAILURE, -2 uncaught bailout.
    pub status: i32,
}

impl Eval {
    /// True when PHP evaluated the snippet without a fatal error/bailout.
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

#[derive(Debug)]
pub enum Error {
    /// `php_embed_init` failed.
    Startup(i32),
    /// The code contained an interior NUL byte.
    NulByte,
    /// `php_request_startup` failed for a request.
    RequestStartup,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Startup(c) => write!(f, "php_embed_init failed (code {c})"),
            Error::NulByte => write!(f, "PHP code contained an interior NUL byte"),
            Error::RequestStartup => write!(f, "php_request_startup failed"),
        }
    }
}

impl std::error::Error for Error {}

impl Interpreter {
    /// Boot the Zend engine in this thread.
    pub fn new() -> Result<Self, Error> {
        // SAFETY: FFI into the embed shim; single-threaded startup.
        let rc = unsafe { askr_php_startup() };
        if rc != 0 {
            return Err(Error::Startup(rc));
        }
        Ok(Interpreter {
            _not_send: PhantomData,
        })
    }

    /// Evaluate a PHP code string (as if wrapped in `<?php ... ?>`), returning
    /// captured output. Do not include the opening `<?php` tag.
    pub fn eval(&mut self, code: &str) -> Result<Eval, Error> {
        let c = CString::new(code).map_err(|_| Error::NulByte)?;
        let mut out: *mut c_char = std::ptr::null_mut();
        let mut len: usize = 0;

        // SAFETY: `c` outlives the call; the shim writes a malloc'd buffer into
        // `out`/`len` which we copy and then free via askr_php_free.
        let status = unsafe { askr_php_eval(c.as_ptr(), &mut out, &mut len) };

        let output = if out.is_null() {
            String::new()
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(out as *const u8, len) };
            let s = String::from_utf8_lossy(bytes).into_owned();
            unsafe { askr_php_free(out) };
            s
        };

        Ok(Eval {
            output,
            status: status as i32,
        })
    }

    /// Run a PHP file to completion like a CLI invocation (for queue/scheduler
    /// sidecars). Blocks until the script returns; output goes to stdout.
    /// Returns the script's exit status.
    pub fn run_script(&mut self, path: &str) -> Result<i32, Error> {
        let c = CString::new(path).map_err(|_| Error::NulByte)?;
        // SAFETY: FFI into the shim on this thread; `c` outlives the call.
        let rc = unsafe { askr_php_run_script(c.as_ptr()) };
        Ok(rc as i32)
    }

    /// Report the embedded engine's `PHP_VERSION`.
    pub fn php_version(&mut self) -> String {
        self.eval("echo PHP_VERSION;")
            .map(|e| e.output)
            .unwrap_or_default()
    }
}

impl Interpreter {
    /// Execute a real PHP script file as a web request: sets `$_SERVER`, feeds
    /// the body to `php://input`, runs the front controller, and captures the
    /// HTTP status, headers and body. This is the full contract grove's
    /// `serve_php()` needs — the in-process replacement for a FastCGI round-trip.
    pub fn handle(&mut self, req: &Request) -> Result<Response, Error> {
        // Own every C string for the duration of the call.
        let script = cstring(&req.script_filename)?;
        let method = cstring(&req.method)?;
        let query = cstring(&req.query_string)?;
        let content_type = opt_cstring(req.content_type.as_deref())?;
        let cookie = opt_cstring(req.cookie.as_deref())?;

        let mut names: Vec<CString> = Vec::with_capacity(req.server_vars.len());
        let mut values: Vec<CString> = Vec::with_capacity(req.server_vars.len());
        for (k, v) in &req.server_vars {
            names.push(cstring(k)?);
            values.push(cstring(v)?);
        }
        let name_ptrs: Vec<*const c_char> = names.iter().map(|c| c.as_ptr()).collect();
        let value_ptrs: Vec<*const c_char> = values.iter().map(|c| c.as_ptr()).collect();

        let mut out_body: *mut c_char = std::ptr::null_mut();
        let mut out_body_len: usize = 0;
        let mut out_headers: *mut c_char = std::ptr::null_mut();
        let mut out_headers_len: usize = 0;
        let mut status: c_int = 0;

        // SAFETY: all pointers outlive the call; output buffers are copied then
        // freed via askr_php_free.
        let rc = unsafe {
            askr_php_handle(
                script.as_ptr(),
                method.as_ptr(),
                query.as_ptr(),
                content_type
                    .as_ref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
                req.body.len(),
                req.body.as_ptr() as *const c_char,
                req.body.len(),
                name_ptrs.as_ptr(),
                value_ptrs.as_ptr(),
                names.len() as c_int,
                cookie.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                &mut out_body,
                &mut out_body_len,
                &mut out_headers,
                &mut out_headers_len,
                &mut status,
            )
        };

        if rc < 0 {
            return Err(Error::RequestStartup);
        }

        let body = take_bytes(out_body, out_body_len);
        let headers_raw = take_bytes(out_headers, out_headers_len);
        let headers = parse_headers(&headers_raw);

        Ok(Response {
            status: status as u16,
            headers,
            body,
            php_status: rc as i32,
        })
    }
}

/// A web request handed to the embedded interpreter.
#[derive(Debug, Default, Clone)]
pub struct Request {
    /// Absolute path to the PHP script to execute (the front controller).
    pub script_filename: String,
    /// HTTP method (`GET`, `POST`, …).
    pub method: String,
    /// Raw query string (without the leading `?`).
    pub query_string: String,
    /// `Content-Type` of the request body, if any.
    pub content_type: Option<String>,
    /// Raw `Cookie` header, if any.
    pub cookie: Option<String>,
    /// Raw request body (available to PHP via `php://input`).
    pub body: Vec<u8>,
    /// The full `$_SERVER` map (CGI-style: REQUEST_METHOD, REQUEST_URI,
    /// SCRIPT_NAME, HTTP_* headers, DOCUMENT_ROOT, HTTPS, …).
    pub server_vars: Vec<(String, String)>,
}

/// The HTTP response produced by the embedded interpreter.
#[derive(Debug)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response headers, in order.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
    /// Shim status: 0 ok, 1 script fatal, 2 engine bailout.
    pub php_status: i32,
}

fn cstring(s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::NulByte)
}

fn opt_cstring(s: Option<&str>) -> Result<Option<CString>, Error> {
    match s {
        Some(s) => Ok(Some(cstring(s)?)),
        None => Ok(None),
    }
}

fn take_bytes(ptr: *mut c_char, len: usize) -> Vec<u8> {
    if ptr.is_null() {
        return Vec::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }.to_vec();
    unsafe { askr_php_free(ptr) };
    bytes
}

fn parse_headers(raw: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            line.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

impl Drop for Interpreter {
    fn drop(&mut self) {
        // SAFETY: matches a successful startup on this thread.
        unsafe { askr_php_shutdown() };
    }
}

/// Persistent worker-loop bridge (A4). The interpreter runs a long-lived worker
/// script that boots the app once and loops; each iteration blocks in
/// `wait`, runs the app, and delivers the response through `reply`. All items
/// here are raw FFI — the callbacks use the C ABI and receive an opaque `ctx`
/// pointer, so the caller wires up its own trampolines and state.
pub mod worker {
    use std::ffi::{c_char, c_int, c_void};

    /// Blocks until a request is ready. Return 1 to process, 0 to stop the loop.
    pub type WaitFn = extern "C" fn(*mut c_void) -> c_int;
    /// Delivers a finished response (body, headers, status) back to the caller.
    pub type ReplyFn =
        extern "C" fn(*mut c_void, *const c_char, usize, *const c_char, usize, c_int);

    extern "C" {
        /// Run the worker script in one long-lived request context. Blocks
        /// until the loop ends. Must be called on the interpreter's thread,
        /// after the engine has started.
        pub fn askr_php_run_worker(
            script: *const c_char,
            wait: WaitFn,
            reply: ReplyFn,
            ctx: *mut c_void,
        ) -> c_int;

        /// Clear the current worker request.
        pub fn askr_req_reset();
        /// Set method / uri / query for the current worker request.
        pub fn askr_req_set_meta(method: *const c_char, uri: *const c_char, query: *const c_char);
        /// Append a header to the current worker request.
        pub fn askr_req_add_header(name: *const c_char, value: *const c_char);
        /// Set the body of the current worker request.
        pub fn askr_req_set_body(ptr: *const c_char, len: usize);
    }
}

/// Shared-cache bridge (A #3). The shim registers `askr_cache_*` PHP functions
/// whose C implementations call these callbacks; the server registers callbacks
/// backed by its shared-memory cache. Raw FFI — the caller provides C-ABI
/// trampolines.
pub mod cache_bridge {
    use std::ffi::{c_char, c_int, c_long};

    pub type GetFn = extern "C" fn(*const c_char, usize, *mut *mut c_char, *mut usize) -> c_int;
    pub type SetFn = extern "C" fn(*const c_char, usize, *const c_char, usize, c_long) -> c_int;
    pub type DelFn = extern "C" fn(*const c_char, usize) -> c_int;
    pub type IncrFn = extern "C" fn(*const c_char, usize, c_long, c_long) -> c_long;
    pub type FlushFn = extern "C" fn();

    extern "C" {
        /// Register the cache callbacks with the shim. Call once per process,
        /// after the engine has started.
        pub fn askr_php_set_cache_bridge(g: GetFn, s: SetFn, d: DelFn, i: IncrFn, f: FlushFn);
    }
}

/// Broadcast bridge (A #4). `askr_broadcast($channel, $payload)` in PHP calls
/// this callback, which publishes into the shared broadcast ring.
pub mod broadcast_bridge {
    use std::ffi::{c_char, c_int};

    pub type BroadcastFn = extern "C" fn(*const c_char, usize, *const c_char, usize) -> c_int;

    extern "C" {
        pub fn askr_php_set_broadcast_bridge(f: BroadcastFn);
    }
}

/// Convenience: helper used by tests to read a C string (unused in normal flow).
#[allow(dead_code)]
unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The engine is a process-global singleton; run every assertion inside one
    // boot so the whole suite is a single init/shutdown cycle.
    #[test]
    fn embedding_works() {
        let mut php = Interpreter::new().expect("php_embed_init");

        // 1. Hello world — the M0 goal.
        let hello = php
            .eval(r#"echo "hello from PHP " . PHP_VERSION;"#)
            .unwrap();
        assert!(hello.ok(), "status {}", hello.status);
        assert!(hello.output.starts_with("hello from PHP 8.4"), "{hello:?}");

        // 2. Real computation crosses the FFI boundary intact.
        let calc = php.eval("echo array_sum(range(1, 100));").unwrap();
        assert_eq!(calc.output, "5050");

        // 3. Bundled extensions (json) are available.
        let json = php
            .eval(r#"echo json_encode(["a" => 1, "b" => [2, 3]]);"#)
            .unwrap();
        assert_eq!(json.output, r#"{"a":1,"b":[2,3]}"#);

        // 4. $_SERVER superglobal exists — this is the seam serve_php() fills.
        let srv = php
            .eval(r#"$_SERVER["ASKR"] = "yes"; echo $_SERVER["ASKR"];"#)
            .unwrap();
        assert_eq!(srv.output, "yes");

        // 5. Full request contract: a real script file, $_SERVER injected,
        //    body via php://input, headers + status captured.
        let script = std::env::temp_dir().join("askr_front.php");
        std::fs::write(
            &script,
            r#"<?php
                header('X-Askr: hit');
                setcookie('sess', 'abc');
                http_response_code(201);
                $in = file_get_contents('php://input');
                echo json_encode([
                    'method' => $_SERVER['REQUEST_METHOD'],
                    'uri'    => $_SERVER['REQUEST_URI'],
                    'q'      => $_SERVER['QUERY_STRING'],
                    'custom' => $_SERVER['HTTP_X_CUSTOM'] ?? null,
                    'ct'     => $_SERVER['CONTENT_TYPE'] ?? null,
                    'body'   => $in,
                ]);
            "#,
        )
        .unwrap();

        let script_path = script.to_string_lossy().into_owned();
        let req = Request {
            script_filename: script_path.clone(),
            method: "POST".into(),
            query_string: "a=1&b=2".into(),
            content_type: Some("application/json".into()),
            cookie: None,
            body: br#"{"hi":1}"#.to_vec(),
            server_vars: vec![
                ("REQUEST_METHOD".into(), "POST".into()),
                ("REQUEST_URI".into(), "/api?a=1&b=2".into()),
                ("QUERY_STRING".into(), "a=1&b=2".into()),
                ("SCRIPT_NAME".into(), "/index.php".into()),
                ("SCRIPT_FILENAME".into(), script_path.clone()),
                ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
                ("CONTENT_TYPE".into(), "application/json".into()),
                ("CONTENT_LENGTH".into(), "8".into()),
                ("HTTP_X_CUSTOM".into(), "abc".into()),
            ],
        };

        let resp = php.handle(&req).unwrap();
        assert_eq!(resp.status, 201, "resp: {resp:?}");
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "X-Askr" && v == "hit"),
            "headers: {:?}",
            resp.headers
        );
        // setcookie() must reach us as a Set-Cookie header.
        assert!(
            resp.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("Set-Cookie")),
            "headers: {:?}",
            resp.headers
        );

        let body: serde_json::Value = serde_json::from_slice(&resp.body).expect("json body");
        assert_eq!(body["method"], "POST");
        assert_eq!(body["uri"], "/api?a=1&b=2");
        assert_eq!(body["q"], "a=1&b=2");
        assert_eq!(body["custom"], "abc");
        assert_eq!(body["ct"], "application/json");
        assert_eq!(body["body"], r#"{"hi":1}"#);

        let _ = std::fs::remove_file(&script);
    }
}
