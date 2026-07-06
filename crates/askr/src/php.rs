//! The PHP execution worker.
//!
//! A non-ZTS interpreter is a per-thread, single-instance thing: it must be
//! created on, and only ever touched by, one OS thread. So we pin an
//! [`askr_php::Interpreter`] to a dedicated thread and feed it requests over a
//! channel. tokio owns the sockets; this thread owns PHP.
//!
//! Two modes, same `Php` handle and the same `handle()` seam for the server:
//!   - **per-request** ([`Php::spawn`]): each request runs the front controller
//!     from scratch (full framework bootstrap every time, like FPM).
//!   - **worker** ([`Php::spawn_worker`]): a long-lived worker script boots the
//!     app once and loops; each request reuses the booted app (the Octane model,
//!     in-process) — no per-request bootstrap.

use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tokio::sync::{mpsc, oneshot};

use askr_php::{Interpreter, Request, Response};

struct Job {
    req: Request,
    reply: oneshot::Sender<Result<Response, String>>,
}

/// A handle to the pinned PHP interpreter thread. Cheap to clone.
#[derive(Clone)]
pub struct Php {
    tx: mpsc::Sender<Job>,
}

impl Php {
    /// Per-request mode: boot an interpreter and run the front controller fresh
    /// for every request. The loop ends when the server drops the sender (during
    /// a graceful drain on recycle/shutdown).
    pub fn spawn(ini: Option<String>) -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<Job>(1024);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        thread::Builder::new()
            .name("askr-php".into())
            .spawn(move || {
                if let Some(ini) = ini {
                    std::env::set_var("ASKR_PHP_INI", ini);
                }
                let mut php = match Interpreter::new() {
                    Ok(p) => {
                        let _ = ready_tx.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                crate::cache::register_bridge();
                crate::squeue::register_bridge();
                crate::broadcast::register_bridge();
                tracing::info!(version = %php.php_version(), "embedded PHP ready (per-request)");

                while let Some(job) = rx.blocking_recv() {
                    let res = php.handle(&job.req).map_err(|e| e.to_string());
                    let _ = job.reply.send(res);
                }
            })?;

        wait_ready(ready_rx)?;
        Ok(Php { tx })
    }

    /// Worker mode: boot the app once via `script`, then serve many requests
    /// against the booted app — no per-request bootstrap. The loop ends when the
    /// server drops the sender (graceful drain on recycle/shutdown).
    pub fn spawn_worker(
        script: PathBuf,
        ini: Option<String>,
        draining: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>(1024);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let script_c = CString::new(script.to_string_lossy().as_ref().to_owned())?;

        thread::Builder::new()
            .name("askr-php-worker".into())
            .spawn(move || {
                if let Some(ini) = ini {
                    std::env::set_var("ASKR_PHP_INI", ini);
                }
                let _php = match Interpreter::new() {
                    Ok(p) => {
                        let _ = ready_tx.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                crate::cache::register_bridge();
                crate::squeue::register_bridge();
                crate::broadcast::register_bridge();
                tracing::info!("embedded PHP ready (worker mode), running worker script");

                let mut bridge = WorkerBridge { rx, pending: None };
                let ctx = &mut bridge as *mut WorkerBridge as *mut c_void;
                // SAFETY: runs on this thread with the engine started; ctx
                // outlives the call (the loop blocks here until it ends).
                let rc = unsafe {
                    askr_php::worker::askr_php_run_worker(
                        script_c.as_ptr(),
                        wait_trampoline,
                        reply_trampoline,
                        ctx,
                    )
                };
                tracing::debug!(rc, "worker loop ended");
                // Resilience: the worker loop only ends when the interpreter is
                // finished — either we're draining (graceful shutdown / recycle),
                // or the app fatal'd inside the loop (classically: hit PHP's
                // `memory_limit` after a slow leak in the long-lived worker). In
                // that second case the interpreter is dead but the process would
                // keep answering `502 php worker unavailable` forever, so exit and
                // let the supervisor respawn a fresh worker instead of flooding.
                if !draining.load(Ordering::SeqCst) {
                    tracing::error!(
                        rc,
                        "php worker interpreter exited unexpectedly (fatal/OOM?) — \
                         exiting for supervisor respawn"
                    );
                    std::process::exit(75);
                }
            })?;

        wait_ready(ready_rx)?;
        Ok(Php { tx })
    }

    /// CoW: run the worker script on the **current** thread (the template must be
    /// single-threaded when it forks). Boots the app; `askr_cow_ready()` inside
    /// the script forks the workers. Returns only in a recycled child.
    pub fn run_worker_current(script: &std::path::Path) -> i32 {
        let script_c = match CString::new(script.to_string_lossy().as_ref().to_owned()) {
            Ok(c) => c,
            Err(_) => return -1,
        };
        // SAFETY: uses the worker-bridge trampolines; the initial ctx is null —
        // each CoW worker swaps in its own via `cow_bridge()` before serving.
        unsafe {
            askr_php::worker::askr_php_run_worker(
                script_c.as_ptr(),
                wait_trampoline,
                reply_trampoline,
                std::ptr::null_mut(),
            )
        }
    }

    /// CoW: install a fresh serving bridge for this forked worker and return a
    /// `Php` handle for its server. The interpreter + booted app are inherited.
    pub fn cow_bridge() -> Php {
        let (tx, rx) = mpsc::channel::<Job>(1024);
        let bridge = Box::into_raw(Box::new(WorkerBridge { rx, pending: None }));
        // SAFETY: the shim uses this pointer as the worker ctx for wait/reply.
        unsafe { askr_php::worker::askr_php_swap_worker_ctx(bridge as *mut c_void) };
        Php { tx }
    }

    /// Run one request through the interpreter.
    pub async fn handle(&self, req: Request) -> Result<Response, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job { req, reply })
            .await
            .map_err(|_| "php worker unavailable".to_string())?;
        rx.await
            .map_err(|_| "php worker dropped reply".to_string())?
    }
}

fn wait_ready(ready_rx: std::sync::mpsc::Receiver<Result<(), String>>) -> anyhow::Result<()> {
    ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("php thread died during startup"))?
        .map_err(|e| anyhow::anyhow!("php_embed_init failed: {e}"))
}

