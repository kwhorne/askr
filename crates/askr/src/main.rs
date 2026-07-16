//! Askr — a standalone, memory-safe PHP application server.
//!
//! A1: serve a single PHP application over HTTP through the embedded interpreter.
//! A3: scale across cores with SO_REUSEPORT + one forked worker process per core
//!     (non-ZTS means one interpreter per process, so we scale by processes).

mod acme;
mod admin;
mod broadcast;
mod cache;
mod cgi;
mod compress;
mod config;
mod doctor;
mod metrics;
mod php;
mod pusher;
mod rcache;
mod record;
mod sandbox;
mod server;
mod shadow;
mod shmlock;
mod squeue;
mod tls;
mod upgrade;
mod upload;
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

        /// CoW autoscaling floor: minimum web workers to keep alive.
        /// Defaults to --workers.
        #[arg(long)]
        workers_min: Option<usize>,

        /// CoW autoscaling ceiling: maximum web workers to scale up to under
        /// load. When greater than --workers-min, the CoW template adds and
        /// harvests workers based on live queue depth — the ~ms warm respawn
        /// makes process autoscaling practical (impossible with ~300ms cold boot).
        #[arg(long)]
        workers_max: Option<usize>,

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

        /// Recycle a worker gracefully once its resident memory (RSS) exceeds
        /// this many MB (0 = never). Leak-aware, predictive recycling: the
        /// supervisor drains and respawns a worker *before* it hits PHP's
        /// `memory_limit` and OOMs — no 502s, unlike a crash. Linux only (reads
        /// /proc). Set it comfortably below `memory_limit` × workers.
        #[arg(long, default_value = "0")]
        max_rss: usize,

        /// Traffic shadowing: mirror a sampled fraction of *safe* (GET/HEAD,
        /// cookie-less) requests to this upstream URL — e.g. a staging deploy of
        /// the next version — after serving the real response, and report where
        /// the shadow's response diverges (on /metrics). The client is never
        /// affected. Only idempotent, non-user-specific requests are mirrored.
        #[arg(long)]
        shadow_to: Option<String>,

        /// Percent (1..=100) of eligible requests to mirror to `--shadow-to`.
        #[arg(long, default_value = "100")]
        shadow_sample: u8,

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
        /// --queue-script). Supervised and respawned like web workers. With
        /// --queue-max, N is the floor of an autoscaling range.
        #[arg(long, default_value = "0")]
        queue: usize,

        /// Autoscaling ceiling for queue workers. When greater than --queue, the
        /// supervisor scales the queue-worker pool between --queue and this value
        /// based on the shared-memory backlog (Horizon `balance=auto`, no extra
        /// daemon). Defaults to --queue (fixed count).
        #[arg(long)]
        queue_max: Option<usize>,

        /// Queue runner script (e.g. examples/askr-queue.php).
        #[arg(long)]
        queue_script: Option<PathBuf>,

        /// Enable the shared-memory job queue with this many slots (0 = off;
        /// 32 KB each). Exposes askr_queue_* — Redis-free queues via AskrQueue.
        #[arg(long, default_value = "0")]
        queue_slots: usize,

        /// Run the scheduler with this runner script (e.g. examples/askr-scheduler.php).
        #[arg(long)]
        scheduler_script: Option<PathBuf>,

        /// Supervise an arbitrary external command alongside the workers
        /// (repeatable). Run via `sh -c` in the app base; respawned if it dies.
        /// E.g. `--sidecar "node bootstrap/ssr/ssr.mjs"` for Inertia SSR.
        #[arg(long)]
        sidecar: Vec<String>,

        /// Enable the shared cache with this many slots (0 = off; ~4.3 KB each).
        /// Exposes askr_cache_* to PHP (cache, counters, locks — no Redis).
        #[arg(long, default_value = "0")]
        cache_slots: usize,

        /// Large-value cache region slots (64 KB each; 0 = off). Enables cache
        /// values over 4 KB — Laravel sessions, cached fragments/collections.
        #[arg(long, default_value = "0")]
        cache_large_slots: usize,

        /// Enable the response cache with this many slots (0 = off; ~140 KB each).
        /// PHP marks responses cacheable via `header('Askr-Cache: 60, tags=posts')`;
        /// matching anonymous GETs are served from Rust without touching PHP, and
        /// `askr_cache_forget_tag('posts')` invalidates across all workers at once.
        #[arg(long, default_value = "0")]
        response_cache: usize,

        /// Enable broadcasting: askr_broadcast() from PHP + the SSE endpoint
        /// GET /askr/events?channel=NAME (live updates without Reverb/Pusher).
        #[arg(long)]
        broadcast: bool,

        /// Canary reload: on SIGHUP, roll one worker and health-check it (a
        /// short window with no error spike) before rolling the rest. A bad
        /// deploy takes down one worker instead of all.
        #[arg(long)]
        canary: bool,

        /// Experimental: CoW template. Boot the app once and fork workers from
        /// it (copy-on-write) — ~ms warm respawn and shared memory. Requires
        /// --worker-script; the admin plane is unavailable in this mode.
        #[arg(long)]
        cow: bool,

        /// Record failing (5xx) requests into this directory so they can be
        /// replayed with `askr replay <id.json>`. Captures request bodies —
        /// treat the directory as sensitive.
        #[arg(long)]
        record_errors: Option<PathBuf>,

        /// Auto-TLS via ACME (Let's Encrypt): obtain + renew a certificate over
        /// HTTP-01. The master answers challenges on --acme-http (default
        /// 0.0.0.0:80) before forking; workers serve HTTPS from the cache. Set
        /// --listen to the HTTPS port (e.g. 0.0.0.0:443).
        #[arg(long)]
        acme: bool,

        /// Domain(s) to obtain a certificate for (repeatable). Required with --acme.
        #[arg(long)]
        acme_domain: Vec<String>,

        /// Contact email for the ACME account.
        #[arg(long)]
        acme_email: Option<String>,

        /// Directory to cache the ACME account + cert (cert.pem/key.pem).
        #[arg(long, default_value = "/var/lib/askr/acme")]
        acme_dir: PathBuf,

        /// Use the Let's Encrypt staging environment (higher rate limits, untrusted).
        #[arg(long)]
        acme_staging: bool,

        /// Custom ACME directory URL (e.g. a Pebble test server).
        #[arg(long)]
        acme_directory: Option<String>,

        /// Address to answer HTTP-01 challenges on.
        #[arg(long, default_value = "0.0.0.0:80")]
        acme_http: SocketAddr,

        /// Trust this CA root PEM for the ACME directory (for Pebble/testing).
        #[arg(long)]
        acme_ca_root: Option<PathBuf>,

        /// Harden workers on Linux: a seccomp filter blocks process creation
        /// (execve/ptrace → EPERM), so a PHP exploit can't spawn a shell. No-op
        /// off Linux; PHP's own exec()/Process calls will fail.
        #[arg(long)]
        sandbox: bool,

        /// Also restrict the filesystem with Landlock: writes allowed only under
        /// these paths (repeatable, e.g. the app's storage/ and /tmp). Reads stay
        /// open so PHP/templates keep working. Implies stronger --sandbox.
        #[arg(long)]
        sandbox_write: Vec<PathBuf>,

        /// Enable a Pusher-compatible WebSocket endpoint (/app/{key}) and HTTP
        /// trigger (/apps/{id}/events) — a drop-in Reverb for Laravel Echo.
        /// Auto-enables broadcasting.
        #[arg(long)]
        pusher: bool,

        /// Pusher app secret: when set, private/presence channel subscriptions
        /// must carry a valid HMAC auth signature. Also read from
        /// $ASKR_PUSHER_SECRET. Without it, such subscriptions are accepted (dev).
        #[arg(long)]
        pusher_secret: Option<String>,

        /// Write a structured (JSON) access log line per request to this file,
        /// or `-` for stdout. Off if unset.
        #[arg(long)]
        access_log: Option<PathBuf>,
    },

    /// Run tests by forking a fresh, warm process per test file (#5-style CoW).
    /// Boots the interpreter once (opcache warm, shared); each file runs in its
    /// own process for perfect isolation, in parallel. Point --runner at
    /// examples/askr-test.php for PHPUnit/Pest, or omit it to run files directly.
    Test {
        /// Test files or directories (directories are scanned for *Test.php).
        /// Defaults to ./tests.
        paths: Vec<PathBuf>,
        /// Application base (exported as $ASKR_APP_BASE). Defaults to cwd.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Runner script invoked per file with $ASKR_TEST_FILE set (e.g.
        /// examples/askr-test.php). Omit to execute each file directly.
        #[arg(long)]
        runner: Option<PathBuf>,
        /// Max test files to run concurrently. Defaults to CPU cores.
        #[arg(long)]
        parallel: Option<usize>,
        /// Extra php.ini lines (e.g. to load opcache — recommended).
        #[arg(long)]
        ini: Option<String>,
    },

    /// Replay a recorded failing request against a fresh interpreter (#5).
    Replay {
        /// Path to a recorded `<id>.json` (see `serve --record-errors`).
        file: PathBuf,
        /// Extra php.ini lines (e.g. to load opcache).
        #[arg(long)]
        ini: Option<String>,
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

    /// Update Askr to the latest release in place (self-upgrade).
    Upgrade {
        /// Only report whether a newer release is available; don't install.
        #[arg(long)]
        check: bool,
        /// Install a specific version instead of the latest (e.g. 0.8.0) — also for rollback.
        #[arg(long)]
        version: Option<String>,
        /// Restart the service afterwards (`systemctl restart askr`).
        #[arg(long)]
        restart: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // instant-acme pulls in aws-lc-rs while our TLS uses ring, so rustls can no
    // longer auto-select a provider — pin ring process-wide (matches our stack).
    let _ = rustls::crypto::ring::default_provider().install_default();

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
            workers_min,
            workers_max,
            https,
            ini,
            worker_script,
            max_requests,
            max_rss,
            shadow_to,
            shadow_sample,
            tls_cert,
            tls_key,
            tls_self_signed,
            max_body_size,
            paranoid,
            queue,
            queue_max,
            queue_script,
            queue_slots,
            scheduler_script,
            sidecar,
            cache_slots,
            cache_large_slots,
            response_cache,
            broadcast,
            canary,
            cow,
            record_errors,
            acme,
            acme_domain,
            acme_email,
            acme_dir,
            acme_staging,
            acme_directory,
            acme_http,
            acme_ca_root,
            sandbox,
            sandbox_write,
            pusher,
            pusher_secret,
            access_log,
        } => {
            // The config file, when given, is the single source of truth.
            #[allow(clippy::type_complexity)]
            let (
                config,
                workers,
                ini,
                admin_listen,
                paranoid,
                sidecars,
                cache_slots,
                cache_large_slots,
                response_cache,
                broadcast,
            ) = if let Some(path) = config_file {
                let r = config::FileConfig::load(&path)?.resolve(default_workers())?;
                if let Some(base) = &r.app_base {
                    // Exported for the worker script; children inherit it across fork.
                    std::env::set_var("ASKR_APP_BASE", base);
                }
                CANARY_ENABLED.store(r.canary_reload, Ordering::SeqCst);
                WORKERS_MIN.store(r.workers_min, Ordering::SeqCst);
                WORKERS_MAX.store(r.workers_max, Ordering::SeqCst);
                QUEUE_CAP.store(r.queue_slots, Ordering::SeqCst);
                let sc = Sidecars {
                    queue: r.queue_workers,
                    queue_max: r.queue_workers_max.max(r.queue_workers),
                    queue_script: r.queue_script,
                    scheduler_script: r.scheduler_script,
                    commands: r.sidecars,
                };
                (
                    r.config,
                    r.workers,
                    r.ini,
                    r.admin_listen,
                    r.paranoid,
                    sc,
                    r.cache_slots,
                    r.cache_large_slots,
                    r.response_cache_slots,
                    r.broadcast,
                )
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
                CANARY_ENABLED.store(canary, Ordering::SeqCst);
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
                    max_rss_mb: max_rss,
                    tls_cert,
                    tls_key,
                    tls_self_signed,
                    max_body_size,
                    record_dir: record_errors,
                    pusher,
                    pusher_secret: pusher_secret
                        .or_else(|| std::env::var("ASKR_PUSHER_SECRET").ok()),
                    access_log,
                    sandbox: sandbox || !sandbox_write.is_empty(),
                    sandbox_write,
                    shadow_to,
                    shadow_sample,
                };
                let w = workers.unwrap_or_else(default_workers).max(1);
                let wmin = workers_min.unwrap_or(w).max(1);
                let wmax = workers_max.unwrap_or(w).max(wmin);
                WORKERS_MIN.store(wmin, Ordering::SeqCst);
                WORKERS_MAX.store(wmax, Ordering::SeqCst);
                QUEUE_CAP.store(queue_slots, Ordering::SeqCst);
                let qw = if queue_script.is_some() { queue } else { 0 };
                let sc = Sidecars {
                    queue: qw,
                    queue_max: queue_max.unwrap_or(qw).max(qw),
                    queue_script,
                    scheduler_script,
                    commands: sidecar,
                };
                (
                    cfg,
                    w,
                    ini.or_else(|| std::env::var("ASKR_PHP_INI").ok()),
                    admin,
                    paranoid,
                    sc,
                    cache_slots,
                    cache_large_slots,
                    response_cache,
                    broadcast,
                )
            };

            // Map shared regions before any fork so all workers share them.
            if cache_slots > 0 || cache_large_slots > 0 {
                cache::init(cache_slots.max(1), cache_large_slots);
            }
            if response_cache > 0 {
                rcache::init(response_cache);
            }
            if broadcast || config.pusher {
                broadcast::init(); // the Pusher endpoints ride the broadcast ring
            }
            let queue_slots = QUEUE_CAP.load(Ordering::SeqCst);
            if queue_slots > 0 {
                squeue::init(queue_slots);
            }

            if paranoid {
                std::env::set_var("ASKR_PARANOID", "1");
                tracing::warn!(
                    "paranoid mode ON — state-bleed detection (dev only). \
                     Use --workers 1 for readable output."
                );
            }

            // Map shared metrics before any fork so all workers share them.
            metrics::Metrics::init();

            // Auto-TLS via ACME: obtain the cert in the master (HTTP-01 on
            // --acme-http) before forking; workers serve HTTPS from the cache.
            let mut config = config;
            if acme {
                anyhow::ensure!(!acme_domain.is_empty(), "--acme requires --acme-domain");
                let email = acme_email
                    .clone()
                    .unwrap_or_else(|| format!("admin@{}", acme_domain[0]));
                let directory_url = acme_directory.clone().unwrap_or_else(|| {
                    if acme_staging {
                        instant_acme::LetsEncrypt::Staging.url().to_string()
                    } else {
                        instant_acme::LetsEncrypt::Production.url().to_string()
                    }
                });
                let acfg = acme::AcmeConfig {
                    domains: acme_domain.clone(),
                    email,
                    cache_dir: acme_dir.clone(),
                    directory_url,
                    challenge_addr: acme_http,
                    ca_root: acme_ca_root.clone(),
                    renew_after_days: 60,
                };
                if acme::needs_renewal(&acme_dir) {
                    acme::obtain_blocking(&acfg)?;
                } else {
                    tracing::info!("acme: cached certificate still valid");
                }
                config.tls_cert = Some(acme::cert_path(&acme_dir));
                config.tls_key = Some(acme::key_path(&acme_dir));
                config.tls_self_signed = false;
                config.https = true;
                // Renew before expiry in a background thread, then roll workers.
                let renew = acfg.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
                    if acme::needs_renewal(&renew.cache_dir) {
                        match acme::obtain_blocking(&renew) {
                            Ok(()) => {
                                tracing::info!("acme: renewed; rolling workers");
                                trigger_reload();
                            }
                            Err(e) => {
                                tracing::error!(error = %format!("{e:#}"), "acme: renewal failed")
                            }
                        }
                    }
                });
            }

            let listener = bind_listener(config.listen)?;
            // The supervisor is needed for recycling, the admin plane, sidecars,
            // ACME renewal (rolling reload), or >1 worker.
            let has_sidecars = sidecars.queue > 0
                || sidecars.scheduler_script.is_some()
                || !sidecars.commands.is_empty();
            let need_supervisor = workers > 1
                || config.max_requests > 0
                || config.max_rss_mb > 0
                || admin_listen.is_some()
                || has_sidecars
                || acme;
            if cow {
                anyhow::ensure!(
                    config.worker_script.is_some(),
                    "--cow requires --worker-script"
                );
                if admin_listen.is_some() || has_sidecars {
                    tracing::warn!("--cow: admin plane and sidecars are unavailable in CoW mode");
                }
                let wmin = WORKERS_MIN.load(Ordering::SeqCst).clamp(1, MAX_WORKERS);
                let wmax = WORKERS_MAX.load(Ordering::SeqCst).clamp(wmin, MAX_WORKERS);
                tracing::info!(listen = %config.listen, workers_min = wmin, workers_max = wmax, "askr serving (CoW template)");
                run_cow(listener, config, ini, wmin, wmax)
            } else if !need_supervisor {
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
        Command::Test {
            paths,
            root,
            runner,
            parallel,
            ini,
        } => run_test(paths, root, runner, parallel, ini),
        Command::Replay { file, ini } => {
            if let Some(ini) = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok()) {
                std::env::set_var("ASKR_PHP_INI", ini);
            }
            let req = record::load(&file)?;
            eprintln!("↻ replaying {} → {}", file.display(), req.method);
            let mut php =
                askr_php::Interpreter::new().map_err(|e| anyhow::anyhow!("php init: {e}"))?;
            let resp = php
                .handle(&req)
                .map_err(|e| anyhow::anyhow!("replay failed: {e}"))?;
            println!("HTTP {}", resp.status);
            for (k, v) in &resp.headers {
                println!("{k}: {v}");
            }
            println!();
            print!("{}", String::from_utf8_lossy(&resp.body));
            Ok(())
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
            println!("  max_rss_mb:    {}", c.max_rss_mb);
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
        Command::Upgrade {
            check,
            version,
            restart,
        } => upgrade::run(upgrade::Options {
            check,
            version,
            restart,
        }),
    }
}

