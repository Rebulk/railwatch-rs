//! Processes, releases, deploys, thresholds, usage, and the telemetry half
//! of issues#show. (issues#index and alerts#index read only control tables;
//! they are p3-control's.)

use std::collections::HashMap;

use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value};

use super::*;
use crate::dashboard::control::{recent_deploys, recent_issues};
use crate::dashboard::db::{jf, ji, js, jt};
use crate::dashboard::execution_presenter::viewable;
use crate::dashboard::models;
use crate::dashboard::rb;
use crate::dashboard::rollup;
use crate::dashboard::window::{DAY, HOUR, MINUTE, STEPS, default_step, step_seconds};

fn i(v: &Sql) -> i64 {
    match v {
        Sql::Integer(i) => *i,
        Sql::Real(x) => *x as i64,
        _ => 0,
    }
}

fn fl(v: &Sql) -> f64 {
    match v {
        Sql::Integer(i) => *i as f64,
        Sql::Real(x) => *x,
        _ => 0.0,
    }
}

// -- processes ---------------------------------------------------------------

pub const LIVE_WINDOW: i64 = 10 * MINUTE;

pub fn processes_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let live_since = ctx.now - LIVE_WINDOW;
    let (from, to) = ctx.range();
    let live = db::query(
        conn,
        "SELECT \"health_samples\".* FROM \"health_samples\" WHERE \"health_samples\".\"id\" IN (SELECT MAX(id) FROM \"health_samples\" \
         WHERE \"health_samples\".\"sampled_at\" >= ?1 GROUP BY \"health_samples\".\"server\", \"health_samples\".\"pid\") \
         ORDER BY \"health_samples\".\"server\" ASC, \"health_samples\".\"pid\" ASC",
        [qt(live_since)],
    )?;
    let secs = step_seconds(ctx.step).unwrap();
    let slice = format!("strftime('%Y-%m-%dT%H:%M:00Z', (strftime('%s', sampled_at) / {secs}) * {secs}, 'unixepoch')");
    let series: Vec<Value> = db::query(
        conn,
        &format!(
            "SELECT {slice}, AVG(CASE WHEN threads_max > 0 THEN threads_busy * 100.0 / threads_max END), SUM(backlog), MAX(queue_depth), MAX(queue_latency) \
             FROM \"health_samples\" WHERE \"health_samples\".\"sampled_at\" BETWEEN ?1 AND ?2 GROUP BY {slice} ORDER BY {slice}"
        ),
        rusqlite::params![qt(from), qt(to)],
    )?
    .iter()
    .map(|r| {
        crate::obj! {
            "t" => db::raw_json(r.at(0)), "utilisation" => jf(rb::round(fl(r.at(1)), 1)), "backlog" => i(r.at(2)),
            "queue_depth" => i(r.at(3)), "queue_latency" => jf(rb::round(i(r.at(4)) as f64 / 1000.0, 1)),
        }
    })
    .collect();
    // queue_depths
    let mut qorder: Vec<String> = vec![];
    let mut depths: HashMap<String, i64> = HashMap::new();
    for s in &live {
        if let Value::Object(q) = s.json("detail").get("queues").cloned().unwrap_or(Value::Null) {
            for (k, v) in q {
                let d = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
                let e = depths.entry(k.clone()).or_insert_with(|| {
                    qorder.push(k.clone());
                    0
                });
                *e = (*e).max(d);
            }
        }
    }
    let qs: Vec<(String, i64)> = qorder
        .into_iter()
        .map(|k| {
            let d = depths[&k];
            (k, d)
        })
        .collect();
    let qs = rb::sort_by_num(qs, |(_, d)| rb::Num::I(-d));
    // live servers | servers_since
    let mut live_servers: Vec<Option<String>> = vec![];
    for s in &live {
        let v = s.s("server").map(str::to_string);
        if !live_servers.contains(&v) {
            live_servers.push(v);
        }
    }
    for kind in ["request", "job_attempt", "scheduled_task", "command", "channel_action"] {
        for r in db::query(
            conn,
            "SELECT DISTINCT \"executions\".\"server\" FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" >= ?2",
            rusqlite::params![kind, qt(live_since)],
        )? {
            let v = r.s("server").map(str::to_string);
            if !live_servers.contains(&v) {
                live_servers.push(v);
            }
        }
    }
    let live_set: Vec<String> = live_servers.into_iter().flatten().collect();
    let silent: Vec<String> = control.expected_servers().into_iter().filter(|s| !live_set.contains(s)).collect();
    let processes = db::query(conn, "SELECT \"processes\".* FROM \"processes\" ORDER BY \"processes\".\"booted_at\" DESC LIMIT 200", [])?;
    Ok(crate::obj! {
        "samples" => live.iter().map(|s| crate::obj! {
            "id" => ji(s.i("id")), "server" => js(s.s("server")), "role" => js(s.s("role")), "pid" => ji(s.i("pid")), "deploy" => js(s.s("deploy")),
            "sampled_at" => jt(s.t("sampled_at")), "threads_busy" => ji(s.i("threads_busy")), "threads_max" => ji(s.i("threads_max")),
            "backlog" => ji(s.i("backlog")), "pool_busy" => ji(s.i("pool_busy")), "pool_size" => ji(s.i("pool_size")),
            "pool_waiting" => ji(s.i("pool_waiting")), "queue_depth" => ji(s.i("queue_depth")),
            "queue_latency" => s.i("queue_latency").map_or(Value::Null, |q| jf(rb::round(q as f64 / 1000.0, 1))), "memory" => ji(s.i("memory")),
        }).collect::<Vec<_>>(),
        "series" => series,
        "queues" => qs.into_iter().map(|(q, d)| crate::obj! { "queue" => q, "depth" => d }).collect::<Vec<_>>(),
        "silent_servers" => silent,
        "processes" => processes.iter().map(|p| crate::obj! {
            "id" => ji(p.i("id")), "booted_at" => jt(p.t("booted_at")), "pid" => ji(p.i("pid")), "role" => js(p.s("role")), "server" => js(p.s("server")),
            "deploy" => js(p.s("deploy")), "ruby_version" => js(p.s("ruby_version")), "rails_version" => js(p.s("rails_version")),
            "railwatch_version" => js(p.s("railwatch_version")), "boot_seconds" => p.attr("processes", "boot_seconds"), "detail" => p.json("detail"),
        }).collect::<Vec<_>>(),
    })
}

