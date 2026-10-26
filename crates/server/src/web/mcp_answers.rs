//! The answer-shaped MCP tools: one call each for a question the low-level
//! tools need four to six for, built from the same pieces (get_issue's
//! occurrences, get_execution's summary, get_route's rollups and N+1s, the
//! dashboard's page queries), so their numbers agree with those tools and
//! with the pages.
//!
//! - investigate_issue: what broke, where, what led up to it, which deploy
//!   it started in (with its commits), how often, for whom, and similar
//!   issues; JSON plus a `markdown` brief.
//! - explain_route: where a route's time goes across a sample of its recent
//!   requests, its N+1s with fixes, its costliest query shapes with stored
//!   plans, profile frames or how to get them; led by `answer`.

use rusqlite::{Connection, params, params_from_iter};
use serde_json::{Value, json};

use super::{Args, IssueRow, Mcp, R, db_time_s, ms, not_found, segment, time_s};
use crate::dashboard::control::{ControlData, Deploy, recent_deploys};
use crate::dashboard::db::{self, Rec, jf, qt};
use crate::dashboard::{rb, rollup};
use crate::web::mcp_summary::{self, TimeSplit};

const HOUR: i64 = 3_600_000_000;
const DAY: i64 = 24 * HOUR;

/// Lines of the lead-up (queries, logs, calls before the raise) to keep.
const LEAD_UP: usize = 15;
/// Commits listed for the deploy an issue started in.
const COMMITS: usize = 20;
/// Requests whose timelines explain_route reads.
const ROUTE_SAMPLE: usize = 20;

