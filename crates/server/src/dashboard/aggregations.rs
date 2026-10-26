//! Port of Railwatch::Telemetry::Aggregations: series (raw tables under an
//! hour, rollups at an hour and up), grouped tables, summary_with_delta and
//! sparklines.
//!
//! Not ported: the one-minute Rails.cache in front of `grouped` and
//! `summary_with_delta`. Rust computes them on every call (they are fast
//! enough; see docs/DIFFERENCES.md), so a Rust page is never up to a minute
//! staler than the rollups.

use std::collections::HashMap;

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
    // Each hour first, then the hours into steps: the rows come in bucket
    // order (the index), so the inner GROUP BY needs no sort and the outer
    // one sorts one row per hour instead of every row (30 days of a busy
    // tenant's queries: 440k rows, 217 -> 83 ms). Sums of sums and maxes of
    // maxes of integer columns are the same integers.
    let sql = format!(
        "SELECT {b}, SUM(c), SUM(e), SUM(ce), SUM(ds), MAX(m50), MAX(m95), MAX(m99) FROM (SELECT bucket, SUM(count) AS c, SUM(error_count) AS e, \
         SUM(client_error_count) AS ce, SUM(duration_sum) AS ds, MAX(p50) AS m50, MAX(p95) AS m95, MAX(p99) AS m99 \
         FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ?1 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3{gh} GROUP BY bucket) GROUP BY {b} ORDER BY {b}"
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
    Ok(Groups::between(conn, record_type, from, to, false)?.rows(limit, order, dir))
}

/// The window's rollups folded by group in one pass over the statement,
/// for `grouped` and the window's summary, instead of a `Rollup` per row: a
/// 30-day queries page on a busy tenant reads ~440k rows of ~12k groups, and
/// building every one (name, digest and extra, then grouping them) was most
/// of the page. Each group keeps its sums, its digests as bytes, its
/// sparkline and its latest bucket's name. (Reading the shown groups'
/// digests in a second statement was slower: their rows are spread over the
/// whole file.)
///
/// The statement has `rollup::between`'s WHERE (only the column list
/// differs, and no index covers either), so it has the same plan and row
/// order: groups keep its order of first appearance, which is where
/// `sort_by`'s ties come from.
pub struct Groups {
    groups: Vec<Group>,
    /// Every row's numbers and extras in row order, when asked for: the
    /// window's `summarize` (its percentiles are over the groups' digests).
    total: Option<rollup::Totals>,
}

struct Group {
    hash: String,
    totals: rollup::Totals,
    /// `max_by(&:bucket)`'s bucket and name (as stored): the first row with
    /// the latest bucket.
    latest: (i64, Option<Vec<u8>>),
    sparkline: Vec<i64>,
}