/// Default worker count: the container's CPU limit (cgroup) when running in one,
/// else the host's core count. Without this a `cpus: 2` container on a 64-core
/// host would fork 64 workers (nproc reads the host, not the cgroup limit).
fn default_workers() -> usize {
    #[cfg(target_os = "linux")]
    if let Some(n) = cgroup_cpu_limit() {
        return n.max(1);
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Read the effective CPU limit from cgroup v2 (`cpu.max`), falling back to
/// cgroup v1 (`cpu.cfs_quota_us`/`cpu.cfs_period_us`). None if unlimited/absent.
#[cfg(target_os = "linux")]
fn cgroup_cpu_limit() -> Option<usize> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        if let Some(n) = cpu_limit_from_cgroup_v2(&s) {
            return Some(n);
        }
    }
    // cgroup v1.
    let quota: f64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let period: f64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if quota > 0.0 && period > 0.0 {
        Some((quota / period).ceil() as usize)
    } else {
        None
    }
}

/// Parse a cgroup v2 `cpu.max` value (`"<quota> <period>"` or `"max <period>"`)
/// into a whole-core limit (rounded up). None when unlimited.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cpu_limit_from_cgroup_v2(cpu_max: &str) -> Option<usize> {
    let mut it = cpu_max.split_whitespace();
    let quota = it.next()?;
    if quota == "max" {
        return None;
    }
    let q: f64 = quota.parse().ok()?;
    let p: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(100_000.0);
    if q > 0.0 && p > 0.0 {
        Some((q / p).ceil() as usize)
    } else {
        None
    }
}

