//! Tiered retention for a per-day tenant: whole files go, nothing is
//! DELETEd row by row (except in the small meta file).
//!
//! | tier   | holds                                         | default |
//! |--------|-----------------------------------------------|---------|
//! | detail | queries, cache events, views, logs, spans     | 7 days  |
//! | core   | executions, exceptions, the rest; exemplars   | 30 days (the account's retention_days, capped at 90) |
//! | meta   | rollups, release_health                       | 13 months (PruneTelemetryJob) |
//!
//! A day is past a tier when its *end* is older than now minus the tier's
//! length, so a day file is only deleted once every row in it is expired
//! (Rails' row-level cutoff deletes the older part of a day earlier; the
//! cutoff recorded in raw_pruned_through is the day boundary). Detail days
//! get their exemplars copied into the core file first (exemplars.rs).

use std::path::Path;

use rusqlite::Connection;

use super::exemplars;
use super::layout::{self, Tier};

/// Tier lengths (Config: RAILWATCH_DETAIL_RETENTION_DAYS,
/// RAILWATCH_CORE_RETENTION_DAYS_MAX, RAILWATCH_EXEMPLARS_PER_HOUR).
#[derive(Debug, Clone, Copy)]
pub struct Tiers {
    pub detail_days: i64,
    /// The core tier follows the account's retention_days up to this cap.
    pub core_days_max: i64,
    /// Slowest executions kept per (kind, group, hour); errored ones always.
    pub exemplars_per_hour: usize,
}

impl Default for Tiers {
    fn default() -> Self {
        Tiers { detail_days: 7, core_days_max: 90, exemplars_per_hour: 3 }
    }
}

#[derive(Debug, Default)]
pub struct Pruned {
    pub detail_files: usize,
    pub core_files: usize,
    pub bytes: u64,
    pub exemplar_executions: i64,
    pub exemplar_rows: i64,
    pub rollups: usize,
    pub people: usize,
    /// The day boundary everything before which is gone (raw_pruned_through).
    pub core_cutoff_micros: i64,
}

/// Days whose whole span ends at or before `cutoff_micros`.
fn expired(dir: &Path, tier: Tier, cutoff_micros: i64) -> Vec<String> {
    layout::days(dir, tier)
        .into_iter()
        .filter(|d| crate::time::parse_db_time(&format!("{d} 00:00:00")).is_some_and(|start| start + 86_400_000_000 <= cutoff_micros))
        .collect()
}

/// The day boundary at or below `micros`.
fn floor_day(micros: i64) -> i64 {
    micros - micros.rem_euclid(86_400_000_000)
}

/// Deletes expired day files and trims meta. `meta` is any write
/// connection to meta.sqlite3. Files are unlinked while holding meta's
/// write lock: every ingest transaction holds it from BEGIN to COMMIT
/// (writer.rs), so no batch can be writing into a file as it goes, and the
/// writer reopens a cached file that is gone before its next transaction.
pub fn prune(dir: &Path, meta: &Connection, tiers: Tiers, retention_days: i64, now_micros: i64) -> rusqlite::Result<Pruned> {
    let mut out = Pruned::default();
    let core_days = retention_days.clamp(1, tiers.core_days_max);
    let detail_days = tiers.detail_days.clamp(1, core_days);
    let detail_cutoff = floor_day(now_micros - detail_days * 86_400_000_000);
    let core_cutoff = floor_day(now_micros - core_days * 86_400_000_000);
    out.core_cutoff_micros = core_cutoff;

    let core_expired = expired(dir, Tier::Core, core_cutoff);
    let detail_expired: Vec<String> = expired(dir, Tier::Detail, detail_cutoff).into_iter().filter(|d| !core_expired.contains(d)).collect();
    // Exemplars first, outside meta's lock (each copy is its own
    // transaction on the core file, idempotent by its marker).
    for day in &detail_expired {
        let c = exemplars::copy(dir, day, tiers.exemplars_per_hour)?;
        out.exemplar_executions += c.executions;
        out.exemplar_rows += c.rows;
    }

    let agg = crate::time::utc_seconds_string(crate::jobs::prune::thirteen_months_ago(now_micros / 1_000_000));
    let cutoff = crate::time::quoted_date_micros(core_cutoff);
    meta.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> rusqlite::Result<()> {
        for day in &detail_expired {
            out.bytes += remove(&layout::day_path(dir, Tier::Detail, day));
            out.detail_files += 1;
        }
        for day in &core_expired {
            // A detail day past core retention needs no exemplars.
            out.bytes += remove(&layout::day_path(dir, Tier::Detail, day));
            out.bytes += remove(&layout::day_path(dir, Tier::Core, day));
            out.core_files += 1;
        }
        // PruneTelemetryJob's aggregate cutoff (13 months), and the people
        // not seen within core retention. Small tables; plain DELETEs.
        // query_shapes are kept (docs/DIFFERENCES.md, storage #5).
        out.rollups = meta.execute("DELETE FROM rollups WHERE bucket < ?", [&agg])?;
        meta.execute("DELETE FROM release_health WHERE bucket < ?", [&agg])?;
        out.people = meta.execute("DELETE FROM people WHERE last_seen_at < ?", [&cutoff])?;
        Ok(())
    })();
    match r {
        Ok(()) => meta.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = meta.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(out)
}

