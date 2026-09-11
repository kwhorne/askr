//! Queue backend dispatch: pick the L1 shared-memory queue (`squeue`) or, when
//! the `sql-backend` feature is built and `ASKR_QUEUE_DB` is set, the L2 durable
//! SQL Anywhere queue (`squeue_sql`).
//!
//! Every place that used to call `squeue::register_bridge()` calls
//! [`register_bridge`] here instead, so the choice is made once, at registration
//! time, per process.

/// Whether the durable L2 queue backend is active for this process.
pub fn l2_enabled() -> bool {
    #[cfg(feature = "sql-backend")]
    {
        crate::squeue_sql::enabled()
    }
    #[cfg(not(feature = "sql-backend"))]
    {
        false
    }
}

/// Whether *a* queue backend (L1 or L2) is active — used to decide whether to
/// read backlog and run the worker autoscaler.
pub fn enabled() -> bool {
    #[cfg(feature = "sql-backend")]
    if crate::squeue_sql::enabled() {
        return true;
    }
    crate::squeue::enabled()
}

/// Backlog stats `(ready, total, oldest_ms)` from the active backend, for
/// metrics and backlog-driven worker autoscaling (elyra-8).
pub fn stats() -> (usize, usize, u64) {
    #[cfg(feature = "sql-backend")]
    if crate::squeue_sql::enabled() {
        return crate::squeue_sql::stats();
    }
    crate::squeue::stats()
}

/// Every queue holding a job, with its counts, from the active backend.
///
/// Named rather than aggregated on purpose: the backlog watchdog exists to say *which*
/// queue is not being drained, and an aggregate count cannot.
pub fn by_queue() -> Vec<(String, crate::squeue::Counts)> {
    #[cfg(feature = "sql-backend")]
    if crate::squeue_sql::enabled() {
        return crate::squeue_sql::by_queue();
    }
    crate::squeue::by_queue()
}

/// Default for [`stall_secs`]: ten seconds of queue latency is unremarkable, thirty
/// means nothing is listening.
pub const DEFAULT_STALL_SECS: u64 = 30;

static STALL_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(DEFAULT_STALL_SECS);

