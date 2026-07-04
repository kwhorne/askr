//! Askr — a standalone, memory-safe PHP application server.
//!
//! A1: serve a single PHP application over HTTP through the embedded interpreter.
//! A3: scale across cores with SO_REUSEPORT + one forked worker process per core
//!     (non-ZTS means one interpreter per process, so we scale by processes).

mod admin;
mod cgi;
mod config;
mod doctor;
mod php;
mod server;
mod tls;
mod worker;

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use crate::server::Config;
use crate::worker::{bind_listener, run_worker};

#[derive(Parser)]
#[command(
    name = "askr",
    version,
    about = "The smartest, most efficient PHP web server."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // clap owns these once at startup
enum Command {
    /// Serve a PHP application over HTTP.
    Serve {
        /// Load all settings from a config file (askr.toml). When set, the
        /// other flags are ignored — the file is the source of truth.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Admin dashboard/API listen address (e.g. 127.0.0.1:9000). Off if unset.
        #[arg(long)]
        admin: Option<SocketAddr>,

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

        /// Maximum request body size (e.g. 16M, 512K, 2G). Larger requests get
        /// a 413. Protects against memory exhaustion.
        #[arg(long, default_value = "16M")]
        max_body_size: String,

        /// Dev only: detect state bleed between requests in worker mode
        /// (reports app state that keeps growing). Expensive — not for prod.
        #[arg(long)]
        paranoid: bool,

        /// Run N queue-worker processes alongside the web workers (requires
        /// --queue-script). Supervised and respawned like web workers.
        #[arg(long, default_value = "0")]
        queue: usize,

        /// Queue runner script (e.g. examples/askr-queue.php).
        #[arg(long)]
        queue_script: Option<PathBuf>,

        /// Run the scheduler with this runner script (e.g. examples/askr-scheduler.php).
        #[arg(long)]
        scheduler_script: Option<PathBuf>,
    },

    /// Pre-flight checks: PHP build, extensions, and platform support.
    Doctor {
        /// Extra php.ini lines (e.g. to load opcache).
        #[arg(long)]
        ini: Option<String>,
    },