// --- multi-process supervisor --------------------------------------------

const MAX_WORKERS: usize = 512;
// Queue autoscaling target: ~1 worker per this many ready (waiting) jobs.
const QUEUE_BACKLOG_PER_WORKER: usize = 10;
static CHILDREN: [AtomicI32; MAX_WORKERS] = [const { AtomicI32::new(0) }; MAX_WORKERS];
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static WORKER_COUNT: AtomicUsize = AtomicUsize::new(0);
// Next slot to roll during a graceful reload; >= WORKER_COUNT means "not rolling".
static RELOAD_CURSOR: AtomicUsize = AtomicUsize::new(usize::MAX);
static START_TIME: AtomicU64 = AtomicU64::new(0);
static RESPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
// Leak-aware recycling: the pid we last SIGTERM'd for exceeding --max-rss (per
// slot), so we don't re-signal a worker that's already draining, and a count of
// how many times it has fired (observability).
static RECYCLE_SENT: [AtomicI32; MAX_WORKERS] = [const { AtomicI32::new(0) }; MAX_WORKERS];
static MEM_RECYCLE_COUNT: AtomicUsize = AtomicUsize::new(0);
// Queue-worker autoscaling: current desired count within [QUEUE_MIN, QUEUE_MAX],
// driven by the shared-memory queue backlog.
static QUEUE_DESIRED: AtomicUsize = AtomicUsize::new(0);
// CoW autoscaling bounds + the current desired web-worker count.
static WORKERS_MIN: AtomicUsize = AtomicUsize::new(1);
static WORKERS_MAX: AtomicUsize = AtomicUsize::new(1);
static DESIRED: AtomicUsize = AtomicUsize::new(0);
// Shared-memory job queue slot count (mapped before fork if > 0).
static QUEUE_CAP: AtomicUsize = AtomicUsize::new(0);
// Canary reload: roll one worker, then health-check before rolling the rest.
static CANARY_ENABLED: AtomicBool = AtomicBool::new(false);
static CANARY_ACTIVE: AtomicBool = AtomicBool::new(false);
static CANARY_DEADLINE: AtomicU64 = AtomicU64::new(0);
static CANARY_ERR_BASE: AtomicU64 = AtomicU64::new(0);
const CANARY_WINDOW_SECS: u64 = 5;
const CANARY_ERR_THRESHOLD: u64 = 3;

