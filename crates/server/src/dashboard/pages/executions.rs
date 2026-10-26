//! Overview, requests, jobs, scheduled tasks, commands, executions, traces,
//! people and tenants.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::Value;

use super::*;
use crate::dashboard::control::{recent_deploys, recent_issues};
use crate::dashboard::cursor_page::{self, Order};
use crate::dashboard::db::{jf, ji, js, jt};
use crate::dashboard::filter_query::{self, Resource, s};
use crate::dashboard::models::{self, execution_row};
use crate::dashboard::{execution_presenter, rb, tenant};

// -- overview ----------------------------------------------------------------

pub fn overview_show(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    // One rollup read per type for its table and its totals.
    let request_groups = aggregations::Groups::between(conn, "request", from, to, true)?;
    let job_groups = aggregations::Groups::between(conn, "job_attempt", from, to, true)?;
    let requests = request_groups.rows(8, Some("p95"), None);
    let jobs = job_groups.rows(8, None, None);
    let issues: Vec<Value> = recent_issues(control.issues()).into_iter().filter(|i| i.status == "open").take(8).map(|i| issue_row(&i)).collect();
    Ok(crate::obj! {
        "totals" => crate::obj! {
            "requests" => aggregations::summary_with_delta_of(conn, "request", &request_groups.summary(), ctx.previous_range())?,
            "jobs" => aggregations::summary_with_delta_of(conn, "job_attempt", &job_groups.summary(), ctx.previous_range())?,
        },
        "request_series" => series(conn, ctx, "request", None)?,
        "job_series" => series(conn, ctx, "job_attempt", None)?,
        "slow_routes" => requests,
        "top_jobs" => jobs,
        "issues" => issues,
        "deploys" => deploys_in_window(control, ctx),
        "release_health" => overview_release_health(conn, ctx, control)?,
        "processes" => overview_processes(conn)?,
    })
}

fn overview_release_health(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> rusqlite::Result<Value> {
    let Some(deploy) = recent_deploys(control.deploys()).into_iter().next() else { return Ok(Value::Null) };
    let (from, to) = ctx.range();
    let h = super::misc::release_health_for(conn, Some(&deploy.deploy), from, to)?;
    if h["crash_free_sessions"].is_null() {
        return Ok(Value::Null);
    }
    Ok(crate::obj! {
        "sessions" => h["sessions"].clone(), "crash_free_sessions" => h["crash_free_sessions"].clone(),
        "deploy" => deploy.deploy.clone(), "ref" => deploy.short_ref(),
    })
}

fn overview_processes(conn: &Connection) -> rusqlite::Result<Value> {
    let rows = db::query(conn, "SELECT \"processes\".* FROM \"processes\" ORDER BY \"processes\".\"booted_at\" DESC LIMIT 20", [])?;
    let mut out = serde_json::Map::new();
    for r in rows {
        let server = r.s("server").map_or("null".to_string(), str::to_string);
        if out.contains_key(&server) {
            continue;
        }
        out.insert(
            server,
            crate::obj! {
                "role" => js(r.s("role")), "deploy" => js(r.s("deploy")), "ruby_version" => js(r.s("ruby_version")),
                "rails_version" => js(r.s("rails_version")), "railwatch_version" => js(r.s("railwatch_version")),
                "booted_at" => jt(r.t("booted_at")),
            },
        );
    }
    Ok(Value::Object(out))
}

pub(crate) fn issue_row(i: &control::Issue) -> Value {
    crate::obj! {
        "id" => i.id, "key" => i.key.clone(), "title" => i.title.clone(), "kind" => i.kind.clone(), "status" => i.status.clone(),
        "priority" => i.priority.clone(), "occurrences" => i.occurrences, "affected_users" => i.affected_users,
        "last_seen_at" => rb::t_json(i.last_seen_at), "culprit" => js(i.culprit.as_deref()),
    }
}

// -- requests ----------------------------------------------------------------

pub fn requests_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let routes = grouped(conn, ctx, "request", 200, true)?;
    let routes = filter_query::filter_routes(routes, ctx.param("q"));
    Ok(crate::obj! {
        "routes" => routes, "series" => series(conn, ctx, "request", None)?, "deploys" => deploys_in_window(control, ctx),
        "sort" => sort_param(ctx, "count"), "dir" => dir_param(ctx), "q" => ctx.param_s("q"),
    })
}