impl Groups {
    /// `total`: also sum every row, for `summary`.
    pub fn between(conn: &Connection, record_type: &str, from: i64, to: i64, total: bool) -> rusqlite::Result<Groups> {
        let extra = if total { r#", "rollups"."extra""# } else { "" };
        let sql = rollup::BETWEEN.replace(
            r#""rollups".*"#,
            &format!(
                r#""rollups"."group_hash", "rollups"."name", "rollups"."bucket", "rollups"."count", "rollups"."error_count", "rollups"."client_error_count", "rollups"."duration_sum", "rollups"."duration_max", "rollups"."digest"{extra}"#
            ),
        );
        let spark = Sparkline::new(from, to);
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        let mut sum = total.then(rollup::Totals::default);
        // A bucket's rows come together: place each bucket once.
        let mut last_bucket = (i64::MIN, None);
        rollup::each(conn, &sql, rusqlite::params![record_type, qt(rb::beginning_of_hour(from)), qt(to)], |r| {
            let i = match index.get(r.group_hash.as_ref()) {
                Some(&i) => i,
                None => {
                    index.insert(r.group_hash.to_string(), groups.len());
                    let latest = (r.bucket, r.name_bytes().map(<[u8]>::to_vec));
                    groups.push(Group { hash: r.group_hash.to_string(), totals: rollup::Totals::default(), latest, sparkline: vec![0; spark.len] });
                    groups.len() - 1
                }
            };
            let g = &mut groups[i];
            g.totals.add(r);
            // A group's rows come by bucket, nearly always with the same
            // name: copy only a different one.
            if r.bucket > g.latest.0 {
                g.latest.0 = r.bucket;
                if g.latest.1.as_deref() != r.name_bytes() {
                    g.latest.1 = r.name_bytes().map(<[u8]>::to_vec);
                }
            }
            if last_bucket.0 != r.bucket {
                last_bucket = (r.bucket, spark.index(r.bucket));
            }
            if let Some(at) = last_bucket.1 {
                g.sparkline[at] += r.count;
            }
            if let Some(t) = &mut sum {
                t.add_numbers(r);
                if let Some(e) = r.extra() {
                    t.merge_extra(&e);
                }
            }
        })?;
        Ok(Groups { groups, total: sum })
    }

    /// `summarize` of every row (`Groups::between(.., true)`).
    pub fn summary(&self) -> rollup::Summary {
        let total = self.total.as_ref().expect("Groups::between with total");
        total.summary_over(self.groups.iter().flat_map(|g| g.totals.digests()))
    }

    /// `grouped_rows` over the same rows. A group's summary from its folded
    /// totals is `summarize` of its rows: sums, a max, and nearest-rank
    /// percentiles of its digests in row order.
    pub fn rows(&self, limit: usize, order: Option<&str>, dir: Option<&str>) -> Vec<Value> {
        let order = order.filter(|o| GROUPED_SORT_FIELDS.contains(o)).unwrap_or("count");
        let sign: i64 = if dir == Some("asc") { 1 } else { -1 };
        let cheap = matches!(order, "count" | "errors" | "avg" | "max");
        let ranked: Vec<(&Group, rollup::Summary)> = self.groups.iter().map(|g| (g, if cheap { g.totals.cheap() } else { g.totals.summary() })).collect();
        rb::sort_by_num(ranked, |(_, s)| Num::I(s.get(order) * sign))
            .into_iter()
            .take(limit)
            .map(|(g, s)| {
                let s = if cheap { g.totals.summary() } else { s };
                let name = g.latest.1.as_deref().map(String::from_utf8_lossy);
                grouped_row(&g.hash, name.as_deref(), &s, Value::from(g.sparkline.clone()))
            })
            .collect()
    }
}

fn grouped_row(group_hash: &str, name: Option<&str>, s: &rollup::Summary, sparkline: Value) -> Value {
    crate::obj! {
        "group_hash" => group_hash, "name" => db::js(name),
        "count" => s.count, "errors" => s.errors, "client_errors" => s.client_errors,
        "avg" => jf(rb::round(s.avg as f64 / 1000.0, 2)), "p50" => jf(rb::round(s.p50 as f64 / 1000.0, 2)),
        "p95" => jf(rb::round(s.p95 as f64 / 1000.0, 2)), "p99" => jf(rb::round(s.p99 as f64 / 1000.0, 2)),
        "max" => jf(rb::round(s.max as f64 / 1000.0, 2)), "sparkline" => sparkline,
    }
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
            grouped_row(&g, rollup::latest_name(&rs).as_deref(), &s, sparkline(&rs, from, to))
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
    let s = Sparkline::new(from, to);
    let mut buckets = vec![0i64; s.len];
    for (at, count) in items {
        if let Some(i) = s.index(at) {
            buckets[i] += count;
        }
    }
    buckets
}

/// `sparkline`'s buckets over [from, to].
struct Sparkline {
    base: i64,
    coarsen: i64,
    len: usize,
}

impl Sparkline {
    fn new(from: i64, to: i64) -> Sparkline {
        let span = rb::time_sub(to, from);
        let hours = ((span / 3600.0).ceil() as i64).max(1);
        let coarsen = (hours as f64 / 30.0).ceil() as i64;
        let bucket_count = (hours as f64 / coarsen as f64).ceil() as i64;
        Sparkline { base: rb::beginning_of_hour(from), coarsen, len: bucket_count.max(0) as usize }
    }

