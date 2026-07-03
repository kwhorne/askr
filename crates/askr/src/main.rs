//! Askr — a standalone, memory-safe PHP application server.
//!
//! A1: serve a single PHP application over HTTP through the embedded interpreter.
//! A3: scale across cores with SO_REUSEPORT + one forked worker process per core
//!     (non-ZTS means one interpreter per process, so we scale by processes).

mod cgi;
mod php;
mod server;
mod worker;

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use clap::{Parser, Subcommand};

use crate::server::Config;
use crate::worker::{bind_listener, run_worker};

#[derive(Parser)]
#[command(name = "askr", version, about = "The smartest, most efficient PHP web server.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a PHP application over HTTP.
    Serve {
        /// Document root (the app's public/ directory). Defaults to ./public
        /// if present, otherwise the current directory.
        #[arg(long)]
        root: Option<PathBuf>,

        /// Front controller, relative to the document root.
        #[arg(long, default_value = "index.php")]
        front: PathBuf,

        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:8000")]
        listen: SocketAddr,

        /// Worker processes. Defaults to the number of CPU cores. Each worker
        /// is an independent process with its own PHP interpreter.
        #[arg(long)]
        workers: Option<usize>,

        /// Mark requests as HTTPS in $_SERVER (when behind a TLS terminator).
        #[arg(long)]
        https: bool,

        /// Extra php.ini lines, e.g. to load opcache. Overrides $ASKR_PHP_INI.
        #[arg(long)]
        ini: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "askr=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            root,
            front,
            listen,
            workers,
            https,
            ini,
        } => {
            let docroot = resolve_root(root)?;
            let script = docroot.join(&front);
            if !script.is_file() {
                anyhow::bail!(
                    "front controller not found: {} (use --root / --front)",
                    script.display()
                );
            }

            let ini = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok());
            let config = Config {
                docroot,
                front_controller: front,
                listen,
                https,
            };

            let workers = workers.unwrap_or_else(default_workers).max(1);
            let listener = bind_listener(listen)?;
            if workers == 1 {
                // Single process: no fork, run the worker inline.
                tracing::info!(%listen, workers = 1, "askr serving (single process)");
                run_worker(listener, config, ini)
            } else {
                supervise(listener, config, ini, workers)
            }
        }
    }
}

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

// --- multi-process supervisor --------------------------------------------

const MAX_WORKERS: usize = 512;
static CHILDREN: [AtomicI32; MAX_WORKERS] = [const { AtomicI32::new(0) }; MAX_WORKERS];

/// Fork `workers` child processes, each running an independent worker on the
/// shared inherited listener, then supervise them: forward termination signals
/// and reap exits.
fn supervise(
    listener: std::net::TcpListener,
    config: Config,
    ini: Option<String>,
    workers: usize,
) -> anyhow::Result<()> {
    let workers = workers.min(MAX_WORKERS);
    let listen_fd: RawFd = listener.as_raw_fd();

    for i in 0..workers {
        // SAFETY: fork before any tokio runtime exists on this thread; the child
        // builds its own runtime. Only async-signal-safe work runs pre-exec.
        match unsafe { libc::fork() } {
            0 => {
                // Child: adopt the inherited listener fd and run the worker.
                let inherited = unsafe { std::net::TcpListener::from_raw_fd(listen_fd) };
                let code = match run_worker(inherited, config.clone(), ini.clone()) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("askr worker {i}: {e}");
                        1
                    }
                };
                std::process::exit(code);
            }
            -1 => anyhow::bail!("fork failed: {}", std::io::Error::last_os_error()),
            pid => {
                CHILDREN[i].store(pid, Ordering::SeqCst);
                tracing::info!(pid, worker = i, "spawned worker");
            }
        }
    }

    install_signal_forwarding();
    tracing::info!(%config.listen, workers, "askr master supervising");

    // Reap children. A3: no auto-restart yet (that's A5 hardening).
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid <= 0 {
            break;
        }
        for c in CHILDREN.iter() {
            let _ = c.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
        tracing::warn!(pid, "worker exited");
        if CHILDREN.iter().all(|c| c.load(Ordering::SeqCst) == 0) {
            break;
        }
    }
    Ok(())
}

extern "C" fn forward_signal(_sig: libc::c_int) {
    // async-signal-safe: atomic loads + kill()
    for c in CHILDREN.iter() {
        let pid = c.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
    unsafe { libc::_exit(0) };
}

fn install_signal_forwarding() {
    unsafe {
        libc::signal(libc::SIGINT, forward_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, forward_signal as *const () as libc::sighandler_t);
    }
}

fn resolve_root(root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let root = match root {
        Some(r) => r,
        None => {
            let public = PathBuf::from("public");
            if public.is_dir() {
                public
            } else {
                PathBuf::from(".")
            }
        }
    };
    let canonical = std::fs::canonicalize(&root)
        .map_err(|e| anyhow::anyhow!("bad --root {}: {e}", root.display()))?;
    Ok(canonical)
}