pub fn requests_route(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let group_hash = ctx.param_s("group_hash");
    let (from, to) = ctx.range();
    // requests.where(group_hash:).between(...): group_hash before the range.
    let mut c = Cond::new();
    c.eq("executions", "kind", s("request"));
    c.eq("executions", "group_hash", s(&group_hash));
    c.and("\"executions\".\"occurred_at\" BETWEEN ? AND ?", vec![s(&qt(from)), s(&qt(to))]);
    let rows = select(conn, "executions", "\"executions\".*", &c, &recent("executions"), Some(200))?;
    let route = match rows.first().and_then(|r| r.s("name")) {
        Some(n) => Value::String(n.to_string()),
        None => rollup_name(conn, "request", &group_hash)?,
    };
    Ok(crate::obj! {
        "route" => route, "group_hash" => group_hash.clone(),
        "summary" => summary_with_delta(conn, ctx, "request", Some(&group_hash))?,
        "series" => series(conn, ctx, "request", Some(&group_hash))?,
        "deploys" => deploys_in_window(control, ctx),
        "requests" => rows.iter().map(execution_row).collect::<Vec<_>>(),
    })
}

/// `Rollup.for_type(t).where(group_hash:).pick(:name)`
pub fn rollup_name(conn: &Connection, record_type: &str, group_hash: &str) -> rusqlite::Result<Value> {
    Ok(db::query_one(
        conn,
        "SELECT \"rollups\".\"name\" FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ?1 AND \"rollups\".\"group_hash\" = ?2 LIMIT 1",
        [record_type, group_hash],
    )?
    .map_or(Value::Null, |r| js(r.s("name"))))
}

// -- jobs --------------------------------------------------------------------

pub fn jobs_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let cond = filter_query::apply(conn, Resource::Jobs, ctx.param("q"), from, to, &[])?;
    let page = cursor_page::call(
        conn,
        &ctx.cursor_key,
        "executions",
        "Railwatch::Telemetry::Execution",
        "\"executions\".*",
        cond,
        ctx.param("cursor"),
        ctx.param("limit"),
        Order::Recent,
        &ctx.cursor_context("jobs"),
    )??;
    let recent = models::rows_with_identity(conn, &page.rows)?;
    Ok(crate::obj! {
        "classes" => grouped(conn, ctx, "job_attempt", 200, false)?,
        "series" => series(conn, ctx, "job_attempt", None)?,
        "queues" => queue_stats(conn, from, to)?,
        "recent" => recent, "pagination" => page.meta,
        "deploys" => deploys_in_window(control, ctx), "q" => ctx.param_s("q"),
    })
}

