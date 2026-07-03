//! Askr — a standalone, memory-safe PHP application server.
//!
//! A1: serve a single PHP application over HTTP through the embedded interpreter.
//! A3: scale across cores with SO_REUSEPORT + one forked worker process per core
//!     (non-ZTS means one interpreter per process, so we scale by processes).

mod cgi;
mod doctor;
mod php;
mod server;
mod tls;
mod worker;

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

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

        /// Worker script: boot the app once and serve many requests against it
        /// (the Octane model, in-process). When omitted, each request runs the
        /// front controller from scratch.
        #[arg(long)]
        worker_script: Option<PathBuf>,

        /// Recycle each worker after handling this many requests (0 = never).
        /// Guards against memory leaks / state drift; the supervisor respawns a
        /// fresh worker to replace it. Requires the multi-process supervisor.
        #[arg(long, default_value = "0")]
        max_requests: usize,

        /// TLS certificate chain (PEM). Enables HTTPS (ALPN: h2, http/1.1).
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,

        /// TLS private key (PEM).
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,

        /// Generate a self-signed cert on startup (dev/testing; browsers warn).
        #[arg(long, conflicts_with = "tls_cert")]
        tls_self_signed: bool,
    },

    /// Pre-flight checks: PHP build, extensions, and platform support.
    Doctor {
        /// Extra php.ini lines (e.g. to load opcache).
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
            worker_script,
            max_requests,
            tls_cert,
            tls_key,
            tls_self_signed,
        } => {
            let docroot = resolve_root(root)?;
            let script = docroot.join(&front);
            if !script.is_file() {
                anyhow::bail!(
                    "front controller not found: {} (use --root / --front)",
                    script.display()
                );
            }
            if let Some(ws) = &worker_script {
                if !ws.is_file() {
                    anyhow::bail!("worker script not found: {}", ws.display());
                }
            }

            if let Some(c) = &tls_cert {
                if !c.is_file() {
                    anyhow::bail!("TLS cert not found: {}", c.display());
                }
            }
            let tls_on = tls_cert.is_some() || tls_self_signed;
            let ini = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok());
            let config = Config {
                docroot,
                front_controller: front,
                listen,
                https: https || tls_on, // TLS implies https in $_SERVER
                worker_script,
                max_requests,
                tls_cert,
                tls_key,
                tls_self_signed,
            };

            let workers = workers.unwrap_or_else(default_workers).max(1);
            let listener = bind_listener(listen)?;
            // Recycling needs the supervisor to respawn workers, so use it
            // whenever workers > 1 or a request cap is set.
            if workers == 1 && max_requests == 0 {
                tracing::info!(%listen, workers = 1, "askr serving (single process)");
                run_worker(listener, config, ini)
            } else {
                supervise(listener, config, ini, workers)
            }
        }
        Command::Doctor { ini } => {
            let ini = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok());
            let ok = doctor::run(ini);
            if ok {
                Ok(())
            } else {
                std::process::exit(1);
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
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER_COUNT: AtomicUsize = AtomicUsize::new(0);
// Next slot to roll during a graceful reload; >= WORKER_COUNT means "not rolling".
static RELOAD_CURSOR: AtomicUsize = AtomicUsize::new(usize::MAX);

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
    WORKER_COUNT.store(workers, Ordering::SeqCst);
    let listen_fd: RawFd = listener.as_raw_fd();

    // Fork one worker into slot `i`. In the child this never returns (it runs
    // the worker and exits); in the parent it records the pid.
    let spawn_slot = |i: usize| {
        // SAFETY: fork before any tokio runtime exists on this thread; the child
        // builds its own runtime. Only async-signal-safe work runs pre-exec.
        match unsafe { libc::fork() } {
            0 => {
                // Child: the master coordinates lifecycle. Ignore SIGINT/SIGHUP
                // (don't inherit the master's handlers); SIGTERM is left for the
                // worker's tokio runtime to catch and drain gracefully.
                unsafe {
                    libc::signal(libc::SIGINT, libc::SIG_IGN);
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                    libc::signal(libc::SIGTERM, libc::SIG_DFL);
                }
                let inherited = unsafe { std::net::TcpListener::from_raw_fd(listen_fd) };
                let code = match run_worker(inherited, config.clone(), ini.clone()) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("askr worker {i}: {e:#}");
                        1
                    }
                };
                std::process::exit(code);
            }
            -1 => {
                tracing::error!(worker = i, "fork failed: {}", std::io::Error::last_os_error());
            }
            pid => {
                CHILDREN[i].store(pid, Ordering::SeqCst);
                tracing::info!(pid, worker = i, "spawned worker");
            }
        }
    };

    for i in 0..workers {
        spawn_slot(i);
    }

    install_signals();
    tracing::info!(%config.listen, workers, max_requests = config.max_requests, "askr master supervising (SIGHUP = graceful reload)");

    // Reap exited workers. Respawn a replacement (recycling / crash resilience /
    // rolling reload) unless we're shutting down. SIGTERM to a worker makes it
    // drain gracefully; the master forwards SIGTERM on SIGINT/SIGTERM (shutdown)
    // and on SIGHUP (reload, but keeps respawning fresh workers).
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == -1 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue, // interrupted by a signal; retry
                _ => break,                     // ECHILD: no children left
            }
        }
        if pid <= 0 {
            continue;
        }
        for i in 0..workers {
            if CHILDREN[i].load(Ordering::SeqCst) == pid {
                CHILDREN[i].store(0, Ordering::SeqCst);
                if SHUTDOWN.load(Ordering::SeqCst) {
                    tracing::info!(pid, worker = i, "worker exited (shutdown)");
                } else {
                    tracing::info!(pid, worker = i, "worker exited; respawning");
                    spawn_slot(i);
                    // Rolling reload: give the fresh worker time to boot (so it's
                    // accepting) before rolling the next one — keeps enough live
                    // workers to serve throughout.
                    if RELOAD_CURSOR.load(Ordering::SeqCst) < WORKER_COUNT.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(600));
                        roll_next();
                    }
                }
            }
        }
        if SHUTDOWN.load(Ordering::SeqCst) && CHILDREN.iter().all(|c| c.load(Ordering::SeqCst) == 0) {
            break;
        }
    }
    tracing::info!("askr master exiting");
    Ok(())
}

