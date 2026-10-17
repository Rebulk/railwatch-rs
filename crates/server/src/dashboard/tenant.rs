//! Port of Railwatch::Telemetry::Tenant (the tenants pages' aggregates).

use indexmap::IndexMap;
use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::Value;

use super::db::{self, jf, js, jt, qt};
use super::models::execution_row;
use super::rb::{self, Num};

pub const MAX_TENANTS: usize = 200;
pub const P95_TENANTS: usize = 50;
pub const SPARKLINE_BUCKETS: usize = 12;
pub const TENANT_SLICE: usize = 1_000;

const ERRORS: &str = "SUM(CASE WHEN status >= 500 THEN 1 ELSE 0 END)";
const FAILED: &str = "SUM(CASE WHEN outcome = 'failed' THEN 1 ELSE 0 END)";

#[derive(Debug, Clone)]
struct Row {
    tenant: Option<String>,
    requests: i64,
    errors: i64,
    avg: f64,
    max: f64,
    p95: f64,
    jobs: i64,
    failed_jobs: i64,
    exceptions: i64,
    logs: i64,
    users: i64,
    last_seen_at: Value,
    sparkline: Vec<i64>,
}

impl Row {
    fn blank(t: Option<String>) -> Row {
        Row {
            tenant: t,
            requests: 0,
            errors: 0,
            avg: 0.0,
            max: 0.0,
            p95: 0.0,
            jobs: 0,
            failed_jobs: 0,
            exceptions: 0,
            logs: 0,
            users: 0,
            last_seen_at: Value::Null,
            sparkline: vec![0; SPARKLINE_BUCKETS],
        }
    }
    fn json(&self) -> Value {
        crate::obj! {
            "tenant" => js(self.tenant.as_deref()), "requests" => self.requests, "errors" => self.errors,
            "avg" => jf(self.avg), "max" => jf(self.max), "p95" => jf(self.p95), "jobs" => self.jobs, "failed_jobs" => self.failed_jobs,
            "exceptions" => self.exceptions, "logs" => self.logs, "users" => self.users, "last_seen_at" => self.last_seen_at.clone(),
            "sparkline" => self.sparkline.clone(),
        }
    }
}

fn ms(v: &Sql) -> f64 {
    match v {
        Sql::Integer(i) => rb::round(*i as f64 / 1000.0, 2),
        Sql::Real(f) => rb::round(f / 1000.0, 2),
        _ => 0.0,
    }
}

fn int(v: &Sql) -> i64 {
    match v {
        Sql::Integer(i) => *i,
        Sql::Real(f) => *f as i64,
        _ => 0,
    }
}

fn key(v: &Sql) -> Option<String> {
    match v {
        Sql::Text(t) => Some(t.clone()),
        _ => None,
    }
}

/// `filtered(scope, q)`: tagged rows only, or LIKE %q%.
fn filtered(q: Option<&str>) -> (String, Vec<Sql>) {
    match q.filter(|q| rb::present(Some(q))) {
        None => ("\"app_tenant\" IS NOT NULL".into(), vec![]),
        Some(q) => ("(app_tenant LIKE ?)".into(), vec![Sql::Text(format!("%{}%", rb::sanitize_sql_like(q)))]),
    }
}

