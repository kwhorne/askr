//! `askr tune` — measure the app, then suggest settings.
//!
//! Coming from "FPM with defaults", the hard part isn't running Askr, it's knowing
//! what to put in `askr.toml`. This runs a short self-benchmark and prints a config
//! you can paste, with one line of reasoning per number.
//!
//! **Why there's no HTTP load generator here.** Askr's own benchmarks showed PHP is
//! ~99.5 % of request time and I/O ~0.5 % (see `docs/BENCHMARKS.md`) — the same data
//! that de-prioritised the io_uring work. So the useful measurements come from
//! driving the interpreter directly: boot time, per-request wall vs CPU time, memory
//! growth and response size. That's both simpler and closer to what actually decides
//! these settings.
//!
//! Recommendations are deliberately conservative. A wrong suggestion is worse than
//! none: too low a `max_rss_mb` gives you a recycling storm in production.

use anyhow::{Context, Result};
use askr_php::{Interpreter, Request};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// CPU time (user + sys) this process has consumed.
fn cpu_time() -> Duration {
    // SAFETY: getrusage with a zeroed, correctly-typed output struct.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return Duration::ZERO;
        }
        let s = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        s(ru.ru_utime) + s(ru.ru_stime)
    }
}

fn rss_mb() -> Option<u64> {
    crate::metrics::rss_kb(std::process::id() as i32).map(|kb| kb / 1024)
}

struct Measurements {
    boot: Duration,
    mean_wall: Duration,
    mean_cpu: Duration,
    slowest: Duration,
    body_bytes: usize,
    rss_after_warmup: Option<u64>,
    rss_final: Option<u64>,
    requests: usize,
    failures: usize,
}

pub fn run(
    root: Option<PathBuf>,
    front: String,
    requests: usize,
    ini: Option<String>,
) -> Result<()> {
    let docroot = match root {
        Some(r) => std::fs::canonicalize(&r)
            .with_context(|| format!("document root not found: {}", r.display()))?,
        None => std::fs::canonicalize("public").context(
            "no --root given and ./public doesn't exist — point tune at your document root",
        )?,
    };
    let script = docroot.join(&front);
    anyhow::ensure!(
        script.is_file(),
        "front controller not found: {}",
        script.display()
    );

    println!("askr tune\n");
    println!("  app     {}", script.display());
    println!("  cores   {}", num_cores());
    println!("  samples {requests}\n");
    println!("  measuring… (this runs your front controller {requests} times)\n");

    let m = measure(&docroot, &front, requests, ini)?;
    report(&m, &docroot, &front);
    Ok(())
}