// --- worker-mode bridge ---------------------------------------------------

struct WorkerBridge {
    rx: mpsc::Receiver<Job>,
    pending: Option<oneshot::Sender<Result<Response, String>>>,
}

impl WorkerBridge {
    fn wait(&mut self) -> i32 {
        match self.rx.blocking_recv() {
            Some(job) => {
                load_request(&job.req);
                self.pending = Some(job.reply);
                1
            }
            None => 0,
        }
    }

    fn reply(&mut self, body: Vec<u8>, headers: Vec<(String, String)>, status: u16) {
        if let Some(tx) = self.pending.take() {
            let _ = tx.send(Ok(Response {
                status,
                headers,
                body,
                php_status: 0,
            }));
        }
    }
}

/// Load a request into the shim's worker slot via the FFI setters.
fn load_request(req: &Request) {
    let method = CString::new(req.method.as_str()).unwrap_or_default();
    let uri = req
        .server_vars
        .iter()
        .find(|(k, _)| k == "REQUEST_URI")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "/".to_string());
    let uri = CString::new(uri).unwrap_or_default();
    let query = CString::new(req.query_string.as_str()).unwrap_or_default();

    // SAFETY: called on the interpreter thread from inside wait(); all pointers
    // are copied by the shim before the next call.
    unsafe {
        askr_php::worker::askr_req_reset();
        askr_php::worker::askr_req_set_meta(method.as_ptr(), uri.as_ptr(), query.as_ptr());
        for (k, v) in &req.server_vars {
            if let (Ok(kk), Ok(vv)) = (CString::new(k.as_str()), CString::new(v.as_str())) {
                askr_php::worker::askr_req_add_header(kk.as_ptr(), vv.as_ptr());
            }
        }
        askr_php::worker::askr_req_set_body(req.body.as_ptr() as *const c_char, req.body.len());
        for (k, v) in &req.post_fields {
            if let (Ok(kk), Ok(vv)) = (CString::new(k.as_str()), CString::new(v.as_str())) {
                askr_php::worker::askr_req_add_post(kk.as_ptr(), vv.as_ptr());
            }
        }
        for f in &req.files {
            if let (Ok(field), Ok(name), Ok(ty), Ok(tmp)) = (
                CString::new(f.field_name.as_str()),
                CString::new(f.file_name.as_str()),
                CString::new(f.content_type.as_str()),
                CString::new(f.tmp_path.as_str()),
            ) {
                askr_php::worker::askr_req_add_file(
                    field.as_ptr(),
                    name.as_ptr(),
                    ty.as_ptr(),
                    tmp.as_ptr(),
                    f.size,
                    f.error,
                );
            }
        }
    }
}

extern "C" fn wait_trampoline(ctx: *mut c_void) -> i32 {
    let bridge = unsafe { &mut *(ctx as *mut WorkerBridge) };
    bridge.wait()
}

extern "C" fn reply_trampoline(
    ctx: *mut c_void,
    body: *const c_char,
    blen: usize,
    hdrs: *const c_char,
    hlen: usize,
    status: i32,
) {
    let bridge = unsafe { &mut *(ctx as *mut WorkerBridge) };
    let body = if body.is_null() || blen == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(body as *const u8, blen) }.to_vec()
    };
    let headers = if hdrs.is_null() || hlen == 0 {
        Vec::new()
    } else {
        let raw = unsafe { std::slice::from_raw_parts(hdrs as *const u8, hlen) };
        parse_headers(raw)
    };
    bridge.reply(body, headers, status.max(0) as u16);
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