/// `Telemetry::Tenant.index`.
pub fn index(conn: &Connection, from: i64, to: i64, q: Option<&str>, sort: Option<&str>, dir: Option<&str>) -> rusqlite::Result<Vec<Value>> {
    let mut rows: IndexMap<Option<String>, Row> = IndexMap::new();
    let (fsql, fb) = filtered(q);
    let range = vec![Sql::Text(qt(from)), Sql::Text(qt(to))];
    let summary_from = "\"executions\" INDEXED BY idx_executions_tenant_summary";

    let mut binds = vec![Sql::Text("request".into())];
    binds.extend(range.clone());
    binds.extend(fb.clone());
    for r in db::query(
        conn,
        &format!(
            "SELECT \"executions\".\"app_tenant\", COUNT(*), {ERRORS}, AVG(duration), MAX(duration), COUNT(DISTINCT user_ref) FROM {summary_from} \
             WHERE \"executions\".\"kind\" = ? AND \"executions\".\"app_tenant\" IS NOT NULL AND \"executions\".\"occurred_at\" BETWEEN ? AND ? AND {fsql} \
             GROUP BY \"executions\".\"app_tenant\""
        ),
        rusqlite::params_from_iter(binds),
    )? {
        let t = key(r.at(0));
        let row = rows.entry(t.clone()).or_insert_with(|| Row::blank(t));
        row.requests = int(r.at(1));
        row.errors = int(r.at(2));
        row.avg = ms(r.at(3));
        row.max = ms(r.at(4));
        row.users = int(r.at(5));
    }

    let mut binds = vec![Sql::Text("job_attempt".into())];
    binds.extend(range.clone());
    binds.extend(fb.clone());
    for r in db::query(
        conn,
        &format!(
            "SELECT \"executions\".\"app_tenant\", COUNT(*), {FAILED} FROM {summary_from} WHERE \"executions\".\"kind\" = ? \
             AND \"executions\".\"app_tenant\" IS NOT NULL AND \"executions\".\"occurred_at\" BETWEEN ? AND ? AND {fsql} GROUP BY \"executions\".\"app_tenant\""
        ),
        rusqlite::params_from_iter(binds),
    )? {
        let t = key(r.at(0));
        let row = rows.entry(t.clone()).or_insert_with(|| Row::blank(t));
        row.jobs = int(r.at(1));
        row.failed_jobs = int(r.at(2));
    }

    for (table, which) in [("exceptions", 0), ("logs", 1)] {
        let mut binds = range.clone();
        binds.extend(fb.clone());
        for r in db::query(
            conn,
            &format!(
                "SELECT COUNT(*) AS \"count_all\", \"{table}\".\"app_tenant\" AS \"{table}_app_tenant\" FROM \"{table}\" \
                 WHERE \"{table}\".\"occurred_at\" BETWEEN ? AND ? AND {fsql} GROUP BY \"{table}\".\"app_tenant\""
            ),
            rusqlite::params_from_iter(binds),
        )? {
            let t = key(r.at(1));
            let row = rows.entry(t.clone()).or_insert_with(|| Row::blank(t));
            if which == 0 {
                row.exceptions = int(r.at(0));
            } else {
                row.logs = int(r.at(0));
            }
        }
    }

    // absorb_last_seen: kind IN (all kinds)
    let mut binds: Vec<Sql> = ["request", "job_attempt", "scheduled_task", "command", "channel_action"].iter().map(|k| Sql::Text(k.to_string())).collect();
    binds.extend(range.clone());
    binds.extend(fb.clone());
    for r in db::query(
        conn,
        &format!(
            "SELECT MAX(\"executions\".\"occurred_at\") AS \"maximum_occurred_at\", \"executions\".\"app_tenant\" AS \"executions_app_tenant\" FROM {summary_from} \
             WHERE \"executions\".\"kind\" IN (?, ?, ?, ?, ?) AND \"executions\".\"app_tenant\" IS NOT NULL AND \"executions\".\"occurred_at\" BETWEEN ? AND ? \
             AND {fsql} GROUP BY \"executions\".\"app_tenant\""
        ),
        rusqlite::params_from_iter(binds),
    )? {
        let t = key(r.at(1));
        let row = rows.entry(t.clone()).or_insert_with(|| Row::blank(t));
        row.last_seen_at = jt(r.s("maximum_occurred_at").and_then(crate::time::parse_db_time));
    }

    // absorb_sparklines
    let tenants: Vec<String> = rows.keys().flatten().cloned().collect();
    if !tenants.is_empty() {
        let width = (rb::time_sub(to, from) / SPARKLINE_BUCKETS as f64).max(1.0);
        let bucket = format!("CAST((strftime('%s', occurred_at) - {}) / {} AS INTEGER)", rb::to_i(from), float_lit(width));
        for slice in tenants.chunks(TENANT_SLICE) {
            let ph = vec!["?"; slice.len()].join(", ");
            let mut binds = vec![Sql::Text("request".into())];
            binds.extend(range.clone());
            binds.extend(slice.iter().map(|t| Sql::Text(t.clone())));
            for r in db::query(
                conn,
                &format!(
                    "SELECT COUNT(*), \"executions\".\"app_tenant\", {bucket} FROM \"executions\" WHERE \"executions\".\"kind\" = ? \
                     AND \"executions\".\"occurred_at\" BETWEEN ? AND ? AND \"executions\".\"app_tenant\" IN ({ph}) GROUP BY \"executions\".\"app_tenant\", {bucket}"
                ),
                rusqlite::params_from_iter(binds),
            )? {
                let t = key(r.at(1));
                let idx = (int(r.at(2)) as usize).min(SPARKLINE_BUCKETS - 1);
                if let Some(row) = rows.get_mut(&t) {
                    row.sparkline[idx] += int(r.at(0));
                }
            }
        }
    }

    // absorb_p95s over the busiest P95_TENANTS (max_by(n) order).
    let mut vals: Vec<Row> = rows.into_values().collect();
    let top = max_by_n(&vals, P95_TENANTS, |r| r.requests);
    for i in top {
        let t = vals[i].tenant.clone();
        vals[i].p95 = p95_ms(conn, "request", t.as_deref(), from, to, vals[i].requests)?;
    }

    let key_name = match sort {
        Some("errors") => "errors",
        Some("p95") => "p95",
        Some("users") => "users",
        _ => "requests",
    };
    let sign = if dir == Some("asc") { 1.0 } else { -1.0 };
    let sorted = rb::sort_by_num(vals, |r| match key_name {
        "errors" => Num::I(r.errors * sign as i64),
        "p95" => Num::F(r.p95 * sign),
        "users" => Num::I(r.users * sign as i64),
        _ => Num::I(r.requests * sign as i64),
    });
    Ok(sorted.into_iter().take(MAX_TENANTS).map(|r| r.json()).collect())
}