// -- releases ----------------------------------------------------------------

fn rate(num: i64, den: i64) -> Value {
    if den == 0 { Value::Null } else { jf(rb::round(num as f64 * 100.0 / den as f64, 2)) }
}

/// `ReleaseHealth.for_deploy(deploy, from, to)`.
pub fn release_health_for(conn: &Connection, deploy: Option<&str>, from: i64, to: i64) -> rusqlite::Result<Value> {
    let cols = ["sessions", "sessions_errored", "sessions_crashed", "users", "users_crashed", "duration_sum", "duration_count"]
        .map(|c| format!("COALESCE(SUM({c}), 0)"))
        .join(", ");
    let (cond, binds): (&str, Vec<Sql>) = match deploy {
        Some(d) => (" AND \"release_health\".\"deploy\" = ?3", vec![Sql::Text(d.into())]),
        None => ("", vec![]),
    };
    let mut all = vec![Sql::Text(qt(rb::beginning_of_hour(from))), Sql::Text(qt(to))];
    all.extend(binds);
    let r = db::query_one(
        conn,
        &format!("SELECT {cols} FROM \"release_health\" WHERE \"release_health\".\"bucket\" BETWEEN ?1 AND ?2{cond} LIMIT 1"),
        rusqlite::params_from_iter(all),
    )?
    .unwrap();
    let v: Vec<i64> = (0..7).map(|k| i(r.at(k))).collect();
    let (sessions, errored, crashed, users, users_crashed, dsum, dcount) = (v[0], v[1], v[2], v[3], v[4], v[5], v[6]);
    Ok(crate::obj! {
        "sessions" => sessions, "users" => users,
        "crash_free_sessions" => rate(sessions - crashed, sessions), "crash_free_users" => rate(users - users_crashed, users),
        "errored" => rate(errored, sessions),
        "avg_duration_ms" => if dcount == 0 { Value::Null } else { jf(rb::round(dsum.div_euclid(dcount) as f64 / 1000.0, 2)) },
    })
}

/// `ReleaseHealth.series(deploy, from, to, step:)`.
pub fn release_health_series(conn: &Connection, deploy: Option<&str>, from: i64, to: i64, step: Option<&str>) -> rusqlite::Result<Value> {
    let key = step.unwrap_or_else(|| default_step(from, to));
    let step_us = STEPS.iter().find(|(k, _)| *k == key).unwrap().1.max(HOUR);
    let secs = step_us / 1_000_000;
    let b = aggregations::bucket_sql("bucket", secs);
    let (cond, extra): (&str, Vec<Sql>) = match deploy {
        Some(d) => (" AND \"release_health\".\"deploy\" = ?3", vec![Sql::Text(d.into())]),
        None => ("", vec![]),
    };
    let mut binds = vec![Sql::Text(qt(rb::beginning_of_hour(from))), Sql::Text(qt(to))];
    binds.extend(extra);
    let pts: Vec<aggregations::Point> = db::query(
        conn,
        &format!(
            "SELECT {b}, SUM(sessions), SUM(sessions_errored), SUM(sessions_crashed) FROM \"release_health\" \
             WHERE \"release_health\".\"bucket\" BETWEEN ?1 AND ?2{cond} GROUP BY {b} ORDER BY {b}"
        ),
        rusqlite::params_from_iter(binds),
    )?
    .iter()
    .map(|r| {
        let (t, s, e, c) = (i(r.at(0)), i(r.at(1)), i(r.at(2)), i(r.at(3)));
        aggregations::Point { t, json: crate::obj! { "t" => rb::t_json(t * 1_000_000), "sessions" => s, "ok" => s - e - c, "errored" => e, "crashed" => c } }
    })
    .collect();
    Ok(Value::Array(aggregations::fill(pts, from, to, secs, |t| {
        crate::obj! { "t" => rb::t_json(t * 1_000_000), "sessions" => 0, "ok" => 0, "errored" => 0, "crashed" => 0 }
    })))
}

