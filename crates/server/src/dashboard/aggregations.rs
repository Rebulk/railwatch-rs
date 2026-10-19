//! Port of Railwatch::Telemetry::Aggregations: series (raw tables under an
//! hour, rollups at an hour and up), grouped tables, summary_with_delta and
//! sparklines.
//!
//! Not ported: the one-minute Rails.cache in front of `grouped` and
//! `summary_with_delta`. Rust computes them on every call (they are fast
//! enough; see docs/DIFFERENCES.md), so a Rust page is never up to a minute
//! staler than the rollups.

use rusqlite::Connection;
use serde_json::Value;

use super::db::{self, jf, qt};
use super::rb::{self, Num};
use super::rollup::{self, Rollup};
use super::window::step_seconds;

pub const GROUPED_SORT_FIELDS: [&str; 7] = ["count", "p50", "p95", "p99", "errors", "avg", "max"];

/// RAW: (table, scope condition, error predicate, client error predicate).
pub type RawSource = (&'static str, Option<&'static str>, Option<&'static str>, Option<&'static str>);

pub fn raw_source(record_type: &str) -> Option<RawSource> {
    Some(match record_type {
        "request" => ("executions", Some("\"executions\".\"kind\" = 'request'"), Some("status >= 500"), Some("status BETWEEN 400 AND 499")),
        "job_attempt" => ("executions", Some("\"executions\".\"kind\" = 'job_attempt'"), Some("outcome = 'failed'"), None),
        "scheduled_task" => ("executions", Some("\"executions\".\"kind\" = 'scheduled_task'"), Some("outcome = 'failed'"), None),
        "command" => ("executions", Some("\"executions\".\"kind\" = 'command'"), Some("status != 0"), None),
        "channel_action" => ("executions", Some("\"executions\".\"kind\" = 'channel_action'"), Some("outcome = 'failed'"), None),
        "query" => ("queries", None, None, None),
        "outgoing_request" => ("outgoing_requests", None, Some("status_code >= 500 OR status_code IS NULL OR status_code = 0"), None),
        "cache_event" => ("cache_events", None, None, None),
        "mail" => ("mails", None, Some("failed"), None),
        "visit" => ("visits", None, Some("status = 'error'"), None),
        "span" => ("spans", None, Some("status = 'failed'"), None),
        "notification" => ("notifications", None, Some("failed"), None),
        "view_render" => ("view_renders", None, None, None),
        "transaction" => ("transactions", None, Some("outcome = 'rollback'"), None),
        "llm_call" => ("llm_calls", Some("\"llm_calls\".\"operation\" != 'tool'"), Some("status = 'failed'"), None),
        "llm_tool" => ("llm_calls", Some("\"llm_calls\".\"operation\" = 'tool'"), Some("status = 'failed'"), None),
        _ => return None,
    })
}

fn flag_sql(p: Option<&str>) -> String {
    p.map_or("0".to_string(), |p| format!("CASE WHEN {p} THEN 1 ELSE 0 END"))
}

pub fn bucket_sql(column: &str, step_secs: i64) -> String {
    format!("(strftime('%s', {column}) / {step_secs}) * {step_secs}")
}

/// One series point.
#[derive(Debug, Clone)]
pub struct Point {
    pub t: i64, // epoch seconds
    pub json: Value,
}

#[allow(clippy::too_many_arguments)]
fn point(b: i64, count: i64, errors: i64, client_errors: i64, duration_sum: i64, p50: i64, p95: i64, p99: i64) -> Point {
    let avg = if count == 0 { Value::from(0) } else { jf(duration_sum.div_euclid(count) as f64 / 1000.0) };
    Point {
        t: b,
        json: crate::obj! {
            "t" => rb::t_json(b * 1_000_000), "count" => count, "errors" => errors, "client_errors" => client_errors,
            "avg" => avg, "p50" => jf(p50 as f64 / 1000.0), "p95" => jf(p95 as f64 / 1000.0), "p99" => jf(p99 as f64 / 1000.0),
        },
    }
}

pub fn empty_point(t: i64) -> Value {
    crate::obj! {
        "t" => rb::t_json(t * 1_000_000), "count" => 0, "errors" => 0, "client_errors" => 0,
        "avg" => Value::Null, "p50" => Value::Null, "p95" => Value::Null, "p99" => Value::Null,
    }
}

/// `Aggregations.fill`: one point per step from `from` to `to`.
pub fn fill(points: Vec<Point>, from: i64, to: i64, step_secs: i64, empty: impl Fn(i64) -> Value) -> Vec<Value> {
    let mut by: std::collections::HashMap<i64, Value> = points.into_iter().map(|p| (p.t, p.json)).collect();
    let first = rb::to_i(from).div_euclid(step_secs) * step_secs;
    let last = rb::to_i(to).div_euclid(step_secs) * step_secs;
    let mut out = Vec::new();
    let mut t = first;
    while t <= last {
        out.push(by.remove(&t).unwrap_or_else(|| empty(t)));
        t += step_secs;
    }
    out
}