impl Mcp<'_> {
    pub(super) fn investigate_issue(&self, a: &Args) -> R<String> {
        let Some(issue) = self.resolve_issue(a)? else {
            return Ok(json!({"error": "issue not found", "hint": super::ISSUE_NOT_FOUND_HINT}).to_string());
        };
        let env = issue.env_row();
        let mut v = self.issue_json(&issue);
        db::merge_into(&mut v, "sample", issue.sample_json());
        let detection = self.detection(&issue)?;
        if !detection.is_null() {
            db::merge_into(&mut v, "detection", detection);
        }
        let (sample, last) = self.agent_occurrences(&issue, false)?;
        let deploys = self.env.control.with(|c| super::super::dashboard_bridge::DbControl::load(c, env.id))?.deploys();
        let sample_meta = issue.sample_json();
        let (lead_up, trend) = self.with_telemetry(&env, |conn| {
            let lead_up = match sample.get("execution_id").and_then(Value::as_str).or_else(|| sample_meta["execution_id"].as_str()) {
                Some(eid) => lead_up(conn, eid, sample.get("occurred_at").and_then(Value::as_str))?,
                None => Value::Null,
            };
            Ok((lead_up, trend(conn, &issue.group_hash, self.now)?))
        })?;
        let started = started_in(&issue, &sample, &trend, &deploys);
        let similar = self.similar_issues(&issue, &sample)?;
        let who = crate::obj! {
            "affected_users" => issue.affected_users,
            "users_30d" => trend["users"].clone(), "tenants_30d" => trend["tenants"].clone(),
        };

        if sample.is_null() && issue.kind == "exception" {
            db::merge_into(&mut v, "note", Value::from("This issue's occurrences are past the environment's retention: frames and context are gone."));
        }
        db::merge_into(&mut v, "occurrence", sample.clone());
        if last.get("same_as").is_none() && !last.is_null() {
            db::merge_into(
                &mut v,
                "last_occurrence",
                crate::obj! { "id" => last["id"].clone(), "occurred_at" => last["occurred_at"].clone(), "message" => last["message"].clone(), "execution_url" => last["execution_url"].clone() },
            );
        }
        db::merge_into(&mut v, "lead_up", lead_up.clone());
        db::merge_into(&mut v, "started_in", started.clone());
        db::merge_into(
            &mut v,
            "trend",
            crate::obj! { "daily" => trend["daily"].clone(), "by_deploy" => trend["by_deploy"].clone(), "last_7d" => trend["last_7d"].clone(), "previous_7d" => trend["previous_7d"].clone() },
        );
        db::merge_into(&mut v, "who", who);
        db::merge_into(&mut v, "similar_issues", similar.clone());
        let next = if issue.kind == "exception" {
            "Open the in-app frames' files at the lines shown. get_execution on occurrence.execution_id pages the full timeline; get_issue with frames: \"all\" has every frame."
        } else {
            "detection names the metric and the records behind it; explain_route (or get_route) on the route, or get_execution on a representative record."
        };
        db::merge_into(&mut v, "next", Value::from(next));
        let md = issue_markdown(&issue, &v);
        db::merge_into(&mut v, "markdown", Value::from(md));
        Ok(v.to_string())
    }

    /// Other issues of the application with the same exception class or the
    /// same culprit file, resolved ones included, newest first.
    fn similar_issues(&self, issue: &IssueRow, sample: &Value) -> R<Value> {
        let class = sample.get("class_name").and_then(Value::as_str).map(str::to_string).or_else(|| issue.title.split(':').next().map(str::to_string));
        let file = issue.culprit.as_deref().map(|c| c.rsplit_once(':').map_or(c, |(f, _)| f).to_string());
        let (sql, mut binds) = self.issues_scope_sql();
        let mut sql = format!("{sql} AND i.application_id = ? AND i.id != ? AND (0");
        binds.push(issue.application_id.into());
        binds.push(issue.id.into());
        if let Some(c) = class.filter(|c| rb::present(Some(c))) {
            sql.push_str(" OR i.title = ? OR i.title LIKE ? ESCAPE '\\'");
            binds.push(c.clone().into());
            binds.push(format!("{}:%", rb::sanitize_sql_like(&c)).into());
        }
        if let Some(f) = file.filter(|f| rb::present(Some(f))) {
            sql.push_str(" OR i.culprit = ? OR i.culprit LIKE ? ESCAPE '\\'");
            binds.push(f.clone().into());
            binds.push(format!("{}:%", rb::sanitize_sql_like(&f)).into());
        }
        sql.push_str(") ORDER BY i.last_seen_at DESC LIMIT 5");
        let rows = self.query_issues(&sql, binds)?;
        let mut out = vec![];
        for i in rows {
            let resolution = self.env.control.with(|c| {
                db::query_one(
                    c,
                    "SELECT kind, data, created_at FROM issue_activities WHERE issue_id = ?1 AND kind IN ('status', 'comment', 'agent') ORDER BY created_at DESC, id DESC LIMIT 1",
                    [i.id],
                )
            })?;
            let comment = self
                .env
                .control
                .with(|c| db::query_one(c, "SELECT body FROM comments WHERE issue_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1", [i.id]))?;
            let mut row = crate::obj! {
                "key" => i.key.clone(), "issue_id" => i.id, "title" => i.title.clone(), "status" => i.status.clone(), "culprit" => i.culprit.clone(),
                "environment" => i.environment_name.clone(), "occurrences" => i.occurrences, "last_seen_at" => db_time_s(i.last_seen_at.clone()),
                "url" => self.url(&format!("/issues/{}", i.id)),
            };
            if i.status == "resolved" {
                let deploy: Option<String> =
                    self.env.control.with(|c| c.query_row("SELECT resolved_in_deploy FROM issues WHERE id = ?", [i.id], |r| r.get(0)))?;
                db::merge_into(&mut row, "resolved_in_deploy", deploy.map_or(Value::Null, Value::from));
            }
            if let Some(c) = comment {
                db::merge_into(&mut row, "last_comment", Value::from(rb::first_chars(c.s("body").unwrap_or(""), 300)));
            } else if let Some(r) = resolution {
                db::merge_into(&mut row, "last_activity", crate::obj! { "kind" => r.s("kind"), "data" => r.json("data") });
            }
            out.push(row);
        }
        Ok(Value::Array(out))
    }

    pub(super) fn explain_route(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let route = a.s("route");
        self.with_telemetry(&env, |conn| {
            let Some(group_hash) = super::find_group(conn, "request", &route, a.presence("group_hash"), from, to)? else {
                return Ok(json!({"error": "route not found", "hint": "Pass route as list_slow_routes shows it (e.g. \"GET /orders/:id\"), or its group_hash; pass a longer window (7d, 30d) for a route not hit recently."}).to_string());
            };
            let summary = rollup::summary_between_group(conn, "request", from, to, &group_hash)?;
            // The slowest requests say most about where the time goes; the
            // newest ones say what a typical request does. Half of each.
            let slowest = db::query(
                conn,
                &format!(
                    "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"kind\" = 'request' AND \"executions\".\"group_hash\" = ?1 \
                     AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 ORDER BY \"executions\".\"duration\" DESC LIMIT {}",
                    ROUTE_SAMPLE / 2
                ),
                params![group_hash, qt(from), qt(to)],
            )?;
            let newest = db::query(
                conn,
                &format!(
                    "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"kind\" = 'request' AND \"executions\".\"group_hash\" = ?1 \
                     AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {ROUTE_SAMPLE}"
                ),
                params![group_hash, qt(from), qt(to)],
            )?;
            let mut sample: Vec<&Rec> = slowest.iter().collect();
            for r in &newest {
                if sample.len() >= ROUTE_SAMPLE {
                    break;
                }
                if !sample.iter().any(|s| s.s("execution_id") == r.s("execution_id")) {
                    sample.push(r);
                }
            }
            if sample.is_empty() && summary.count == 0 {
                return not_found("route");
            }
            let name = sample.first().and_then(|r| r.s("name")).map(str::to_string).or_else(|| rb::present(Some(&route)).then(|| route.clone()));
            let explained = explain_sample(conn, &sample)?;
            let ids: Vec<String> = sample.iter().filter_map(|r| r.s("execution_id").map(str::to_string)).collect();
            // get_route's N+1s, with how often each shape actually ran per
            // request (the gem records an N+1 at its threshold count).
            // Ranked by those runs: the line that repeats most costs most.
            let mut n_plus_ones: Vec<Value> = super::n_plus_ones_for(conn, &ids)?.as_array().cloned().unwrap_or_default();
            for p in &mut n_plus_ones {
                let runs = p["group_hash"].as_str().and_then(|g| explained.runs.get(g)).or_else(|| p["source"].as_str().and_then(|s| explained.runs_by_source.get(s))).copied();
                db::merge_into(p, "runs_per_request", runs.map_or(Value::Null, |r| jf(rb::round(r.0, 1))));
                db::merge_into(p, "max_per_request", runs.map_or(Value::Null, |r| Value::from(r.1)));
            }
            let key = |p: &Value| (p["max_per_request"].as_u64().unwrap_or(0), p["runs_per_request"].as_f64().unwrap_or(0.0));
            n_plus_ones.sort_by(|a, b| key(b).0.cmp(&key(a).0).then(key(b).1.total_cmp(&key(a).1)));
            let n_plus_ones = Value::Array(n_plus_ones);
            let profiles = route_profiles(conn, &group_hash, &ids, from, to)?;
            let sampled = sample.len();
            let per = explained.split.to_json(sampled as f64);
            let (mut notes, wants_profile) = mcp_summary::time_notes(&avg_split(&explained.split, sampled), " per request", "top_queries");
            if wants_profile || profiles.is_null() {
                notes.push(match &profiles {
                    Value::Null => format!("No profile of this route in the window. {}", super::PROFILE_SETUP),
                    _ => "profile.frames are the hottest methods in this route's profiles: what the untracked time is spent in.".to_string(),
                });
            }
            if let Some(d) = explained.dropped {
                notes.push(format!("{d} of the sampled requests dropped child records (the gem's per-execution buffer was full): their query and time totals are low."));
            }
            let answer = route_answer(&explained.split, sampled, &n_plus_ones, &explained.top_queries, &profiles, summary.count);
            let v = crate::obj! {
                "route" => name, "group_hash" => group_hash.clone(),
                "url" => self.url(&format!("/apps/{}/envs/{}/requests/routes/{}", env.application_id, env.id, segment(&group_hash))),
                "window" => json!({"from": time_s(Some(from)), "to": time_s(Some(to))}),
                "answer" => answer,
                "requests" => crate::obj! {
                    "count" => summary.count, "errors" => summary.errors, "avg_ms" => ms(Some(summary.avg)), "p50_ms" => ms(Some(summary.p50)),
                    "p95_ms" => ms(Some(summary.p95)), "max_ms" => ms(Some(summary.max)),
                },
                "sampled_requests" => sampled,
                "time_ms_per_request" => per,
                "queries_per_request" => jf(rb::round(explained.queries as f64 / sampled.max(1) as f64, 1)),
                "n_plus_ones" => n_plus_ones,
                "top_queries" => explained.top_queries.clone(),
                "profile" => profiles,
                "notes" => notes,
                "slowest_requests" => slowest.iter().take(5).map(|r| crate::obj! {
                    "execution_id" => r.s("execution_id"), "duration_ms" => ms(r.i("duration")), "occurred_at" => time_s(r.t("occurred_at")),
                    "queries" => r.json("counters").get("queries").cloned().unwrap_or(Value::Null),
                    "url" => self.execution_url(&env, r.s("kind"), r.s("execution_id")),
                }).collect::<Vec<_>>(),
                "next" => "get_execution on a slowest_requests execution_id for its full timeline; explain_query on a top_queries group_hash for its plan.",
            };
            Ok(v.to_string())
        })
    }
}

