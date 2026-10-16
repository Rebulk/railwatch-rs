//! DrainIngestFollowupsJob + Ingest::Followups.drain!/collect! +
//! Telemetry::IngestReceipt.prune! (railwatch-cloud app/jobs/
//! drain_ingest_followups_job.rb, app/models/ingest/followups.rb).
//!
//! Per receipt still owing work, in (committed_at, id) order, pages of 200:
//! - grouping: Ingest::ExceptionGrouping with a claim per group, claim and
//!   effect in one control transaction (exactly once across replays/crashes);
//! - rollup/session buckets: recomputed only for receipts committed before
//!   this process started. A receipt committed by this process had its rows
//!   folded into the incremental accumulator on commit; one from before a
//!   crash may not have, so its hours are recomputed (idempotent). Within a
//!   pass each hour is recomputed once (Rails' `recomputed` set);
//! - usage: counted once at commit by the follow-ups (docs/DIFFERENCES.md);
//!   the drain does not count it again;
//! - then the receipt's followups are cleared.
//!
//! Afterwards the replay floor moves (collect!), follow-up receipts below it
//! are pruned (500), and finished ingest receipts past retain_until (200).

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Instant;

use rusqlite::{Connection, params};

use crate::control::Control;
use crate::grouping::{self, Receipt};
use crate::rjson::{self, Value};
use crate::rollup::{release_health_rollup_job, rollup_job};
use crate::time;

pub const PAGE: i64 = 200;
pub const MAX_PAGES: usize = 50;
pub const TIME_BUDGET_SECS: u64 = 45;
/// Ingest::Followups::HORIZON
pub const HORIZON_MICROS: i64 = 8 * 86_400 * 1_000_000;

/// When this process started, in epoch microseconds.
pub fn process_started() -> i64 {
    static STARTED: OnceLock<i64> = OnceLock::new();
    *STARTED.get_or_init(time::now_micros)
}

#[derive(Debug, Default, PartialEq)]
pub struct Drained {
    pub receipts: usize,
    pub failed: usize,
    pub recomputed_hours: usize,
    pub gc_moved: bool,
    pub followup_receipts_pruned: usize,
    pub ingest_receipts_pruned: usize,
}

struct Pending {
    id: i64,
    producer_id: String,
    delivery_id: String,
    delivery_created_at: i64,
    committed_at: i64,
    followups: Value,
}

fn pending(read: &Connection, tried: &HashSet<i64>) -> rusqlite::Result<Vec<Pending>> {
    let not_in = if tried.is_empty() { String::new() } else {
        format!(" AND id NOT IN ({})", tried.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(","))
    };
    let sql = format!(
        "SELECT id, producer_id, delivery_id, delivery_created_at, committed_at, followups FROM ingest_receipts WHERE followups IS NOT NULL{not_in} ORDER BY committed_at, id LIMIT {PAGE}"
    );
    let mut s = read.prepare(&sql)?;
    let rows = s.query_map([], |r| {
        let d: String = r.get(3)?;
        let c: String = r.get(4)?;
        let f: String = r.get(5)?;
        Ok(Pending {
            id: r.get(0)?,
            producer_id: r.get(1)?,
            delivery_id: r.get(2)?,
            delivery_created_at: time::parse_db_time(&d).unwrap_or(0),
            committed_at: time::parse_db_time(&c).unwrap_or(0),
            followups: rjson::parse(f.as_bytes()).unwrap_or(Value::Nil),
        })
    })?;
    rows.collect()
}

fn ints(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| if let Value::Int(i) = x { Some(*i) } else { None }).collect(),
        _ => vec![],
    }
}

fn buckets(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|x| if let Value::Str(s) = x { time::parse_db_time(&String::from_utf8_lossy(s)).map(|m| m / 1_000_000) } else { None })
            .collect(),
        _ => vec![],
    }
}

