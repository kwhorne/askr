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