/// `Aggregations.points`.
pub fn points(
    conn: &Connection,
    record_type: &str,
    from: i64,
    to: i64,
    group_hash: Option<&str>,
    step: &str,
    tenant: Option<&str>,
) -> rusqlite::Result<Vec<Value>> {
    let step_secs = step_seconds(step).expect("known step");
    let raw = step_secs < 3600 || tenant.is_some();
    let pts = if raw {
        raw_series(conn, record_type, from, to, group_hash, step_secs, tenant)?
    } else {
        rollup_series(conn, record_type, from, to, group_hash, step_secs)?
    };
    Ok(fill(pts, from, to, step_secs, empty_point))
}

fn rollup_series(conn: &Connection, record_type: &str, from: i64, to: i64, group_hash: Option<&str>, step: i64) -> rusqlite::Result<Vec<Point>> {
    let b = bucket_sql("bucket", step);
    let gh = if group_hash.is_some() { " AND \"rollups\".\"group_hash\" = ?4" } else { "" };
    let sql = format!(
        "SELECT {b}, SUM(count), SUM(error_count), SUM(client_error_count), SUM(duration_sum), MAX(p50), MAX(p95), MAX(p99) \
         FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ?1 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3{gh} GROUP BY {b} ORDER BY {b}"
    );
    let lo = qt(rb::beginning_of_hour(from));
    let hi = qt(to);
    let rows = match group_hash {
        Some(g) => db::query(conn, &sql, rusqlite::params![record_type, lo, hi, g])?,
        None => db::query(conn, &sql, rusqlite::params![record_type, lo, hi])?,
    };
    Ok(rows.iter().map(|r| point(ri(r, 0), ri(r, 1), ri(r, 2), ri(r, 3), ri(r, 4), ri(r, 5), ri(r, 6), ri(r, 7))).collect())
}

fn ri(r: &db::Rec, i: usize) -> i64 {
    match r.at(i) {
        rusqlite::types::Value::Integer(v) => *v,
        rusqlite::types::Value::Real(f) => *f as i64,
        rusqlite::types::Value::Text(t) => t.parse().unwrap_or(0),
        _ => 0,
    }
}

fn raw_series(
    conn: &Connection,
    record_type: &str,
    from: i64,
    to: i64,
    group_hash: Option<&str>,
    step: i64,
    tenant: Option<&str>,
) -> rusqlite::Result<Vec<Point>> {
    let (table, scope, err, cerr) = raw_source(record_type).expect("known record type");
    let mut conds = vec![];
    if let Some(s) = scope {
        conds.push(s.to_string());
    }
    conds.push(format!("\"{table}\".\"occurred_at\" BETWEEN ?1 AND ?2"));
    let mut binds: Vec<String> = vec![qt(from), qt(to)];
    if let Some(g) = group_hash {
        binds.push(g.to_string());
        conds.push(format!("\"{table}\".\"group_hash\" = ?{}", binds.len()));
    }
    if let Some(t) = tenant {
        binds.push(t.to_string());
        conds.push(format!("\"{table}\".\"app_tenant\" = ?{}", binds.len()));
    }
    let sql = format!(
        "SELECT b, COUNT(*), SUM(err), SUM(cerr), SUM(duration), \
         MAX(CASE WHEN rn = (n * 50 + 99) / 100 THEN duration END), \
         MAX(CASE WHEN rn = (n * 95 + 99) / 100 THEN duration END), \
         MAX(CASE WHEN rn = (n * 99 + 99) / 100 THEN duration END) \
         FROM (SELECT b, duration, err, cerr, ROW_NUMBER() OVER (PARTITION BY b ORDER BY duration) AS rn, COUNT(*) OVER (PARTITION BY b) AS n \
         FROM (SELECT {bucket} AS b, duration, {e} AS err, {ce} AS cerr FROM \"{table}\" WHERE {where}) r) w GROUP BY b ORDER BY b",
        bucket = bucket_sql("occurred_at", step),
        e = flag_sql(err),
        ce = flag_sql(cerr),
        where = conds.join(" AND "),
    );
    let rows = db::query(conn, &sql, rusqlite::params_from_iter(binds.iter()))?;
    Ok(rows.iter().map(|r| point(ri(r, 0), ri(r, 1), ri(r, 2), ri(r, 3), ri(r, 4), ri(r, 5), ri(r, 6), ri(r, 7))).collect())
}

