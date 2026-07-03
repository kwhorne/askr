//! `askr-php` — embedded PHP for the Askr application server.
//!
//! M0 spike: prove that PHP's embed SAPI can be booted in-process from Rust and
//! that we can run PHP code and capture its output — with no FastCGI hop.
//!
//! The interpreter is non-ZTS (single interpreter per process/thread; memory is
//! shared later via CoW fork, per PRD §6.1). This type is therefore **not**
//! `Send`/`Sync`: an [`Interpreter`] must live and die on the thread that
//! created it. Later milestones pin one interpreter per core.

use std::ffi::{c_char, c_int, CStr, CString};
use std::marker::PhantomData;

extern "C" {
    fn askr_php_startup() -> c_int;
    fn askr_php_shutdown();
    fn askr_php_eval(code: *const c_char, out: *mut *mut c_char, out_len: *mut usize) -> c_int;
    fn askr_php_free(p: *mut c_char);
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
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Startup(c) => write!(f, "php_embed_init failed (code {c})"),
            Error::NulByte => write!(f, "PHP code contained an interior NUL byte"),
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

    /// Report the embedded engine's `PHP_VERSION`.
    pub fn php_version(&mut self) -> String {
        self.eval("echo PHP_VERSION;")
            .map(|e| e.output)
            .unwrap_or_default()
    }
}

impl Drop for Interpreter {
    fn drop(&mut self) {
        // SAFETY: matches a successful startup on this thread.
        unsafe { askr_php_shutdown() };
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
        let hello = php.eval(r#"echo "hello from PHP " . PHP_VERSION;"#).unwrap();
        assert!(hello.ok(), "status {}", hello.status);
        assert!(hello.output.starts_with("hello from PHP 8.4"), "{hello:?}");

        // 2. Real computation crosses the FFI boundary intact.
        let calc = php.eval("echo array_sum(range(1, 100));").unwrap();
        assert_eq!(calc.output, "5050");

        // 3. Bundled extensions (json) are available.
        let json = php.eval(r#"echo json_encode(["a" => 1, "b" => [2, 3]]);"#).unwrap();
        assert_eq!(json.output, r#"{"a":1,"b":[2,3]}"#);

        // 4. $_SERVER superglobal exists — this is the seam serve_php() fills.
        let srv = php
            .eval(r#"$_SERVER["ASKR"] = "yes"; echo $_SERVER["ASKR"];"#)
            .unwrap();
        assert_eq!(srv.output, "yes");
    }
}
