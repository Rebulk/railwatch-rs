//! The jobs and LLM-calls pages over MCP: list_jobs, get_job and
//! list_llm_calls call the dashboard's own page functions (jobs_index,
//! jobs_klass, llm_calls_index) with a PageContext for the window, and
//! reshape their props for an agent: milliseconds named as such, the
//! columns an agent reads, each row's dashboard url.

use rusqlite::Connection;
use serde_json::{Value, json};

use super::{Args, EnvRow, Mcp, R, clamp, segment, time_s};
use crate::dashboard::db::{self, jf};
use crate::dashboard::{PageContext, PageError, Params, pages, rb};

/// Recent failures and slowest runs get_job lists.
const JOB_RUNS: usize = 10;

impl Mcp<'_> {
    /// The PageContext a dashboard page of `env` would get for the tool's
    /// window (and `extra` params).
    fn page_context(&self, env: &EnvRow, a: &Args, extra: &[(&str, String)]) -> PageContext {
        let mut p = Params::new();
        let key = a.s("window");
        p.insert("window".into(), crate::dashboard::window::Window::preset(Some(&key), self.now).key);
        for (k, v) in extra {
            p.insert((*k).into(), v.clone());
        }
        p.insert("_environment_path".into(), format!("/apps/{}/envs/{}", env.application_id, env.id));
        PageContext::new(env.id, self.now, p, self.env.cursor_key.clone())
    }

    fn page(
        &self,
        conn: &Connection,
        ctx: &PageContext,
        f: fn(&Connection, &PageContext, &dyn crate::dashboard::control::ControlData) -> pages::R,
    ) -> R<Value> {
        match f(conn, ctx, &crate::dashboard::control::StaticControl::default()) {
            Ok(v) => Ok(v),
            Err(PageError::Sql(e)) => Err(e.into()),
            Err(PageError::NotFound) => Ok(Value::Null),
            Err(PageError::Unprocessable(m)) => Err(super::Fail::Invalid(m)),
        }
    }

    fn job_url(&self, env: &EnvRow, group_hash: &str) -> Value {
        self.url(&format!("/apps/{}/envs/{}/jobs/classes/{}", env.application_id, env.id, segment(group_hash)))
    }

    /// The jobs page: every job class with runs, failures, avg/p95/max and
    /// its queue's latency, plus the queues.
    pub(super) fn list_jobs(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let sort = a.presence("sort").unwrap_or_else(|| "count".into());
        if !["count", "errors", "p95", "avg", "max"].contains(&sort.as_str()) {
            return Ok(json!({"error": "sort must be count, errors, p95, avg or max"}).to_string());
        }
        let limit = clamp(a.get("limit"), 30, 200)? as usize;
        let ctx = self.page_context(&env, a, &[("sort", sort.clone()), ("limit", "1".into())]);
        let (from, to) = ctx.range();
        self.with_telemetry(&env, |conn| {
            let classes = crate::dashboard::aggregations::grouped(conn, "job_attempt", from, to, 200, Some(&sort), None)?;
            let queues = pages::queue_stats(conn, from, to)?;
            // Per class: its queue and queue latency over the window.
            let latency = class_latency(conn, from, to)?;
            let total = classes.len();
            let rows: Vec<Value> = classes
                .iter()
                .take(limit)
                .map(|c| {
                    let g = c["group_hash"].as_str().unwrap_or("");
                    let (queue, lat) = latency.iter().find(|(h, ..)| h == g).map_or((Value::Null, Value::Null), |(_, q, l)| (q.clone(), l.clone()));
                    crate::obj! {
                        "job" => c["name"].clone(), "group_hash" => g, "runs" => c["count"].clone(), "failed" => c["errors"].clone(),
                        "failure_rate_pct" => match (c["count"].as_f64(), c["errors"].as_f64()) { (Some(n), Some(e)) if n > 0.0 => jf(rb::round(e / n * 100.0, 1)), _ => Value::Null },
                        "avg_ms" => c["avg"].clone(), "p95_ms" => c["p95"].clone(), "max_ms" => c["max"].clone(),
                        "queue" => queue, "avg_queue_latency_ms" => lat, "url" => self.job_url(&env, g),
                    }
                })
                .collect();
            let v = crate::obj! {
                "window" => json!({"from": time_s(Some(from)), "to": time_s(Some(to))}), "sort" => sort,
                "jobs" => rows, "job_classes" => total,
                "queues" => queues.as_array().cloned().unwrap_or_default().into_iter().map(|q| crate::obj! {
                    "queue" => q["queue"].clone(), "runs" => q["count"].clone(), "failed" => q["failed"].clone(), "avg_latency_ms" => q["avg_latency"].clone(),
                }).collect::<Vec<_>>(),
                "url" => self.url(&format!("/apps/{}/envs/{}/jobs", env.application_id, env.id)),
                "next" => "get_job with a job (class name) for its recent failures and slowest runs.",
            };
            Ok(v.to_string())
        })
    }

    /// The job class page: its summary against the window before, its
    /// recent failures with their exceptions, its slowest runs.
    pub(super) fn get_job(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let ctx0 = self.page_context(&env, a, &[]);
        let (from, to) = ctx0.range();
        self.with_telemetry(&env, |conn| {
            let Some(group) = super::find_group(conn, "job_attempt", &a.s("job"), a.presence("group_hash"), from, to)? else {
                return Ok(json!({"error": "job not found", "hint": "Pass job as list_jobs names it (the class, e.g. \"InvoiceSyncJob\"), or its group_hash."}).to_string());
            };
            let ctx = self.page_context(&env, a, &[("group_hash", group.clone())]);
            let page = self.page(conn, &ctx, pages::jobs_klass)?;
            let summary = &page["summary"]["current"];
            let previous = &page["summary"]["previous"];
            let attempts: Vec<Value> = page["attempts"].as_array().cloned().unwrap_or_default();
            if attempts.is_empty() && summary["count"].as_i64().unwrap_or(0) == 0 {
                return Ok(json!({"error": "job not found", "hint": format!("No runs of this job in the window {}; pass a longer window (7d, 30d).", ctx.window.key)}).to_string());
            }
            let row = |r: &Value| {
                crate::obj! {
                    "execution_id" => r["execution_id"].clone(), "outcome" => r["outcome"].clone(), "duration_ms" => r["duration"].clone(),
                    "queue" => r["queue"].clone(), "queue_latency_ms" => r["queue_latency"].clone(), "attempt" => r["attempt"].clone(),
                    "occurred_at" => time_of(&r["occurred_at"]), "deploy" => r["deploy"].clone(), "tenant" => r["tenant"].clone(),
                    "exception" => r["exception_preview"].clone(),
                    "url" => self.execution_url(&env, Some("job_attempt"), r["execution_id"].as_str()),
                }
            };
            let failures: Vec<Value> = attempts.iter().filter(|r| r["outcome"] == "failed").take(JOB_RUNS).map(row).collect();
            let mut by_duration: Vec<&Value> = attempts.iter().collect();
            by_duration.sort_by(|x, y| y["duration"].as_f64().unwrap_or(0.0).total_cmp(&x["duration"].as_f64().unwrap_or(0.0)));
            let slowest: Vec<Value> = by_duration.into_iter().take(JOB_RUNS).map(row).collect();
            // The failures' exceptions, most frequent first, among the runs read.
            let mut exceptions: Vec<(String, usize)> = vec![];
            for r in attempts.iter().filter(|r| r["outcome"] == "failed") {
                let e = r["exception_preview"].as_str().unwrap_or("(no exception recorded)").to_string();
                match exceptions.iter_mut().find(|(x, _)| *x == e) {
                    Some(slot) => slot.1 += 1,
                    None => exceptions.push((e, 1)),
                }
            }
            exceptions.sort_by_key(|e| std::cmp::Reverse(e.1));
            let ms = |v: &Value| v.as_i64().map_or(Value::Null, |u| jf(rb::round(u as f64 / 1000.0, 2)));
            let side = |s: &Value| {
                crate::obj! {
                    "runs" => s["count"].clone(), "failed" => s["errors"].clone(), "avg_ms" => ms(&s["avg"]), "p50_ms" => ms(&s["p50"]),
                    "p95_ms" => ms(&s["p95"]), "max_ms" => ms(&s["max"]),
                }
            };
            let v = crate::obj! {
                "job" => page["name"].clone(), "group_hash" => group.clone(), "url" => self.job_url(&env, &group),
                "window" => json!({"from": time_s(Some(from)), "to": time_s(Some(to))}),
                "summary" => side(summary), "previous_window" => side(previous),
                "exceptions" => exceptions.into_iter().take(5).map(|(e, n)| json!({"exception": e, "runs": n})).collect::<Vec<_>>(),
                "recent_failures" => failures, "slowest_runs" => slowest, "runs_read" => attempts.len(),
                "next" => "get_execution on a run's execution_id for its timeline and summary; investigate_issue on the exception's issue (list_issues) for its root cause.",
            };
            Ok(v.to_string())
        })
    }

    /// The LLM page: by model (calls, errors, tokens, cost, latency) with
    /// the tools agents called, or the recent calls themselves.
    pub(super) fn list_llm_calls(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let group = a.presence("group").unwrap_or_else(|| "model".into());
        if group != "model" && group != "none" {
            return Ok(json!({"error": "group must be model or none"}).to_string());
        }
        let limit = clamp(a.get("limit"), 20, 100)? as usize;
        let mut extra = vec![];
        if let Some(q) = a.presence("q") {
            extra.push(("q", q));
        }
        let ctx = self.page_context(&env, a, &extra);
        let (from, to) = ctx.range();
        self.with_telemetry(&env, |conn| {
            let page = self.page(conn, &ctx, pages::llm_calls_index)?;
            let t = &page["totals"];
            let totals = crate::obj! {
                "calls" => t["count"].clone(), "errors" => t["errors"].clone(), "tool_calls" => t["tool_count"].clone(), "tool_errors" => t["tool_errors"].clone(),
                "input_tokens" => t["input_tokens"].clone(), "output_tokens" => t["output_tokens"].clone(),
                "cost_usd" => t["cost"].clone(), "unpriced_calls" => t["unpriced"].clone(),
            };
            let llm_url = self.url(&format!("/apps/{}/envs/{}/llm", env.application_id, env.id));
            let mut v = crate::obj! {
                "window" => json!({"from": time_s(Some(from)), "to": time_s(Some(to))}), "group" => group.clone(), "totals" => totals, "url" => llm_url,
            };
            if group == "model" {
                let models: Vec<Value> = page["models"].as_array().cloned().unwrap_or_default().into_iter().take(limit).map(|m| crate::obj! {
                    "model" => m["name"].clone(), "group_hash" => m["group_hash"].clone(), "calls" => m["count"].clone(), "errors" => m["errors"].clone(),
                    "input_tokens" => m["input_tokens"].clone(), "output_tokens" => m["output_tokens"].clone(),
                    "cache_read_tokens" => m["cache_read_tokens"].clone(), "cache_write_tokens" => m["cache_write_tokens"].clone(),
                    "cost_usd" => m["cost"].clone(), "unpriced_calls" => m["unpriced"].clone(), "truncated" => m["truncated"].clone(),
                    "avg_ms" => m["avg"].clone(), "p95_ms" => m["p95"].clone(),
                }).collect();
                let tools: Vec<Value> = page["tools"].as_array().cloned().unwrap_or_default().into_iter().take(limit).map(|m| crate::obj! {
                    "tool" => m["name"].clone(), "calls" => m["count"].clone(), "errors" => m["errors"].clone(), "avg_ms" => m["avg"].clone(), "p95_ms" => m["p95"].clone(),
                }).collect();
                db::merge_into(&mut v, "models", Value::Array(models));
                db::merge_into(&mut v, "tools", Value::Array(tools));
                db::merge_into(&mut v, "next", Value::from("group: none lists the recent calls (prompt and completion when the app captures them); q filters by model:, provider:, operation:, finish:."));
            } else {
                let calls: Vec<Value> = page["recent"].as_array().cloned().unwrap_or_default().into_iter().take(limit).map(|r| {
                    let mut c = crate::obj! {
                        "id" => r["id"].clone(), "operation" => r["operation"].clone(), "provider" => r["provider"].clone(), "model" => r["model"].clone(),
                        "tool" => r["tool_name"].clone(), "duration_ms" => r["duration"].clone(), "status" => r["status"].clone(), "error" => r["error"].clone(),
                        "input_tokens" => r["input_tokens"].clone(), "output_tokens" => r["output_tokens"].clone(), "cost_usd" => r["cost"].clone(),
                        "finish_reason" => r["finish_reason"].clone(), "occurred_at" => time_of(&r["occurred_at"]),
                        "execution_id" => r["execution_id"].clone(), "execution" => r["execution_preview"].clone(),
                    };
                    // Prompt and completion only when captured, shortened.
                    for (k, out) in [("prompt", "prompt"), ("completion", "completion")] {
                        if let Some(s) = r[k].as_str().filter(|s| !s.is_empty()) {
                            db::merge_into(&mut c, out, Value::from(rb::first_chars(s, 500)));
                        }
                    }
                    c
                }).collect();
                db::merge_into(&mut v, "calls", Value::Array(calls));
                db::merge_into(&mut v, "next", Value::from("get_execution on a call's execution_id for the request or job that made it."));
            }
            Ok(v.to_string())
        })
    }
}

