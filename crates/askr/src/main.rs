//! Askr — a standalone, memory-safe PHP application server.
//!
//! A1: serve a single PHP application over HTTP through the embedded interpreter.
//! A3: scale across cores with SO_REUSEPORT + one forked worker process per core
//!     (non-ZTS means one interpreter per process, so we scale by processes).

mod acme;
mod admin;
mod broadcast;
#[cfg(feature = "sql-backend")]
mod broadcast_sql;
mod cache;
#[cfg(feature = "sql-backend")]
mod cache_sql;
mod cgi;
mod compress;
mod config;
mod doctor;
#[cfg(feature = "http3")]
mod http3;
mod metrics;
#[cfg(feature = "observ")]
mod observ_sql;
#[cfg(feature = "otel")]
mod otel;
mod php;
mod pusher;
mod queue;
mod rcache;
mod record;
mod sandbox;
mod server;
mod shadow;
mod shmlock;
mod squeue;
#[cfg(feature = "sql-backend")]
mod squeue_sql;
mod supervisor;
mod tls;
mod upgrade;
mod upload;
mod worker;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use clap::{Parser, Subcommand};

use crate::server::Config;
use crate::supervisor::*;
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

        /// Serve HTTP/3 (QUIC) on the TLS port alongside HTTP/1.1+2, advertised
        /// via Alt-Svc. Requires `--tls-cert`/`--tls-key`. (Build with
        /// `--features http3`.)
        #[arg(long)]
        http3: bool,

        /// Redirect plain HTTP requests to HTTPS (308). (Host redirects like
        /// www→apex are configured via `[[redirect]]` in askr.toml.)
        #[arg(long)]
        force_https: bool,

        /// Seconds a client may take to finish the TLS handshake (slowloris guard).
        #[arg(long, default_value = "10")]
        tls_handshake_timeout: u64,

        /// Seconds a client may take to send request headers (slowloris guard).
        #[arg(long, default_value = "15")]
        header_read_timeout: u64,

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

        /// Custom ACME directory URL (e.g. a Pebble test server). Distinct from
        /// `--acme-dir`, which is the local cert-cache directory.
        #[arg(long = "acme-directory-url", alias = "acme-directory")]
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
            http3,
            force_https,
            tls_handshake_timeout,
            header_read_timeout,
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
                    http3,
                    tls_handshake_timeout,
                    header_read_timeout,
                    force_https,
                    redirects: Vec::new(),
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
            queue::warn_if_unavailable();
            let queue_slots = QUEUE_CAP.load(Ordering::SeqCst);
            // The L2 (SQL Anywhere) queue needs no shared-memory ring; it opens a
            // per-process database connection when the bridge is registered.
            if queue_slots > 0 && !queue::l2_enabled() {
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