/// Aggregate error signal (BAD_GATEWAY + app 5xx) for the canary check.
fn error_count() -> u64 {
    match crate::metrics::Metrics::get() {
        Some(m) => {
            use std::sync::atomic::Ordering::Relaxed;
            m.errors.load(Relaxed) + m.status[4].load(Relaxed)
        }
        None => 0,
    }
}

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
    /// Queue autoscaling / backlog (0 when the job queue is disabled).
    pub queue_workers: usize,
    pub queue_ready: usize,
    pub queue_total: usize,
    pub queue_oldest_secs: u64,
}

pub fn status() -> Status {
    let pids: Vec<i32> = CHILDREN
        .iter()
        .map(|c| c.load(Ordering::SeqCst))
        .filter(|&p| p > 0)
        .collect();
    let (queue_ready, queue_total, queue_oldest_ms) = if crate::squeue::enabled() {
        crate::squeue::stats()
    } else {
        (0, 0, 0)
    };
    Status {
        uptime_secs: now_secs().saturating_sub(START_TIME.load(Ordering::SeqCst)),
        workers_configured: WORKER_COUNT.load(Ordering::SeqCst),
        workers_alive: pids.len(),
        respawns: RESPAWN_COUNT.load(Ordering::SeqCst),
        pids,
        queue_workers: QUEUE_DESIRED.load(Ordering::SeqCst),
        queue_ready,
        queue_total,
        queue_oldest_secs: queue_oldest_ms / 1000,
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
    /// Initial queue-worker count (= floor when autoscaling).
    pub queue: usize,
    /// Autoscaling ceiling for queue workers (== `queue` when not autoscaling).
    pub queue_max: usize,
    pub queue_script: Option<PathBuf>,
    pub scheduler_script: Option<PathBuf>,
    /// Arbitrary external commands supervised alongside the workers (e.g. an
    /// Inertia SSR node server: `node bootstrap/ssr/ssr.mjs`). Run via `sh -c`
    /// in $ASKR_APP_BASE; respawned if they die.
    pub commands: Vec<String>,
}

/// What a supervised slot runs.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Web,
    Queue,
    Scheduler,
    Command,
}

