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
pub(crate) const CANARY_WINDOW_SECS: u64 = 5;
pub(crate) const CANARY_ERR_THRESHOLD: u64 = 3;

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
            && crate::queue::enabled()
            && last_queue_check.elapsed() >= std::time::Duration::from_secs(2)
        {
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
