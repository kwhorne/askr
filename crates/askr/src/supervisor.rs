//! Multi-process supervisor: prefork/CoW worker pools, graceful recycling,
//! RSS-based recycling, queue-worker autoscaling, canary + rolling reload, and
//! the status/reload surface the admin plane reads. Extracted from `main.rs`.

use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::server::Config;
use crate::worker::run_worker;

// --- multi-process supervisor --------------------------------------------

pub(crate) const MAX_WORKERS: usize = 512;
// Queue autoscaling target: ~1 worker per this many ready (waiting) jobs.
pub(crate) const QUEUE_BACKLOG_PER_WORKER: usize = 10;

/// How long a job may sit available and unclaimed before Askr says so. Ten seconds of
/// normal queue latency is unremarkable; thirty means nothing is listening.
const STALE_BACKLOG_SECS: u64 = 30;
pub(crate) static CHILDREN: [AtomicI32; MAX_WORKERS] = [const { AtomicI32::new(0) }; MAX_WORKERS];
pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);
pub(crate) static WORKER_COUNT: AtomicUsize = AtomicUsize::new(0);
// Next slot to roll during a graceful reload; >= WORKER_COUNT means "not rolling".
pub(crate) static RELOAD_CURSOR: AtomicUsize = AtomicUsize::new(usize::MAX);
pub(crate) static START_TIME: AtomicU64 = AtomicU64::new(0);
pub(crate) static RESPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);
// Leak-aware recycling: the pid we last SIGTERM'd for exceeding --max-rss (per
// slot), so we don't re-signal a worker that's already draining, and a count of
// how many times it has fired (observability).
pub(crate) static RECYCLE_SENT: [AtomicI32; MAX_WORKERS] =
    [const { AtomicI32::new(0) }; MAX_WORKERS];
pub(crate) static MEM_RECYCLE_COUNT: AtomicUsize = AtomicUsize::new(0);
// Queue-worker autoscaling: current desired count within [QUEUE_MIN, QUEUE_MAX],
// driven by the shared-memory queue backlog.
pub(crate) static QUEUE_DESIRED: AtomicUsize = AtomicUsize::new(0);
// CoW autoscaling bounds + the current desired web-worker count.
pub(crate) static WORKERS_MIN: AtomicUsize = AtomicUsize::new(1);
pub(crate) static WORKERS_MAX: AtomicUsize = AtomicUsize::new(1);
pub(crate) static DESIRED: AtomicUsize = AtomicUsize::new(0);
// Shared-memory job queue slot count (mapped before fork if > 0).
pub(crate) static QUEUE_CAP: AtomicUsize = AtomicUsize::new(0);
// Canary reload: roll one worker, then health-check before rolling the rest.
pub(crate) static CANARY_ENABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static CANARY_ACTIVE: AtomicBool = AtomicBool::new(false);
pub(crate) static CANARY_DEADLINE: AtomicU64 = AtomicU64::new(0);
pub(crate) static CANARY_ERR_BASE: AtomicU64 = AtomicU64::new(0);
/// Fleet totals (slots 1..) captured when the canary started, so the comparison
/// window matches the canary's own lifetime.
pub(crate) static CANARY_FLEET_REQ: AtomicU64 = AtomicU64::new(0);
pub(crate) static CANARY_FLEET_ERR: AtomicU64 = AtomicU64::new(0);
pub(crate) static CANARY_FLEET_US: AtomicU64 = AtomicU64::new(0);
/// Outcome of the last rollout, for `/api/status`. An atomic rather than a string
/// behind a lock because `on_reload` is a **signal handler** — taking a mutex there
/// isn't async-signal-safe.
/// 0 = idle, 1 = rolling, 2 = ok, 3 = aborted, 4 = inconclusive.
pub(crate) static ROLLOUT_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(crate) fn rollout_state_str() -> &'static str {
    match ROLLOUT_STATE.load(Ordering::SeqCst) {
        1 => "rolling",
        2 => "ok",
        3 => "aborted",
        4 => "inconclusive",
        _ => "idle",
    }
}