/// Ruby's `Float#to_s` as interpolated into SQL.
fn float_lit(f: f64) -> String {
    crate::rjson::ruby::float_to_s(f)
}

/// `Enumerable#max_by(n) { }`: indices of the n greatest, greatest first.
/// MRI: a full sort_by on the negated key? No -- rb_nmin_run buffers and
/// sorts with ruby_qsort (stable mergesort on glibc) by the key descending;
/// the order only matters to which rows get a p95, not to the output.
fn max_by_n<T>(items: &[T], n: usize, key: impl Fn(&T) -> i64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..items.len()).collect();
    idx.sort_by(|&a, &b| key(&items[b]).cmp(&key(&items[a])));
    idx.truncate(n);
    idx
}

/// `p95_ms(scope, count)`: the row `count * 5%` from the slowest end.
fn p95_ms(conn: &Connection, kind: &str, tenant: Option<&str>, from: i64, to: i64, count: i64) -> rusqlite::Result<f64> {
    if count == 0 {
        return Ok(0.0);
    }
    let offset = (count as f64 * 0.05).floor() as i64;
    let r = db::query_one(
        conn,
        &format!(
            "SELECT \"executions\".\"duration\" FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 \
             AND \"executions\".\"app_tenant\" {} ORDER BY \"executions\".\"duration\" DESC LIMIT 1 OFFSET {offset}",
            if tenant.is_some() { "= ?4" } else { "IS NULL" }
        ),
        rusqlite::params_from_iter([Sql::Text(kind.into()), Sql::Text(qt(from)), Sql::Text(qt(to))].into_iter().chain(tenant.map(|t| Sql::Text(t.into())))),
    )?;
    Ok(r.map_or(0.0, |r| ms(r.at(0))))
}

