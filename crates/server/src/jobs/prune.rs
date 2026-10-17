//! PruneTelemetryJob (railwatch-cloud app/jobs/prune_telemetry_job.rb):
//! raw rows past retention, rollups/release_health past 13 months,
//! raw_pruned_through recorded first.

use rusqlite::{Connection, params};

use crate::control::Control;
use crate::time;

/// 13.months, as Active Support counts it from now (calendar months).
pub fn thirteen_months_ago(now_secs: i64) -> i64 {
    let s = time::utc_seconds_string(now_secs);
    let (y, m): (i64, i64) = (s[0..4].parse().unwrap(), s[5..7].parse().unwrap());
    let d: u32 = s[8..10].parse().unwrap();
    let total = y * 12 + (m - 1) - 13;
    let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let days_in =
        |y: i64, m: u32| (time::days_from_civil(if m == 12 { y + 1 } else { y }, if m == 12 { 1 } else { m + 1 }, 1) - time::days_from_civil(y, m, 1)) as u32;
    let nd = d.min(days_in(ny, nm));
    time::days_from_civil(ny, nm, nd) * 86_400 + now_secs.rem_euclid(86_400)
}

pub const RAW: &[&str] = &[
    "executions",
    "queries",
    "exceptions",
    "cache_events",
    "mails",
    "broadcasts",
    "notifications",
    "outgoing_requests",
    "storage_ops",
    "view_renders",
    "logs",
    "enqueued_jobs",
    "transactions",
    "n_plus_ones",
    "deprecations",
    "visits",
    "spans",
    "llm_calls",
    "profiles",
    "attachments",
];
pub const SESSION_HOURS_PER_RUN: usize = 48;
pub const BATCH: i64 = 5_000;

#[derive(Debug, Default)]
pub struct Pruned {
    pub raw: i64,
    pub rollups: usize,
}

pub fn prune(control: &Control, w: &Connection, env: i64) -> Result<Pruned, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let now = time::now_micros();
    let retention: i64 = super::retention_days(&control.conn(), env).map_err(e)?;
    let cutoff_micros = now - retention * 86_400 * 1_000_000;
    // mark_pruned_through!: forwards only, before the deletes.
    control
        .conn()
        .execute(
            "UPDATE environments SET raw_pruned_through = ?1 WHERE id = ?2 AND (raw_pruned_through IS NULL OR raw_pruned_through < ?1)",
            params![time::quoted_date_micros(cutoff_micros), env],
        )
        .map_err(e)?;
    let cutoff = time::quoted_date_micros(cutoff_micros);
    let agg = time::utc_seconds_string(thirteen_months_ago(now / 1_000_000));
    let mut out = Pruned::default();
    // unindex_logs while the messages still exist.
    w.execute("INSERT INTO logs_fts(logs_fts, rowid, message) SELECT 'delete', id, message FROM logs WHERE occurred_at < ?", [&cutoff]).map_err(e)?;
    for t in RAW {
        loop {
            let n = w
                .execute(
                    &format!("DELETE FROM \"{t}\" WHERE id IN (SELECT id FROM \"{t}\" WHERE occurred_at < ? ORDER BY occurred_at LIMIT {BATCH})"),
                    [&cutoff],
                )
                .map_err(e)? as i64;
            out.raw += n;
            if n < BATCH {
                break;
            }
        }
    }
    w.execute("DELETE FROM query_shapes WHERE NOT EXISTS (SELECT 1 FROM queries WHERE queries.group_hash = query_shapes.group_hash)", []).map_err(e)?;
    // prune_sessions: whole hours below the cutoff's hour.
    let cut_secs = cutoff_micros.div_euclid(1_000_000);
    let hour_floor = time::utc_seconds_string(cut_secs - cut_secs.rem_euclid(3600));
    for _ in 0..SESSION_HOURS_PER_RUN {
        let oldest: Option<String> = w.query_row("SELECT MIN(occurred_at) FROM sessions WHERE occurred_at < ?", [&hour_floor], |r| r.get(0)).map_err(e)?;
        let Some(o) = oldest.and_then(|s| time::parse_db_time(&s)) else { break };
        let h = o / 1_000_000;
        let h = h - h.rem_euclid(3600);
        w.execute("DELETE FROM sessions WHERE occurred_at >= ? AND occurred_at < ?", params![time::utc_seconds_string(h), time::utc_seconds_string(h + 3600)])
            .map_err(e)?;
    }
    out.rollups = w.execute("DELETE FROM rollups WHERE bucket < ?", [&agg]).map_err(e)?;
    w.execute("DELETE FROM release_health WHERE bucket < ?", [&agg]).map_err(e)?;
    w.execute("DELETE FROM people WHERE last_seen_at < ?", [&cutoff]).map_err(e)?;
    w.execute("DELETE FROM ingest_batches WHERE received_at < ?", [&cutoff]).map_err(e)?;
    w.execute("DELETE FROM processes WHERE booted_at < ?", [&cutoff]).map_err(e)?;
    w.execute("DELETE FROM health_samples WHERE sampled_at < ?", [&cutoff]).map_err(e)?;
    w.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").map_err(e)?;
    Ok(out)
}

/// The per-day layout's prune: whole files (storage::retention).
/// raw_pruned_through is recorded first, as in `prune`, so the ingest floor
/// rejects rows for the days about to be deleted.
pub fn prune_days(
    control: &Control,
    dir: &std::path::Path,
    meta: &Connection,
    env: i64,
    tiers: crate::storage::retention::Tiers,
) -> Result<crate::storage::retention::Pruned, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let now = time::now_micros();
    let retention: i64 = super::retention_days(&control.conn(), env).map_err(e)?;
    let core_days = retention.clamp(1, tiers.core_days_max);
    let cutoff = now - core_days * 86_400_000_000;
    let cutoff = cutoff - cutoff.rem_euclid(86_400_000_000);
    control
        .conn()
        .execute(
            "UPDATE environments SET raw_pruned_through = ?1 WHERE id = ?2 AND (raw_pruned_through IS NULL OR raw_pruned_through < ?1)",
            params![time::quoted_date_micros(cutoff), env],
        )
        .map_err(e)?;
    let p = crate::storage::retention::prune(dir, meta, tiers, retention, now).map_err(e)?;
    tracing::info!(
        environment = env,
        detail_files = p.detail_files,
        core_files = p.core_files,
        bytes = p.bytes,
        exemplar_executions = p.exemplar_executions,
        exemplar_rows = p.exemplar_rows,
        rollups = p.rollups,
        "pruned day files"
    );
    Ok(p)
}

#[cfg(test)]
mod tests {
    #[test]
    fn thirteen_months_clamps_month_end() {
        // 2026-03-31 12:00 -> 2025-02-28 12:00 (Active Support clamps the day).
        let t = crate::time::days_from_civil(2026, 3, 31) * 86_400 + 43_200;
        assert_eq!(crate::time::utc_seconds_string(super::thirteen_months_ago(t)), "2025-02-28 12:00:00");
    }
}