pub fn queue_stats(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<Value> {
    let binds = [s("job_attempt"), s(&qt(from)), s(&qt(to))];
    // Per day file and combined on a per-day tenant (storage::reader::
    // aggregate). AVG of an integer column is its exact sum over its count.
    let per_file = crate::storage::reader::aggregate(
        conn,
        "executions",
        "SELECT \"executions\".\"queue\" AS queue, COUNT(*) AS n, SUM(queue_latency) AS lat, COUNT(queue_latency) AS lat_n, \
         SUM(CASE WHEN outcome = 'failed' THEN 1 ELSE 0 END) AS failed FROM {t} \
         WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"executions\".\"queue\"",
        "SELECT queue, SUM(n), CAST(SUM(lat) AS REAL) / SUM(lat_n), SUM(failed) FROM {u} GROUP BY queue",
        &binds,
    )?;
    let rows = match per_file {
        Some(rows) => rows,
        None => db::query(
            conn,
            "SELECT \"executions\".\"queue\", COUNT(*), AVG(queue_latency), SUM(CASE WHEN outcome = 'failed' THEN 1 ELSE 0 END) FROM \"executions\" \
             WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"executions\".\"queue\"",
            rusqlite::params_from_iter(binds.iter()),
        )?,
    };
    Ok(Value::Array(
        rows.iter()
            .map(|r| {
                let lat = match r.at(2) {
                    rusqlite::types::Value::Real(f) => *f,
                    rusqlite::types::Value::Integer(i) => *i as f64,
                    _ => 0.0,
                };
                crate::obj! {
                    "queue" => db::raw_json(r.at(0)), "count" => db::raw_json(r.at(1)),
                    "avg_latency" => jf(rb::round(lat / 1000.0, 1)), "failed" => db::raw_json(r.at(3)),
                }
            })
            .collect(),
    ))
}

pub fn jobs_klass(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let group_hash = ctx.param_s("group_hash");
    let (from, to) = ctx.range();
    let mut cond = filter_query::apply(conn, Resource::Jobs, ctx.param("q"), from, to, &[])?;
    cond.eq("executions", "group_hash", s(&group_hash));
    let rows = select(conn, "executions", "\"executions\".*", &cond, &recent("executions"), Some(200))?;
    let name = db::query_one(
        conn,
        "SELECT \"executions\".\"name\" FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"group_hash\" = ?2 \
         AND \"executions\".\"occurred_at\" BETWEEN ?3 AND ?4 LIMIT 1",
        rusqlite::params!["job_attempt", group_hash, qt(from), qt(to)],
    )?
    .map_or(Value::Null, |r| js(r.s("name")));
    let attempts = models::rows_with_identity(conn, &rows)?;
    Ok(crate::obj! {
        "name" => name, "group_hash" => group_hash.clone(),
        "summary" => summary_with_delta(conn, ctx, "job_attempt", Some(&group_hash))?,
        "series" => series(conn, ctx, "job_attempt", Some(&group_hash))?,
        "attempts" => attempts, "deploys" => deploys_in_window(control, ctx), "q" => ctx.param_s("q"),
    })
}

// -- scheduled tasks ---------------------------------------------------------

pub fn scheduled_tasks_index(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let rows = db::query(
        conn,
        "SELECT \"executions\".\"task_key\", COUNT(*), SUM(CASE WHEN outcome = 'failed' THEN 1 ELSE 0 END), AVG(duration), MAX(occurred_at) \
         FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"executions\".\"task_key\"",
        rusqlite::params!["scheduled_task", qt(from), qt(to)],
    )?;
    let mut tasks = Vec::new();
    for r in &rows {
        let key = r.at(0).clone();
        let schedule = match &key {
            rusqlite::types::Value::Text(k) => latest_schedule(conn, k)?,
            _ => None,
        };
        let avg = match r.at(3) {
            rusqlite::types::Value::Real(f) => *f,
            rusqlite::types::Value::Integer(i) => *i as f64,
            _ => 0.0,
        };
        let next = schedule.as_deref().and_then(|s| cron::next_run(s, ctx.now)).map_or(Value::Null, |t| Value::String(rb::t_json_offset(t)));
        tasks.push(crate::obj! {
            "task_key" => db::raw_json(&key), "runs" => db::raw_json(r.at(1)), "failed" => db::raw_json(r.at(2)),
            "avg" => jf(rb::round(avg / 1000.0, 1)), "last_run_at" => db::raw_json(r.at(4)),
            "schedule" => js(schedule.as_deref()), "next_run_at" => next,
        });
    }
    let runs_rows = select(conn, "executions", "\"executions\".*", &executions_between(Some("scheduled_task"), from, to), &recent("executions"), Some(100))?;
    let runs = models::rows_with_identity(conn, &runs_rows)?;
    let missed = control::open_missed_issues(control);
    Ok(crate::obj! {
        "tasks" => tasks, "runs" => runs,
        "missed" => missed.iter().map(|i| crate::obj! { "key" => i.key.clone(), "title" => i.title.clone(), "id" => i.id }).collect::<Vec<_>>(),
    })
}

/// `Execution.latest_schedules` for one key.
fn latest_schedule(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    Ok(db::query_one(
        conn,
        "SELECT json_extract(detail, '$.schedule') FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"group_hash\" = ?2 \
         AND \"executions\".\"task_key\" = ?3 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT 1",
        rusqlite::params!["scheduled_task", filter_query::md5_hex(key), key],
    )?
    .and_then(|r| match r.at(0) {
        rusqlite::types::Value::Text(t) => Some(t.clone()),
        rusqlite::types::Value::Integer(i) => Some(i.to_string()),
        _ => None,
    }))
}

// -- commands ----------------------------------------------------------------

pub fn commands_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let rows = select(conn, "executions", "\"executions\".*", &executions_between(Some("command"), from, to), &recent("executions"), Some(200))?;
    Ok(crate::obj! {
        "commands" => grouped(conn, ctx, "command", 100, false)?,
        "runs" => rows.iter().map(execution_row).collect::<Vec<_>>(),
    })
}

// -- executions --------------------------------------------------------------