// ---------------------------------------------------------------------------
// investigate_issue's pieces

/// What ran in the occurrence's execution before the raise: its summary
/// and the last LEAD_UP queries, calls and log lines up to the exception.
fn lead_up(conn: &Connection, eid: &str, raised_at: Option<&str>) -> rusqlite::Result<Value> {
    let Some(exe) = db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [eid])? else {
        return Ok(json!({"execution_id": eid, "note": "The execution is not stored (sampled out, or past retention): only the occurrence remains."}));
    };
    let facts = super::execution_facts(conn, &exe)?;
    let start = exe.t("occurred_at").unwrap_or(0);
    // The raise's offset in the execution; without it, the whole timeline.
    let cut = raised_at.and_then(|t| crate::time::parse_db_time(t.trim_end_matches(" UTC"))).map(|t| rb::time_sub(t, start) * 1000.0 + 1000.0);
    let before: Vec<&mcp_summary::Entry> = facts
        .entries
        .iter()
        .filter(|e| matches!(e.kind, "query" | "log" | "outgoing_request" | "cache_event" | "llm_call" | "mail" | "enqueued_job" | "storage_op"))
        .filter(|e| cut.is_none_or(|c| e.offset <= c))
        .collect();
    let skipped = before.len().saturating_sub(LEAD_UP);
    let rows: Vec<Value> = before
        .iter()
        .skip(skipped)
        .map(|e| {
            let mut r = e.row.clone();
            if let Value::Object(m) = &mut r {
                m.remove("id");
                m.remove("stage");
                if m.get("source").is_some_and(Value::is_null) {
                    m.remove("source");
                }
            }
            r
        })
        .collect();
    Ok(crate::obj! {
        "execution_id" => eid, "kind" => exe.s("kind"), "name" => exe.s("name"), "duration_ms" => ms(exe.i("duration")),
        "status" => exe.i("status"), "outcome" => exe.s("outcome"), "path" => facts.detail.get("path").cloned().unwrap_or(Value::Null),
        "summary" => slim_summary(facts.summary),
        "before_raise" => rows, "earlier_rows_omitted" => skipped,
    })
}

/// The execution summary without its slowest list (the lead-up rows say
/// what ran, and investigate_issue is about the raise, not the time).
fn slim_summary(mut s: Value) -> Value {
    if let Value::Object(m) = &mut s {
        m.remove("slowest");
        if let Some(Value::Array(r)) = m.get_mut("repeated_queries") {
            r.truncate(5);
        }
    }
    s
}

