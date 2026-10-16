//! Hourly rollups: RollupJob and ReleaseHealthRollupJob, the t-digest they
//! store (the tdigest gem, ported with Ruby's RNG), Telemetry::Rollup's
//! summarize, and the incremental accumulator that folds committed batches
//! in without re-reading the hour.

pub mod incremental;
pub mod random;
pub mod release_health_rollup_job;
pub mod rollup_job;
pub mod sqlval;
pub mod summary;
pub mod tdigest;

/// Tables RollupJob and ReleaseHealthRollupJob read.
pub const ROLLED_TABLES: &[&str] = &[
    "executions", "queries", "outgoing_requests", "cache_events", "mails", "visits", "spans", "notifications", "view_renders",
    "transactions", "llm_calls", "sessions",
];