/// Tunables, set from `[reload]` before the supervisor starts.
pub(crate) static CANARY_WINDOW: AtomicU64 = AtomicU64::new(5);
pub(crate) static CANARY_MIN_REQUESTS: AtomicU64 = AtomicU64::new(20);
/// Percentage points of error rate the canary may exceed the fleet by, ×100.
pub(crate) static CANARY_MAX_ERR_RATE: AtomicU64 = AtomicU64::new(200);
/// Mean-latency factor vs the fleet, ×100 (300 = 3×).
pub(crate) static CANARY_MAX_LAT_FACTOR: AtomicU64 = AtomicU64::new(300);

/// Why the canary gate decided what it did.
pub(crate) enum Verdict {
    Healthy { requests: u64, err_pct: f64 },
    Inconclusive { requests: u64, needed: u64 },
    Unhealthy { reason: String },
}

/// Compare the canary (slot 0) against the rest of the fleet over the same window.
///
/// The comparison is *relative* and *concurrent*: an absolute error count measured
/// fleet-wide can't tell a bad new worker from a site that always serves a few 5xx,
/// and it charges the canary for errors the old workers produced.
pub(crate) fn canary_verdict(web: usize, alive: bool) -> Verdict {
    if !alive {
        return Verdict::Unhealthy {
            reason: "canary worker died".to_string(),
        };
    }
    let Some(m) = crate::metrics::Metrics::get() else {
        return Verdict::Inconclusive {
            requests: 0,
            needed: 0,
        };
    };
    let (c_req, c_err, c_mean) = m.per_worker[0].snapshot();
    let min_req = CANARY_MIN_REQUESTS.load(Ordering::SeqCst);
    if c_req < min_req {
        return Verdict::Inconclusive {
            requests: c_req,
            needed: min_req,
        };
    }
    // Fleet deltas over the canary's window only.
    let (f_req_now, f_err_now, f_us_now) = fleet_totals(0, web);
    let f_req = f_req_now.saturating_sub(CANARY_FLEET_REQ.load(Ordering::SeqCst));
    let f_err = f_err_now.saturating_sub(CANARY_FLEET_ERR.load(Ordering::SeqCst));
    let f_us = f_us_now.saturating_sub(CANARY_FLEET_US.load(Ordering::SeqCst));

    let c_rate = c_err as f64 * 100.0 / c_req as f64;
    let f_rate = if f_req > 0 {
        f_err as f64 * 100.0 / f_req as f64
    } else {
        0.0
    };
    let max_over = CANARY_MAX_ERR_RATE.load(Ordering::SeqCst) as f64 / 100.0;
    if c_rate > f_rate + max_over {
        return Verdict::Unhealthy {
            reason: format!(
                "error rate {c_rate:.2}% vs fleet {f_rate:.2}% (allowed +{max_over:.2} points)"
            ),
        };
    }

    // Latency is only compared when the fleet has enough traffic of its own to be a
    // meaningful baseline — otherwise a quiet fleet makes any canary look slow.
    if f_req >= min_req {
        let f_mean = f_us / f_req.max(1);
        let factor = CANARY_MAX_LAT_FACTOR.load(Ordering::SeqCst) as f64 / 100.0;
        if f_mean > 0 && (c_mean as f64) > f_mean as f64 * factor {
            return Verdict::Unhealthy {
                reason: format!(
                    "mean latency {}ms vs fleet {}ms (allowed {factor:.1}x)",
                    c_mean / 1000,
                    f_mean / 1000
                ),
            };
        }
    }
    Verdict::Healthy {
        requests: c_req,
        err_pct: c_rate,
    }
}