/// `ReleaseHealth.adoption(from, to)`: deploy => share (insertion order of
/// the GROUP BY).
pub fn adoption(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<Vec<(Option<String>, f64)>> {
    let rows = db::query(
        conn,
        "SELECT SUM(\"release_health\".\"sessions\") AS \"sum_sessions\", \"release_health\".\"deploy\" AS \"release_health_deploy\" FROM \"release_health\" \
         WHERE \"release_health\".\"bucket\" BETWEEN ?1 AND ?2 GROUP BY \"release_health\".\"deploy\"",
        rusqlite::params![qt(rb::beginning_of_hour(from)), qt(to)],
    )?;
    let counts: Vec<(Option<String>, i64)> = rows.iter().map(|r| (r.s("release_health_deploy").map(str::to_string), i(r.at(0)))).collect();
    let total: i64 = counts.iter().map(|c| c.1).sum();
    Ok(counts.into_iter().map(|(d, n)| (d, if total == 0 { 0.0 } else { rb::round(n as f64 * 100.0 / total as f64, 1) })).collect())
}

pub fn releases_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let deploys: Vec<control::Deploy> =
        recent_deploys(control.deploys()).into_iter().filter(|d| d.deployed_at >= from && d.deployed_at <= to).take(50).collect();
    // new_issue_counts
    let mut new_issues: HashMap<i64, i64> = HashMap::new();
    if !deploys.is_empty() {
        let mut ordered = deploys.clone();
        ordered.sort_by_key(|d| d.deployed_at);
        let first_seen: Vec<i64> = control.issues().iter().map(|i| i.first_seen_at).filter(|t| *t >= ordered[0].deployed_at && *t <= to).collect();
        for (k, d) in ordered.iter().enumerate() {
            let end = ordered.get(k + 1).map_or(to, |n| n.deployed_at);
            new_issues.insert(d.id, first_seen.iter().filter(|t| **t >= d.deployed_at && **t < end).count() as i64);
        }
    }
    let summary = release_health_for(conn, None, from, to)?;
    let series = release_health_series(conn, None, from, to, Some(ctx.step))?;
    let adoption = adoption(conn, from, to)?;
    let mut releases = vec![];
    for d in &deploys {
        let h = release_health_for(conn, Some(&d.deploy), from, to)?;
        let share = adoption.iter().find(|(k, _)| k.as_deref() == Some(d.deploy.as_str())).map_or(0.0, |a| a.1);
        releases.push(db::merge(
            h,
            crate::obj! {
                "deploy" => d.deploy.clone(), "ref" => d.short_ref(), "name" => js(d.name.as_deref()), "deployed_at" => rb::t_json(d.deployed_at),
                "adoption" => jf(share), "new_issues" => new_issues.get(&d.id).copied().unwrap_or(0),
            },
        ));
    }
    Ok(crate::obj! {
        "releases" => releases, "series" => series,
        "summary" => db::merge(summary, crate::obj! { "releases" => adoption.iter().filter(|a| a.1 > 0.0).count() }),
    })
}

/// `Deploy#window`: until the next deploy, or now.
fn deploy_window(d: &control::Deploy, all: &[control::Deploy], now: i64) -> (i64, i64) {
    let next = all.iter().filter(|o| o.deployed_at > d.deployed_at).map(|o| o.deployed_at).min();
    (d.deployed_at, next.unwrap_or(now))
}

fn previous_deploy<'a>(d: &control::Deploy, all: &'a [control::Deploy]) -> Option<&'a control::Deploy> {
    recent_deploys(all.to_vec()).into_iter().find(|o| o.deployed_at < d.deployed_at).and_then(|p| all.iter().find(|o| o.id == p.id))
}