/// The issue's occurrences over 30 days: per day, per deploy (with each
/// deploy's first occurrence), the last week against the one before, and
/// how many users and tenants saw it.
fn trend(conn: &Connection, group_hash: &str, now: i64) -> rusqlite::Result<Value> {
    let from = now - 30 * DAY;
    let range = params![group_hash, qt(from), qt(now)];
    let daily = db::query(
        conn,
        "SELECT date(occurred_at), COUNT(*) FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY date(occurred_at) ORDER BY 1",
        range,
    )?;
    let by_deploy = db::query(
        conn,
        "SELECT \"exceptions\".\"deploy\", COUNT(*), MIN(occurred_at) FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"exceptions\".\"deploy\" ORDER BY 3",
        range,
    )?;
    let people = db::query(
        conn,
        "SELECT COUNT(DISTINCT user_ref), COUNT(DISTINCT app_tenant) FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3",
        range,
    )?;
    let tenants = db::query(
        conn,
        "SELECT app_tenant, COUNT(*) FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 \
         AND \"exceptions\".\"occurred_at\" BETWEEN ?2 AND ?3 AND app_tenant IS NOT NULL GROUP BY app_tenant ORDER BY 2 DESC LIMIT 5",
        range,
    )?;
    let int = |v: &rusqlite::types::Value| if let rusqlite::types::Value::Integer(i) = v { *i } else { 0 };
    let week = |d: &str| crate::time::parse_db_time(&format!("{d} 00:00:00")).unwrap_or(0);
    let (mut last_7d, mut previous_7d) = (0, 0);
    for r in &daily {
        let at = week(r.s("date(occurred_at)").unwrap_or(""));
        let n = int(r.at(1));
        if at > now - 7 * DAY {
            last_7d += n;
        } else if at > now - 14 * DAY {
            previous_7d += n;
        }
    }
    Ok(crate::obj! {
        "daily" => daily.iter().map(|r| json!([db::raw_json(r.at(0)), int(r.at(1))])).collect::<Vec<_>>(),
        "by_deploy" => by_deploy.iter().map(|r| crate::obj! {
            "deploy" => db::raw_json(r.at(0)), "count" => int(r.at(1)),
            "first_at" => time_s(match r.at(2) { rusqlite::types::Value::Text(t) => crate::time::parse_db_time(t), _ => None }),
        }).collect::<Vec<_>>(),
        "last_7d" => last_7d, "previous_7d" => previous_7d,
        "users" => people.first().map_or(0, |p| int(p.at(0))), "tenants" => people.first().map_or(0, |p| int(p.at(1))),
        "top_tenants" => tenants.iter().map(|r| json!([db::raw_json(r.at(0)), int(r.at(1))])).collect::<Vec<_>>(),
    })
}

/// The deploy an issue first appeared in, from what is stored: the deploy
/// its earliest retained occurrence ran (the trend's first by_deploy), else
/// the sample's, else the last deploy before first_seen_at. With the deploy
/// before it, its commits (newest first, as the deploy hook sends them), and
/// how long after the deploy the issue first showed.
fn started_in(issue: &IssueRow, sample: &Value, trend: &Value, deploys: &[Deploy]) -> Value {
    let first_seen = issue.first_seen_at.as_deref().and_then(crate::time::parse_db_time);
    let recorded = trend["by_deploy"].as_array().and_then(|a| a.first()).and_then(|d| d["deploy"].as_str()).map(str::to_string);
    let recorded = recorded.or_else(|| sample.get("deploy").and_then(Value::as_str).map(str::to_string));
    let recorded = recorded.or_else(|| issue.sample_json()["deploy"].as_str().map(str::to_string));
    let sorted = recent_deploys(deploys.to_vec());
    // The occurrence's deploy value is the gem's (a SHA shortened to 12);
    // a deploy row may carry the full SHA, or the same value as its ref.
    let matches = |d: &Deploy, v: &str| d.deploy == v || (v.len() >= 7 && (d.deploy.starts_with(v) || d.ref_.as_deref().is_some_and(|r| r.starts_with(v))));
    let (deploy, how) = match recorded.as_deref() {
        Some(v) => match sorted.iter().find(|d| matches(d, v)) {
            Some(d) => (Some(d), "the deploy its first retained occurrence ran"),
            None => (None, "the deploy its first retained occurrence ran (no deploy was recorded for it)"),
        },
        None => match first_seen {
            Some(t) => (sorted.iter().find(|d| d.deployed_at <= t), "the last deploy before first_seen_at"),
            None => (None, ""),
        },
    };
    let Some(d) = deploy else {
        return match recorded {
            Some(v) => crate::obj! { "deploy" => v, "how" => how, "deploy_recorded" => false },
            None => Value::Null,
        };
    };
    let previous = sorted.iter().find(|o| o.deployed_at < d.deployed_at);
    let commits: Vec<Value> = d
        .commits
        .iter()
        .take(COMMITS)
        .map(|c| crate::obj! { "sha" => rb::first_chars(c["sha"].as_str().unwrap_or(""), 12), "message" => c["message"].as_str().map(|m| rb::first_chars(m.lines().next().unwrap_or(""), 120)), "author" => c["author"].clone() })
        .collect();
    let after = first_seen.map(|t| t - d.deployed_at);
    let short = |s: &str| rb::first_chars(s, 12).to_string();
    let mut v = crate::obj! {
        "deploy" => d.deploy.clone(), "ref" => d.ref_.as_deref().map(short), "name" => d.name.clone(), "deployed_at" => time_s(Some(d.deployed_at)),
        "how" => how,
        "first_seen_after_deploy" => after.filter(|a| *a >= 0).map_or(Value::Null, |a| Value::from(duration_words(a))),
        "previous_deploy" => previous.map_or(Value::Null, |p| crate::obj! { "deploy" => p.deploy.clone(), "ref" => p.ref_.as_deref().map(short), "deployed_at" => time_s(Some(p.deployed_at)) }),
        "commit_range" => match (previous.and_then(|p| p.ref_.as_deref().or(Some(p.deploy.as_str()))), d.ref_.as_deref().or(Some(d.deploy.as_str()))) {
            (Some(p), Some(n)) => Value::from(format!("{}..{}", short(p), short(n))),
            _ => Value::Null,
        },
        "commits" => commits,
    };
    if d.commits.len() > COMMITS {
        db::merge_into(&mut v, "more_commits", Value::from(d.commits.len() - COMMITS));
    }
    if d.commits.is_empty() {
        db::merge_into(
            &mut v,
            "commits_hint",
            Value::from(
                "This deploy was recorded without commits. `bin/rails railwatch:deploy` sends them; with only refs, run `git log <commit_range>` in the repository.",
            ),
        );
    }
    v
}

