//! `askr doctor` — pre-flight checks before deploying.
//!
//! Verifies the embedded PHP build (version, non-ZTS), the extensions a modern
//! Laravel app needs, and platform support (io_uring on Linux). Returns false
//! if any critical check fails so `askr doctor` can gate a deploy.

use std::thread;

use askr_php::Interpreter;

/// Extensions a modern Laravel app requires.
const REQUIRED: &[&str] = &[
    "ctype",
    "filter",
    "hash",
    "json",
    "mbstring",
    "openssl",
    "pdo",
    "session",
    "tokenizer",
    "dom",
    "fileinfo",
    "phar",
];

/// Extensions many real apps need (Filament needs intl; gd for images; curl for
/// the HTTP client; pdo_mysql/zip are common). Present in the Linux release/
/// Docker image; the macOS dev build omits them. Not fatal.
const RECOMMENDED: &[&str] = &["intl", "curl", "gd", "pdo_mysql", "zip"];

struct PhpInfo {
    version: String,
    zts: bool,
    extensions: Vec<String>,
}

pub fn run(ini: Option<String>) -> bool {
    println!("askr doctor\n");

    let mut ok = true;

    match probe_php(ini) {
        Ok(info) => {
            check(&mut ok, true, &format!("embedded PHP {}", info.version));

            // non-ZTS is required.
            check(
                &mut ok,
                !info.zts,
                if info.zts {
                    "thread safety: ZTS  (REQUIRED: non-ZTS / NTS build)"
                } else {
                    "thread safety: non-ZTS (NTS)"
                },
            );

            for ext in REQUIRED {
                let present = info.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext));
                check(&mut ok, present, &format!("ext-{ext}"));
            }
            for ext in RECOMMENDED {
                let present = info.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext));
                mark(present, &format!("ext-{ext} (recommended)"));
            }

            let loaded = info.extensions.len();
            println!("  · {loaded} extensions loaded");
        }
        Err(e) => {
            check(
                &mut ok,
                false,
                &format!("embedded PHP failed to start: {e}"),
            );
        }
    }

    // Platform / io_uring (prod is Linux).
    println!();
    platform_check();

    println!();
    if ok {
        println!("✓ all critical checks passed");
    } else {
        println!("✗ critical checks failed — see above");
    }
    ok
}

fn probe_php(ini: Option<String>) -> Result<PhpInfo, String> {
    // The interpreter is non-Send, so probe it on its own thread.
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        if let Some(ini) = ini {
            std::env::set_var("ASKR_PHP_INI", ini);
        }
        let result = (|| {
            let mut php = Interpreter::new().map_err(|e| e.to_string())?;
            let out = php
                .eval(
                    r#"echo PHP_VERSION . "\n" . (PHP_ZTS ? "1" : "0") . "\n" . implode(",", get_loaded_extensions());"#,
                )
                .map_err(|e| e.to_string())?;
            Ok::<String, String>(out.output)
        })();
        let _ = tx.send(result);
    });

    let raw = rx.recv().map_err(|_| "probe thread died".to_string())??;
    let mut lines = raw.splitn(3, '\n');
    let version = lines.next().unwrap_or("").trim().to_string();
    let zts = lines.next().unwrap_or("0").trim() == "1";
    let extensions = lines
        .next()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(PhpInfo {
        version,
        zts,
        extensions,
    })
}

#[cfg(target_os = "linux")]
fn platform_check() {
    println!("platform: linux");
    // io_uring appeared in 5.1; report the running kernel.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } == 0 {
        let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let ok = kernel_at_least(&release, 5, 1);
        mark(ok, &format!("kernel {release} (io_uring needs ≥ 5.1)"));
    }

    // Actually probe io_uring — a recent kernel can still have it disabled via
    // `sysctl kernel.io_uring_disabled`. Not being available isn't fatal: Askr
    // falls back to the epoll/tokio I/O path.
    match probe_io_uring() {
        Ok(()) => mark(true, "io_uring: available (probed io_uring_setup)"),
        Err(reason) => mark(
            true,
            &format!("io_uring: unavailable ({reason}) — using the epoll/tokio path"),
        ),
    }
}

/// Probe io_uring by attempting `io_uring_setup(2)`; closes the ring on success.
#[cfg(target_os = "linux")]
fn probe_io_uring() -> Result<(), String> {
    // A zeroed `struct io_uring_params` (120 bytes on all current ABIs).
    let mut params = [0u8; 120];
    // SAFETY: raw syscall with 1 SQ entry and a correctly-sized params buffer.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            1 as libc::c_uint,
            params.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ret >= 0 {
        unsafe { libc::close(ret as libc::c_int) };
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        // ENOSYS = kernel too old; EPERM = disabled by sysctl/seccomp.
        Err(err.to_string())
    }
}

#[cfg(target_os = "linux")]
fn kernel_at_least(release: &str, major: u32, minor: u32) -> bool {
    let mut it = release.split(['.', '-']);
    let maj: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (maj, min) >= (major, minor)
}

#[cfg(not(target_os = "linux"))]
fn platform_check() {
    let os = std::env::consts::OS;
    println!("platform: {os}");
    mark(
        true,
        "io_uring: n/a on this OS (dev target; production is Linux with io_uring)",
    );
}

fn check(ok: &mut bool, pass: bool, label: &str) {
    mark(pass, label);
    if !pass {
        *ok = false;
    }
}

fn mark(pass: bool, label: &str) {
    println!("  {} {label}", if pass { "✓" } else { "✗" });
}