pub fn releases_show(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let release = ctx.param_s("id");
    let all = control.deploys();
    let deploy = all.iter().find(|d| d.deploy == release);
    let previous = deploy.and_then(|d| previous_deploy(d, &all));
    let (from, to) = deploy.map_or(ctx.range(), |d| deploy_window(d, &all, ctx.now));
    let sessions = db::query(
        conn,
        "SELECT \"sessions\".* FROM \"sessions\" WHERE \"sessions\".\"deploy\" = ?1 ORDER BY \"sessions\".\"occurred_at\" DESC LIMIT 50",
        [&release],
    )?;
    let issue_rows = |v: Vec<control::Issue>| -> Vec<Value> {
        recent_issues(v)
            .into_iter()
            .take(50)
            .map(|i| crate::obj! { "id" => i.id, "key" => i.key.clone(), "title" => i.title.clone(), "status" => i.status.clone() })
            .collect()
    };
    let issues = control.issues();
    let new_issues: Vec<control::Issue> = issues
        .iter()
        .filter(|i| (i.first_seen_at >= from && i.first_seen_at <= to) || i.sample.get("deploy").and_then(Value::as_str) == Some(release.as_str()))
        .cloned()
        .collect();
    let resolved: Vec<control::Issue> = issues.iter().filter(|i| i.resolved_in_deploy.as_deref() == Some(release.as_str())).cloned().collect();
    Ok(crate::obj! {
        "summary" => release_health_for(conn, Some(&release), from, to)?,
        "previous_summary" => match previous {
            Some(p) => { let (pf, pt) = deploy_window(p, &all, ctx.now); release_health_for(conn, Some(&p.deploy), pf, pt)? }
            None => Value::Null,
        },
        "series" => release_health_series(conn, Some(&release), from, to, None)?,
        "sessions" => sessions.iter().map(|s| crate::obj! {
            "id" => ji(s.i("id")), "session_id" => js(s.s("session_id")), "source" => js(s.s("source")), "status" => js(s.s("status")),
            "user_ref" => js(s.s("user_ref")), "duration" => models::dur_ms(s).map_or(Value::Null, jf), "requests" => ji(s.i("requests")),
            "visits" => ji(s.i("visits")), "errors" => ji(s.i("error_count")), "occurred_at" => jt(s.t("occurred_at")), "started_at" => jt(s.t("started_at")),
        }).collect::<Vec<_>>(),
        "release" => crate::obj! {
            "deploy" => release.clone(), "ref" => deploy.map_or_else(|| rb::first_chars(&release, 12).to_string(), |d| d.short_ref()),
            "name" => deploy.map_or(Value::Null, |d| js(d.name.as_deref())), "deploy_id" => deploy.map_or(Value::Null, |d| Value::from(d.id)),
            "deployed_at" => deploy.map_or(Value::Null, |d| Value::from(rb::t_json(d.deployed_at))),
            "previous_ref" => previous.map_or(Value::Null, |p| Value::from(p.short_ref())),
        },
        "range" => crate::obj! { "from" => crate::time::iso8601(rb::to_i(from)), "to" => crate::time::iso8601(rb::to_i(to)) },
        "new_issues" => issue_rows(new_issues), "resolved_issues" => issue_rows(resolved),
    })
}

// -- deploys -----------------------------------------------------------------

fn deploy_row(d: &control::Deploy) -> Value {
    crate::obj! {
        "id" => d.id, "deploy" => d.deploy.clone(), "ref" => d.short_ref(), "name" => js(d.name.as_deref()), "url" => js(d.url.as_deref()),
        "server" => js(d.server.as_deref()), "deployed_at" => rb::t_json(d.deployed_at), "commits_count" => d.commits.len(),
        "performer" => d.detail.get("performer").cloned().unwrap_or(Value::Null),
    }
}

pub fn deploys_index(_conn: &Connection, _ctx: &PageContext, control: &dyn ControlData) -> R {
    Ok(crate::obj! { "deploys" => recent_deploys(control.deploys()).iter().take(100).map(deploy_row).collect::<Vec<_>>() })
}

pub fn deploys_show(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let id: i64 = ctx.param_s("id").parse().map_err(|_| PageError::NotFound)?;
    let all = control.deploys();
    let d = all.iter().find(|d| d.id == id).ok_or(PageError::NotFound)?;
    let at = d.deployed_at;
    let mut compare = Map::new();
    for t in ["request", "job_attempt"] {
        let before = rollup::summarize_owned(&rollup::bucket_range(conn, t, rb::beginning_of_hour(at - HOUR), at)?);
        let after = rollup::summarize_owned(&rollup::bucket_range(conn, t, rb::beginning_of_hour(at), at + HOUR)?);
        compare.insert(t.into(), crate::obj! { "before" => before.to_json(), "after" => after.to_json() });
    }
    let (wf, wt) = deploy_window(d, &all, ctx.now);
    let health = release_health_for(conn, Some(&d.deploy), wf, wt)?;
    let issues = recent_issues(control.issues());
    let new_issues: Vec<Value> = issues
        .iter()
        .filter(|i| i.first_seen_at >= at && i.first_seen_at <= at + HOUR + 23 * HOUR)
        .map(|i| crate::obj! { "id" => i.id, "key" => i.key.clone(), "title" => i.title.clone(), "status" => i.status.clone() })
        .collect();
    let resolved = control::issues_resolved_in(control, &d.deploy);
    Ok(crate::obj! {
        "deploy" => db::merge(deploy_row(d), crate::obj! {
            "commits" => d.commits.clone(), "previous_ref" => js(d.previous_ref.as_deref()), "repository_ref" => js(d.ref_.as_deref()),
            "detail" => Value::Object(d.detail.clone()),
        }),
        "compare" => Value::Object(compare), "health" => health, "new_issues" => new_issues,
        "resolved_issues" => resolved.iter().map(|i| crate::obj! { "id" => i.id, "key" => i.key.clone(), "title" => i.title.clone() }).collect::<Vec<_>>(),
    })
}