// -- grouped -------------------------------------------------------------

/// `Aggregations.grouped`.
pub fn grouped(conn: &Connection, record_type: &str, from: i64, to: i64, limit: usize, order: Option<&str>, dir: Option<&str>) -> rusqlite::Result<Vec<Value>> {
    let rows = rollup::between(conn, record_type, from, to)?;
    Ok(grouped_rows(&rows, from, to, limit, order, dir))
}

pub fn grouped_rows(rows: &[Rollup], from: i64, to: i64, limit: usize, order: Option<&str>, dir: Option<&str>) -> Vec<Value> {
    let order = order.filter(|o| GROUPED_SORT_FIELDS.contains(o)).unwrap_or("count");
    let sign: i64 = if dir == Some("asc") { 1 } else { -1 };
    let cheap = matches!(order, "count" | "errors" | "avg" | "max");
    let groups = rollup::group_by_hash(rows);
    let ranked: Vec<(String, Vec<&Rollup>, rollup::Summary)> = groups
        .into_iter()
        .map(|(g, rs)| {
            let s = if cheap { rollup::cheap_summary(&rs) } else { rollup::summarize(&rs) };
            (g, rs, s)
        })
        .collect();
    let ranked = rb::sort_by_num(ranked, |(_, _, s)| Num::I(s.get(order) * sign));
    ranked
        .into_iter()
        .take(limit)
        .map(|(g, rs, s)| {
            let s = if cheap { rollup::summarize(&rs) } else { s };
            crate::obj! {
                "group_hash" => g, "name" => db::js(rollup::latest_name(&rs).as_deref()),
                "count" => s.count, "errors" => s.errors, "client_errors" => s.client_errors,
                "avg" => jf(rb::round(s.avg as f64 / 1000.0, 2)), "p50" => jf(rb::round(s.p50 as f64 / 1000.0, 2)),
                "p95" => jf(rb::round(s.p95 as f64 / 1000.0, 2)), "p99" => jf(rb::round(s.p99 as f64 / 1000.0, 2)),
                "max" => jf(rb::round(s.max as f64 / 1000.0, 2)), "sparkline" => sparkline(&rs, from, to),
            }
        })
        .collect()
}

/// `Aggregations.sparkline`: hourly counts, coarsened to at most 30 points.
pub fn sparkline(rows: &[&Rollup], from: i64, to: i64) -> Value {
    let pts = sparkline_counts(rows.iter().map(|r| (r.bucket, r.count)), from, to);
    Value::from(pts)
}

/// The same bucketing for any (time, count) pairs (deprecations' raw rows).
pub fn sparkline_counts(items: impl Iterator<Item = (i64, i64)>, from: i64, to: i64) -> Vec<i64> {
    let span = rb::time_sub(to, from);
    let hours = ((span / 3600.0).ceil() as i64).max(1);
    let coarsen = (hours as f64 / 30.0).ceil() as i64;
    let base = rb::beginning_of_hour(from);
    let bucket_count = (hours as f64 / coarsen as f64).ceil() as i64;
    let mut buckets = vec![0i64; bucket_count.max(0) as usize];
    for (at, count) in items {
        // ((t - base) / 1.hour).to_i / coarsen: Float truncated, then Integer floor division.
        let idx = ((rb::time_sub(at, base) / 3600.0) as i64).div_euclid(coarsen);
        if idx >= 0 && (idx as usize) < buckets.len() {
            buckets[idx as usize] += count;
        }
    }
    buckets
}

/// `Aggregations.summary_with_delta`.
pub fn summary_with_delta(conn: &Connection, record_type: &str, window: (i64, i64), previous: (i64, i64), group_hash: Option<&str>) -> rusqlite::Result<Value> {
    let load = |(f, t): (i64, i64)| match group_hash {
        Some(g) => rollup::between_group(conn, record_type, f, t, g),
        None => rollup::between(conn, record_type, f, t),
    };
    let current = rollup::summarize_owned(&load(window)?);
    let prev = rollup::summarize_owned(&load(previous)?);
    Ok(crate::obj! { "current" => current.to_json(), "previous" => prev.to_json() })
}

/// `summary_with_delta` over all groups, given the current window's rows
/// (`rollup::between(record_type, window)`) a page already read for its
/// grouped table: the same statement, so the same rows in the same order.
pub fn summary_with_delta_of(conn: &Connection, record_type: &str, current: &[Rollup], previous: (i64, i64)) -> rusqlite::Result<Value> {
    let prev = rollup::summarize_owned(&rollup::between(conn, record_type, previous.0, previous.1)?);
    Ok(crate::obj! { "current" => rollup::summarize_owned(current).to_json(), "previous" => prev.to_json() })
}