/// `Telemetry::Tenant.overview`.
pub fn overview(conn: &Connection, rows: &[Value], from: i64, to: i64) -> rusqlite::Result<Value> {
    let total = db::count(
        conn,
        "SELECT COUNT(*) FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3",
        rusqlite::params!["request", qt(from), qt(to)],
    )?;
    let tagged_tenants: Vec<String> = db::query(
        conn,
        "SELECT DISTINCT \"executions\".\"app_tenant\" FROM \"executions\" WHERE \"executions\".\"occurred_at\" BETWEEN ?1 AND ?2 AND \"executions\".\"app_tenant\" IS NOT NULL",
        rusqlite::params![qt(from), qt(to)],
    )?
    .iter()
    .filter_map(|r| r.s("app_tenant").map(str::to_string))
    .collect();
    let mut tagged_total = 0;
    for slice in tagged_tenants.chunks(TENANT_SLICE) {
        let ph = vec!["?"; slice.len()].join(", ");
        let mut binds = vec![Sql::Text("request".into()), Sql::Text(qt(from)), Sql::Text(qt(to))];
        binds.extend(slice.iter().map(|t| Sql::Text(t.clone())));
        tagged_total += db::count(
            conn,
            &format!(
                "SELECT COUNT(*) FROM \"executions\" WHERE \"executions\".\"kind\" = ? AND \"executions\".\"occurred_at\" BETWEEN ? AND ? AND \"executions\".\"app_tenant\" IN ({ph})"
            ),
            rusqlite::params_from_iter(binds),
        )?;
    }
    let req = |r: &Value| r["requests"].as_i64().unwrap_or(0);
    let tagged: i64 = rows.iter().map(req).sum();
    let top = rb::max_by(rows, |r| req(r));
    Ok(crate::obj! {
        "tenants" => rows.len(),
        "top_tenant" => top.map_or(Value::Null, |t| t["tenant"].clone()),
        "top_share" => jf(if tagged == 0 { 0.0 } else { rb::round(req(top.unwrap()) as f64 * 100.0 / tagged as f64, 1) }),
        "with_errors" => rows.iter().filter(|r| r["errors"].as_i64().unwrap_or(0) > 0).count(),
        "untagged_share" => jf(if total == 0 { 0.0 } else { rb::round((total - tagged_total) as f64 * 100.0 / total as f64, 1) }),
    })
}

/// `Telemetry::Tenant.summary`.
pub fn summary(conn: &Connection, t: &str, from: i64, to: i64) -> rusqlite::Result<Value> {
    let base =
        "FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 AND \"executions\".\"app_tenant\" = ?4";
    let r =
        db::query_one(conn, &format!("SELECT COUNT(*), {ERRORS}, COUNT(DISTINCT user_ref) {base} LIMIT 1"), rusqlite::params!["request", qt(from), qt(to), t])?
            .unwrap();
    let count = int(r.at(0));
    let j = db::query_one(conn, &format!("SELECT COUNT(*), {FAILED} {base} LIMIT 1"), rusqlite::params!["job_attempt", qt(from), qt(to), t])?.unwrap();
    let cnt = |table: &str| {
        db::count(
            conn,
            &format!("SELECT COUNT(*) FROM \"{table}\" WHERE \"{table}\".\"occurred_at\" BETWEEN ?1 AND ?2 AND \"{table}\".\"app_tenant\" = ?3"),
            rusqlite::params![qt(from), qt(to), t],
        )
    };
    Ok(crate::obj! {
        "requests" => count, "errors" => int(r.at(1)), "p95" => jf(p95_ms(conn, "request", Some(t), from, to, count)?),
        "jobs" => int(j.at(0)), "failed_jobs" => int(j.at(1)), "exceptions" => cnt("exceptions")?, "users" => int(r.at(2)), "logs" => cnt("logs")?,
    })
}