// -- thresholds --------------------------------------------------------------

const TARGET_KINDS: [&str; 8] = ["requests", "jobs", "commands", "queries", "scheduled_tasks", "outgoing_requests", "llm_calls", "llm_tools"];
const METRICS: [&str; 9] = ["p95", "max", "avg", "error_rate", "failure_rate", "missed", "spend", "tokens", "truncation_rate"];
const SHARED: [&str; 6] = ["p95", "max", "avg", "error_rate", "failure_rate", "missed"];
const ANOMALY_KINDS: [&str; 6] = ["requests", "jobs", "queries", "outgoing_requests", "scheduled_tasks", "llm_calls"];
const ANOMALY_METRICS: [&str; 5] = ["p95", "avg", "count", "error_rate", "spend"];

fn names_since(conn: &Connection, t: &str, since: i64, compact: bool) -> rusqlite::Result<Vec<Value>> {
    let rows = db::query(
        conn,
        "SELECT DISTINCT \"rollups\".\"name\" FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ?1 AND (bucket > ?2)",
        rusqlite::params![t, qt(since)],
    )?;
    let mut names: Vec<Option<String>> = rows.iter().map(|r| r.s("name").map(str::to_string)).collect();
    if compact {
        names.retain(Option::is_some);
    }
    // Array#sort over Strings (nil would raise in Ruby; compact first).
    names.sort();
    Ok(names.into_iter().map(|n| js(n.as_deref())).collect())
}

pub fn thresholds_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let since = ctx.now - 7 * DAY;
    let metrics_for = |k: &str| -> Vec<&str> {
        let mut v: Vec<&str> = SHARED.to_vec();
        if k == "llm_calls" {
            v.extend(["spend", "tokens", "truncation_rate"]);
        }
        v
    };
    let amf = |k: &str| -> Vec<&str> {
        let mut v = vec!["p95", "avg", "count", "error_rate"];
        if k == "llm_calls" {
            v.push("spend");
        }
        v
    };
    Ok(crate::obj! {
        "thresholds" => control.thresholds_props(),
        "routes" => names_since(conn, "request", since, false)?,
        "jobs" => names_since(conn, "job_attempt", since, false)?,
        "kinds" => TARGET_KINDS.to_vec(), "metrics" => METRICS.to_vec(),
        "metrics_for_kind" => Value::Object(TARGET_KINDS.iter().map(|k| (k.to_string(), Value::from(metrics_for(k)))).collect()),
        "units" => crate::obj! { "spend" => "$", "tokens" => "", "truncation_rate" => "%", "error_rate" => "%", "failure_rate" => "%" },
        "llm_models" => names_since(conn, "llm_call", since, true)?,
        "anomaly_rules" => control.anomaly_rules_props(),
        "anomaly_target_kinds" => ANOMALY_KINDS.to_vec(), "anomaly_metrics" => ANOMALY_METRICS.to_vec(),
        "anomaly_metrics_for_kind" => Value::Object(ANOMALY_KINDS.iter().map(|k| (k.to_string(), Value::from(amf(k)))).collect()),
    })
}

// -- usage (telemetry half) --------------------------------------------------