pub fn drain(control: &Control, (read, write): (&Connection, &Connection), env: i64, users: &mut grouping::AffectedUsers) -> Result<Drained, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let deadline = Instant::now() + std::time::Duration::from_secs(TIME_BUDGET_SECS);
    let mut out = Drained::default();
    let mut tried = HashSet::new();
    let mut recomputed: HashSet<(&'static str, i64)> = HashSet::new();
    let retention_days: i64 = super::retention_days(&control.conn(), env).map_err(e)?;
    for _ in 0..MAX_PAGES {
        let page = pending(read, &tried).map_err(e)?;
        if page.is_empty() {
            break;
        }
        for p in page {
            tried.insert(p.id);
            match drain_one(control, read, write, env, &p, &mut recomputed, retention_days, users, &mut out) {
                Ok(()) => out.receipts += 1,
                Err(err) => {
                    // One receipt that cannot drain must not block the rest;
                    // it stays pending and is retried next pass.
                    out.failed += 1;
                    tracing::error!(environment = env, delivery_id = p.delivery_id, error = %err, "follow-up drain failed");
                }
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    collect(control, read, env, time::now_micros(), &mut out).map_err(e)?;
    out.ingest_receipts_pruned = write
        .execute(
            "DELETE FROM ingest_receipts WHERE id IN (SELECT id FROM ingest_receipts WHERE followups IS NULL AND retain_until < ? LIMIT 200)",
            [time::quoted_date_micros(time::now_micros())],
        )
        .map_err(e)?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn drain_one(
    control: &Control,
    read: &Connection,
    write: &Connection,
    env: i64,
    p: &Pending,
    recomputed: &mut HashSet<(&'static str, i64)>,
    retention_days: i64,
    users: &mut grouping::AffectedUsers,
    out: &mut Drained,
) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    let ids = ints(p.followups.get("group_exception_ids"));
    if !ids.is_empty() {
        let rows = grouping::load_exceptions(read, &ids).map_err(e)?;
        if rows.len() < ids.len() {
            tracing::error!(environment = env, delivery_id = p.delivery_id, lost = ids.len() - rows.len(), "exceptions were pruned before grouping");
        }
        let rows: Vec<_> = rows.into_iter().filter(|r| r.group_hash.as_deref().is_some_and(|g| !g.is_empty())).collect();
        if !rows.is_empty() {
            let receipt = Receipt { producer_id: p.producer_id.clone(), delivery_id: p.delivery_id.clone(), delivery_created_at: p.delivery_created_at };
            let conn = control.conn();
            grouping::group_with(&conn, read, env, rows, Some(&receipt), users)?;
        }
    }
    // Rails' drain could skip an hour: a receipt committed after that hour's
    // recompute had read the rows, but within the same pass, found the hour
    // already in `recomputed`. Not here: only receipts committed before
    // process_started() are recomputed, and process_started() is fixed no
    // later than the first drain, so every such receipt's rows were
    // committed before any pass read them; one recompute per hour sees them
    // all. A receipt committed after that (during a pass or not) was written
    // by this process, whose commit folded its rows into the accumulator
    // (tests: a_late_row_for_an_old_rolled_up_hour_is_counted,
    // the_drain_recomputes_a_pre_start_hour_once_and_counts_every_receipts_rows).
    if p.committed_at < process_started() {
        let now = time::now_micros() / 1_000_000;
        let floor = now - retention_days * 86_400;
        let floor = floor - floor.rem_euclid(3600);
        for b in buckets(p.followups.get("rollup_buckets")) {
            if recomputed.insert(("rollup_buckets", b)) {
                rollup_job::perform(read, write, b, &mut rollup_job::Rng::PerGroup).map_err(e)?;
                out.recomputed_hours += 1;
            }
        }
        for b in buckets(p.followups.get("session_buckets")) {
            if recomputed.insert(("session_buckets", b)) {
                release_health_rollup_job::perform(read, write, b, floor).map_err(e)?;
                out.recomputed_hours += 1;
            }
        }
    }
    write
        .execute(
            "UPDATE ingest_receipts SET followups = NULL, followups_completed_at = ? WHERE id = ?",
            params![time::quoted_date_micros(time::now_micros()), p.id],
        )
        .map_err(e)?;
    Ok(())
}

/// Ingest::Followups.collect!: move the floor up to min(now - HORIZON,
/// oldest pending delivery), never backwards, then prune claims below it.
fn collect(control: &Control, read: &Connection, env: i64, now: i64, out: &mut Drained) -> rusqlite::Result<()> {
    let oldest: Option<String> = read.query_row("SELECT MIN(delivery_created_at) FROM ingest_receipts WHERE followups IS NOT NULL", [], |r| r.get(0))?;
    let oldest = oldest.and_then(|s| time::parse_db_time(&s));
    let candidate = match oldest {
        Some(o) => o.min(now - HORIZON_MICROS),
        None => now - HORIZON_MICROS,
    };
    let conn = control.conn();
    // datetime(6) column, as Rails stores it; NULL is "never collected".
    let candidate_db = time::quoted_date_micros(candidate);
    let moved = conn.execute(
        "UPDATE environments SET ingest_followup_gc_before = ?1 WHERE id = ?2 AND (ingest_followup_gc_before IS NULL OR ingest_followup_gc_before < ?1)",
        params![candidate_db, env],
    )?;
    out.gc_moved = moved > 0;
    if moved > 0 {
        out.followup_receipts_pruned = grouping::prune_receipts(&conn, env, candidate, 500)?;
    }
    Ok(())
}