/// `remove` for other modules' tests.
#[cfg(test)]
pub fn remove_for_test(path: &Path) -> u64 {
    remove(path)
}

pub(super) fn remove(path: &Path) -> u64 {
    let mut freed = 0;
    for suffix in ["", "-wal", "-shm"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if let Ok(m) = std::fs::metadata(&p) {
            freed += m.len();
        }
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::error!(path = %p.display(), error = %e, "could not delete expired day file"),
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::layout::{day_path, meta_path, open_or_create};

    #[test]
    fn drops_detail_after_7_days_and_core_after_retention() {
        let dir = std::env::temp_dir().join(format!("rw-retention-{}", crate::control::base58(8)));
        let meta = open_or_create(&meta_path(&dir)).unwrap();
        let now = crate::time::parse_db_time("2026-10-14 12:00:00").unwrap();
        for day in ["2026-09-10", "2026-09-14", "2026-09-15", "2026-10-06", "2026-10-07", "2026-10-14"] {
            let c = open_or_create(&day_path(&dir, Tier::Core, day)).unwrap();
            c.execute(
                "INSERT INTO executions (execution_id, kind, name, group_hash, status, occurred_at, duration) VALUES (?, 'request', 'GET /', 'g', 200, ?, 1.0)",
                rusqlite::params![format!("e{day}"), format!("{day} 10:00:00")],
            )
            .unwrap();
            let d = open_or_create(&day_path(&dir, Tier::Detail, day)).unwrap();
            d.execute(
                "INSERT INTO queries (execution_id, sql, occurred_at, duration) VALUES (?, 'x', ?, 1.0)",
                rusqlite::params![format!("e{day}"), format!("{day} 10:00:00")],
            )
            .unwrap();
        }
        meta.execute("INSERT INTO rollups (record_type, group_hash, bucket, name, count) VALUES ('request', 'g', '2025-08-01 00:00:00', 'x', 1), ('request', 'g', '2026-10-01 00:00:00', 'x', 1)", [])
            .unwrap();
        let p = prune(&dir, &meta, Tiers::default(), 30, now).unwrap();
        // core: 30 days before 2026-10-14 12:00 floors to 2026-09-14 00:00;
        // days ending at or before it go: only 09-10 (both its files).
        assert_eq!(layout::days(&dir, Tier::Core), vec!["2026-09-14", "2026-09-15", "2026-10-06", "2026-10-07", "2026-10-14"]);
        // detail: cutoff 2026-10-07 00:00 -> 10-06 and older go.
        assert_eq!(layout::days(&dir, Tier::Detail), vec!["2026-10-07", "2026-10-14"]);
        assert_eq!(p.core_files, 1);
        assert_eq!(p.detail_files, 3);
        // Each expired detail day's one execution is its group's slowest.
        assert_eq!(p.exemplar_executions, 3);
        assert_eq!(p.rollups, 1);
        let kept: i64 =
            Connection::open(day_path(&dir, Tier::Core, "2026-10-06")).unwrap().query_row("SELECT COUNT(*) FROM queries", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