#[allow(clippy::too_many_arguments)]
fn grouped_executions(
    conn: &Connection,
    kind: &str,
    t: &str,
    from: i64,
    to: i64,
    error_sql: &str,
    error_key: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Value>> {
    let rows = db::query(
        conn,
        &format!(
            "SELECT \"executions\".\"group_hash\", \"executions\".\"name\", COUNT(*), {error_sql}, AVG(duration), MAX(duration) FROM \"executions\" \
             WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 AND \"executions\".\"app_tenant\" = ?4 \
             GROUP BY \"executions\".\"group_hash\", \"executions\".\"name\""
        ),
        rusqlite::params![kind, qt(from), qt(to), t],
    )?;
    let rows = rb::sort_by_num(rows, |r| Num::I(-int(r.at(2))));
    Ok(rows
        .iter()
        .take(limit)
        .map(|r| {
            let mut m = serde_json::Map::new();
            m.insert("group_hash".into(), db::raw_json(r.at(0)));
            m.insert("name".into(), db::raw_json(r.at(1)));
            m.insert("count".into(), db::raw_json(r.at(2)));
            m.insert(error_key.into(), db::raw_json(r.at(3)));
            m.insert("avg".into(), jf(ms(r.at(4))));
            m.insert("max".into(), jf(ms(r.at(5))));
            Value::Object(m)
        })
        .collect())
}

pub fn routes(conn: &Connection, t: &str, from: i64, to: i64) -> rusqlite::Result<Vec<Value>> {
    grouped_executions(conn, "request", t, from, to, ERRORS, "errors", 20)
}

pub fn job_classes(conn: &Connection, t: &str, from: i64, to: i64) -> rusqlite::Result<Vec<Value>> {
    grouped_executions(conn, "job_attempt", t, from, to, FAILED, "failed", 10)
}

pub fn exceptions(conn: &Connection, t: &str, from: i64, to: i64) -> rusqlite::Result<Vec<Value>> {
    Ok(db::query(
        conn,
        "SELECT \"exceptions\".* FROM \"exceptions\" WHERE \"exceptions\".\"occurred_at\" BETWEEN ?1 AND ?2 AND \"exceptions\".\"app_tenant\" = ?3 \
         ORDER BY \"exceptions\".\"occurred_at\" DESC LIMIT 20",
        rusqlite::params![qt(from), qt(to), t],
    )?
    .iter()
    .map(|e| {
        crate::obj! {
            "id" => db::ji(e.i("id")), "class_name" => js(e.s("class_name")), "message" => rb::first_chars(e.s("message").unwrap_or(""), 500),
            "occurred_at" => jt(e.t("occurred_at")), "execution_id" => js(e.s("execution_id")), "group_hash" => js(e.s("group_hash")),
        }
    })
    .collect())
}

pub fn people(conn: &Connection, t: &str) -> rusqlite::Result<Vec<Value>> {
    Ok(db::query(conn, "SELECT \"people\".* FROM \"people\" WHERE \"people\".\"app_tenant\" = ?1 ORDER BY \"people\".\"last_seen_at\" DESC LIMIT 20", [t])?
        .iter()
        .map(|p| {
            let name = p.s("name").filter(|s| rb::present(Some(s))).or(p.s("email").filter(|s| rb::present(Some(s)))).or(p.s("ref"));
            crate::obj! { "ref" => js(p.s("ref")), "name" => js(name), "email" => js(p.s("email")), "last_seen_at" => jt(p.t("last_seen_at")) }
        })
        .collect())
}

pub fn recent_requests(conn: &Connection, t: &str, from: i64, to: i64) -> rusqlite::Result<Vec<Value>> {
    Ok(db::query(
        conn,
        "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 \
         AND \"executions\".\"app_tenant\" = ?4 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT 50",
        rusqlite::params!["request", qt(from), qt(to), t],
    )?
    .iter()
    .map(execution_row)
    .collect())
}