/// Each job class's latest queue and its average queue latency (ms) over
/// [from, to], as (group_hash, queue, latency).
fn class_latency(conn: &Connection, from: i64, to: i64) -> rusqlite::Result<Vec<(String, Value, Value)>> {
    let binds: [rusqlite::types::Value; 3] = ["job_attempt".to_string().into(), db::qt(from).into(), db::qt(to).into()];
    let per_file = crate::storage::reader::aggregate(
        conn,
        "executions",
        "SELECT \"executions\".\"group_hash\" AS g, MAX(\"executions\".\"queue\") AS q, SUM(queue_latency) AS lat, COUNT(queue_latency) AS lat_n FROM {t} \
         WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"executions\".\"group_hash\"",
        "SELECT g, MAX(q), CAST(SUM(lat) AS REAL) / SUM(lat_n) FROM {u} GROUP BY g",
        &binds,
    )?;
    let rows = match per_file {
        Some(rows) => rows,
        None => db::query(
            conn,
            "SELECT \"executions\".\"group_hash\", MAX(\"executions\".\"queue\"), AVG(queue_latency) FROM \"executions\" \
             WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 GROUP BY \"executions\".\"group_hash\"",
            rusqlite::params_from_iter(binds.iter()),
        )?,
    };
    Ok(rows
        .iter()
        .filter_map(|r| {
            let g = match r.at(0) {
                rusqlite::types::Value::Text(t) => t.clone(),
                _ => return None,
            };
            let lat = match r.at(2) {
                rusqlite::types::Value::Real(f) => jf(rb::round(*f / 1000.0, 1)),
                rusqlite::types::Value::Integer(i) => jf(rb::round(*i as f64 / 1000.0, 1)),
                _ => Value::Null,
            };
            Some((g, db::raw_json(r.at(1)), lat))
        })
        .collect())
}

/// A page row's `as_json` time ("2026-10-15T02:20:00.123Z") as Time#to_s.
fn time_of(v: &Value) -> Value {
    let mut v = v.clone();
    super::to_s_times(&mut v);
    v
}
