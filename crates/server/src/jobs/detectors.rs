//! The scheduled detectors, ported from railwatch-cloud app/jobs:
//! DetectPerformanceIssuesJob, DetectAnomaliesJob, SilentHostCheckJob,
//! CheckScheduledTasksJob, ReleaseHealthCheckJob, AutoResolveIssuesJob,
//! QuotaCheckJob. Each takes the control connection and, where it reads
//! telemetry, the tenant's read-only connection.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::alerts::rules;
use crate::detection::snapshot;
use crate::grouping::issue::{self, Occurrence, Outcome};
use crate::rjson::{self, Value};
use crate::rollup::summary::{self, ROLLUP_COLUMNS, StoredRollup, Summary};
use crate::time;

pub fn type_for(target_kind: &str) -> Option<&'static str> {
    Some(match target_kind {
        "requests" => "request",
        "jobs" => "job_attempt",
        "commands" => "command",
        "queries" => "query",
        "scheduled_tasks" => "scheduled_task",
        "outgoing_requests" => "outgoing_request",
        "llm_calls" => "llm_call",
        "llm_tools" => "llm_tool",
        _ => return None,
    })
}

fn hour_floor(secs: i64) -> i64 {
    secs - secs.rem_euclid(3600)
}

/// `Rollup.for_type(type).between(from, to)[.where(name:)]`, busiest
/// `max_groups` groups, at most `max_rows` rows (None = skip, as Rails logs).
fn bounded_groups(
    t: &Connection,
    record_type: &str,
    from: i64,
    to: i64,
    target: &str,
    max_groups: usize,
    max_rows: usize,
) -> rusqlite::Result<Option<Vec<(String, String, Summary)>>> {
    let name_filter = if target == "*" { "" } else { " AND name = ?4" };
    let (f, tt) = (time::utc_seconds_string(hour_floor(from)), time::utc_seconds_string(to));
    let groups: Vec<String> = {
        let sql = format!(
            "SELECT group_hash FROM rollups WHERE record_type = ?1 AND bucket >= ?2 AND bucket <= ?3{name_filter} GROUP BY group_hash ORDER BY SUM(count) DESC, group_hash LIMIT {}",
            max_groups + 1
        );
        let mut s = t.prepare_cached(&sql)?;
        if target == "*" {
            s.query_map(params![record_type, f, tt], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        } else {
            s.query_map(params![record_type, f, tt, target], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        }
    };
    let mut groups = groups;
    if groups.len() > max_groups {
        tracing::warn!(record_type, "detector evaluated only the {max_groups} busiest groups");
        groups.pop();
    }
    if groups.is_empty() {
        return Ok(Some(vec![]));
    }
    let ph = vec!["?"; groups.len()].join(",");
    let sql = format!(
        "SELECT {ROLLUP_COLUMNS} FROM rollups WHERE record_type = ? AND bucket >= ? AND bucket <= ?{} AND group_hash IN ({ph}) ORDER BY bucket, group_hash LIMIT {}",
        if target == "*" { "" } else { " AND name = ?" },
        max_rows + 1
    );
    let mut binds: Vec<rusqlite::types::Value> = vec![record_type.to_string().into(), f.into(), tt.into()];
    if target != "*" {
        binds.push(target.to_string().into());
    }
    binds.extend(groups.iter().map(|g| rusqlite::types::Value::Text(g.clone())));
    let rows: Vec<StoredRollup> = t.prepare(&sql)?.query_map(rusqlite::params_from_iter(binds), summary::stored_from_row)?.collect::<rusqlite::Result<_>>()?;
    if rows.len() > max_rows {
        tracing::error!(record_type, "detector skipped: more than {max_rows} rollups");
        return Ok(None);
    }
    let mut by: indexmap::IndexMap<String, Vec<StoredRollup>> = indexmap::IndexMap::new();
    for r in rows {
        by.entry(r.group_hash.clone()).or_default().push(r);
    }
    Ok(Some(by.into_iter().map(|(g, rows)| (g, rows[0].name.clone(), summary::summarize(&rows))).collect()))
}

fn float_s(f: f64) -> String {
    rjson::ruby::float_to_s(f)
}

/// Threshold#format_value
fn format_value(metric: &str, v: f64, is_float: bool) -> String {
    let num = if is_float { float_s(v) } else { format!("{}", v as i64) };
    match metric {
        "spend" => if is_float { format!("${v:.2}") } else { format!("${num}") },
        "tokens" => num,
        "truncation_rate" | "error_rate" | "failure_rate" => format!("{num}%"),
        _ => format!("{num}ms"),
    }
}

/// `Float#round(2)` (MRI's round_half_up, dashboard::rb::round).
fn round2(f: f64) -> f64 {
    crate::dashboard::rb::round(f, 2)
}

/// DetectPerformanceIssuesJob#value_for
pub fn threshold_value(metric: &str, s: &Summary) -> Option<f64> {
    let ex = &s.extra;
    Some(match metric {
        "p95" => s.p95 as f64 / 1000.0,
        "max" => s.max as f64 / 1000.0,
        "avg" => s.avg as f64 / 1000.0,
        "error_rate" | "failure_rate" => {
            if s.count == 0 {
                return None;
            }
            s.errors as f64 * 100.0 / s.count as f64
        }
        "spend" => summary::extra_i(ex, "cost_nanos") as f64 / 1e9,
        "tokens" => (summary::extra_i(ex, "input_tokens") + summary::extra_i(ex, "output_tokens")) as f64,
        "truncation_rate" => {
            if s.count == 0 {
                return None;
            }
            summary::extra_i(ex, "truncated") as f64 * 100.0 / s.count as f64
        }
        _ => return None,
    })
}

/// DetectPerformanceIssuesJob#perform for one environment.
pub fn detect_performance(c: &Connection, t: &Connection, env: i64, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let thresholds: Vec<(i64, String, String, String, f64, i64)> = c
        .prepare("SELECT id, target_kind, target, metric, \"limit\", window_minutes FROM thresholds WHERE environment_id = ? ORDER BY id")
        .and_then(|mut s| s.query_map([env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect())
        .map_err(e)?;
    let mut opened = 0;
    for (id, kind, target, metric, limit, window) in thresholds {
        let Some(rt) = type_for(&kind) else { continue };
        let from = now - window * 60;
        let Some(groups) = bounded_groups(t, rt, from, now, &target, 200, 200 * 25).map_err(e)? else { continue };
        for (group_hash, name, s) in groups {
            let Some(value) = threshold_value(&metric, &s) else { continue };
            if value <= limit {
                continue;
            }
            // value_for answers an Integer for tokens (Integer#round(2) is
            // the Integer itself) and a Float for everything else.
            let integer = metric == "tokens";
            let rounded = if integer { Value::Int(value as i64) } else { Value::Float(round2(value)) };
            let title = format!(
                "{name} exceeded {metric} {} ({})",
                // thresholds.limit is a float column: Ruby formats it as a Float.
                format_value(&metric, limit, true),
                format_value(&metric, if integer { value } else { round2(value) }, !integer)
            );
            let sample = crate::rhash! { "threshold_id" => id, "value" => rounded.clone(), "metric" => metric.clone(), "limit" => limit };
            let issue_hash = format!("perf:{id}:{group_hash}");
            let previous = previous_sample(c, env, &issue_hash).map_err(e)?;
            let (iss, outcome) = record(c, env, &issue_hash, "performance", &title, Some(&name), now, sample)?;
            let newly_open = matches!(outcome, Outcome::New | Outcome::Regressed);
            snapshot::carry(c, &iss, newly_open, previous.as_ref(), || {
                snapshot::capture(t, &snapshot::Capture {
                    kind: "performance", telemetry_type: rt, telemetry_group_hash: &group_hash, target: &name, metric: &metric,
                    from: from * 1_000_000, to: now * 1_000_000, value: rounded.clone(), limit: Value::Float(limit), baseline: Value::Nil,
                    rule: crate::rhash! { "type" => "threshold", "id" => id, "target_kind" => kind.clone(), "target" => target.clone() },
                    representative_from: None,
                })
            })
            .map_err(e)?;
            if newly_open {
                rules::fire_for_issue_plain(c, &iss, "threshold").map_err(e)?;
                opened += 1;
            }
        }
    }
    Ok(opened)
}

#[allow(clippy::too_many_arguments)]
fn record(c: &Connection, env: i64, group_hash: &str, kind: &str, title: &str, culprit: Option<&str>, now: i64, sample: Value) -> Result<(issue::Issue, Outcome), String> {
    c.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let r = issue::record_occurrence(
        c,
        &Occurrence { environment_id: env, group_hash, kind, title, culprit, occurred_at: now * 1_000_000, deploy: None, sample, source: None },
    );
    match r {
        Ok(v) => {
            c.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            Ok(v)
        }
        Err(err) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// The sample of the issue record_occurrence! will bump for `group_hash`
/// (following merges): `issue.sample_previously_was` for carry_detection.
fn previous_sample(c: &Connection, env: i64, group_hash: &str) -> rusqlite::Result<Option<Value>> {
    let mut issue = issue::find_by_group(c, env, group_hash)?;
    for _ in 0..=issue::MERGE_CHAIN_LIMIT {
        match issue {
            Some(ref i) if i.status == "merged" || i.merged_into_id.is_some() => issue = i.merged_into_id.map(|id| issue::find(c, id)).transpose()?.flatten(),
            _ => break,
        }
    }
    Ok(issue.map(|i| i.sample))
}

/// DetectAnomaliesJob#value_for
fn anomaly_value(metric: &str, s: &Summary, window: i64) -> Option<f64> {
    Some(match metric {
        "p95" => s.p95 as f64 / 1000.0,
        "avg" => s.avg as f64 / 1000.0,
        "count" => s.count as f64 / window as f64,
        "error_rate" => {
            if s.count == 0 {
                return None;
            }
            s.errors as f64 * 100.0 / s.count as f64
        }
        "spend" => crate::dashboard::rb::round(summary::extra_i(&s.extra, "cost_nanos") as f64 / 1e9, 6),
        _ => return None,
    })
}

/// DetectAnomaliesJob#perform for one environment.
pub fn detect_anomalies(c: &Connection, t: &Connection, env: i64, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let rules_: Vec<(i64, String, String, String, f64, i64, i64)> = c
        .prepare("SELECT id, target_kind, target, metric, deviation, window_minutes, baseline_days FROM anomaly_rules WHERE environment_id = ? AND enabled = 1 ORDER BY id")
        .and_then(|mut s| s.query_map([env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect())
        .map_err(e)?;
    let mut opened = 0;
    for (id, kind, target, metric, deviation, window, days) in rules_ {
        let Some(rt) = type_for(&kind) else { continue };
        let from = now - window * 60;
        let Some(groups) = bounded_groups(t, rt, from, now, &target, 50, 50 * 25).map_err(e)? else { continue };
        if groups.is_empty() {
            continue;
        }
        // baseline_rows: the hour buckets the shifted windows cover.
        let mut buckets = Vec::new();
        for d in 1..=days {
            let mut b = hour_floor(from - d * 86_400);
            while b <= now - d * 86_400 {
                buckets.push(time::utc_seconds_string(b));
                b += 3600;
            }
        }
        let gh: Vec<&String> = groups.iter().map(|(g, _, _)| g).collect();
        let sql = format!(
            "SELECT {ROLLUP_COLUMNS} FROM rollups WHERE record_type = ? AND group_hash IN ({}) AND bucket IN ({}) ORDER BY bucket, group_hash LIMIT 50001",
            vec!["?"; gh.len()].join(","),
            vec!["?"; buckets.len().max(1)].join(",")
        );
        let mut binds: Vec<rusqlite::types::Value> = vec![rt.to_string().into()];
        binds.extend(gh.iter().map(|g| rusqlite::types::Value::Text((*g).clone())));
        if buckets.is_empty() {
            binds.push("".to_string().into());
        }
        binds.extend(buckets.iter().map(|b| rusqlite::types::Value::Text(b.clone())));
        let base: Vec<StoredRollup> = t.prepare(&sql).and_then(|mut s| s.query_map(rusqlite::params_from_iter(binds), summary::stored_from_row)?.collect()).map_err(e)?;
        if base.len() > 50_000 {
            tracing::error!(rule_id = id, "anomaly detector skipped: more than 50000 baseline rollups");
            continue;
        }
        for (group_hash, name, s) in groups {
            let issue_hash = format!("anomaly:{id}:{group_hash}");
            let cooling: bool = c
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM issues WHERE environment_id = ? AND group_hash = ? AND last_seen_at >= ?)",
                    params![env, issue_hash, time::utc_seconds_string(now - 3600)],
                    |r| r.get(0),
                )
                .map_err(e)?;
            if cooling {
                continue;
            }
            let Some(current) = anomaly_value(&metric, &s, window) else { continue };
            if (metric == "count" || metric == "error_rate") && s.count < 20 {
                continue;
            }
            let rows: Vec<&StoredRollup> = base.iter().filter(|r| r.group_hash == group_hash).collect();
            let baseline: Vec<f64> = (1..=days)
                .filter_map(|d| {
                    let lo = hour_floor(from - d * 86_400);
                    let hi = now - d * 86_400;
                    let day: Vec<StoredRollup> = rows.iter().filter(|r| r.bucket >= lo && r.bucket <= hi).map(|r| (*r).clone()).collect();
                    if day.is_empty() { None } else { anomaly_value(&metric, &summary::summarize(&day), window) }
                })
                .collect();
            if baseline.len() < 3 {
                continue;
            }
            let mean = baseline.iter().sum::<f64>() / baseline.len() as f64;
            let sd = (baseline.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / baseline.len() as f64).sqrt();
            if !(current > mean + deviation * sd && current > mean * 1.25) {
                continue;
            }
            let sigmas = if sd > 0.0 { (current - mean) / sd } else { deviation };
            let (cur, m, sdr, sg) = (round2(current), round2(mean), round2(sd), round2(sigmas));
            let title = format!("{metric} of {name} is {}σ above its {days}-day baseline ({} vs {})", float_s(crate::dashboard::rb::round(sg, 1)), float_s(cur), float_s(m));
            let sample = crate::rhash! {
                "rule_id" => id, "metric" => metric.clone(), "current" => cur, "mean" => m, "stddev" => sdr, "sigmas" => sg, "window_minutes" => window,
            };
            let previous = previous_sample(c, env, &issue_hash).map_err(e)?;
            let (iss, outcome) = record(c, env, &issue_hash, "anomaly", &title, Some(&name), now, sample)?;
            let newly_open = matches!(outcome, Outcome::New | Outcome::Regressed);
            snapshot::carry(c, &iss, newly_open, previous.as_ref(), || {
                snapshot::capture(t, &snapshot::Capture {
                    kind: "anomaly", telemetry_type: rt, telemetry_group_hash: &group_hash, target: &name, metric: &metric,
                    from: (now - window * 60) * 1_000_000, to: now * 1_000_000, value: Value::Float(cur), limit: Value::Nil,
                    baseline: crate::rhash! { "mean" => m, "stddev" => sdr, "sigmas" => sg, "days" => days, "deviation" => deviation },
                    rule: crate::rhash! { "type" => "anomaly", "id" => id, "target_kind" => kind.clone(), "target" => target.clone() },
                    representative_from: None,
                })
            })
            .map_err(e)?;
            if newly_open {
                let extra = crate::alerts::as_hash(crate::rhash! { "rule_id" => id, "metric" => metric.clone(), "current" => cur, "mean" => m, "sigmas" => sg });
                rules::fire_for_issue(c, &iss, "anomaly", extra).map_err(e)?;
                opened += 1;
            }
            c.execute("UPDATE anomaly_rules SET last_fired_at = ? WHERE id = ?", params![time::utc_seconds_string(now), id]).map_err(e)?;
        }
    }
    Ok(opened)
}

/// SilentHostCheckJob for one environment (expected_servers from Kamal).
pub fn silent_hosts(c: &Connection, t: &Connection, env: i64, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let app = issue::application_of(c, env).map_err(e)?;
    if rules::rules_for(c, app, "silent_host").map_err(e)?.is_empty() {
        return Ok(0);
    }
    let expected: String = c.query_row("SELECT expected_servers FROM environments WHERE id = ?", [env], |r| r.get(0)).map_err(e)?;
    let expected: Vec<String> = match rjson::parse(expected.as_bytes()) {
        Ok(Value::Array(a)) => a.iter().map(|v| String::from_utf8_lossy(&rjson::ruby::to_s(v)).into_owned()).collect(),
        _ => vec![],
    };
    if expected.is_empty() {
        return Ok(0);
    }
    let since = time::utc_seconds_string(now - 600);
    let mut seen: HashSet<String> = HashSet::new();
    for sql in [
        "SELECT DISTINCT server FROM health_samples WHERE sampled_at >= ?1".to_string(),
        "SELECT DISTINCT server FROM executions WHERE kind = 'request' AND occurred_at >= ?1".into(),
        "SELECT DISTINCT server FROM executions WHERE kind = 'job_attempt' AND occurred_at >= ?1".into(),
        "SELECT DISTINCT server FROM executions WHERE kind = 'scheduled_task' AND occurred_at >= ?1".into(),
        "SELECT DISTINCT server FROM executions WHERE kind = 'command' AND occurred_at >= ?1".into(),
        "SELECT DISTINCT server FROM executions WHERE kind = 'channel_action' AND occurred_at >= ?1".into(),
    ] {
        let mut s = t.prepare_cached(&sql).map_err(e)?;
        for v in s.query_map([&since], |r| r.get::<_, Option<String>>(0)).map_err(e)? {
            if let Some(v) = v.map_err(e)? {
                seen.insert(v);
            }
        }
    }
    let silent: Vec<&String> = expected.iter().filter(|s| !seen.contains(*s)).collect();
    let name = issue::environment_name(c, env);
    let display = issue::environment_display_name(c, env);
    let mut fired = 0;
    for server in &silent {
        if rules::alerted_recently(c, app, "silent_host", 6 * 3600, "server", &Value::from(server.as_str())).map_err(e)? {
            continue;
        }
        let reporting = expected.len() - silent.len();
        let payload = crate::alerts::as_hash(crate::rhash! {
            "server" => server.as_str(), "environment" => name.clone(), "environment_id" => env, "expected" => expected.len(), "reporting" => reporting,
            "message" => format!("{server} in {display} has reported nothing for 10 minutes ({reporting} of {} expected servers still reporting)", expected.len()),
        });
        fired += rules::fire_issueless(c, app, "silent_host", &payload).map_err(e)?.len();
    }
    Ok(fired)
}

/// ReleaseHealthCheckJob for one environment.
pub fn release_health_check(c: &Connection, t: &Connection, env: i64, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let app = issue::application_of(c, env).map_err(e)?;
    if rules::rules_for(c, app, "crash_free_drop").map_err(e)?.is_empty() {
        return Ok(0);
    }
    let Some((deploy, rf)) = c
        .query_row("SELECT deploy, ref FROM deploys WHERE environment_id = ? ORDER BY deployed_at DESC LIMIT 1", [env], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
        .optional()
        .map_err(e)?
    else {
        return Ok(0);
    };
    // `return if threshold.blank?`: a cleared threshold turns the check off.
    let Some(threshold) = c.query_row("SELECT crash_free_threshold FROM environments WHERE id = ?", [env], |r| r.get::<_, Option<f64>>(0)).map_err(e)? else {
        return Ok(0);
    };
    let (sessions, crashed): (i64, i64) = t
        .query_row(
            "SELECT COALESCE(SUM(sessions), 0), COALESCE(SUM(sessions_crashed), 0) FROM release_health WHERE bucket >= ? AND bucket <= ? AND deploy = ?",
            params![time::utc_seconds_string(hour_floor(now - 3600)), time::utc_seconds_string(now), deploy],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(e)?;
    if sessions == 0 || sessions < 20 {
        return Ok(0);
    }
    // ReleaseHealth.rate: (numerator * 100.0 / denominator).round(2)
    let rate = round2((sessions - crashed) as f64 * 100.0 / sessions as f64);
    if rate >= threshold || rules::alerted_recently(c, app, "crash_free_drop", 6 * 3600, "deploy", &Value::from(deploy.as_str())).map_err(e)? {
        return Ok(0);
    }
    let short = rf.as_deref().filter(|r| !r.is_empty()).map(|r| r.chars().take(7).collect::<String>()).unwrap_or_else(|| deploy.chars().take(12).collect());
    let name = issue::environment_name(c, env);
    let display = issue::environment_display_name(c, env);
    let payload = crate::alerts::as_hash(crate::rhash! {
        "deploy" => deploy.clone(), "ref" => short.clone(), "crash_free_sessions" => rate, "threshold" => threshold, "sessions" => sessions,
        "environment" => name.clone(), "environment_id" => env,
        "message" => format!("{short} in {display} is {}% crash-free over the last hour ({sessions} sessions), below the {}% threshold", float_s(rate), float_s(threshold)),
    });
    Ok(rules::fire_issueless(c, app, "crash_free_drop", &payload).map_err(e)?.len())
}

/// AutoResolveIssuesJob: per account with auto_resolve_after_days set (nil
/// = off), its open issues last seen before that many days ago are resolved
/// with no deploy (record_status_change! fires resolved_issue).
pub fn auto_resolve(c: &Connection, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let accounts: Vec<(i64, i64)> = c
        .prepare("SELECT id, auto_resolve_after_days FROM accounts WHERE auto_resolve_after_days IS NOT NULL ORDER BY id")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
        .map_err(e)?;
    let mut n = 0;
    for (account, days) in accounts {
        let cutoff = time::quoted_date_micros(now * 1_000_000 - days * 86_400_000_000);
        let ids: Vec<i64> = c
            .prepare(
                "SELECT i.id FROM issues i JOIN applications ap ON ap.id = i.application_id \
                 WHERE i.status = 'open' AND ap.account_id = ? AND i.last_seen_at < ? ORDER BY i.id",
            )
            .and_then(|mut s| s.query_map(params![account, cutoff], |r| r.get(0))?.collect())
            .map_err(e)?;
        for id in ids {
            if let Some(i) = issue::find(c, id).map_err(e)?
                && issue::resolve(c, &i, None).map_err(e)?
            {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// QuotaCheckJob: per account, one "quota" alert at 80% and one at 100% of
/// monthly_event_quota + additional_events_cap, remembered in
/// accounts.quota_alerted_80_at / _100_at (the month's first day) and reset
/// when the month rolls over. A quota rule of the account's applications
/// fires when any environment of its own application matches it
/// (`rule.matches?` with that environment_id); the payload itself carries
/// no environment_id.
pub fn quota_check(c: &Connection, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let month = format!("{}-01", crate::control::month_of(now * 1_000_000));
    type AccountRow = (i64, String, i64, i64, Option<String>, Option<String>);
    let accounts: Vec<AccountRow> = c
        .prepare("SELECT id, name, monthly_event_quota, additional_events_cap, quota_alerted_80_at, quota_alerted_100_at FROM accounts ORDER BY id")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect())
        .map_err(e)?;
    let mut fired = 0;
    for (id, name, quota, cap, mut at80, mut at100) in accounts {
        // reset_for_new_month
        let stale = |d: &Option<String>| d.as_deref().is_some_and(|d| d < month.as_str());
        if stale(&at80) || stale(&at100) {
            if stale(&at80) {
                at80 = None;
                c.execute("UPDATE accounts SET quota_alerted_80_at = NULL WHERE id = ?", [id]).map_err(e)?;
            }
            if stale(&at100) {
                at100 = None;
                c.execute("UPDATE accounts SET quota_alerted_100_at = NULL WHERE id = ?", [id]).map_err(e)?;
            }
        }
        // Account#events_this_month / #quota_percent
        let used: i64 = c
            .query_row(
                "SELECT COALESCE(SUM(m.events), 0) FROM ingest_usage_months m WHERE m.month = ?2 AND m.environment_id IN \
                   (SELECT e.id FROM environments e JOIN applications ap ON ap.id = e.application_id WHERE ap.account_id = ?1)",
                params![id, month],
                |r| r.get(0),
            )
            .map_err(e)?;
        let total = quota + cap;
        let percent = if total == 0 { 0.0 } else { used as f64 / total as f64 * 100.0 };
        for (threshold, already, column) in [(80i64, &at80, "quota_alerted_80_at"), (100, &at100, "quota_alerted_100_at")] {
            if percent < threshold as f64 || already.is_some() {
                continue;
            }
            let payload = crate::alerts::as_hash(crate::rhash! {
                "message" => format!("{name} reached {threshold}% of its monthly event quota"), "account" => name.clone(), "threshold" => threshold,
                "events_this_month" => used, "quota" => quota,
            });
            let rules_: Vec<rules::AlertRule> = c
                .prepare(
                    "SELECT r.id FROM alert_rules r JOIN applications ap ON ap.id = r.application_id WHERE ap.account_id = ? AND r.event = 'quota' ORDER BY r.id",
                )
                .and_then(|mut s| s.query_map([id], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<i64>>>())
                .map_err(e)?
                .into_iter()
                .filter_map(|rid| rules::find(c, rid).transpose())
                .collect::<rusqlite::Result<_>>()
                .map_err(e)?;
            for rule in &rules_ {
                let envs: Vec<i64> = c
                    .prepare_cached("SELECT id FROM environments WHERE application_id = ? ORDER BY id")
                    .and_then(|mut s| s.query_map([rule.application_id], |r| r.get(0))?.collect())
                    .map_err(e)?;
                let matched = envs.into_iter().any(|env| {
                    let mut p = payload.clone();
                    p.insert(b"environment_id".to_vec(), Value::Int(env));
                    rule.matches(&rules::Subject { issue: None, payload: &p })
                });
                if matched && rules::fire(c, rule, "quota", None, &payload).map_err(e)?.is_some() {
                    fired += 1;
                }
            }
            c.execute(&format!("UPDATE accounts SET {column} = ? WHERE id = ?"), params![month, id]).map_err(e)?;
        }
    }
    Ok(fired)
}

/// CheckScheduledTasksJob for one environment.
pub fn check_scheduled_tasks(c: &Connection, t: &Connection, env: i64, now: i64) -> Result<usize, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let since = now - 30 * 86_400;
    let mut groups: Vec<String> = t
        .prepare("SELECT DISTINCT group_hash FROM rollups WHERE record_type = 'scheduled_task' AND bucket >= ?")
        .and_then(|mut s| s.query_map([time::utc_seconds_string(hour_floor(since))], |r| r.get(0))?.collect())
        .map_err(e)?;
    let fresh: Vec<Option<String>> = t
        .prepare("SELECT DISTINCT group_hash FROM executions WHERE kind = 'scheduled_task' AND occurred_at >= ?")
        .and_then(|mut s| s.query_map([time::utc_seconds_string(now - 7200)], |r| r.get(0))?.collect())
        .map_err(e)?;
    for g in fresh.into_iter().flatten() {
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    // `.group_by(&:first)`: tasks in the order their groups were found.
    let mut last: indexmap::IndexMap<String, i64> = indexmap::IndexMap::new();
    for g in &groups {
        let r: Option<(Option<String>, String)> = t
            .query_row(
                "SELECT task_key, occurred_at FROM executions WHERE kind = 'scheduled_task' AND group_hash = ? AND occurred_at > ? ORDER BY occurred_at DESC LIMIT 1",
                params![g, time::utc_seconds_string(since)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(e)?;
        if let Some((Some(k), at)) = r {
            // Microseconds: representative_from is the exact last run.
            let at = time::parse_db_time(&at).unwrap_or(0);
            let x = last.entry(k).or_insert(at);
            *x = (*x).max(at);
        }
    }
    let configured: Option<Vec<String>> = t
        .query_row(
            "SELECT json_extract(detail, '$.recurring_tasks') FROM health_samples WHERE sampled_at >= ? AND json_extract(detail, '$.recurring_tasks') IS NOT NULL ORDER BY sampled_at DESC LIMIT 1",
            [time::utc_seconds_string(now - 600)],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(e)?
        .flatten()
        .and_then(|m| match rjson::parse(m.as_bytes()) {
            Ok(Value::Hash(h)) => Some(h.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect()),
            _ => None,
        });
    let last_deploy: Option<i64> = c
        .query_row("SELECT MAX(deployed_at) FROM deploys WHERE environment_id = ?", [env], |r| r.get::<_, Option<String>>(0))
        .map_err(e)?
        .and_then(|s| time::parse_db_time(&s))
        .map(|m| m / 1_000_000);
    let mut flagged = 0;
    for (key, last_run_us) in last {
        let last_run = last_run_us.div_euclid(1_000_000);
        let schedule: Option<String> = t
            .query_row(
                "SELECT json_extract(detail, '$.schedule') FROM executions WHERE kind = 'scheduled_task' AND task_key = ? ORDER BY occurred_at DESC LIMIT 1",
                [&key],
                |r| r.get(0),
            )
            .optional()
            .map_err(e)?
            .flatten();
        let Some(schedule) = schedule.filter(|s| !s.trim().is_empty()) else { continue };
        let open_missed = issue::find_by_group(c, env, &format!("missed:{key}")).map_err(e)?.filter(|i| i.status == "open");
        if configured.as_ref().is_some_and(|k| !k.contains(&key)) {
            resolve_missed(c, open_missed.as_ref(), &format!("Auto-resolved: {key} is no longer a configured recurring task."))?;
            continue;
        }
        let Some(expected) = super::schedule::next_after(&schedule, last_run) else { continue };
        if expected > now - 300 {
            resolve_missed(c, open_missed.as_ref(), &format!("Auto-resolved: {key} ran again on schedule."))?;
            continue;
        }
        if last_deploy.is_some_and(|d| expected >= d - 900 && expected <= d + 900) {
            continue;
        }
        let title = format!("Scheduled task {key} missed its run (expected {})", time::iso8601(expected));
        let sample = crate::rhash! {
            "task_key" => key.clone(), "schedule" => schedule.clone(), "last_run" => time::iso8601(last_run), "expected" => time::iso8601(expected),
        };
        let issue_hash = format!("missed:{key}");
        let previous = previous_sample(c, env, &issue_hash).map_err(e)?;
        let (iss, outcome) = record(c, env, &issue_hash, "performance", &title, Some(&key), now, sample)?;
        let newly_open = matches!(outcome, Outcome::New | Outcome::Regressed);
        // carry_detection: a :seen occurrence keeps the snapshot the
        // opening scan captured (record_occurrence! replaced `sample`).
        snapshot::carry(c, &iss, newly_open, previous.as_ref(), || {
            let latest: String = t.query_row(
                "SELECT group_hash FROM executions WHERE kind = 'scheduled_task' AND task_key = ? ORDER BY occurred_at DESC LIMIT 1",
                [&key],
                |r| r.get(0),
            )?;
            snapshot::capture(t, &snapshot::Capture {
                kind: "performance", telemetry_type: "scheduled_task", telemetry_group_hash: &latest, target: &key, metric: "missed",
                from: expected * 1_000_000, to: now * 1_000_000, value: Value::Nil, limit: Value::Nil, baseline: Value::Nil,
                rule: crate::rhash! {
                    "type" => "schedule", "task_key" => key.clone(), "schedule" => schedule.clone(),
                    "last_run" => time::iso8601(last_run), "expected" => time::iso8601(expected),
                },
                representative_from: Some(last_run_us),
            })
        })
        .map_err(e)?;
        if newly_open {
            rules::fire_for_issue_plain(c, &iss, "threshold").map_err(e)?;
            flagged += 1;
        }
    }
    Ok(flagged)
}

fn resolve_missed(c: &Connection, issue: Option<&issue::Issue>, body: &str) -> Result<(), String> {
    let Some(i) = issue else { return Ok(()) };
    let e = |e: rusqlite::Error| e.to_string();
    issue::resolve(c, i, None).map_err(e)?;
    issue::add_activity(c, i.id, "comment", crate::rhash! { "body" => body }).map_err(e)
}