/// Set the stall threshold from `[queue] stall_secs`. 0 keeps the default.
pub fn set_stall_secs(secs: u64) {
    if secs > 0 {
        STALL_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How old the oldest pending job must be before a backlog counts as stalled.
///
/// One definition, used by the watchdog that logs it, the admin API that reports it and
/// the Prometheus series that alerts on it. They disagreed by construction while the
/// threshold lived privately in the supervisor: a consumer of `/api/status` had to
/// hardcode Askr's number to draw the same conclusion Askr had already drawn, and would
/// then have drifted from it the moment either side changed.
pub fn stall_secs() -> u64 {
    STALL_SECS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How long without a poll before a lane counts as unattended.
///
/// A Laravel queue worker polls continuously — sub-second while jobs flow, and on its
/// `--sleep` interval (3 s by default) when idle. A minute of silence is therefore many
/// missed polls, not a slow tick, while still leaving room for a long `--sleep` or a
/// worker mid-restart.
pub const POLL_STALE_SECS: u64 = 60;

/// Why a lane is being flagged. The distinction is the actionable part: the two faults
/// look identical in job age and have opposite remedies.
#[derive(Debug, PartialEq, Eq)]
pub enum LaneFault {
    /// Jobs are waiting and no worker is asking this lane for them. The queue name is
    /// almost always the cause — an app dispatching to `onQueue('mail')` while the
    /// worker polls `default`. Adding workers does nothing.
    Unattended,
    /// Workers are polling and jobs are waiting anyway. The lane is saturated, or jobs
    /// keep being released back. More workers, or a look at what is failing.
    NotDraining,
}

impl LaneFault {
    /// A stable machine-readable tag. Distinct from the prose, which is not stable.
    pub fn kind(&self) -> &'static str {
        match self {
            LaneFault::Unattended => "queue_unattended",
            LaneFault::NotDraining => "queue_not_draining",
        }
    }
}

/// One flagged lane, with the numbers that justify the flag.
#[derive(Debug)]
pub struct LaneWarning {
    pub queue: String,
    pub fault: LaneFault,
    pub pending: u64,
    pub oldest_pending_secs: u64,
    /// Seconds since a worker last asked this lane for work; `None` = never.
    pub last_polled_secs: Option<u64>,
    /// Seconds since a worker last took a job from it; `None` = never.
    pub last_drained_secs: Option<u64>,
}

/// Age in seconds of a unix-ms stamp, or `None` when it was never set.
///
/// Saturating, because a clock that stepped backwards must not turn into a huge age and
/// a spurious alert.
fn age_secs(now_ms: u64, stamp_ms: u64) -> Option<u64> {
    (stamp_ms != 0).then(|| now_ms.saturating_sub(stamp_ms) / 1000)
}

/// Every lane a worker has polled, with its liveness stamps. Empty on the L2 backend,
/// which has no shared-memory lane table.
pub fn lanes() -> Vec<crate::squeue::LaneStats> {
    #[cfg(feature = "sql-backend")]
    if crate::squeue_sql::enabled() {
        return Vec::new();
    }
    crate::squeue::lanes()
}

/// Lanes that are in trouble, and why — the one computation behind the log line, the
/// `warnings` array in `/api/status` and the Prometheus series.
///
/// Returns empty when nothing is wrong, which is what lets a product render it directly
/// instead of reimplementing Askr's thresholds and then drifting from them.
pub fn warnings(now_ms: u64) -> Vec<LaneWarning> {
    warnings_from(now_ms, &by_queue(), &lanes())
}

/// Same, over data the caller already has.
///
/// `by_queue` walks every job slot taking a per-slot lock, so a caller that needs both
/// the per-queue report and the warnings — `/api/status` does — must not pay for the
/// scan twice.
pub fn warnings_from(
    now_ms: u64,
    occupied: &[(String, crate::squeue::Counts)],
    lanes: &[crate::squeue::LaneStats],
) -> Vec<LaneWarning> {
    let mut out = Vec::new();
    for (name, c) in occupied {
        if c.pending == 0 || c.oldest_pending_created_ms == 0 {
            continue;
        }
        let oldest = now_ms.saturating_sub(c.oldest_pending_created_ms) / 1000;
        if oldest < stall_secs() {
            continue;
        }
        let lane = lanes.iter().find(|l| &l.name == name);
        let last_polled_secs = lane.and_then(|l| age_secs(now_ms, l.last_polled_ms));
        let last_drained_secs = lane.and_then(|l| age_secs(now_ms, l.last_drained_ms));
        // Never polled, or not polled for a long time, means nobody is listening. Note
        // that an absent lane entry (a full table) reads as unattended only together
        // with a backlog this old, which is already the fault condition.
        // `is_none_or` would read better and is stable only from 1.82; the MSRV here is
        // 1.80.
        let unattended = !matches!(last_polled_secs, Some(s) if s <= POLL_STALE_SECS);
        out.push(LaneWarning {
            queue: name.clone(),
            fault: if unattended {
                LaneFault::Unattended
            } else {
                LaneFault::NotDraining
            },
            pending: c.pending,
            oldest_pending_secs: oldest,
            last_polled_secs,
            last_drained_secs,
        });
    }
    out.sort_by(|a, b| {
        b.pending
            .cmp(&a.pending)
            .then_with(|| a.queue.cmp(&b.queue))
    });
    out
}

/// Register the PHP queue bridge with the appropriate backend.
pub fn register_bridge() {
    #[cfg(feature = "sql-backend")]
    if crate::squeue_sql::enabled() {
        crate::squeue_sql::register_bridge();
        return;
    }
    crate::squeue::register_bridge();
}

/// Warn if the L2 backend was requested but this binary was built without it, so
/// a misconfigured deployment fails loudly instead of silently using L1.
pub fn warn_if_unavailable() {
    #[cfg(not(feature = "sql-backend"))]
    if std::env::var_os("ASKR_QUEUE_DB").is_some() {
        tracing::warn!(
            "ASKR_QUEUE_DB is set but this build lacks the `sql-backend` feature; \
             falling back to the L1 shared-memory queue"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::squeue::{Counts, LaneStats};

    fn lane(name: &str, polled_ms: u64, drained_ms: u64) -> LaneStats {
        LaneStats {
            name: name.into(),
            last_polled_ms: polled_ms,
            last_drained_ms: drained_ms,
        }
    }

    fn backlog(name: &str, pending: u64, oldest_ms: u64) -> (String, Counts) {
        (
            name.into(),
            Counts {
                pending,
                delayed: 0,
                reserved: 0,
                oldest_pending_created_ms: oldest_ms,
            },
        )
    }

    const NOW: u64 = 1_000_000_000_000;

    /// The two faults look identical in job age and have opposite remedies, so the
    /// classification is the whole value of the lane table.
    ///
    /// Before this, the watchdog asserted "no worker is taking jobs from this queue" for
    /// both — which sent an operator hunting a queue-name typo during a plain saturation,
    /// and was right only by luck the rest of the time.
    #[test]
    fn an_unattended_lane_is_told_apart_from_a_saturated_one() {
        // Old backlog, nothing has polled it in ten minutes: nobody is listening.
        let w = warnings_from(
            NOW,
            &[backlog("mail", 41, NOW - 600_000)],
            &[lane("mail", NOW - 600_000, 0)],
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].fault, LaneFault::Unattended);
        assert_eq!(w[0].fault.kind(), "queue_unattended");
        assert_eq!(w[0].last_drained_secs, None, "never drained reads as None");

        // Same backlog, but a worker polled a second ago: it is attached and behind.
        let w = warnings_from(
            NOW,
            &[backlog("mail", 41, NOW - 600_000)],
            &[lane("mail", NOW - 1_000, NOW - 300_000)],
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].fault, LaneFault::NotDraining);
        assert_eq!(w[0].last_drained_secs, Some(300));
    }

    /// A lane nothing has *ever* polled is the Félagi case: an app dispatching to
    /// `onQueue('mail')` while the worker polls `default`. It must not read as healthy
    /// just because there is no stamp to compare against.
    #[test]
    fn a_lane_with_no_entry_at_all_is_unattended() {
        let w = warnings_from(NOW, &[backlog("mail", 3, NOW - 300_000)], &[]);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].fault, LaneFault::Unattended);
        assert_eq!(w[0].last_polled_secs, None);
    }

    #[test]
    fn a_healthy_backlog_warns_about_nothing() {
        // Younger than the stall threshold: normal queue latency, not a fault.
        let fresh = NOW - (stall_secs() - 1) * 1000;
        assert!(
            warnings_from(
                NOW,
                &[backlog("default", 9, fresh)],
                &[lane("default", NOW, NOW)]
            )
            .is_empty(),
            "a queue being worked through is not a warning"
        );
        // Nothing pending at all.
        assert!(warnings_from(
            NOW,
            &[backlog("default", 0, 0)],
            &[lane("default", NOW, NOW)]
        )
        .is_empty());
    }

    /// A clock that steps backwards must not manufacture an alert. `oldest_pending` in
    /// the future would underflow into a huge age and flag every lane at once.
    #[test]
    fn a_backwards_clock_does_not_invent_a_backlog() {
        let w = warnings_from(
            NOW,
            &[backlog("default", 5, NOW + 60_000)],
            &[lane("default", NOW + 60_000, NOW + 60_000)],
        );
        assert!(
            w.is_empty(),
            "a future timestamp saturates to age 0, not to u64::MAX"
        );
    }
}