/// Sum the per-worker counters for slots `range`, as `(requests, errors, us)`.
pub(crate) fn fleet_totals(skip_slot: usize, web: usize) -> (u64, u64, u64) {
    let Some(m) = crate::metrics::Metrics::get() else {
        return (0, 0, 0);
    };
    use std::sync::atomic::Ordering::Relaxed;
    let (mut r, mut e, mut us) = (0u64, 0u64, 0u64);
    for i in 0..web.min(crate::metrics::STAT_SLOTS) {
        if i == skip_slot {
            continue;
        }
        let st = &m.per_worker[i];
        r += st.requests.load(Relaxed);
        e += st.errors.load(Relaxed);
        us += st.us_sum.load(Relaxed);
    }
    (r, e, us)
}
// Crash-loop guard: a worker that dies within BOOT_FAIL_SECS of being spawned is a
// boot failure (bad TLS cert, bad config, panic on startup) rather than normal
// recycling. If enough boot failures pile up within FASTFAIL_WINDOW_SECS the master
// gives up instead of respawning forever and burning a core.
pub(crate) static SPAWN_AT: [AtomicU64; MAX_WORKERS] = [const { AtomicU64::new(0) }; MAX_WORKERS];

/// This process's prefork slot, set in the child right after fork. The master
/// leaves it at `usize::MAX`. Workers use it to attribute their own request
/// counters, which is what lets the canary gate compare one worker to the rest.
/// Slots held empty on purpose: a canary that failed its gate is drained and not
/// respawned, so a known-bad worker doesn't keep serving a slice of traffic.
/// Cleared when the next reload starts.
pub(crate) static QUARANTINED: [AtomicBool; MAX_WORKERS] =
    [const { AtomicBool::new(false) }; MAX_WORKERS];

/// Where to persist the response cache on graceful shutdown, and the app stamp
/// that must match for it to be loaded again. Set once at startup.
pub(crate) static CACHE_PERSIST: std::sync::OnceLock<(std::path::PathBuf, u64)> =
    std::sync::OnceLock::new();

pub(crate) static MY_SLOT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);
pub(crate) static FASTFAIL_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(crate) static FASTFAIL_WINDOW: AtomicU64 = AtomicU64::new(0);
pub(crate) const BOOT_FAIL_SECS: u64 = 3;
pub(crate) const FASTFAIL_WINDOW_SECS: u64 = 30;

/// Aggregate error signal (BAD_GATEWAY + app 5xx) for the canary check.
pub(crate) fn error_count() -> u64 {
    match crate::metrics::Metrics::get() {
        Some(m) => {
            use std::sync::atomic::Ordering::Relaxed;
            m.errors.load(Relaxed) + m.status[4].load(Relaxed)
        }
        None => 0,
    }
}

pub(crate) fn now_secs() -> u64 {
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
    /// Outcome of the last canary rollout: idle | rolling | ok | aborted |
    /// inconclusive. `aborted` means a deploy was stopped and the fleet is still
    /// running the previous code.
    pub rollout: &'static str,
}

pub fn status() -> Status {
    let pids: Vec<i32> = CHILDREN
        .iter()
        .map(|c| c.load(Ordering::SeqCst))
        .filter(|&p| p > 0)
        .collect();
    let (queue_ready, queue_total, queue_oldest_ms) = if crate::queue::enabled() {
        crate::queue::stats()
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
        rollout: rollout_state_str(),
    }
}

/// Trigger a graceful rolling reload (used by SIGHUP and the admin API).
pub fn trigger_reload() {
    RELOAD_CURSOR.store(0, Ordering::SeqCst);
    roll_next();
}

/// Poll the TLS cert (and key) mtime and trigger a graceful reload when it changes,
/// so an external renewal (certbot etc.) is picked up with no downtime. Cheap: two
/// `stat`s every 30 s. Only changes *after* startup trigger a reload.
fn spawn_cert_watcher(cert: PathBuf, key: Option<PathBuf>) {
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    std::thread::Builder::new()
        .name("askr-cert-watch".into())
        .spawn(move || {
            let mut last = (mtime(&cert), key.as_deref().and_then(mtime));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let now = (mtime(&cert), key.as_deref().and_then(mtime));
                // Only reload on a real change where the cert still exists (avoid
                // reacting to a transient mid-renewal unlink).
                if now != last && now.0.is_some() {
                    last = now;
                    tracing::info!(
                        cert = %cert.display(),
                        "TLS certificate changed on disk — triggering graceful reload"
                    );
                    trigger_reload();
                }
            }
        })
        .ok();
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
pub(crate) enum Kind {
    Web,
    Queue,
    Scheduler,
    Command,
}