/// "3 minutes", "2 hours", "4 days".
fn duration_words(micros: i64) -> String {
    let s = micros / 1_000_000;
    let (n, unit) = if s < 120 {
        (s, "second")
    } else if s < 7200 {
        (s / 60, "minute")
    } else if s < 2 * 86_400 {
        (s / 3600, "hour")
    } else {
        (s / 86_400, "day")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// The brief: what broke, where, the lead-up, when it started, how often,
/// who, similar issues; for a person or an agent to read in one go.
fn issue_markdown(issue: &IssueRow, v: &Value) -> String {
    let mut md = String::new();
    let line = |md: &mut String, s: String| {
        md.push_str(&s);
        md.push('\n');
    };
    line(&mut md, format!("# {} {}", issue.key, issue.title));
    line(&mut md, String::new());
    line(
        &mut md,
        format!(
            "{} · {} · {} occurrences · {} affected users · {}/{} · first seen {} · last seen {}",
            issue.kind,
            issue.status,
            issue.occurrences,
            issue.affected_users,
            issue.application_slug,
            issue.environment_name,
            str_of(&db_time_s(issue.first_seen_at.clone())),
            str_of(&db_time_s(issue.last_seen_at.clone()))
        ),
    );
    if let Some(u) = v["url"].as_str() {
        line(&mut md, format!("Dashboard: {u}"));
    }
    let o = &v["occurrence"];
    if o.is_object() {
        line(&mut md, String::new());
        line(&mut md, "## What broke".into());
        line(&mut md, format!("`{}`: {}", str_of(&o["class_name"]), rb::first_chars(o["message"].as_str().unwrap_or(""), 300)));
        if let Some(c) = o["cause"].as_object().filter(|c| !c.is_empty()) {
            line(
                &mut md,
                format!(
                    "Caused by `{}`: {}",
                    c.get("class").or_else(|| c.get("class_name")).and_then(Value::as_str).unwrap_or("?"),
                    rb::first_chars(c.get("message").and_then(Value::as_str).unwrap_or(""), 200)
                ),
            );
        }
        line(&mut md, String::new());
        line(&mut md, "## Where".into());
        // A line already shown (a block and the method it is in) shows its
        // code once.
        let mut shown: Vec<(String, i64)> = vec![];
        for f in o["frames"].as_array().into_iter().flatten() {
            if let Some(n) = f.get("omitted_frames") {
                line(
                    &mut md,
                    format!(
                        "- … {n} framework frames ({})",
                        f["gems"].as_array().map(|g| g.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default()
                    ),
                );
                continue;
            }
            let app = if f["in_app"] == true { "" } else { " (gem)" };
            line(&mut md, format!("- `{}:{}` in `{}`{app}", str_of(&f["file"]), str_of(&f["line"]), str_of(&f["function"])));
            if let (Some(code), Some(at)) = (f["code"].as_object(), f["line"].as_i64())
                && !shown.contains(&(str_of(&f["file"]), at))
            {
                shown.push((str_of(&f["file"]), at));
                let lines: Vec<String> = code
                    .iter()
                    .filter(|(k, _)| k.parse::<i64>().is_ok_and(|n| (n - at).abs() <= 2))
                    .map(|(k, c)| format!("  {}{k}: {}", if k.parse::<i64>().ok() == Some(at) { "> " } else { "  " }, c.as_str().unwrap_or("").trim_end()))
                    .collect();
                if !lines.is_empty() {
                    line(&mut md, "  ```ruby".into());
                    for l in lines {
                        line(&mut md, l);
                    }
                    line(&mut md, "  ```".into());
                }
            }
        }
        if let Some(locals) = o["locals"].as_object().filter(|l| !l.is_empty()) {
            let shown: Vec<String> = locals.iter().take(8).map(|(k, x)| format!("`{k}` = {}", rb::first_chars(&str_of(x), 120))).collect();
            line(&mut md, format!("Locals: {}", shown.join(", ")));
        }
    }
    let lu = &v["lead_up"];
    if lu.is_object() {
        line(&mut md, String::new());
        line(&mut md, "## Lead-up".into());
        if let Some(n) = lu.get("note").and_then(Value::as_str) {
            line(&mut md, n.to_string());
        } else {
            line(
                &mut md,
                format!(
                    "{} `{}` ({} ms, {}){}",
                    str_of(&lu["kind"]),
                    str_of(&lu["name"]),
                    str_of(&lu["duration_ms"]),
                    lu["outcome"].as_str().map_or_else(|| format!("status {}", str_of(&lu["status"])), str::to_string),
                    lu["path"].as_str().map_or(String::new(), |p| format!(" {p}"))
                ),
            );
            let s = &lu["summary"];
            if let Some(t) = s["time_ms"].as_object() {
                let parts: Vec<String> = ["sql", "transactions", "view", "external", "untracked"]
                    .iter()
                    .filter_map(|k| t.get(*k).and_then(Value::as_f64).filter(|x| *x > 0.0).map(|x| format!("{k} {x} ms")))
                    .collect();
                if !parts.is_empty() {
                    line(&mut md, format!("Time: {}", parts.join(", ")));
                }
            }
            for n in s["n_plus_ones"].as_array().into_iter().flatten() {
                line(&mut md, format!("- N+1 at `{}` ({}x): {}", str_of(&n["source"]), str_of(&n["repeats"]), str_of(&n["fix"])));
            }
            if lu["earlier_rows_omitted"].as_u64().unwrap_or(0) > 0 {
                line(&mut md, format!("({} earlier rows omitted)", lu["earlier_rows_omitted"]));
            }
            for r in lu["before_raise"].as_array().into_iter().flatten() {
                let src = r["source"].as_str().map_or(String::new(), |s| format!(" @ {s}"));
                line(
                    &mut md,
                    format!("- +{} ms {} {}{src}", str_of(&r["offset"]), str_of(&r["type"]), rb::first_chars(r["label"].as_str().unwrap_or(""), 120)),
                );
            }
        }
    }
    let s = &v["started_in"];
    line(&mut md, String::new());
    line(&mut md, "## When it started".into());
    if s.is_object() {
        let deploy = rb::first_chars(s["deploy"].as_str().unwrap_or(""), 12);
        if s["deploy_recorded"] == false {
            line(&mut md, format!("Deploy `{deploy}`: its first retained occurrence ran it, but no deploy by that name is recorded (no time, no commits)."));
        } else {
            line(
                &mut md,
                format!(
                    "Deploy `{deploy}`{} at {} ({}){}.",
                    s["name"].as_str().map_or(String::new(), |n| format!(" ({n})")),
                    str_of(&s["deployed_at"]),
                    str_of(&s["how"]),
                    s["first_seen_after_deploy"].as_str().map_or(String::new(), |a| format!("; first seen {a} after it"))
                ),
            );
        }
        if let Some(r) = s["commit_range"].as_str() {
            line(&mut md, format!("Commit range: `{r}`"));
        }
        for c in s["commits"].as_array().into_iter().flatten() {
            line(&mut md, format!("- `{}` {}{}", str_of(&c["sha"]), str_of(&c["message"]), c["author"].as_str().map_or(String::new(), |a| format!(" ({a})"))));
        }
    } else {
        line(&mut md, "No deploy is recorded for its occurrences.".into());
    }
    let t = &v["trend"];
    line(&mut md, String::new());
    line(&mut md, "## How often, for whom".into());
    line(&mut md, format!("Last 7 days: {}; the 7 before: {}.", t["last_7d"], t["previous_7d"]));
    let w = &v["who"];
    line(&mut md, format!("Users (30d): {}; tenants (30d): {}.", w["users_30d"], w["tenants_30d"]));
    let sim = v["similar_issues"].as_array().cloned().unwrap_or_default();
    if !sim.is_empty() {
        line(&mut md, String::new());
        line(&mut md, "## Similar issues".into());
        for i in sim {
            let how = i["last_comment"].as_str().map_or(String::new(), |c| format!(": {}", rb::first_chars(c, 160)));
            line(
                &mut md,
                format!(
                    "- {} [{}] {} ({} occurrences){how}",
                    str_of(&i["key"]),
                    str_of(&i["status"]),
                    rb::first_chars(i["title"].as_str().unwrap_or(""), 100),
                    str_of(&i["occurrences"])
                ),
            );
        }
    }
    md
}

/// A JSON scalar as text ("" for null).
fn str_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// explain_route's pieces

/// The sampled requests' timelines, folded: their time split summed, their
/// query count, and the query shapes by total time across them.
struct Explained {
    split: TimeSplit,
    queries: usize,
    top_queries: Vec<Value>,
    /// Every shape's (runs per sampled request, most runs in one request)
    /// by group_hash, and by source line (over its shapes) for an N+1 whose
    /// group the queries do not share.
    runs: std::collections::HashMap<String, (f64, usize)>,
    runs_by_source: std::collections::HashMap<String, (f64, usize)>,
    dropped: Option<usize>,
}

fn explain_sample(conn: &Connection, sample: &[&Rec]) -> rusqlite::Result<Explained> {
    struct Shape {
        group_hash: Option<String>,
        sql: String,
        source: Option<String>,
        count: usize,
        total: f64,
        max: f64,
        requests: usize,
        last_request: usize,
        /// Runs in the current request, and the most in any one.
        in_current: usize,
        max_in_one: usize,
    }
    let mut split = TimeSplit::default();
    let mut queries = 0;
    let mut shapes: Vec<Shape> = vec![];
    let mut dropped = 0;
    for (n, exe) in sample.iter().enumerate() {
        let eid = exe.s("execution_id").unwrap_or("");
        let entries = super::execution_timeline(conn, eid, exe.t("occurred_at").unwrap_or(0))?;
        split.add(&TimeSplit::of(&entries, exe.i("duration").unwrap_or(0) as f64 / 1000.0));
        if exe.json("detail").get("dropped_records").and_then(Value::as_i64).unwrap_or(0) > 0 {
            dropped += 1;
        }
        for e in entries.iter().filter(|e| e.kind == "query") {
            queries += 1;
            let key = e.group_hash.clone();
            let ix = match shapes.iter().position(|s| s.group_hash == key && (key.is_some() || s.sql == e.sql.clone().unwrap_or_default())) {
                Some(i) => i,
                None => {
                    shapes.push(Shape {
                        group_hash: key,
                        sql: e.sql.clone().unwrap_or_default(),
                        source: e.source.clone(),
                        count: 0,
                        total: 0.0,
                        max: 0.0,
                        requests: 0,
                        last_request: usize::MAX,
                        in_current: 0,
                        max_in_one: 0,
                    });
                    shapes.len() - 1
                }
            };
            let s = &mut shapes[ix];
            let d = e.duration.unwrap_or(0.0);
            s.count += 1;
            s.total += d;
            s.max = s.max.max(d);
            if s.source.is_none() {
                s.source = e.source.clone();
            }
            if s.last_request != n {
                s.last_request = n;
                s.requests += 1;
                s.in_current = 0;
            }
            s.in_current += 1;
            s.max_in_one = s.max_in_one.max(s.in_current);
        }
    }
    shapes.sort_by(|a, b| b.total.total_cmp(&a.total));
    let n = sample.len().max(1) as f64;
    let runs = shapes.iter().filter_map(|s| s.group_hash.clone().map(|g| (g, (rb::round(s.count as f64 / n, 1), s.max_in_one)))).collect();
    let mut runs_by_source: std::collections::HashMap<String, (f64, usize)> = std::collections::HashMap::new();
    for s in &shapes {
        if let Some(src) = &s.source {
            let e = runs_by_source.entry(src.clone()).or_default();
            e.0 += s.count as f64 / n;
            e.1 = e.1.max(s.max_in_one);
        }
    }
    let mut top_queries = vec![];
    for s in shapes.into_iter().take(5) {
        let explain = match &s.group_hash {
            Some(g) => db::query_one(
                conn,
                "SELECT \"queries\".\"explain\" FROM \"queries\" WHERE \"queries\".\"group_hash\" = ?1 AND \"queries\".\"explain\" IS NOT NULL \
                 ORDER BY \"queries\".\"occurred_at\" DESC LIMIT 1",
                [g],
            )?
            .and_then(|r| r.s("explain").map(|e| rb::first_chars(e, 1500).to_string())),
            None => None,
        };
        top_queries.push(crate::obj! {
            "group_hash" => s.group_hash.clone(), "sql" => rb::first_chars(&s.sql, 300), "source" => s.source.clone(),
            "total_ms" => jf(rb::round(s.total, 1)), "runs_per_request" => jf(rb::round(s.count as f64 / n, 1)),
            "max_ms" => jf(rb::round(s.max, 2)), "in_requests" => s.requests, "max_per_request" => s.max_in_one,
            "explain" => explain.map_or(Value::Null, Value::from),
        });
    }
    Ok(Explained { split, queries, top_queries, runs, runs_by_source, dropped: (dropped > 0).then_some(dropped) })
}

/// The per-request average of a summed split (for time_notes' sentences).
fn avg_split(sum: &TimeSplit, n: usize) -> TimeSplit {
    let n = n.max(1) as f64;
    let mut buckets = sum.buckets;
    for b in &mut buckets {
        *b /= n;
    }
    TimeSplit {
        duration: sum.duration / n,
        buckets,
        tracked: sum.tracked / n,
        untracked: sum.untracked / n,
        sql_in_transactions: sum.sql_in_transactions / n,
        transactions: (sum.transactions as f64 / n).round() as usize,
    }
}

/// The route's profiles in the window (the sampled requests' first, else
/// any of the route's), merged into their hottest frames.
fn route_profiles(conn: &Connection, group_hash: &str, ids: &[String], from: i64, to: i64) -> rusqlite::Result<Value> {
    let mut rows = if ids.is_empty() {
        vec![]
    } else {
        let ph = vec!["?"; ids.len()].join(", ");
        db::query(
            conn,
            &format!("SELECT \"profiles\".* FROM \"profiles\" WHERE \"profiles\".\"execution_id\" IN ({ph}) LIMIT 5"),
            params_from_iter(ids.iter()),
        )?
    };
    if rows.is_empty() {
        // A profile's group is its execution's: the route's group_hash.
        rows = db::query(
            conn,
            "SELECT \"profiles\".* FROM \"profiles\" WHERE \"profiles\".\"group_hash\" = ?1 AND \"profiles\".\"occurred_at\" BETWEEN ?2 AND ?3 \
             ORDER BY \"profiles\".\"occurred_at\" DESC LIMIT 5",
            params![group_hash, qt(from), qt(to)],
        )?;
    }
    if rows.is_empty() {
        return Ok(Value::Null);
    }
    let mut collapsed = String::new();
    for p in &rows {
        if let Ok((c, _)) =
            crate::dashboard::execution_presenter::collapsed_capped(p.blob("stacks").unwrap_or(&[]), crate::dashboard::execution_presenter::MAX_COLLAPSED_BYTES)
        {
            collapsed.push_str(&c);
            if !c.ends_with('\n') {
                collapsed.push('\n');
            }
        }
    }
    Ok(crate::obj! {
        "profiles" => rows.len(), "profile_ids" => rows.iter().map(|p| p.i("id")).collect::<Vec<_>>(),
        "frames" => super::hot_frames(&collapsed, 10),
    })
}

/// One paragraph: the biggest causes, in the order a fix should take them.
fn route_answer(sum: &TimeSplit, n: usize, n_plus_ones: &Value, top: &[Value], profile: &Value, count: i64) -> String {
    if n == 0 {
        return format!("{count} requests in the window, none stored to read (sampled out or past retention): get_route has the rollup numbers.");
    }
    let avg = avg_split(sum, n);
    let mut parts: Vec<String> = vec![];
    for p in n_plus_ones.as_array().into_iter().flatten().take(2) {
        let runs = p["max_per_request"].as_u64().map_or_else(|| str_of(&p["max_repeats"]), |r| r.to_string());
        parts.push(format!(
            "N+1 at {} (up to {runs}x per request){}",
            p["source"].as_str().unwrap_or("an unknown line"),
            p["suggestion"]["code"].as_str().map_or(String::new(), |c| format!(", fix: {c}"))
        ));
    }
    let ms = |x: f64| if x >= 1000.0 { format!("{:.1} s", x / 1000.0) } else { format!("{} ms", x.round() as i64) };
    if avg.duration > 0.0 && avg.transaction_ms() >= avg.duration * 0.5 && avg.transaction_ms() - avg.sql_in_transactions > 50.0 {
        parts.push(format!(
            "{} per request inside transactions but only {} of SQL in them (Ruby runs with the transaction open)",
            ms(avg.transaction_ms()),
            ms(avg.sql_in_transactions)
        ));
    } else if avg.duration > 0.0 && avg.untracked >= avg.duration * 0.4 && avg.untracked >= 50.0 {
        parts.push(format!("{} per request untracked (Ruby code, GC or waiting)", ms(avg.untracked)));
    }
    if avg.duration > 0.0
        && avg.sql() >= avg.duration * 0.3
        && let Some(q) = top.first()
    {
        parts.push(format!("SQL {} per request, most in {} ({} ms total)", ms(avg.sql()), q["source"].as_str().unwrap_or("one shape"), str_of(&q["total_ms"])));
    }
    if avg.buckets[2] >= avg.duration * 0.3 && avg.buckets[2] >= 50.0 {
        parts.push(format!("view rendering {} per request", ms(avg.buckets[2])));
    }
    if avg.buckets[3] >= avg.duration * 0.3 && avg.buckets[3] >= 50.0 {
        parts.push(format!("external calls {} per request", ms(avg.buckets[3])));
    }
    let mut s = format!("Over {n} sampled requests (avg {}): ", ms(avg.duration));
    if parts.is_empty() {
        s.push_str("no single cause stands out; time_ms_per_request has the split.");
    } else {
        s.push_str(&parts.join("; "));
        s.push('.');
    }
    if avg.untracked >= avg.duration * 0.4 && avg.untracked >= 50.0 || avg.transaction_ms() - avg.sql_in_transactions > 50.0 {
        s.push_str(if profile.is_null() {
            " Enable the profiler to see which methods that time is in."
        } else {
            " profile.frames shows which methods that time is in."
        });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_words() {
        assert_eq!(duration_words(42 * 1_000_000), "42 seconds");
        assert_eq!(duration_words(3 * 60 * 1_000_000), "3 minutes");
        assert_eq!(duration_words(HOUR), "60 minutes");
        assert_eq!(duration_words(5 * HOUR), "5 hours");
        assert_eq!(duration_words(3 * DAY), "3 days");
    }

    fn deploy(id: i64, deploy: &str, at: i64, commits: usize) -> Deploy {
        Deploy {
            id,
            deploy: deploy.into(),
            ref_: Some(format!("{deploy}0000000000000000000000000000")),
            name: None,
            url: None,
            server: None,
            deployed_at: at,
            previous_ref: None,
            commits: (0..commits)
                .map(|i| json!({"sha": format!("{deploy}{i:02}ffffffff"), "message": format!("commit {i}\n\nbody"), "author": "ada"}))
                .collect(),
            detail: Default::default(),
        }
    }

    #[test]
    fn started_in_names_the_deploy_its_first_occurrence_ran_and_its_commits() {
        let t0 = crate::time::parse_db_time("2026-10-20 12:00:00").unwrap();
        let deploys = vec![deploy(1, "aaaaaaaaaaaa", t0 - DAY, 0), deploy(2, "bbbbbbbbbbbb", t0, 3), deploy(3, "cccccccccccc", t0 + DAY, 1)];
        let issue = IssueRow {
            id: 1,
            key: "SHOP-1".into(),
            title: "E".into(),
            kind: "exception".into(),
            status: "open".into(),
            priority: "normal".into(),
            occurrences: 4,
            affected_users: 0,
            culprit: None,
            first_seen_at: Some("2026-10-20 12:07:00".into()),
            last_seen_at: Some("2026-10-21 12:00:00".into()),
            account_slug: "a".into(),
            application_id: 1,
            application_slug: "shop".into(),
            account_id: 1,
            environment_id: 1,
            environment_name: "production".into(),
            environment_slug: "p".into(),
            group_hash: "g".into(),
            sample: "{}".into(),
        };
        // The earliest retained occurrence ran bbbb: that deploy, with the
        // range from aaaa and its commits.
        let trend = json!({"by_deploy": [{"deploy": "bbbbbbbbbbbb", "count": 3}, {"deploy": "cccccccccccc", "count": 1}]});
        let s = started_in(&issue, &Value::Null, &trend, &deploys);
        assert_eq!(s["deploy"], "bbbbbbbbbbbb");
        assert_eq!(s["commit_range"], "aaaaaaaaaaaa..bbbbbbbbbbbb");
        assert_eq!(s["previous_deploy"]["deploy"], "aaaaaaaaaaaa");
        assert_eq!(s["commits"].as_array().unwrap().len(), 3);
        assert_eq!(s["commits"][0]["message"], "commit 0", "first line only");
        assert_eq!(s["first_seen_after_deploy"], "7 minutes");
        assert!(s.get("commits_hint").is_none());
        // With no occurrence deploy, the last deploy before first_seen_at,
        // and a deploy without commits says how to get them.
        let s = started_in(&issue, &Value::Null, &json!({"by_deploy": []}), &[deploys[0].clone(), deploys[2].clone()]);
        assert_eq!((s["deploy"].as_str(), s["how"].as_str()), (Some("aaaaaaaaaaaa"), Some("the last deploy before first_seen_at")));
        assert!(s["commits_hint"].as_str().unwrap().contains("git log"));
        // An occurrence deploy with no deploy row is reported as such.
        let s = started_in(&issue, &json!({"deploy": "dddddddddddd"}), &json!({"by_deploy": []}), &deploys);
        assert_eq!((s["deploy"].as_str(), s["deploy_recorded"].as_bool()), (Some("dddddddddddd"), Some(false)));
        // A deploy value that is the full SHA matches the gem's 12-char one.
        let full = vec![deploy(9, "bbbbbbbbbbbb0000000000000000000000000000", t0, 0)];
        assert_eq!(started_in(&issue, &Value::Null, &trend, &full)["deploy"], "bbbbbbbbbbbb0000000000000000000000000000");
    }
}
