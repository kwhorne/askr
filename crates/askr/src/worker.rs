//! A single worker process: one PHP interpreter, one small tokio runtime for
//! I/O, accepting on a listening socket that is **shared** with every other
//! worker (the master binds it once; workers inherit the fd across fork).
//!
//! All workers `accept()` on the same socket; the kernel hands each incoming
//! connection to exactly one of them. This is the classic prefork model — it
//! distributes load on Linux *and* macOS, unlike SO_REUSEPORT whose balancing
//! is Linux-only. This is the share-nothing seed: N independent
//! processes, no shared state, no locks on the hot path.

use std::net::{SocketAddr, TcpListener as StdListener};
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::php::Php;
use crate::server::{self, Config};

/// Bind a listening socket once (in the master). SO_REUSEADDR only; the socket
/// itself is shared with workers by inheriting the fd across fork.
pub fn bind_listener(addr: SocketAddr) -> anyhow::Result<StdListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Run the serve loop for one worker on an already-bound (shared) listener.
pub fn run_worker(
    listener: StdListener,
    config: Config,
    ini: Option<String>,
) -> anyhow::Result<()> {
    // Harden the worker (Linux) FIRST, before spawning the PHP/tokio threads —
    // seccomp (all-threads) + Landlock are inherited by every thread we create,
    // so the filter also covers the thread PHP runs on (where an exploit lives).
    if config.sandbox {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig {
            write_paths: config.sandbox_write.clone(),
        });
    }

    // Boot the interpreter (its own dedicated thread) before the runtime.
    // Worker mode boots the app once; otherwise each request runs fresh.
    // Shared flag: set when the server starts draining (graceful shutdown /
    // recycle) so the interpreter thread can tell an expected exit from an
    // unexpected one (a fatal/OOM) and, in the latter case, exit for respawn.
    let draining = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let php = match config.worker_script.clone() {
        Some(script) => Php::spawn_worker(script, ini, draining.clone())?,
        None => Php::spawn(ini)?,
    };

    // Stagger recycling across workers (distinct pid per worker) so they don't
    // all recycle at the same instant and leave a gap with no live workers.
    let recycle_after = stagger(config.max_requests);

    // Build the TLS acceptor (if configured) once per worker.
    let tls = build_tls(&config)?;

    listener.set_nonblocking(true)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2) // I/O only; PHP runs on its own pinned thread
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let listener = TcpListener::from_std(listener)?;
        server::run(
            listener,
            Arc::new(config),
            php,
            recycle_after,
            tls,
            draining,
        )
        .await
    })
}

/// Build the TLS acceptor for a config (self-signed or cert+key), if any.
pub fn build_tls(config: &Config) -> anyhow::Result<Option<tokio_rustls::TlsAcceptor>> {
    if config.tls_self_signed {
        let hosts = vec!["localhost".to_string(), config.listen.ip().to_string()];
        Ok(Some(crate::tls::self_signed(&hosts)?))
    } else {
        match (config.tls_cert.clone(), config.tls_key.clone()) {
            (Some(cert), Some(key)) => Ok(Some(crate::tls::acceptor(&cert, &key)?)),
            _ => Ok(None),
        }
    }
}

/// Run a sidecar process: boot the interpreter and run a PHP script to
/// completion (a queue worker loops forever; the scheduler ticks). Blocks.
/// Returns the script's exit code. No listener, no HTTP.
pub fn run_sidecar(script: std::path::PathBuf, ini: Option<String>) -> i32 {
    if let Some(ini) = ini {
        std::env::set_var("ASKR_PHP_INI", ini);
    }
    let mut php = match askr_php::Interpreter::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("askr sidecar: php init failed: {e}");
            return 1;
        }
    };
    crate::cache::register_bridge();
    crate::squeue::register_bridge();
    crate::broadcast::register_bridge();
    php.run_script(&script.to_string_lossy()).unwrap_or(1)
}

/// Run a supervised external command (e.g. an Inertia SSR node server,
/// `node bootstrap/ssr/ssr.mjs`). Execs it via `sh -c` in `$ASKR_APP_BASE`,
/// replacing this forked child so signals (SIGTERM) reach the command directly.
/// Returns only if exec fails.
pub fn run_command(cmd: &str) -> i32 {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(cmd);
    if let Ok(base) = std::env::var("ASKR_APP_BASE") {
        c.current_dir(base);
    }
    let err = c.exec(); // replaces the process on success
    eprintln!("askr sidecar: exec failed for {cmd:?}: {err}");
    127
}

/// Add a per-process jitter so workers recycle at different times.
fn stagger(max: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let span = (max / 2).max(1);
    max + (std::process::id() as usize) % span
}