    /// The bucket a time counts in, if any.
    fn index(&self, at: i64) -> Option<usize> {
        // ((t - base) / 1.hour).to_i / coarsen: Float truncated, then Integer floor division.
        let idx = ((rb::time_sub(at, self.base) / 3600.0) as i64).div_euclid(self.coarsen);
        (idx >= 0 && (idx as usize) < self.len).then_some(idx as usize)
    }
}

/// `Aggregations.summary_with_delta`.
pub fn summary_with_delta(conn: &Connection, record_type: &str, window: (i64, i64), previous: (i64, i64), group_hash: Option<&str>) -> rusqlite::Result<Value> {
    let summary = |(f, t): (i64, i64)| match group_hash {
        Some(g) => rollup::summary_between_group(conn, record_type, f, t, g),
        None => rollup::summary_between(conn, record_type, f, t),
    };
    let current = summary(window)?;
    let prev = summary(previous)?;
    Ok(crate::obj! { "current" => current.to_json(), "previous" => prev.to_json() })
}

/// `summary_with_delta` over all groups, given the current window's
/// summary a page already computed with its grouped table (`Groups::summary`).
pub fn summary_with_delta_of(conn: &Connection, record_type: &str, current: &rollup::Summary, previous: (i64, i64)) -> rusqlite::Result<Value> {
    let prev = rollup::summary_between(conn, record_type, previous.0, previous.1)?;
    Ok(crate::obj! { "current" => current.to_json(), "previous" => prev.to_json() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup::random::RubyRandom;
    use crate::rollup::tdigest::TDigest;

    /// Random rollups for a few record types over ~40 days: groups with
    /// equal counts and maxes (sort_by's ties), rows inserted in random
    /// order so the statement's order is not the id order, names that
    /// change between hours, extras, missing digests.
    fn random_tenant(dir: &std::path::Path, seed: u32) -> (std::path::PathBuf, i64) {
        let file = dir.join(format!("t{seed}.sqlite3"));
        let t = crate::tenants::open_tenant(&file).unwrap();
        let now = crate::time::parse_db_time("2026-10-25 21:37:40.123456").unwrap();
        let mut rng = RubyRandom::new(seed);
        let mut rows = vec![];
        for ty in ["query", "request", "cache_event"] {
            for g in 0..(5 + rng.limited(60)) {
                let hours = 1 + rng.limited(900) as i64;
                for _ in 0..(1 + rng.limited(40)) {
                    let bucket = rb::beginning_of_hour(now) - rng.limited(hours as u32) as i64 * 3_600_000_000;
                    let count = [1, 2, 5, 5, 10][rng.limited(4) as usize] as i64;
                    let mut d = TDigest::new(0.01);
                    for _ in 0..count {
                        d.push_int([100, 250, 1000, 5000, 77][rng.limited(4) as usize], &mut rng);
                    }
                    d.compress(&mut rng);
                    let digest = if rng.limited(10) == 0 { None } else { Some(d.as_small_bytes()) };
                    let name = format!("{ty} {g} v{}", rng.limited(3));
                    let extra = match (ty, rng.limited(3)) {
                        ("cache_event", _) => format!(r#"{{"hits":{},"misses":{}}}"#, rng.limited(4), rng.limited(4)),
                        (_, 0) => r#"{"kind":"partial","cost":1.5}"#.to_string(),
                        _ => "{}".to_string(),
                    };
                    let max = [100, 5000, 5000][rng.limited(2) as usize];
                    let ps = (rng.limited(3000) as i64, rng.limited(6000) as i64, rng.limited(9000) as i64);
                    rows.push((
                        ty,
                        format!("g{g:03}"),
                        bucket,
                        name,
                        count,
                        (rng.limited(2) as i64, rng.limited(3) as i64),
                        count * 300,
                        max,
                        digest,
                        extra,
                        ps,
                    ));
                }
            }
        }
        rng.shuffle(&mut rows);
        for (ty, g, bucket, name, count, (errors, client_errors), sum, max, digest, extra, (p50, p95, p99)) in rows {
            t.execute(
                "INSERT OR IGNORE INTO rollups (record_type, group_hash, bucket, name, count, error_count, client_error_count, duration_sum, duration_max, digest, extra, p50, p95, p99) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![ty, g, qt(bucket), name, count, errors, client_errors, sum, max, digest, extra, p50, p95, p99],
            )
            .unwrap();
        }
        (file, now)
    }

    /// `Groups` (one pass, folded) gives `grouped_rows` and `summarize` over
    /// `rollup::between`'s rows, on every sort and direction, at every
    /// limit, window and record type.
    #[test]
    fn folded_groups_equal_the_row_by_row_tables() {
        let dir = std::env::temp_dir().join(format!("rw-groups-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let mut checked = 0;
        for seed in 1..=6 {
            let (file, now) = random_tenant(&dir, seed);
            let conn = crate::dashboard::pool::open_reader(&file).unwrap();
            for ty in ["query", "request", "cache_event", "nothing"] {
                for hours in [1, 6, 24, 24 * 7, 24 * 30] {
                    let from = now - hours * 3_600_000_000;
                    let rows = rollup::between(&conn, ty, from, now).unwrap();
                    let groups = Groups::between(&conn, ty, from, now, true).unwrap();
                    assert_eq!(groups.summary().to_json(), rollup::summarize_owned(&rows).to_json(), "{ty} {hours}h seed {seed}");
                    for order in [None, Some("count"), Some("p50"), Some("p95"), Some("p99"), Some("errors"), Some("avg"), Some("max"), Some("bogus")] {
                        for dir in [None, Some("asc"), Some("desc")] {
                            for limit in [1, 8, 200] {
                                let want = grouped_rows(&rows, from, now, limit, order, dir);
                                assert_eq!(groups.rows(limit, order, dir), want, "{ty} {hours}h {order:?} {dir:?} {limit} seed {seed}");
                                checked += want.len();
                            }
                        }
                    }
                    assert_eq!(grouped(&conn, ty, from, now, 200, Some("p95"), None).unwrap(), grouped_rows(&rows, from, now, 200, Some("p95"), None));
                }
            }
        }
        assert!(checked > 10_000, "compared only {checked} rows");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The hour-first rollup series is the one-statement GROUP BY's.
    #[test]
    fn rollup_series_by_hour_first_equals_grouping_every_row() {
        let dir = std::env::temp_dir().join(format!("rw-series-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        for seed in 1..=4 {
            let (file, now) = random_tenant(&dir, seed);
            let conn = crate::dashboard::pool::open_reader(&file).unwrap();
            for ty in ["query", "request"] {
                for (hours, step) in [(24, 3600), (24 * 7, 3600), (24 * 30, 6 * 3600), (24 * 30, 86400)] {
                    let from = now - hours * 3_600_000_000;
                    let b = bucket_sql("bucket", step);
                    let plain = db::query(
                        &conn,
                        &format!(
                            "SELECT {b}, SUM(count), SUM(error_count), SUM(client_error_count), SUM(duration_sum), MAX(p50), MAX(p95), MAX(p99) \
                             FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ?1 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3 GROUP BY {b} ORDER BY {b}"
                        ),
                        rusqlite::params![ty, qt(rb::beginning_of_hour(from)), qt(now)],
                    )
                    .unwrap();
                    let want: Vec<Value> =
                        plain.iter().map(|r| point(ri(r, 0), ri(r, 1), ri(r, 2), ri(r, 3), ri(r, 4), ri(r, 5), ri(r, 6), ri(r, 7)).json).collect();
                    assert!(!want.is_empty());
                    let got: Vec<Value> = rollup_series(&conn, ty, from, now, None, step).unwrap().into_iter().map(|p| p.json).collect();
                    assert_eq!(got, want, "{ty} {hours}h step {step} seed {seed}");
                }
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