    /// Validate a config file and print the resolved settings (no server start).
    ConfigCheck {
        /// Path to askr.toml.
        file: PathBuf,
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
            config: config_file,
            admin,
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
            max_body_size,
            paranoid,
            queue,
            queue_script,
            scheduler_script,
        } => {
            // The config file, when given, is the single source of truth.
            let (config, workers, ini, admin_listen, paranoid, sidecars) = if let Some(path) =
                config_file
            {
                let r = config::FileConfig::load(&path)?.resolve(default_workers())?;
                if let Some(base) = &r.app_base {
                    // Exported for the worker script; children inherit it across fork.
                    std::env::set_var("ASKR_APP_BASE", base);
                }
                let sc = Sidecars {
                    queue: r.queue_workers,
                    queue_script: r.queue_script,
                    scheduler_script: r.scheduler_script,
                };
                (r.config, r.workers, r.ini, r.admin_listen, r.paranoid, sc)
            } else {
                let max_body_size = parse_size(&max_body_size)?;
                let docroot = resolve_root(root)?;
                if !docroot.join(&front).is_file() {
                    anyhow::bail!(
                        "front controller not found: {} (use --root / --front)",
                        docroot.join(&front).display()
                    );
                }
                if let Some(ws) = &worker_script {
                    anyhow::ensure!(ws.is_file(), "worker script not found: {}", ws.display());
                }
                if let Some(c) = &tls_cert {
                    anyhow::ensure!(c.is_file(), "TLS cert not found: {}", c.display());
                }
                let tls_on = tls_cert.is_some() || tls_self_signed;
                if let Some(qs) = &queue_script {
                    anyhow::ensure!(qs.is_file(), "queue script not found: {}", qs.display());
                }
                if let Some(ss) = &scheduler_script {
                    anyhow::ensure!(ss.is_file(), "scheduler script not found: {}", ss.display());
                }
                let cfg = Config {
                    docroot,
                    front_controller: front,
                    listen,
                    https: https || tls_on,
                    worker_script,
                    max_requests,
                    tls_cert,
                    tls_key,
                    tls_self_signed,
                    max_body_size,
                };
                let w = workers.unwrap_or_else(default_workers).max(1);
                let sc = Sidecars {
                    queue: if queue_script.is_some() { queue } else { 0 },
                    queue_script,
                    scheduler_script,
                };
                (
                    cfg,
                    w,
                    ini.or_else(|| std::env::var("ASKR_PHP_INI").ok()),
                    admin,
                    paranoid,
                    sc,
                )
            };

            if paranoid {
                std::env::set_var("ASKR_PARANOID", "1");
                tracing::warn!(
                    "paranoid mode ON — state-bleed detection (dev only). \
                     Use --workers 1 for readable output."
                );
            }

            let listener = bind_listener(config.listen)?;
            // The supervisor is needed for recycling, the admin plane, sidecars,
            // or >1 worker.
            let has_sidecars = sidecars.queue > 0 || sidecars.scheduler_script.is_some();
            let need_supervisor =
                workers > 1 || config.max_requests > 0 || admin_listen.is_some() || has_sidecars;
            if !need_supervisor {
                tracing::info!(listen = %config.listen, workers = 1, "askr serving (single process)");
                run_worker(listener, config, ini)
            } else {
                supervise(listener, config, ini, workers, admin_listen, sidecars)
            }
        }
        Command::Doctor { ini } => {
            let ini = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok());
            if doctor::run(ini) {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Command::ConfigCheck { file } => {
            let raw = config::FileConfig::load(&file)?;
            let resolved = raw.resolve(default_workers())?;
            println!("✓ config OK: {}", file.display());
            let c = &resolved.config;
            println!("  listen:        {}", c.listen);
            println!("  root:          {}", c.docroot.display());
            println!("  front:         {}", c.front_controller.display());
            println!("  workers:       {}", resolved.workers);
            println!(
                "  mode:          {}",
                if c.worker_script.is_some() {
                    "worker (boot once)"
                } else {
                    "per-request"
                }
            );
            if let Some(ws) = &c.worker_script {
                println!("  worker script: {}", ws.display());
            }
            println!("  max_requests:  {}", c.max_requests);
            println!("  max_body_size: {} bytes", c.max_body_size);
            println!(
                "  tls:           {}",
                if c.tls_self_signed {
                    "self-signed".into()
                } else if c.tls_cert.is_some() {
                    "cert + key".into()
                } else {
                    "off".to_string()
                }
            );
            println!(
                "  admin:         {}",
                resolved
                    .admin_listen
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "off".into())
            );
            Ok(())
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
static START_TIME: AtomicU64 = AtomicU64::new(0);
static RESPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Live supervisor status, consumed by the admin API/dashboard.
pub struct Status {
    pub uptime_secs: u64,
    pub workers_configured: usize,
    pub workers_alive: usize,
    pub respawns: usize,
    pub pids: Vec<i32>,
}

pub fn status() -> Status {
    let pids: Vec<i32> = CHILDREN
        .iter()
        .map(|c| c.load(Ordering::SeqCst))
        .filter(|&p| p > 0)
        .collect();
    Status {
        uptime_secs: now_secs().saturating_sub(START_TIME.load(Ordering::SeqCst)),
        workers_configured: WORKER_COUNT.load(Ordering::SeqCst),
        workers_alive: pids.len(),
        respawns: RESPAWN_COUNT.load(Ordering::SeqCst),
        pids,
    }
}

/// Trigger a graceful rolling reload (used by SIGHUP and the admin API).
pub fn trigger_reload() {
    RELOAD_CURSOR.store(0, Ordering::SeqCst);
    roll_next();
}

/// Fork `workers` child processes, each running an independent worker on the
/// shared inherited listener, then supervise them: forward termination signals
/// and reap exits.
/// Queue/scheduler sidecar processes supervised alongside the web workers.
#[derive(Clone)]
pub struct Sidecars {
    pub queue: usize,
    pub queue_script: Option<PathBuf>,
    pub scheduler_script: Option<PathBuf>,
}

/// What a supervised slot runs.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Web,
    Queue,
    Scheduler,
}

fn supervise(
    listener: std::net::TcpListener,
    config: Config,
    ini: Option<String>,
    workers: usize,
    admin_listen: Option<SocketAddr>,
    sidecars: Sidecars,
) -> anyhow::Result<()> {
    let web = workers.max(1);
    let queue = sidecars.queue;
    let sched = if sidecars.scheduler_script.is_some() {
        1
    } else {
        0
    };
    let total = (web + queue + sched).min(MAX_WORKERS);

    // Slot layout: [0, web) web · [web, web+queue) queue · [web+queue] scheduler.
    let kind_of = move |i: usize| -> Kind {
        if i < web {
            Kind::Web
        } else if i < web + queue {
            Kind::Queue
        } else {
            Kind::Scheduler
        }
    };

    let workers = total;
    WORKER_COUNT.store(workers, Ordering::SeqCst);
    START_TIME.store(now_secs(), Ordering::SeqCst);
    let listen_fd: RawFd = listener.as_raw_fd();

    // Optional admin dashboard/API (runs in its own thread in the master).
    if let Some(addr) = admin_listen {
        let info = admin::Info {
            server_listen: config.listen,
            mode: if config.worker_script.is_some() {
                "worker"
            } else {
                "per-request"
            },
        };
        admin::spawn(addr, info);
    }

    // Fork one worker into slot `i`. In the child this never returns (it runs
    // the worker and exits); in the parent it records the pid.
    let spawn_slot = |i: usize| {
        let kind = kind_of(i);
        // SAFETY: fork before any tokio runtime exists on this thread; the child
        // builds its own. Only async-signal-safe work runs pre-exec.
        match unsafe { libc::fork() } {
            0 => {
                // Child: the master coordinates lifecycle. Ignore SIGINT/SIGHUP
                // (don't inherit the master's handlers); SIGTERM stays default so
                // the web worker's tokio / queue:work can catch it.
                unsafe {
                    libc::signal(libc::SIGINT, libc::SIG_IGN);
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                    libc::signal(libc::SIGTERM, libc::SIG_DFL);
                }
                let code = match kind {
                    Kind::Web => {
                        let inherited = unsafe { std::net::TcpListener::from_raw_fd(listen_fd) };
                        match run_worker(inherited, config.clone(), ini.clone()) {
                            Ok(()) => 0,
                            Err(e) => {
                                eprintln!("askr worker {i}: {e:#}");
                                1
                            }
                        }
                    }
                    Kind::Queue => {
                        worker::run_sidecar(sidecars.queue_script.clone().unwrap(), ini.clone())
                    }
                    Kind::Scheduler => {
                        worker::run_sidecar(sidecars.scheduler_script.clone().unwrap(), ini.clone())
                    }
                };
                std::process::exit(code);
            }
            -1 => {
                tracing::error!(
                    worker = i,
                    "fork failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            pid => {
                CHILDREN[i].store(pid, Ordering::SeqCst);
                let label = match kind {
                    Kind::Web => "web",
                    Kind::Queue => "queue",
                    Kind::Scheduler => "scheduler",
                };
                tracing::info!(pid, slot = i, kind = label, "spawned");
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
                _ => break,                    // ECHILD: no children left
            }
        }
        if pid <= 0 {
            continue;
        }
        for (i, child) in CHILDREN.iter().enumerate().take(workers) {
            if child.load(Ordering::SeqCst) == pid {
                child.store(0, Ordering::SeqCst);
                if SHUTDOWN.load(Ordering::SeqCst) {
                    tracing::info!(pid, worker = i, "worker exited (shutdown)");
                } else {
                    tracing::info!(pid, worker = i, "worker exited; respawning");
                    RESPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
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
        if SHUTDOWN.load(Ordering::SeqCst) && CHILDREN.iter().all(|c| c.load(Ordering::SeqCst) == 0)
        {
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
        libc::signal(
            libc::SIGINT,
            on_terminate as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            on_terminate as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGHUP, on_reload as *const () as libc::sighandler_t);
    }
}

/// Parse a size like `16M`, `512K`, `2G`, or a plain byte count.
fn parse_size(s: &str) -> anyhow::Result<usize> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: usize = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid size: {s:?} (use e.g. 16M, 512K, 2G)"))?;
    Ok(n * mult)
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

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_size("16M").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("8m").unwrap(), 8 * 1024 * 1024);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10X").is_err());
    }
}