/// The usage page's telemetry figures: `by_type`, `by_day`,
/// `dropped_by_client`, `rejected`, `backpressure_*`, `recent_rejections`,
/// `payloads`. `account`, `environment_events` and `db_bytes` are control /
/// filesystem facts p3-control merges in.
#[allow(clippy::type_complexity)]
pub fn usage_show(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let today = rb::beginning_of_day(ctx.now);
    let since = ctx.now - 30 * DAY;
    type Figures = (Map<String, Value>, Vec<(String, i64)>);
    let mut days: Vec<(i64, Map<String, Value>, Vec<(String, i64)>)> = vec![];
    let fig = |lo: i64, hi: Option<i64>| -> rusqlite::Result<Figures> {
        let (cond, binds) = match hi {
            Some(h) => ("received_at >= ?1 AND received_at < ?2", vec![qt(lo), qt(h)]),
            None => ("received_at >= ?1", vec![qt(lo)]),
        };
        let r = db::query_one(
            conn,
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(accepted), 0), COALESCE(SUM(dropped_by_client), 0), COALESCE(SUM(rejected), 0), \
                 COALESCE(SUM(backpressure_factor > 1), 0), MAX(backpressure_factor) FROM \"ingest_batches\" WHERE {cond} LIMIT 1"
            ),
            rusqlite::params_from_iter(binds.iter()),
        )?
        .unwrap();
        let by_type: Vec<(String, i64)> = db::query(
            conn,
            &format!(
                "SELECT SUM(counts.value) AS \"sum_counts_value\", counts.key AS \"counts_key\" FROM \"ingest_batches\" \
                 JOIN json_each(ingest_batches.counts_by_type) AS counts WHERE {cond} GROUP BY counts.key"
            ),
            rusqlite::params_from_iter(binds.iter()),
        )?
        .iter()
        .map(|r| (r.s("counts_key").unwrap_or("").to_string(), i(r.at(0))))
        .collect();
        let mut m = Map::new();
        m.insert("batches".into(), Value::from(i(r.at(0))));
        m.insert("accepted".into(), Value::from(i(r.at(1))));
        m.insert("dropped".into(), Value::from(i(r.at(2))));
        m.insert("rejected".into(), Value::from(i(r.at(3))));
        m.insert("backpressure_batches".into(), Value::from(i(r.at(4))));
        m.insert(
            "backpressure_peak".into(),
            match r.at(5) {
                Sql::Null => Value::Null,
                v => jf(fl(v)),
            },
        );
        Ok((m, by_type))
    };
    let since_day = rb::beginning_of_day(since);
    let (m, bt) = fig(since, Some(since_day + DAY))?;
    days.push((since_day, m, bt));
    let mut d = since_day + DAY;
    while d < today {
        let (m, bt) = fig(d, Some(d + DAY))?;
        days.push((d, m, bt));
        d += DAY;
    }
    let (m, bt) = fig(today, None)?;
    match days.iter_mut().find(|x| x.0 == today) {
        Some(x) => {
            x.1 = m;
            x.2 = bt;
        }
        None => days.push((today, m, bt)),
    }
    let mut torder: Vec<String> = vec![];
    let mut totals: HashMap<String, i64> = HashMap::new();
    for (_, _, bt) in &days {
        for (t, n) in bt {
            let e = totals.entry(t.clone()).or_insert_with(|| {
                torder.push(t.clone());
                0
            });
            *e += n;
        }
    }
    let mut by_type: Vec<(String, i64)> = torder
        .into_iter()
        .map(|t| {
            let n = totals[&t];
            (t, n)
        })
        .collect();
    by_type.sort_by(|a, b| (-a.1).cmp(&-b.1).then(a.0.cmp(&b.0)));
    let sum = |k: &str| days.iter().map(|d| d.1[k].as_i64().unwrap_or(0)).sum::<i64>();
    let peak = days.iter().filter_map(|d| d.1["backpressure_peak"].as_f64()).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
    let by_day: Vec<Value> = days
        .iter()
        .filter(|d| d.1["batches"].as_i64().unwrap_or(0) > 0)
        .map(|d| crate::obj! { "day" => crate::time::utc_seconds_string(rb::to_i(d.0))[..10].to_string(), "count" => d.1["accepted"].clone() })
        .collect();
    // recent_rejections
    let mut rejections: Vec<Value> = vec![];
    let mut batches = 0;
    let mut rej_days: Vec<i64> = days.iter().filter(|d| d.1["rejected"].as_i64().unwrap_or(0) > 0).map(|d| d.0).collect();
    rej_days.sort();
    for day in rej_days.into_iter().rev() {
        let rows = db::query(
            conn,
            &format!(
                "SELECT \"ingest_batches\".* FROM \"ingest_batches\" WHERE \"ingest_batches\".\"received_at\" >= ?1 AND \"ingest_batches\".\"received_at\" < ?2 \
                 AND (rejected > 0) ORDER BY \"ingest_batches\".\"received_at\" DESC LIMIT {}",
                5 - batches
            ),
            rusqlite::params![qt(day), qt(day + DAY)],
        )?;
        batches += rows.len();
        for r in rows {
            if let Value::Array(a) = r.json("rejections") {
                rejections.extend(a);
            }
        }
        if batches >= 5 {
            break;
        }
    }
    rejections.truncate(20);
    let payload = |t: &str, col: &str| -> rusqlite::Result<Value> {
        let r = db::query_one(conn, &format!("SELECT COUNT(*), SUM(\"{t}\".\"{col}\") FROM \"{t}\""), [])?.unwrap();
        Ok(crate::obj! { "rows" => i(r.at(0)), "bytes" => i(r.at(1)) })
    };
    Ok(crate::obj! {
        "by_type" => by_type.into_iter().map(|(t, n)| Value::Array(vec![Value::from(t), Value::from(n)])).collect::<Vec<_>>(),
        "by_day" => by_day, "dropped_by_client" => sum("dropped"), "rejected" => sum("rejected"),
        "backpressure_batches" => sum("backpressure_batches"), "backpressure_peak" => jf(peak.unwrap_or(0.0).max(1.0)),
        "recent_rejections" => rejections,
        "payloads" => vec![
            db::merge(crate::obj! { "name" => "Profiles" }, payload("profiles", "stacks_bytes")?),
            db::merge(crate::obj! { "name" => "Attachments" }, payload("attachments", "bytes")?),
        ],
    })
}