/// async-signal-safe: atomic loads + kill().
fn kill_all(sig: libc::c_int) {
    for c in CHILDREN.iter() {
        let pid = c.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { libc::kill(pid, sig) };
        }
    }
}

/// SIGINT / SIGTERM: shut down. Tell workers to drain, don't respawn.
extern "C" fn on_terminate(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
    kill_all(libc::SIGTERM);
}

/// Roll (gracefully restart) the next worker slot: SIGTERM one worker so it
/// drains and exits; the reaper respawns it fresh and then rolls the next.
/// One-at-a-time, so there are always live workers accepting — zero drops.
fn roll_next() {
    let n = WORKER_COUNT.load(Ordering::SeqCst);
    loop {
        let i = RELOAD_CURSOR.fetch_add(1, Ordering::SeqCst);
        if i >= n {
            return; // reload complete
        }
        let pid = CHILDREN[i].load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            return;
        }
        // empty slot; continue to the next
    }
}

/// SIGHUP: graceful **rolling** reload. Restart workers one at a time (each
/// drains, exits, and is respawned fresh — picking up new PHP code) so there's
/// always a live worker accepting. No dropped connections.
extern "C" fn on_reload(_sig: libc::c_int) {
    RELOAD_CURSOR.store(0, Ordering::SeqCst);
    roll_next();
}

fn install_signals() {
    unsafe {
        libc::signal(libc::SIGINT, on_terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_terminate as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_reload as *const () as libc::sighandler_t);
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