pub fn executions_show(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let id = ctx.param_s("id");
    let exe = db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [&id])?
        .ok_or(PageError::NotFound)?;
    Ok(execution_presenter::props(conn, &exe, ctx, control)?)
}

// -- traces ------------------------------------------------------------------

pub fn traces_show(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let trace_id = ctx.param_s("trace_id");
    let executions = db::query(
        conn,
        "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"trace_id\" = ?1 ORDER BY \"executions\".\"occurred_at\" ASC LIMIT 200",
        [&trace_id],
    )?;
    if executions.is_empty() {
        return Err(PageError::NotFound);
    }
    let ids: Vec<String> = executions.iter().map(|e| e.s("execution_id").unwrap_or("").to_string()).collect();
    let mut outgoing: HashMap<String, Vec<db::Rec>> = HashMap::new();
    let mut outgoing_count = 0usize;
    for chunk in ids.chunks(500) {
        let ph = vec!["?"; chunk.len()].join(", ");
        for o in db::query(
            conn,
            &format!(
                "SELECT \"outgoing_requests\".* FROM \"outgoing_requests\" WHERE \"outgoing_requests\".\"execution_id\" IN ({ph}) ORDER BY \"outgoing_requests\".\"occurred_at\" ASC"
            ),
            rusqlite::params_from_iter(chunk),
        )? {
            outgoing_count += 1;
            outgoing.entry(o.s("execution_id").unwrap_or("").to_string()).or_default().push(o);
        }
    }
    let start = executions[0].t("occurred_at").unwrap_or(0);
    let offset = |t: i64| rb::round(rb::time_sub(t, start) * 1000.0, 2);
    // tree
    let by_id: HashMap<&str, usize> = executions.iter().enumerate().map(|(i, e)| (e.s("execution_id").unwrap_or(""), i)).collect();
    let mut by_short: HashMap<String, usize> = HashMap::new();
    for (i, e) in executions.iter().enumerate() {
        let short: String = e.s("execution_id").unwrap_or("").replace('-', "").chars().take(16).collect();
        by_short.insert(short, i);
    }
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = vec![];
    for (i, e) in executions.iter().enumerate() {
        let parent = e.s("parent_id").filter(|p| rb::present(Some(p))).and_then(|p| by_id.get(p).or_else(|| by_short.get(p)).copied());
        let parent = parent.filter(|&p| executions[p].s("execution_id") != e.s("execution_id"));
        match parent {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    fn node(i: usize, ex: &[db::Rec], children: &HashMap<usize, Vec<usize>>, outgoing: &HashMap<String, Vec<db::Rec>>, offset: &dyn Fn(i64) -> f64) -> Value {
        let e = &ex[i];
        let outs: Vec<Value> = outgoing
            .get(e.s("execution_id").unwrap_or(""))
            .map(|os| {
                os.iter()
                    .map(|o| {
                        crate::obj! {
                            "id" => ji(o.i("id")), "method" => js(o.s("method")),
                            "url" => rb::first_chars(o.s("url").unwrap_or(""), 200), "status_code" => ji(o.i("status_code")),
                            "duration" => models::dur(o, 2), "offset" => jf(offset(o.t("occurred_at").unwrap_or(0))),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let row = crate::obj! {
            "execution_id" => js(e.s("execution_id")), "kind" => js(e.s("kind")), "name" => js(e.s("name")),
            "status" => ji(e.i("status")), "outcome" => js(e.s("outcome")), "parent_id" => js(e.s("parent_id")),
            "server" => js(e.s("server")), "occurred_at" => jt(e.t("occurred_at")),
            "duration" => models::dur(e, 2), "offset" => jf(offset(e.t("occurred_at").unwrap_or(0))), "outgoing" => outs,
        };
        let kids: Vec<Value> = children.get(&i).map(|c| c.iter().map(|&k| node(k, ex, children, outgoing, offset)).collect()).unwrap_or_default();
        crate::obj! { "execution" => row, "children" => kids }
    }
    let roots: Vec<Value> = roots.iter().map(|&i| node(i, &executions, &children, &outgoing, &offset)).collect();
    let mut services: Vec<String> = vec![];
    for e in &executions {
        if let Some(sv) = e.s("server")
            && !services.iter().any(|x| x == sv)
        {
            services.push(sv.to_string());
        }
    }
    let duration = executions.iter().map(|e| offset(e.t("occurred_at").unwrap_or(0)) + models::dur_ms(e).unwrap_or(0.0)).fold(f64::NEG_INFINITY, f64::max);
    Ok(crate::obj! {
        "trace_id" => trace_id, "roots" => roots, "span_count" => executions.len() + outgoing_count,
        "services" => services, "duration" => jf(rb::round(duration, 2)),
    })
}

// -- people ------------------------------------------------------------------

/// `requests.between(..).where.not(user_ref: nil).group(:user_ref)` with
/// its count, errors and last seen, sorted by user_ref (the GROUP BY's
/// order). Per day file and combined on a per-day tenant, on the covering
/// idx_executions_people in the window's files (people@7d 111 -> 20 ms of
/// SQL: the kind+occurred_at range read every request row from the table).
pub fn active_people(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<Vec<db::Rec>> {
    let binds = [s("request"), s(&qt(from)), s(&qt(to))];
    let per_file = crate::storage::reader::aggregate_hinted(
        conn,
        "executions",
        Some(crate::storage::reader::Hint { window: (from, to), inside: "idx_executions_people", outside: "index_executions_on_kind_and_occurred_at" }),
        "SELECT \"executions\".\"user_ref\" AS user_ref, COUNT(*) AS n, SUM(CASE WHEN status >= 500 THEN 1 ELSE 0 END) AS errors, MAX(occurred_at) AS last \
         FROM {t} WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 AND \"executions\".\"user_ref\" IS NOT NULL \
         GROUP BY \"executions\".\"user_ref\"",
        "SELECT user_ref AS \"user_ref\", SUM(n) AS \"COUNT(*)\", SUM(errors), MAX(last) FROM {u} GROUP BY user_ref",
        &binds,
    )?;
    match per_file {
        Some(rows) => Ok(rows),
        None => db::query(
            conn,
            "SELECT \"executions\".\"user_ref\", COUNT(*), SUM(CASE WHEN status >= 500 THEN 1 ELSE 0 END), MAX(occurred_at) FROM \"executions\" \
             WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 AND \"executions\".\"user_ref\" IS NOT NULL \
             GROUP BY \"executions\".\"user_ref\"",
            rusqlite::params_from_iter(binds.iter()),
        ),
    }
}

pub fn people_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let rows = active_people(conn, from, to)?;
    let active: Vec<(String, i64, Value, Value)> =
        rows.iter().map(|r| (r.s("user_ref").unwrap_or("").to_string(), r.i("COUNT(*)").unwrap_or(0), db::raw_json(r.at(2)), db::raw_json(r.at(3)))).collect();
    let active = rb::sort_by_num(active, |a| rb::Num::I(-a.1));
    let active: Vec<_> = active.into_iter().take(200).collect();
    let refs: Vec<String> = active.iter().map(|a| a.0.clone()).collect();
    let people = models::people_by_ref(conn, &refs)?;
    Ok(crate::obj! {
        "people" => active.iter().map(|(r, c, e, last)| {
            let p = people.get(r);
            crate::obj! {
                "ref" => r.clone(), "requests" => *c, "errors" => e.clone(), "last_seen_at" => last.clone(),
                "name" => p.map_or(Value::Null, |p| Value::String(p.display_name())),
                "email" => p.map_or(Value::Null, |p| js(p.email.as_deref())),
                "tenant" => p.map_or(Value::Null, |p| js(p.app_tenant.as_deref())),
            }
        }).collect::<Vec<_>>(),
    })
}

pub fn people_show(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let ref_ = ctx.param_s("id");
    let (from, to) = ctx.range();
    let person = models::person_by_ref(conn, &ref_)?;
    let mut c = Cond::new();
    c.eq("executions", "user_ref", s(&ref_));
    c.and("\"executions\".\"occurred_at\" BETWEEN ? AND ?", vec![s(&qt(from)), s(&qt(to))]);
    let exes = select(conn, "executions", "\"executions\".*", &c, &recent("executions"), Some(100))?;
    let groups = db::query(
        conn,
        "SELECT COUNT(*) AS \"count_all\", \"exceptions\".\"group_hash\" AS \"exceptions_group_hash\" FROM \"exceptions\" \
         WHERE \"exceptions\".\"user_ref\" = ?1 AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"exceptions\".\"group_hash\"",
        rusqlite::params![ref_, qt(from), qt(to)],
    )?;
    let counts: HashMap<String, i64> = groups.iter().filter_map(|r| Some((r.s("exceptions_group_hash")?.to_string(), r.i("count_all").unwrap_or(0)))).collect();
    let groups: Vec<&str> = counts.keys().map(String::as_str).collect();
    let issues = control::issues_in_groups(control, &groups);
    Ok(crate::obj! {
        "person" => person.as_ref().map_or(Value::Null, |p| crate::obj! {
            "person_ref" => p.ref_.clone(), "name" => p.display_name(), "email" => js(p.email.as_deref()),
            "tenant" => js(p.app_tenant.as_deref()), "first_seen_at" => jt(p.first_seen_at), "last_seen_at" => jt(p.last_seen_at),
        }),
        "person_ref" => ref_,
        "executions" => exes.iter().map(execution_row).collect::<Vec<_>>(),
        "issues" => issues.iter().map(|i| crate::obj! {
            "id" => i.id, "key" => i.key.clone(), "title" => i.title.clone(), "status" => i.status.clone(), "count" => counts[&i.group_hash],
        }).collect::<Vec<_>>(),
    })
}

// -- tenants -----------------------------------------------------------------

pub fn tenants_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let (from, to) = ctx.range();
    let rows = tenant::index(conn, from, to, ctx.param("q"), ctx.param("sort"), ctx.param("dir"))?;
    let summary = tenant::overview(conn, &rows, from, to)?;
    Ok(crate::obj! {
        "tenants" => rows, "summary" => summary,
        "sort" => sort_param(ctx, "requests"), "dir" => dir_param(ctx), "q" => ctx.param_s("q"),
    })
}

/// `links` needs the app's URL helpers; p3-control passes the two paths in
/// as params `_logs_path` / `_people_path` (already window-qualified).
pub fn tenants_show(conn: &Connection, ctx: &PageContext, control: &dyn ControlData) -> R {
    let t = ctx.param_s("id");
    let (from, to) = ctx.range();
    let (pf, pt) = ctx.previous_range();
    let exceptions = tenant::exceptions(conn, &t, from, to)?;
    let issues = control.issues();
    let exceptions: Vec<Value> = exceptions
        .into_iter()
        .map(|e| {
            let issue = issues.iter().find(|i| Some(i.group_hash.as_str()) == e["group_hash"].as_str());
            db::merge(e, crate::obj! { "issue_id" => issue.map_or(Value::Null, |i| Value::from(i.id)), "issue_key" => issue.map_or(Value::Null, |i| Value::from(i.key.clone())) })
        })
        .collect();
    Ok(crate::obj! {
        "tenant" => t.clone(),
        "summary" => crate::obj! { "current" => tenant::summary(conn, &t, from, to)?, "previous" => tenant::summary(conn, &t, pf, pt)? },
        "series" => Value::Array(aggregations::points(conn, "request", from, to, None, ctx.step, Some(&t))?),
        "routes" => tenant::routes(conn, &t, from, to)?,
        "jobs" => tenant::job_classes(conn, &t, from, to)?,
        "exceptions" => exceptions,
        "people" => tenant::people(conn, &t)?,
        "recent_requests" => tenant::recent_requests(conn, &t, from, to)?,
        "links" => crate::obj! { "logs" => ctx.param_s("_logs_path"), "people" => ctx.param_s("_people_path") },
    })
}

/// Fugit's `next_time`: jobs::schedule's port of Fugit.parse, the one
/// CheckScheduledTasksJob uses (cron with an optional UTC zone, and the
/// natural "every ..." forms). A schedule it cannot parse has no next run
/// (see docs/DIFFERENCES.md).
pub mod cron {
    /// Epoch micros of the first run strictly after `now` (micros).
    pub fn next_run(schedule: &str, now: i64) -> Option<i64> {
        crate::jobs::schedule::next_after(schedule, now.div_euclid(1_000_000)).map(|t| t * 1_000_000)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn next_times() {
            let now = 1_767_225_630_000_000; // 2026-01-01 00:00:30 UTC (Thursday)
            assert_eq!(super::next_run("*/5 * * * *", now), Some(1_767_225_900_000_000));
            assert_eq!(super::next_run("every 5 minutes", now), Some(1_767_225_900_000_000));
            assert_eq!(super::next_run("0 9 * * 1", now).map(|t| crate::time::iso8601(t / 1_000_000)), Some("2026-01-05T09:00:00Z".into()));
            assert_eq!(super::next_run("at noon on weekdays", now), None);
        }

        #[test]
        fn production_schedules_match_fugit() {
            // Fugit.parse(s).next_time(now) in rw-baseline, for schedules the
            // real deployments carry (2026-10-16).
            let now = 1_792_126_799_000_000; // 2026-10-16 04:59:59 UTC
            let at = |s: &str| super::next_run(s, now).map(|t| crate::time::iso8601(t / 1_000_000));
            for (schedule, fugit) in [
                ("every day at 3am", "2026-10-17T03:00:00Z"),
                ("every day at 3am UTC", "2026-10-17T03:00:00Z"),
                ("every day at 4:35am UTC", "2026-10-17T04:35:00Z"),
                ("every day at 06:30", "2026-10-16T06:30:00Z"),
                ("every day at 11am UTC", "2026-10-16T11:00:00Z"),
                ("every hour at minute 5", "2026-10-16T05:05:00Z"),
                ("every hour at minute 41", "2026-10-16T05:41:00Z"),
                ("every 2 hours at minute 5", "2026-10-16T06:05:00Z"),
                ("every hour", "2026-10-16T05:00:00Z"),
                ("every 5 minutes", "2026-10-16T05:00:00Z"),
                ("every monday at 8am", "2026-10-19T08:00:00Z"),
                ("every weekday at 8am", "2026-10-16T08:00:00Z"),
                ("0 4 * * * UTC", "2026-10-17T04:00:00Z"),
            ] {
                assert_eq!(at(schedule).as_deref(), Some(fugit), "{schedule}");
            }
        }
    }
}

/// `overview_show` with its nine independent sections run concurrently,
/// each on its own read connection from `pools` (WAL readers do not block
/// each other or the writer). Same props as `overview_show`, in the same key
/// order; the props-diff harness runs both.
pub fn overview_show_parallel(pools: &crate::dashboard::pool::ReadPools, slug: &str, ctx: &PageContext, control: &(dyn ControlData + Sync)) -> R {
    let (from, to) = ctx.range();
    type Job<'a> = Box<dyn FnOnce(&Connection) -> rusqlite::Result<Value> + Send + 'a>;
    let jobs: Vec<(&str, Job)> = vec![
        ("req_summary", Box::new(|c: &Connection| summary_with_delta(c, ctx, "request", None))),
        ("job_summary", Box::new(|c: &Connection| summary_with_delta(c, ctx, "job_attempt", None))),
        ("request_series", Box::new(|c: &Connection| series(c, ctx, "request", None))),
        ("job_series", Box::new(|c: &Connection| series(c, ctx, "job_attempt", None))),
        ("slow_routes", Box::new(move |c: &Connection| aggregations::grouped(c, "request", from, to, 8, Some("p95"), None).map(Value::from))),
        ("top_jobs", Box::new(move |c: &Connection| aggregations::grouped(c, "job_attempt", from, to, 8, None, None).map(Value::from))),
        ("release_health", Box::new(|c: &Connection| overview_release_health(c, ctx, control))),
        ("processes", Box::new(|c: &Connection| overview_processes(c))),
    ];
    let results: Vec<(&str, Result<Value, PageError>)> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(name, job)| {
                (
                    name,
                    s.spawn(move || -> Result<Value, PageError> {
                        let conn = pools.get(slug)?;
                        Ok(job(&conn)?)
                    }),
                )
            })
            .collect();
        handles.into_iter().map(|(n, h)| (n, h.join().expect("overview section panicked"))).collect()
    });
    let mut got = std::collections::HashMap::new();
    for (n, r) in results {
        got.insert(n, r?);
    }
    let mut take = |k: &str| got.remove(k).unwrap();
    let issues: Vec<Value> = recent_issues(control.issues()).into_iter().filter(|i| i.status == "open").take(8).map(|i| issue_row(&i)).collect();
    Ok(crate::obj! {
        "totals" => crate::obj! { "requests" => take("req_summary"), "jobs" => take("job_summary") },
        "request_series" => take("request_series"),
        "job_series" => take("job_series"),
        "slow_routes" => take("slow_routes"),
        "top_jobs" => take("top_jobs"),
        "issues" => issues,
        "deploys" => deploys_in_window(control, ctx),
        "release_health" => take("release_health"),
        "processes" => take("processes"),
    })
}