// -- issues#show (telemetry half) ---------------------------------------------

pub const SPLIT_SAMPLE: usize = 200;

/// The props of issues#show an exception issue reads from telemetry:
/// occurrences, latest, breadcrumbs, browser_breadcrumbs, fingerprint,
/// fingerprint_source, distinct_messages, top_messages, daily, by_deploy,
/// by_tenant, attachments. `issue` is the control row (group_hash, sample).
pub fn issue_telemetry(conn: &Connection, ctx: &PageContext, issue: &control::Issue) -> R {
    let g = &issue.group_hash;
    let now = ctx.now;
    let from = now - 30 * DAY;
    let range = rusqlite::params![g, qt(from), qt(now)];
    let occ = db::query(
        conn,
        "SELECT \"exceptions\".* FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 \
         ORDER BY \"exceptions\".\"occurred_at\" DESC LIMIT 50",
        range,
    )?;
    let latest = db::query_one(
        conn,
        "SELECT \"exceptions\".* FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 ORDER BY \"exceptions\".\"occurred_at\" DESC LIMIT 1",
        [g],
    )?;
    let ctx_json = |e: &db::Rec| -> Map<String, Value> {
        match serde_json::from_str::<Value>(e.s("context").unwrap_or("")) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    };
    let browser_breadcrumbs: Vec<Value> = latest
        .as_ref()
        .and_then(|l| ctx_json(l).get("browser").cloned())
        .and_then(|b| b.get("breadcrumbs").cloned())
        .and_then(|t| t.as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter_map(|c| {
                    c.as_object().map(|o| {
                        Value::Object(o.iter().filter(|(k, _)| ["at", "kind", "text"].contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let breadcrumbs: Vec<Value> = match latest.as_ref().and_then(|l| l.s("execution_id").map(|e| (l, e))) {
        None => vec![],
        Some((l, eid)) => match db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [eid])? {
            None => vec![],
            Some(exe) => {
                let props = crate::dashboard::execution_presenter::props(conn, &exe, ctx, &control::StaticControl::default())?;
                let cut = rb::time_sub(l.t("occurred_at").unwrap_or(0), exe.t("occurred_at").unwrap_or(0)) * 1000.0 + 1.0;
                let lid = l.i("id");
                let kept: Vec<Value> = props["timeline"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e["offset"].as_f64().unwrap_or(0.0) <= cut)
                    .filter(|e| !(e["type"] == "exception" && e["id"].as_i64() == lid))
                    .collect();
                let n = kept.len();
                kept.into_iter().skip(n.saturating_sub(30)).collect()
            }
        },
    };
    let latest_json = latest.as_ref().map_or(Value::Null, |l| {
        let cx = ctx_json(l);
        let context = match cx.get("browser") {
            Some(Value::Object(b)) if b.contains_key("breadcrumbs") => {
                let mut c2 = cx.clone();
                let mut b2 = b.clone();
                b2.shift_remove("breadcrumbs");
                c2.insert("browser".into(), Value::Object(b2));
                Value::String(crate::rjson::escape_html(&serde_json::to_string(&c2).unwrap()))
            }
            _ => js(l.s("context")),
        };
        crate::obj! {
            "id" => ji(l.i("id")), "class_name" => js(l.s("class_name")), "message" => js(l.s("message")), "handled" => l.attr("exceptions", "handled"),
            "severity" => js(l.s("severity")), "source" => js(l.s("source")), "file" => js(l.s("file")), "line" => ji(l.i("line")),
            "frames" => l.json("frames"), "cause" => l.json("cause"), "locals" => l.json("locals"), "context" => context,
            "ruby_version" => js(l.s("ruby_version")), "rails_version" => js(l.s("rails_version")), "execution_id" => js(l.s("execution_id")),
            "execution_source" => js(l.s("execution_source")), "execution_preview" => js(l.s("execution_preview")), "deploy" => js(l.s("deploy")),
        }
    });
    let fingerprint = latest.as_ref().map(|l| l.json("fingerprint")).filter(|f| matches!(f, Value::Array(a) if !a.is_empty())).unwrap_or_else(|| {
        match issue.sample.get("fingerprint") {
            Some(Value::Array(a)) => Value::Array(a.clone()),
            Some(Value::Null) | None => Value::Array(vec![]),
            Some(v) => Value::Array(vec![v.clone()]),
        }
    });
    let fingerprint_source = latest
        .as_ref()
        .and_then(|l| l.s("fingerprint_source").map(|s| Value::from(s.to_string())))
        .unwrap_or_else(|| issue.sample.get("fingerprint_source").cloned().unwrap_or(Value::Null));
    let messages = db::query(
        conn,
        &format!(
            "SELECT \"exceptions\".\"message\" FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 ORDER BY \"exceptions\".\"occurred_at\" DESC LIMIT {SPLIT_SAMPLE}"
        ),
        [g],
    )?;
    let mut morder: Vec<Option<String>> = vec![];
    let mut mcount: HashMap<Option<String>, i64> = HashMap::new();
    for r in &messages {
        let k = r.s("message").map(str::to_string);
        let e = mcount.entry(k.clone()).or_insert_with(|| {
            morder.push(k.clone());
            0
        });
        *e += 1;
    }
    let tally: Vec<(Option<String>, i64)> = morder.iter().map(|k| (k.clone(), mcount[k])).collect();
    let distinct = tally.len();
    let top = rb::sort_by_num(tally, |(_, n)| rb::Num::I(-n));
    let grouped = |sql: &str| -> rusqlite::Result<Vec<db::Rec>> { db::query(conn, sql, range) };
    let daily_rows = grouped(
        "SELECT COUNT(*) AS \"count_all\", date(occurred_at) AS \"date_occurred_at\" FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY date(occurred_at)",
    )?;
    let daily_counts: HashMap<String, i64> = daily_rows.iter().map(|r| (r.s("date_occurred_at").unwrap_or("").to_string(), i(r.at(0)))).collect();
    let mut daily = vec![];
    let mut d = rb::beginning_of_day(from);
    while d <= rb::beginning_of_day(now) {
        let day = crate::time::utc_seconds_string(rb::to_i(d))[..10].to_string();
        daily.push(crate::obj! { "day" => day.clone(), "count" => daily_counts.get(&day).copied().unwrap_or(0) });
        d += DAY;
    }
    let to_map = |rows: Vec<db::Rec>| -> Value {
        Value::Object(rows.iter().map(|r| (db::raw_json(r.at(1)).as_str().map_or("".to_string(), str::to_string), Value::from(i(r.at(0))))).collect())
    };
    let by_deploy = to_map(grouped(
        "SELECT COUNT(*) AS \"count_all\", \"exceptions\".\"deploy\" AS \"exceptions_deploy\" FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"exceptions\".\"deploy\"",
    )?);
    let by_tenant = to_map(grouped(
        "SELECT COUNT(*) AS \"count_all\", \"exceptions\".\"app_tenant\" AS \"exceptions_app_tenant\" FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 AND \"exceptions\".\"app_tenant\" IS NOT NULL GROUP BY \"exceptions\".\"app_tenant\"",
    )?);
    let attachments = db::query(
        conn,
        "SELECT \"attachments\".\"id\", \"attachments\".\"name\", \"attachments\".\"content_type\", \"attachments\".\"bytes\", \"attachments\".\"truncated\", \
         \"attachments\".\"occurred_at\", \"attachments\".\"execution_id\", \"attachments\".\"execution_preview\" FROM \"attachments\" \
         WHERE \"attachments\".\"exception_group_hash\" = ?1 ORDER BY \"attachments\".\"occurred_at\" DESC LIMIT 20",
        [g],
    )?;
    Ok(crate::obj! {
        "latest" => latest_json, "breadcrumbs" => breadcrumbs, "browser_breadcrumbs" => browser_breadcrumbs,
        "fingerprint" => fingerprint, "fingerprint_source" => fingerprint_source, "distinct_messages" => distinct,
        "top_messages" => top.into_iter().take(5).map(|(m, n)| crate::obj! { "message" => rb::first_chars(m.as_deref().unwrap_or(""), 200), "count" => n }).collect::<Vec<_>>(),
        "occurrences" => occ.iter().map(|e| crate::obj! {
            "id" => ji(e.i("id")), "message" => rb::first_chars(e.s("message").unwrap_or(""), 300), "handled" => e.attr("exceptions", "handled"),
            "occurred_at" => jt(e.t("occurred_at")), "execution_id" => js(e.s("execution_id")), "execution_source" => js(e.s("execution_source")),
            "execution_preview" => js(e.s("execution_preview")), "user_ref" => js(e.s("user_ref")), "tenant" => js(e.s("app_tenant")),
            "deploy" => js(e.s("deploy")), "server" => js(e.s("server")),
        }).collect::<Vec<_>>(),
        "daily" => daily, "by_deploy" => by_deploy, "by_tenant" => by_tenant,
        "attachments" => attachments.iter().map(|a| crate::obj! {
            "id" => ji(a.i("id")), "name" => js(a.s("name")), "content_type" => js(a.s("content_type")), "bytes" => ji(a.i("bytes")),
            "truncated" => a.attr("attachments", "truncated"), "viewable" => viewable(a.s("content_type")), "occurred_at" => jt(a.t("occurred_at")),
            "execution_id" => js(a.s("execution_id")), "execution_preview" => js(a.s("execution_preview")),
        }).collect::<Vec<_>>(),
    })
}