/// A process's resident set size (RSS) in bytes, via `/proc/<pid>/statm` (field 2
/// = resident pages). Linux only; `None` elsewhere or if the process is gone.
#[cfg(target_os = "linux")]
fn worker_rss_bytes(pid: i32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page > 0).then(|| resident_pages * page as u64)
}

#[cfg(not(target_os = "linux"))]
fn worker_rss_bytes(_pid: i32) -> Option<u64> {
    None
}

/// Gracefully recycle any PHP worker whose RSS has crossed `max_rss_mb`, *before*
/// it hits PHP's `memory_limit` and OOMs. Sending SIGTERM triggers the worker's
/// graceful drain (finish in-flight requests, then exit); the supervisor's reap
/// loop respawns a fresh one. Coalesced per slot so we never signal a worker
/// that's already draining. `php_workers` = the leading slots that run PHP
/// (web + queue); sidecars are external and skipped.
fn recycle_over_rss(max_rss_mb: usize, php_workers: usize) {
    if max_rss_mb == 0 {
        return;
    }
    let cap = max_rss_mb as u64 * 1024 * 1024;
    for i in 0..php_workers.min(MAX_WORKERS) {
        let pid = CHILDREN[i].load(Ordering::SeqCst);
        if pid <= 0 {
            continue;
        }
        // Already asked this exact pid to drain? leave it alone.
        if RECYCLE_SENT[i].load(Ordering::SeqCst) == pid {
            continue;
        }
        let Some(rss) = worker_rss_bytes(pid) else {
            continue;
        };
        if rss >= cap {
            RECYCLE_SENT[i].store(pid, Ordering::SeqCst);
            MEM_RECYCLE_COUNT.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(
                pid,
                worker = i,
                rss_mb = rss / (1024 * 1024),
                max_rss_mb,
                "worker over RSS cap — recycling gracefully before OOM"
            );
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
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
    // Queue workers autoscale in [queue_min, queue_max] on backlog. Reserve
    // queue_max contiguous slots; only queue_min run at boot.
    let queue_min = sidecars.queue;
    let queue_max = sidecars.queue_max.max(queue_min);
    let queue = queue_max;
    QUEUE_DESIRED.store(queue_min, Ordering::SeqCst);
    let sched = if sidecars.scheduler_script.is_some() {
        1
    } else {
        0
    };
    let ncmds = sidecars.commands.len();
    let total = (web + queue + sched + ncmds).min(MAX_WORKERS);

    // Slot layout: [web) web · [queue) queue · [scheduler] · [commands…].
    let kind_of = move |i: usize| -> Kind {
        if i < web {
            Kind::Web
        } else if i < web + queue {
            Kind::Queue
        } else if i < web + queue + sched {
            Kind::Scheduler
        } else {
            Kind::Command
        }
    };

    let workers = total;
    WORKER_COUNT.store(workers, Ordering::SeqCst);
    START_TIME.store(now_secs(), Ordering::SeqCst);
    let listen_fd: RawFd = listener.as_raw_fd();

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
                    Kind::Command => {
                        let idx = i - (web + queue + sched);
                        match sidecars.commands.get(idx) {
                            Some(cmd) => worker::run_command(cmd),
                            None => 1,
                        }
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
                    Kind::Command => "sidecar",
                };
                tracing::info!(pid, slot = i, kind = label, "spawned");
            }
        }
    };

    for i in 0..workers {
        // Only the floor number of queue workers start now; the autoscaler adds
        // more (up to queue_max) when the backlog grows.
        if kind_of(i) == Kind::Queue && (i - web) >= queue_min {
            continue;
        }
        spawn_slot(i);
    }

    // Start the admin dashboard/API *after* the initial fork storm. `fork()` only
    // clones the calling thread, so if a background thread (the admin Tokio
    // runtime) held an internal lock — malloc arena, the tracing writer, stdout —
    // at the instant of fork, that lock would stay locked forever in the child and
    // deadlock it on its first allocation or log. Forking the initial workers
    // while the master is still single-threaded closes that window at startup.
    // (Respawns during runtime fork with the admin thread live, but the child's
    // pre-tokio work is minimal; glibc's own atfork handlers keep malloc safe.)
    if let Some(addr) = admin_listen {
        let info = admin::Info {
            server_listen: config.listen,
            mode: if config.worker_script.is_some() {
                "worker"
            } else {
                "per-request"
            },
            record_dir: config.record_dir.clone(),
        };
        admin::spawn(addr, info);
    }

    install_signals();
    tracing::info!(
        %config.listen,
        workers,
        max_requests = config.max_requests,
        canary = CANARY_ENABLED.load(Ordering::SeqCst),
        "askr master supervising (SIGHUP = graceful reload)"
    );

    // Reap exited workers and respawn (recycling / crash resilience / rolling
    // reload) unless shutting down. A non-blocking poll lets us also drive the
    // canary health check and the leak-aware RSS check on a timer.
    let mut last_mem_check = std::time::Instant::now();
    let mut last_queue_check = std::time::Instant::now();
    loop {
        // Reap everything that has exited.
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break; // 0 = none exited yet, -1 = no children
            }
            for (i, child) in CHILDREN.iter().enumerate().take(workers) {
                if child.load(Ordering::SeqCst) == pid {
                    child.store(0, Ordering::SeqCst);
                    if SHUTDOWN.load(Ordering::SeqCst) {
                        tracing::info!(pid, worker = i, "worker exited (shutdown)");
                    } else if kind_of(i) == Kind::Queue
                        && (i - web) >= QUEUE_DESIRED.load(Ordering::SeqCst)
                    {
                        // A queue worker scaled out of the desired set: let it go.
                        tracing::info!(pid, worker = i, "queue worker scaled down");
                    } else {
                        tracing::info!(pid, worker = i, "worker exited; respawning");
                        RESPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
                        spawn_slot(i);
                        // Rolling reload: let the fresh worker boot before rolling
                        // the next, so enough workers stay live throughout.
                        if RELOAD_CURSOR.load(Ordering::SeqCst)
                            < WORKER_COUNT.load(Ordering::SeqCst)
                        {
                            std::thread::sleep(std::time::Duration::from_millis(600));
                            roll_next();
                        }
                    }
                }
            }
        }

        // Canary gate: once the window elapses, decide whether to roll the rest.
        if CANARY_ACTIVE.load(Ordering::SeqCst)
            && now_secs() >= CANARY_DEADLINE.load(Ordering::SeqCst)
        {
            CANARY_ACTIVE.store(false, Ordering::SeqCst);
            let new_errors = error_count().saturating_sub(CANARY_ERR_BASE.load(Ordering::SeqCst));
            let alive = CHILDREN[0].load(Ordering::SeqCst) != 0;
            if alive && new_errors <= CANARY_ERR_THRESHOLD {
                tracing::info!(new_errors, "canary healthy — rolling the rest");
                RELOAD_CURSOR.store(1, Ordering::SeqCst);
                roll_next();
            } else {
                tracing::error!(
                    new_errors,
                    canary_alive = alive,
                    "canary UNHEALTHY — aborting reload; remaining workers keep old code"
                );
            }
        }

        // Leak-aware recycling: sample worker RSS ~once a second and drain any
        // that crossed --max-rss before it OOMs. Reading /proc for a handful of
        // workers is cheap, and a tighter interval keeps a fast leak from
        // overshooting the cap by much before the next sample.
        if config.max_rss_mb > 0 && last_mem_check.elapsed() >= std::time::Duration::from_secs(1) {
            last_mem_check = std::time::Instant::now();
            recycle_over_rss(config.max_rss_mb, web + queue);
        }

        // Queue autoscaling: size the queue-worker pool to the backlog. Askr owns
        // both signals — the depth lives in shared memory (readable here) and the
        // worker pool is ours to fork/drain — so this is Horizon `balance=auto`
        // with no extra daemon. Scale up fast (jump to target), drain gently (one
        // worker per tick) to avoid flapping after a burst clears.
        if queue_max > queue_min
            && crate::squeue::enabled()
            && last_queue_check.elapsed() >= std::time::Duration::from_secs(2)
        {
            last_queue_check = std::time::Instant::now();
            let (ready, _total, _oldest) = crate::squeue::stats();
            let desired = QUEUE_DESIRED.load(Ordering::SeqCst);
            let want = ready
                .div_ceil(QUEUE_BACKLOG_PER_WORKER)
                .clamp(queue_min, queue_max);
            if want > desired {
                for j in desired..want {
                    spawn_slot(web + j);
                }
                QUEUE_DESIRED.store(want, Ordering::SeqCst);
                tracing::info!(ready, from = desired, to = want, "queue: scaling up");
            } else if want < desired {
                let victim = desired - 1;
                QUEUE_DESIRED.store(victim, Ordering::SeqCst); // set before SIGTERM
                let pid = CHILDREN[web + victim].load(Ordering::SeqCst);
                if pid > 0 {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
                tracing::info!(ready, from = desired, to = victim, "queue: scaling down");
            }
        }

        if SHUTDOWN.load(Ordering::SeqCst) && CHILDREN.iter().all(|c| c.load(Ordering::SeqCst) == 0)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::info!("askr master exiting");
    Ok(())
}