/// A process's resident set size (RSS) in bytes, via `/proc/<pid>/statm` (field 2
/// = resident pages). Linux only; `None` elsewhere or if the process is gone.
#[cfg(target_os = "linux")]
pub(crate) fn worker_rss_bytes(pid: i32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page > 0).then(|| resident_pages * page as u64)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn worker_rss_bytes(_pid: i32) -> Option<u64> {
    None
}

/// Gracefully recycle any PHP worker whose RSS has crossed `max_rss_mb`, *before*
/// it hits PHP's `memory_limit` and OOMs. Sending SIGTERM triggers the worker's
/// graceful drain (finish in-flight requests, then exit); the supervisor's reap
/// loop respawns a fresh one. Coalesced per slot so we never signal a worker
/// that's already draining. `php_workers` = the leading slots that run PHP
/// (web + queue); sidecars are external and skipped.
pub(crate) fn recycle_over_rss(max_rss_mb: usize, php_workers: usize) {
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

pub(crate) fn supervise(
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
        // Zero this slot's counters *before* the fork, so a fresh worker's stats
        // describe its own life and not its predecessor's.
        if let Some(m) = crate::metrics::Metrics::get() {
            if let Some(st) = m.per_worker.get(i) {
                st.reset();
            }
        }
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
                MY_SLOT.store(i, Ordering::SeqCst);
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
                    Kind::Queue => crate::worker::run_sidecar(
                        sidecars.queue_script.clone().unwrap(),
                        ini.clone(),
                    ),
                    Kind::Scheduler => crate::worker::run_sidecar(
                        sidecars.scheduler_script.clone().unwrap(),
                        ini.clone(),
                    ),
                    Kind::Command => {
                        let idx = i - (web + queue + sched);
                        match sidecars.commands.get(idx) {
                            Some(cmd) => crate::worker::run_command(cmd),
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
                SPAWN_AT[i].store(now_secs(), Ordering::SeqCst);
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
        let info = crate::admin::Info {
            server_listen: config.listen,
            mode: if config.worker_script.is_some() {
                "worker"
            } else {
                "per-request"
            },
            record_dir: config.record_dir.clone(),
        };
        crate::admin::spawn(addr, info);
    }

    // Watch an external TLS cert on disk (e.g. a certbot renewal) and hot-reload
    // when it changes — respawned workers re-read the cert, so no restart needed.
    // Self-signed doesn't renew, and `--acme` manages its own certificate.
    if !config.tls_self_signed {
        if let Some(cert) = config.tls_cert.clone() {
            spawn_cert_watcher(cert, config.tls_key.clone());
        }
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
    let mut last_stale_check = std::time::Instant::now();
    // Warned-about queues, so a persistent backlog says its piece once a minute instead of
    // every pass. Cleared per queue as soon as it drains, so a recurrence is reported again.
    let mut warned_stale: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
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
                        // Crash-loop guard: a worker that died within BOOT_FAIL_SECS
                        // of spawn *with a non-zero exit* is a boot failure (bad
                        // cert/config, or an app that fatals on the first request) —
                        // not normal recycling, which drains and exits 0. Too many in
                        // a short window ⇒ give up instead of respawning forever.
                        let alive = now_secs().saturating_sub(SPAWN_AT[i].load(Ordering::SeqCst));
                        let failed_exit = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) != 0;
                        if alive < BOOT_FAIL_SECS && failed_exit {
                            let now = now_secs();
                            if now.saturating_sub(FASTFAIL_WINDOW.load(Ordering::SeqCst))
                                > FASTFAIL_WINDOW_SECS
                            {
                                FASTFAIL_WINDOW.store(now, Ordering::SeqCst);
                                FASTFAIL_COUNT.store(1, Ordering::SeqCst);
                            } else {
                                let n = FASTFAIL_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
                                if n >= (workers * 3).max(10) {
                                    tracing::error!(
                                        worker = i,
                                        failures = n,
                                        window_secs = FASTFAIL_WINDOW_SECS,
                                        "workers are crash-looping on boot — check the TLS \
                                         cert (must be X.509 v3 with a SAN) / config; giving up"
                                    );
                                    kill_all(libc::SIGTERM);
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            // Survived long enough to be healthy — clear the streak.
                            FASTFAIL_COUNT.store(0, Ordering::SeqCst);
                        }
                        if QUARANTINED[i].load(Ordering::SeqCst) {
                            tracing::warn!(
                                pid,
                                worker = i,
                                "quarantined slot stays empty until the next reload \
                                 (failed canary) — running with one worker fewer"
                            );
                            continue;
                        }
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

        // Refill any empty web slot that isn't deliberately quarantined. Without
        // this, a slot emptied by a failed canary would stay empty forever: the
        // next reload clears the quarantine but has no live PID to roll, so the
        // gate would see a dead canary and abort again on every attempt.
        //
        // Never during shutdown — draining workers empty their slots, and refilling
        // them there would fight the shutdown and the master would never exit.
        if !SHUTDOWN.load(Ordering::SeqCst) {
            for i in 0..web.min(MAX_WORKERS) {
                if CHILDREN[i].load(Ordering::SeqCst) == 0 && !QUARANTINED[i].load(Ordering::SeqCst)
                {
                    tracing::info!(worker = i, "filling empty worker slot");
                    spawn_slot(i);
                }
            }
        }

        // Canary gate: once the window elapses, decide whether to roll the rest.
        if CANARY_ACTIVE.load(Ordering::SeqCst)
            && now_secs() >= CANARY_DEADLINE.load(Ordering::SeqCst)
        {
            CANARY_ACTIVE.store(false, Ordering::SeqCst);
            let alive = CHILDREN[0].load(Ordering::SeqCst) != 0;
            let verdict = canary_verdict(web, alive);
            match verdict {
                Verdict::Healthy { requests, err_pct } => {
                    tracing::info!(
                        requests,
                        err_pct = format!("{err_pct:.2}%"),
                        "canary healthy — rolling the rest"
                    );
                    ROLLOUT_STATE.store(2, Ordering::SeqCst);
                    RELOAD_CURSOR.store(1, Ordering::SeqCst);
                    roll_next();
                }
                // Not enough traffic to judge. Don't block the deploy on no
                // evidence — but say so, because a silent pass looks like a pass.
                Verdict::Inconclusive { requests, needed } => {
                    tracing::warn!(
                        requests,
                        needed,
                        "canary saw too little traffic to judge — rolling the rest anyway \
                         (lower reload.canary_min_requests or raise reload.canary_window \
                         to make this conclusive)"
                    );
                    ROLLOUT_STATE.store(4, Ordering::SeqCst);
                    RELOAD_CURSOR.store(1, Ordering::SeqCst);
                    roll_next();
                }
                Verdict::Unhealthy { reason } => {
                    tracing::error!(
                        reason = %reason,
                        canary_alive = alive,
                        "canary UNHEALTHY — aborting reload"
                    );
                    ROLLOUT_STATE.store(3, Ordering::SeqCst);
                    // Draining the bad canary matters as much as not rolling the
                    // rest: leaving it up means the failed deploy still serves
                    // 1/N of traffic. Respawning it would just boot the same bad
                    // build, so the slot is quarantined until the next reload.
                    // Never below one worker — an empty fleet is worse than a bad one.
                    let pid = CHILDREN[0].load(Ordering::SeqCst);
                    if web > 1 && pid > 0 {
                        QUARANTINED[0].store(true, Ordering::SeqCst);
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                        tracing::warn!(
                            pid,
                            "draining the failed canary; the fleet keeps serving on \
                             {} worker(s). Fix the deploy and reload again.",
                            web - 1
                        );
                    } else {
                        tracing::error!(
                            "only one worker configured — keeping the failed canary up, \
                             because no workers at all is worse"
                        );
                    }
                }
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
            && crate::queue::enabled()
            && last_queue_check.elapsed() >= std::time::Duration::from_secs(2)
        {
            // (backlog watchdog runs below, independent of autoscaling)
            last_queue_check = std::time::Instant::now();
            let (ready, _total, _oldest) = crate::queue::stats();
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

        // Backlog watchdog. Jobs that sit available and unclaimed mean nothing is
        // consuming that queue, and until 1.4.11 Askr held every number needed to say so
        // and said nothing: a site queued its password-reset and invitation mail to
        // `onQueue('mail')` while the only worker polled `default`, so the mail simply
        // never went out. No exception, no log line, and a worker asleep in nanosleep.
        //
        // Named per queue, because the aggregate is what made it invisible — "1 job ready"
        // was true and useless. Runs regardless of autoscaling: a fixed-size pool is
        // exactly where this goes unnoticed.
        if !SHUTDOWN.load(Ordering::SeqCst)
            && last_stale_check.elapsed() >= std::time::Duration::from_secs(10)
        {
            last_stale_check = std::time::Instant::now();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut seen: Vec<String> = Vec::new();
            for (name, c) in crate::queue::by_queue() {
                if c.pending == 0 || c.oldest_pending_created_ms == 0 {
                    continue;
                }
                let age = now_ms.saturating_sub(c.oldest_pending_created_ms) / 1000;
                if age < STALE_BACKLOG_SECS {
                    continue;
                }
                seen.push(name.clone());
                let due = warned_stale
                    .get(&name)
                    .map(|t| t.elapsed() >= std::time::Duration::from_secs(60))
                    .unwrap_or(true);
                if due {
                    warned_stale.insert(name.clone(), std::time::Instant::now());
                    let workers = QUEUE_DESIRED.load(Ordering::SeqCst);
                    tracing::warn!(
                        queue = %name,
                        pending = c.pending,
                        oldest_secs = age,
                        queue_workers = workers,
                        "queue backlog is not being consumed — no worker is taking jobs \
                         from this queue. Check that a queue worker is running (--queue \
                         with --queue-script) and that it polls this queue name \
                         (ASKR_QUEUE, comma-separated)."
                    );
                }
            }
            // Forget queues that drained, so the next occurrence warns immediately rather
            // than waiting out a stale cooldown.
            warned_stale.retain(|k, _| seen.iter().any(|s| s == k));
        }

        if SHUTDOWN.load(Ordering::SeqCst) && CHILDREN.iter().all(|c| c.load(Ordering::SeqCst) == 0)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Persist the response cache now that every worker is reaped: the region is
    // quiescent, so no slot can be captured mid-lock.
    if let Some((path, stamp)) = CACHE_PERSIST.get() {
        match crate::rcache::dump(path, *stamp) {
            Ok(0) => {}
            Ok(n) => tracing::info!(entries = n, path = %path.display(), "response cache saved"),
            Err(e) => tracing::warn!(error = %e, path = %path.display(),
                "could not save the response cache"),
        }
    }
    tracing::info!("askr master exiting");
    Ok(())
}

// --- CoW template (fork a warm, booted app; ~ms respawn) -----------------

use std::ffi::{c_int, c_void};

pub(crate) struct CowCtx {
    config: Config,
    listener_fd: RawFd,
    min: usize,
    max: usize,
    recycle_after: usize,
}

/// Boot the app once in this (template) process, then supervise workers forked
/// from it. The template is single-threaded when it forks (tokio starts only in
/// the children), so the fork is safe; workers inherit the warm heap via CoW.
pub(crate) fn run_cow(
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
    crate::queue::register_bridge();
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
        // Reap *everything* that has exited, not one per pass: this loop sleeps
        // between iterations, so a single waitpid per round left N-1 zombies parked for
        // N sleeps when a batch of workers died together (a reload, or an OOM sweep) —
        // and their slots stayed empty that whole time instead of being reforked. The
        // main supervisor loop already reaps in a loop; this is the same shape.
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break; // 0 = none exited, -1 = no children
            }
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
pub(crate) fn cow_child_setup(cc: &CowCtx) {
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
        let tls = crate::worker::build_tls(&config).unwrap_or(None);
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
pub(crate) fn kill_all(sig: libc::c_int) {
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
pub(crate) fn roll_next() {
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
        // Snapshot the fleet so the canary is compared against the *same* window.
        // Atomics only in here: this is a signal handler.
        let web = WORKER_COUNT.load(Ordering::SeqCst);
        let (fr, fe, fus) = fleet_totals(0, web);
        CANARY_FLEET_REQ.store(fr, Ordering::SeqCst);
        CANARY_FLEET_ERR.store(fe, Ordering::SeqCst);
        CANARY_FLEET_US.store(fus, Ordering::SeqCst);
        CANARY_DEADLINE.store(
            now_secs() + CANARY_WINDOW.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        CANARY_ACTIVE.store(true, Ordering::SeqCst);
        ROLLOUT_STATE.store(1, Ordering::SeqCst);
        // A new deploy gets a clean slate: refill any slot a previous failed
        // canary left quarantined.
        for q in QUARANTINED.iter().take(web) {
            q.store(false, Ordering::SeqCst);
        }
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

pub(crate) fn install_signals() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The canary gate reads process-wide statics and the shared metrics region, so
    /// these run one at a time.
    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reset the counters and thresholds the gate looks at.
    fn setup(min_requests: u64, max_err_rate_pct: f64) {
        crate::metrics::Metrics::init();
        let m = crate::metrics::Metrics::get().expect("metrics region");
        for st in m.per_worker.iter() {
            st.reset();
        }
        CANARY_MIN_REQUESTS.store(min_requests, Ordering::SeqCst);
        CANARY_MAX_ERR_RATE.store((max_err_rate_pct * 100.0) as u64, Ordering::SeqCst);
        CANARY_MAX_LAT_FACTOR.store(300, Ordering::SeqCst);
        CANARY_FLEET_REQ.store(0, Ordering::SeqCst);
        CANARY_FLEET_ERR.store(0, Ordering::SeqCst);
        CANARY_FLEET_US.store(0, Ordering::SeqCst);
    }

    /// Record `requests` requests on a slot, `errors` of them failing, each taking
    /// `us` microseconds.
    fn serve(slot: usize, requests: u64, errors: u64, us: u64) {
        let m = crate::metrics::Metrics::get().unwrap();
        let st = &m.per_worker[slot];
        st.requests.fetch_add(requests, Ordering::Relaxed);
        st.errors.fetch_add(errors, Ordering::Relaxed);
        st.us_sum.fetch_add(requests * us, Ordering::Relaxed);
    }

    #[test]
    fn a_dead_canary_is_unhealthy() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(5, 2.0);
        match canary_verdict(2, false) {
            Verdict::Unhealthy { reason } => assert!(reason.contains("died"), "got {reason}"),
            _ => panic!("a canary that isn't running can't be healthy"),
        }
    }

    #[test]
    fn a_quiet_canary_is_inconclusive_not_healthy() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(20, 2.0);
        serve(0, 3, 0, 1000); // well under the minimum
        match canary_verdict(2, true) {
            Verdict::Inconclusive { requests, needed } => {
                assert_eq!((requests, needed), (3, 20));
            }
            _ => panic!("3 requests is not evidence of health"),
        }
    }

    #[test]
    fn a_clean_canary_is_healthy() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        serve(0, 50, 0, 1000);
        serve(1, 50, 0, 1000);
        match canary_verdict(2, true) {
            Verdict::Healthy { requests, err_pct } => {
                assert_eq!(requests, 50);
                assert!(err_pct < 0.01, "got {err_pct}");
            }
            Verdict::Unhealthy { reason } => panic!("should be healthy, got: {reason}"),
            Verdict::Inconclusive { .. } => panic!("50 requests is enough to judge"),
        }
    }

    /// The bug this replaced: errors were counted fleet-wide, so the canary was
    /// charged for the *old* workers' failures. A site with a normal error baseline
    /// could then never complete a reload.
    #[test]
    fn the_canary_is_not_blamed_for_the_fleets_errors() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        serve(0, 100, 0, 1000); // canary: flawless
        serve(1, 100, 30, 1000); // fleet: 30% errors, and it's not the canary's fault
        serve(2, 100, 30, 1000);
        match canary_verdict(3, true) {
            Verdict::Healthy { .. } => {}
            Verdict::Unhealthy { reason } => {
                panic!("a clean canary must not be aborted by the fleet's errors: {reason}")
            }
            Verdict::Inconclusive { .. } => panic!("plenty of traffic here"),
        }
    }

    #[test]
    fn a_failing_canary_is_aborted_even_when_the_fleet_is_fine() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        serve(0, 100, 60, 1000); // canary: 60% errors
        serve(1, 100, 0, 1000); // fleet: clean
        match canary_verdict(2, true) {
            Verdict::Unhealthy { reason } => {
                assert!(reason.contains("error rate"), "got {reason}");
                assert!(
                    reason.contains("60.00%"),
                    "the reason should quote the rate: {reason}"
                );
            }
            _ => panic!("60% errors against a clean fleet must abort"),
        }
    }

    /// A relative threshold, not an absolute count: a site that always serves some
    /// 5xx shouldn't abort every deploy.
    #[test]
    fn a_matching_error_rate_is_tolerated() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        serve(0, 100, 5, 1000); // canary 5%
        serve(1, 100, 5, 1000); // fleet 5% — the app is just like that
        match canary_verdict(2, true) {
            Verdict::Healthy { .. } => {}
            Verdict::Unhealthy { reason } => {
                panic!("matching the fleet's baseline is not a regression: {reason}")
            }
            Verdict::Inconclusive { .. } => panic!("enough traffic"),
        }
        // But clearly worse than the fleet is.
        setup(10, 2.0);
        serve(0, 100, 20, 1000); // canary 20% vs fleet 5% ⇒ +15 points
        serve(1, 100, 5, 1000);
        assert!(matches!(canary_verdict(2, true), Verdict::Unhealthy { .. }));
    }

    #[test]
    fn a_much_slower_canary_is_aborted() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        serve(0, 50, 0, 40_000); // canary: 40ms mean
        serve(1, 50, 0, 5_000); // fleet: 5ms mean ⇒ 8× slower
        match canary_verdict(2, true) {
            Verdict::Unhealthy { reason } => assert!(reason.contains("latency"), "got {reason}"),
            _ => panic!("8x the fleet's latency is a regression"),
        }
    }

    /// Latency is only judged against a fleet that has traffic of its own —
    /// otherwise a quiet fleet would make every canary look slow.
    #[test]
    fn latency_is_not_judged_against_an_idle_fleet() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        // The canary is slow, but the fleet served nothing — so there's no baseline.
        serve(0, 50, 0, 40_000);
        match canary_verdict(2, true) {
            Verdict::Healthy { .. } => {}
            Verdict::Unhealthy { reason } => {
                panic!("an idle fleet is not a latency baseline: {reason}")
            }
            Verdict::Inconclusive { .. } => panic!("the canary itself had enough traffic"),
        }
    }

    #[test]
    fn fleet_totals_skip_the_canary_slot() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        setup(10, 2.0);
        serve(0, 10, 1, 1000);
        serve(1, 20, 2, 2000);
        serve(2, 30, 3, 3000);
        let (req, err, _us) = fleet_totals(0, 3);
        assert_eq!((req, err), (50, 5), "slot 0 must be excluded");
    }

    #[test]
    fn rollout_state_is_reported_as_text() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        for (code, want) in [
            (0u8, "idle"),
            (1, "rolling"),
            (2, "ok"),
            (3, "aborted"),
            (4, "inconclusive"),
        ] {
            ROLLOUT_STATE.store(code, Ordering::SeqCst);
            assert_eq!(rollout_state_str(), want);
        }
        ROLLOUT_STATE.store(0, Ordering::SeqCst);
    }
}