fn num_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Drive the interpreter on its own thread (it isn't `Send`).
fn measure(
    docroot: &Path,
    front: &str,
    requests: usize,
    ini: Option<String>,
) -> Result<Measurements> {
    let (docroot, front) = (docroot.to_path_buf(), front.to_string());
    let handle = std::thread::spawn(move || -> Result<Measurements> {
        if let Some(ini) = ini {
            std::env::set_var("ASKR_PHP_INI", ini);
        }
        let script = docroot.join(&front);
        let script_name = format!("/{front}");

        let t0 = Instant::now();
        let mut php = Interpreter::new().map_err(|e| anyhow::anyhow!("{e}"))?;
        let boot = t0.elapsed();

        // A plain anonymous GET "/" through the front controller — the same shape
        // the server builds, minus cookies and query string.
        let server_vars: Vec<(String, String)> = vec![
            ("REQUEST_METHOD".into(), "GET".into()),
            ("REQUEST_URI".into(), "/".into()),
            ("SCRIPT_NAME".into(), script_name.clone()),
            (
                "SCRIPT_FILENAME".into(),
                script.to_string_lossy().into_owned(),
            ),
            (
                "DOCUMENT_ROOT".into(),
                docroot.to_string_lossy().into_owned(),
            ),
            ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
            ("SERVER_NAME".into(), "localhost".into()),
            ("SERVER_PORT".into(), "8000".into()),
            ("REMOTE_ADDR".into(), "127.0.0.1".into()),
            ("HTTP_HOST".into(), "localhost".into()),
        ];
        let make_req = || Request {
            script_filename: script.to_string_lossy().into_owned(),
            method: "GET".into(),
            query_string: String::new(),
            content_type: None,
            cookie: None,
            body: Vec::new(),
            server_vars: server_vars.clone(),
            post_fields: Vec::new(),
            files: Vec::new(),
        };

        // One warm-up request first: the first run pays for OPcache filling and
        // lazy autoloading, and counting that as typical would skew everything.
        let warm = php
            .handle(&make_req())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let rss_after_warmup = rss_mb();

        let mut failures = if warm.status >= 500 { 1 } else { 0 };
        let mut body_bytes = warm.body.len();
        let mut wall_total = Duration::ZERO;
        let mut slowest = Duration::ZERO;
        let cpu_start = cpu_time();

        for _ in 0..requests {
            let t = Instant::now();
            match php.handle(&make_req()) {
                Ok(resp) => {
                    let d = t.elapsed();
                    wall_total += d;
                    slowest = slowest.max(d);
                    body_bytes = body_bytes.max(resp.body.len());
                    if resp.status >= 500 {
                        failures += 1;
                    }
                }
                Err(_) => failures += 1,
            }
        }
        let cpu_total = cpu_time().saturating_sub(cpu_start);
        let n = requests.max(1) as u32;

        Ok(Measurements {
            boot,
            mean_wall: wall_total / n,
            mean_cpu: cpu_total / n,
            slowest,
            body_bytes,
            rss_after_warmup,
            rss_final: rss_mb(),
            requests,
            failures,
        })
    });
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("measurement thread panicked"))?
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn report(m: &Measurements, docroot: &Path, front: &str) {
    let cores = num_cores();
    println!("  PHP boot              {}", ms(m.boot));
    println!(
        "  Request (mean)        {} wall, {} CPU",
        ms(m.mean_wall),
        ms(m.mean_cpu)
    );
    println!("  Slowest request       {}", ms(m.slowest));
    println!(
        "  Response size         {:.1} KB",
        m.body_bytes as f64 / 1024.0
    );
    match (m.rss_after_warmup, m.rss_final) {
        (Some(a), Some(b)) => {
            let growth = b.saturating_sub(a);
            println!("  RSS after warm-up     {a} MB");
            println!(
                "  RSS after {} reqs    {} MB  ({}{} MB)",
                m.requests,
                b,
                if growth > 0 { "+" } else { "" },
                growth
            );
        }
        _ => println!("  RSS                   (unavailable on this platform)"),
    }
    if m.failures > 0 {
        println!(
            "\n  ⚠  {} of {} requests failed (5xx or error). The numbers below describe an\n     app that isn't working — fix that first.",
            m.failures,
            m.requests + 1
        );
    }
    println!();

    // --- worker count ---
    // Wall vs CPU tells us whether the app waits (database, HTTP) or computes. A
    // CPU-bound app is served best by one worker per core; a waiting app can keep
    // more workers busy on the same cores.
    let cpu_ratio = if m.mean_wall.as_secs_f64() > 0.0 {
        (m.mean_cpu.as_secs_f64() / m.mean_wall.as_secs_f64()).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let (workers, why_workers) = suggest_workers(cores, cpu_ratio);

    // --- memory ---
    let (max_rss, why_rss) = match (m.rss_after_warmup, m.rss_final) {
        (Some(a), Some(b)) => {
            let peak = a.max(b);
            let suggested = suggest_max_rss(peak);
            let per_req = (b.saturating_sub(a)) as f64 / m.requests as f64;
            let why = if per_req > 0.05 {
                format!(
                    "2× observed peak ({peak} MB); memory grew {per_req:.2} MB/request, so \
                     recycling will matter"
                )
            } else {
                format!("2× observed peak ({peak} MB); no meaningful growth observed")
            };
            (Some(suggested), why)
        }
        _ => (None, String::new()),
    };

    // --- cache sizing ---
    let resp_kb = (m.body_bytes as f64 / 1024.0).max(1.0);
    let response_slots = 512usize;
    let mode = if m.boot > Duration::from_millis(50) {
        "worker"
    } else {
        "per-request"
    };

    println!("  Suggested askr.toml:\n");
    println!("    [server]");
    println!("    workers = {workers}          # {why_workers}");
    if let Some(mb) = max_rss {
        println!("    max_rss_mb = {mb}         # {why_rss}");
    }
    if mode == "worker" {
        println!(
            "    # boot is {} — use worker mode so it's paid once per worker, not per request:",
            ms(m.boot)
        );
        println!("    # worker = \"examples/laravel-worker.php\"");
    } else {
        println!(
            "    # boot is only {} — per-request mode is fine, and gives you a clean\n    # heap every request",
            ms(m.boot)
        );
    }
    println!();
    println!("    [cache]");
    println!("    slots = 4096              # ~17 MB of small values (sessions, locks, counters)");
    println!(
        "    response_slots = {response_slots}       # ~{:.0} MB; your responses are ~{resp_kb:.1} KB",
        response_slots as f64 * 140.0 / 1024.0
    );
    println!();

    println!("  How this was measured, and what it doesn't cover:\n");
    println!(
        "    {} was run {} times in-process, with no cookies, no query\n    string and no concurrency.",
        docroot.join(front).display(),
        m.requests
    );
    println!(
        "    Real traffic is a mix of routes — a heavy report page can use several times\n    the memory of the one measured here, so treat max_rss_mb as a starting point\n    and watch rss_kb_total on the admin API (/api/status) under real load."
    );
    println!(
        "    There is no HTTP load generator here on purpose: Askr's benchmarks show PHP\n    is ~99.5% of request time, so the interpreter is the thing worth measuring."
    );
    println!("    Run this against a copy of production data, not an empty database.");
}

/// How many workers to suggest, from core count and how CPU-bound the app is.
///
/// The ratio is the whole point: a request that computes is best served by one
/// worker per core, while a request that spends its time waiting on a database can
/// share a core with several others. Capped at 4x cores — past that, memory and
/// context switching cost more than the extra concurrency wins.
fn suggest_workers(cores: usize, cpu_ratio: f64) -> (usize, String) {
    if cpu_ratio >= 0.8 {
        (
            cores,
            format!(
                "{:.0}% CPU-bound \u{21d2} one worker per core",
                cpu_ratio * 100.0
            ),
        )
    } else {
        let w = ((cores as f64 / cpu_ratio.max(0.1)).round() as usize).min(cores * 4);
        (
            w,
            format!(
                "only {:.0}% CPU-bound (waits on I/O) \u{21d2} more workers than cores keeps them busy",
                cpu_ratio * 100.0
            ),
        )
    }
}

/// Twice the observed peak, never below 128 MB: room for a heavier route than the
/// one measured, while still draining long before PHP's own `memory_limit`. Erring
/// low here would trade a memory leak for a recycling storm.
fn suggest_max_rss(peak_mb: u64) -> u64 {
    (peak_mb * 2).max(128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_bound_apps_get_one_worker_per_core() {
        let (w, why) = suggest_workers(8, 1.0);
        assert_eq!(w, 8);
        assert!(why.contains("CPU-bound"), "got {why}");

        // Just over the threshold still counts as CPU-bound.
        assert_eq!(suggest_workers(8, 0.8).0, 8);
    }

    #[test]
    fn io_bound_apps_get_more_workers_than_cores() {
        // Half the time waiting ⇒ roughly twice the cores.
        assert_eq!(suggest_workers(8, 0.5).0, 16);
        // Mostly waiting ⇒ capped at 4x cores rather than growing unbounded.
        assert_eq!(suggest_workers(8, 0.01).0, 32);
        assert_eq!(suggest_workers(4, 0.0).0, 16);
        let (_, why) = suggest_workers(8, 0.1);
        assert!(why.contains("waits on I/O"), "got {why}");
    }

    #[test]
    fn a_single_core_machine_still_gets_a_worker() {
        assert_eq!(suggest_workers(1, 1.0).0, 1);
        assert!(suggest_workers(1, 0.05).0 >= 1);
    }

    #[test]
    fn max_rss_leaves_headroom_and_has_a_floor() {
        assert_eq!(suggest_max_rss(110), 220, "2x the observed peak");
        // A tiny app must not be given a cap so low that normal work recycles it.
        assert_eq!(suggest_max_rss(20), 128);
        assert_eq!(suggest_max_rss(0), 128);
    }
}