// --- CoW template (fork a warm, booted app; ~ms respawn) -----------------

use std::ffi::{c_int, c_void};

struct CowCtx {
    config: Config,
    listener_fd: RawFd,
    min: usize,
    max: usize,
    recycle_after: usize,
}

/// Boot the app once in this (template) process, then supervise workers forked
/// from it. The template is single-threaded when it forks (tokio starts only in
/// the children), so the fork is safe; workers inherit the warm heap via CoW.
fn run_cow(
    listener: std::net::TcpListener,
    config: Config,
    ini: Option<String>,
    min: usize,
    max: usize,
) -> anyhow::Result<()> {
    let listener_fd = listener.as_raw_fd();
    std::mem::forget(listener); // keep the fd open for forked workers
    if let Some(ini) = ini {
        std::env::set_var("ASKR_PHP_INI", ini);
    }
    let script = config
        .worker_script
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--cow requires --worker-script"))?;

    // Boot the interpreter on THIS thread (keep the process single-threaded so
    // the fork in cow_ready is safe).
    let _interp = askr_php::Interpreter::new().map_err(|e| anyhow::anyhow!("php init: {e}"))?;
    crate::cache::register_bridge();
    crate::squeue::register_bridge();
    crate::broadcast::register_bridge();

    let recycle_after = config.max_requests;
    let ctx = Box::into_raw(Box::new(CowCtx {
        config,
        listener_fd,
        min,
        max,
        recycle_after,
    }));
    // SAFETY: ctx lives for the process; the shim calls cow_ready_trampoline.
    unsafe { askr_php::cow_bridge::askr_php_set_cow(cow_ready_trampoline, ctx as *mut c_void) };

    tracing::info!(min, max, "askr CoW: booting the app once in the template…");
    // Runs the worker script: it boots the app and calls askr_cow_ready(), which
    // forks the workers. The template never returns here; a recycled child does.
    let _ = crate::php::Php::run_worker_current(&script);
    std::process::exit(0);
}

/// Called from PHP's `askr_cow_ready()`. In the template it forks + supervises
/// (never returns); in a forked worker it sets up serving and returns so the
/// worker's `while (askr_handle_request())` loop serves the warm app.
extern "C" fn cow_ready_trampoline(ctx: *mut c_void) -> c_int {
    let cc: &CowCtx = unsafe { &*(ctx as *const CowCtx) };
    WORKER_COUNT.store(cc.max, Ordering::SeqCst);
    DESIRED.store(cc.min, Ordering::SeqCst);
    START_TIME.store(now_secs(), Ordering::SeqCst);
    let autoscale = cc.max > cc.min;

    let mut signals_installed = false;
    let mut tick: u32 = 0;
    let mut idle_ticks: u32 = 0;
    loop {
        let desired = DESIRED.load(Ordering::SeqCst);
        // Fork any missing worker slots below `desired` (never while shutting
        // down). Slots at index >= desired are left empty — that's how we harvest.
        for (i, child) in CHILDREN.iter().enumerate().take(desired) {
            if !SHUTDOWN.load(Ordering::SeqCst) && child.load(Ordering::SeqCst) == 0 {
                match unsafe { libc::fork() } {
                    0 => {
                        cow_child_setup(cc);
                        return 0; // child returns to PHP → serves the warm app
                    }
                    -1 => tracing::error!(worker = i, "cow fork failed"),
                    pid => {
                        child.store(pid, Ordering::SeqCst);
                        tracing::info!(pid, worker = i, "cow worker forked (warm)");
                    }
                }
            }
        }
        if !signals_installed {
            // In CoW, all of INT/TERM/HUP shut the template down (new code is
            // picked up by restarting the process, e.g. systemctl restart).
            unsafe {
                libc::signal(
                    libc::SIGINT,
                    on_terminate as *const () as libc::sighandler_t,
                );
                libc::signal(
                    libc::SIGTERM,
                    on_terminate as *const () as libc::sighandler_t,
                );
                libc::signal(
                    libc::SIGHUP,
                    on_terminate as *const () as libc::sighandler_t,
                );
            }
            signals_installed = true;
            tracing::info!(
                min = cc.min,
                max = cc.max,
                autoscale,
                "askr CoW template supervising"
            );
        }
        // Reap and (if the slot is still within `desired`) refork warm.
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid > 0 {
            for (i, c) in CHILDREN.iter().enumerate().take(cc.max) {
                if c.load(Ordering::SeqCst) == pid {
                    c.store(0, Ordering::SeqCst);
                    if !SHUTDOWN.load(Ordering::SeqCst) {
                        RESPAWN_COUNT.fetch_add(1, Ordering::SeqCst);
                        tracing::info!(pid, worker = i, "cow worker exited");
                    }
                }
            }
        }

        // Autoscale on the shared queue-depth signal (~ every second).
        tick = tick.wrapping_add(1);
        if autoscale && !SHUTDOWN.load(Ordering::SeqCst) && tick % 20 == 0 {
            let alive = CHILDREN
                .iter()
                .take(cc.max)
                .filter(|c| c.load(Ordering::SeqCst) > 0)
                .count();
            let busy = crate::metrics::Metrics::get()
                .map(|m| m.inflight.load(Ordering::Relaxed))
                .unwrap_or(0) as usize;
            let d = DESIRED.load(Ordering::SeqCst);
            if busy >= alive && d < cc.max {
                // All workers busy and requests queueing — add one (warm, ~ms).
                DESIRED.store(d + 1, Ordering::SeqCst);
                idle_ticks = 0;
                tracing::info!(busy, alive, desired = d + 1, "cow autoscale up");
            } else if d > cc.min && busy + 1 < d {
                // Sustained idle — harvest the top worker back down toward min.
                idle_ticks += 1;
                if idle_ticks >= 4 {
                    let nd = d - 1;
                    DESIRED.store(nd, Ordering::SeqCst);
                    idle_ticks = 0;
                    let pid = CHILDREN[nd].load(Ordering::SeqCst);
                    if pid > 0 {
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                    }
                    tracing::info!(busy, alive, desired = nd, "cow autoscale down (harvest)");
                }
            } else {
                idle_ticks = 0;
            }
        }

        if SHUTDOWN.load(Ordering::SeqCst)
            && CHILDREN
                .iter()
                .take(cc.max)
                .all(|c| c.load(Ordering::SeqCst) == 0)
        {
            tracing::info!("askr CoW template exiting");
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// In a forked CoW worker: install its serving bridge and spawn its tokio
/// runtime + accept loop, then return so the inherited PHP serving loop runs.
fn cow_child_setup(cc: &CowCtx) {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
    if cc.config.sandbox {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig {
            write_paths: cc.config.sandbox_write.clone(),
        });
    }
    let php = crate::php::Php::cow_bridge();
    let listener_fd = cc.listener_fd;
    let config = cc.config.clone();
    let recycle = cc.recycle_after;
    std::thread::spawn(move || {
        let tls = worker::build_tls(&config).unwrap_or(None);
        let std_listener = unsafe { std::net::TcpListener::from_raw_fd(listener_fd) };
        let _ = std_listener.set_nonblocking(true);
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("cow worker runtime: {e}");
                std::process::exit(1);
            }
        };
        rt.block_on(async move {
            match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => {
                    // CoW already self-heals: this child exits after run() returns
                    // and the template reforks a warm worker, so the draining flag
                    // here is only to satisfy the signature.
                    let draining = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let _ = crate::server::run(
                        l,
                        std::sync::Arc::new(config),
                        php,
                        recycle,
                        tls,
                        draining,
                    )
                    .await;
                }
                Err(e) => tracing::error!(error = %e, "cow listener"),
            }
        });
        // Server returned (recycle/drain) → exit so the template reforks warm.
        std::process::exit(0);
    });
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
///
/// With canary enabled, roll only the first worker, then health-check it (in the
/// reaper) before rolling the rest — a bad deploy takes down one worker, not all.
extern "C" fn on_reload(_sig: libc::c_int) {
    if CANARY_ENABLED.load(Ordering::SeqCst) {
        CANARY_ERR_BASE.store(error_count(), Ordering::SeqCst);
        CANARY_DEADLINE.store(now_secs() + CANARY_WINDOW_SECS, Ordering::SeqCst);
        CANARY_ACTIVE.store(true, Ordering::SeqCst);
        // Roll only slot 0 (the canary); the reaper rolls the rest if healthy.
        let pid = CHILDREN[0].load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    } else {
        RELOAD_CURSOR.store(0, Ordering::SeqCst);
        roll_next();
    }
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

/// Fork-based test runner (#7). Boots the interpreter once (opcache warm and
/// shared across children via CoW), then forks a fresh process per test file —
/// perfect isolation (no state bleed between files) with parallelism, and no
/// cold boot per file. Reuses the single-threaded-fork discipline from CoW.
fn run_test(
    paths: Vec<PathBuf>,
    root: Option<PathBuf>,
    runner: Option<PathBuf>,
    parallel: Option<usize>,
    ini: Option<String>,
) -> anyhow::Result<()> {
    let base = match root {
        Some(r) => std::fs::canonicalize(&r)?,
        None => std::env::current_dir()?,
    };
    std::env::set_var("ASKR_APP_BASE", &base);
    if let Some(ini) = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok()) {
        std::env::set_var("ASKR_PHP_INI", ini);
    }

    // Collect test files: expand directories to their *Test.php.
    let roots = if paths.is_empty() {
        vec![base.join("tests")]
    } else {
        paths
    };
    let mut files = Vec::new();
    for p in roots {
        if p.is_dir() {
            collect_tests(&p, &mut files);
        } else if p.is_file() {
            files.push(p);
        }
    }
    files.sort();
    anyhow::ensure!(!files.is_empty(), "no test files found");
    if let Some(r) = &runner {
        anyhow::ensure!(r.is_file(), "runner not found: {}", r.display());
    }

    let jobs = parallel.unwrap_or_else(default_workers).max(1);
    tracing::info!(
        files = files.len(),
        parallel = jobs,
        "askr test: booting interpreter once, forking a warm process per file"
    );

    // Boot the interpreter (template) and warm opcache with the autoloader so
    // forked children inherit compiled bytecode via shared memory.
    let mut php = askr_php::Interpreter::new().map_err(|e| anyhow::anyhow!("php init: {e}"))?;
    let autoload = base.join("vendor/autoload.php");
    if autoload.is_file() {
        let _ = php.eval(&format!("require '{}';", autoload.display()));
    }

    let started = std::time::Instant::now();
    let mut running: Vec<(i32, PathBuf)> = Vec::new();
    let mut passed = 0usize;
    let mut failed = 0usize;

    // Reap one child, printing its outcome.
    let reap_one = |running: &mut Vec<(i32, PathBuf)>, passed: &mut usize, failed: &mut usize| {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid > 0 {
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                1
            };
            if let Some(idx) = running.iter().position(|(p, _)| *p == pid) {
                let (_, file) = running.remove(idx);
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                if code == 0 {
                    *passed += 1;
                    println!("  ✓ {name}");
                } else {
                    *failed += 1;
                    println!("  ✗ {name} (exit {code})");
                }
            }
        }
    };

    for file in &files {
        if running.len() >= jobs {
            reap_one(&mut running, &mut passed, &mut failed);
        }
        let script = runner.clone().unwrap_or_else(|| file.clone());
        // SAFETY: single-threaded fork (no tokio here); the child runs one script
        // against the inherited warm interpreter and exits.
        match unsafe { libc::fork() } {
            0 => {
                std::env::set_var("ASKR_TEST_FILE", file);
                let code = php.run_script(&script.to_string_lossy()).unwrap_or(1);
                std::process::exit(code);
            }
            -1 => anyhow::bail!("fork failed: {}", std::io::Error::last_os_error()),
            pid => running.push((pid, file.clone())),
        }
    }
    while !running.is_empty() {
        reap_one(&mut running, &mut passed, &mut failed);
    }

    let elapsed = started.elapsed();
    println!(
        "\n{} file(s): {passed} passed, {failed} failed in {:.2}s",
        files.len(),
        elapsed.as_secs_f64()
    );
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Recursively collect `*Test.php` files under `dir`.
fn collect_tests(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_tests(&p, out);
        } else if p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with("Test.php"))
        {
            out.push(p);
        }
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

#[cfg(test)]
mod tests {
    use super::{cpu_limit_from_cgroup_v2, parse_size};

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_rss_reads_proc() {
        // Our own RSS must be readable and non-trivial.
        let rss = super::worker_rss_bytes(std::process::id() as i32);
        assert!(rss.is_some(), "should read /proc/self RSS on Linux");
        assert!(rss.unwrap() > 512 * 1024, "RSS should be > 512 KB");
        // A pid that cannot exist yields None (not a panic).
        assert!(super::worker_rss_bytes(-1).is_none());
    }

    #[test]
    fn cgroup_cpu_parsing() {
        assert_eq!(cpu_limit_from_cgroup_v2("200000 100000"), Some(2));
        assert_eq!(cpu_limit_from_cgroup_v2("150000 100000"), Some(2)); // 1.5 → ceil 2
        assert_eq!(cpu_limit_from_cgroup_v2("50000 100000"), Some(1)); // 0.5 → ceil 1
        assert_eq!(cpu_limit_from_cgroup_v2("max 100000"), None); // unlimited
        assert_eq!(cpu_limit_from_cgroup_v2("100000"), Some(1)); // default period
        assert_eq!(cpu_limit_from_cgroup_v2("garbage"), None);
    }

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
