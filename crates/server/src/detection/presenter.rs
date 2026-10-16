//! IssueDetectionPresenter#as_json (railwatch-cloud
//! app/models/issue_detection_presenter.rb): the stored snapshot, validated,
//! with live trend, deploy comparison and representative records read from
//! the tenant. Times in `representative_records` are `as_json` strings
//! (rb::t_json); MCP rewrites them to Time#to_s as JSON.generate would.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::dashboard::db::{jf, jfo};
use crate::dashboard::rb;
use crate::rollup::summary::{self, ROLLUP_COLUMNS, Summary};

const TREND_DAYS: i64 = 30;
const DEPLOY_LIMIT: usize = 20;
const SCHEMA_VERSION: i64 = 1;
const RULE_STRING_FIELDS: &[&str] = &["type", "target_kind", "target", "task_key", "schedule", "last_run", "expected"];
const EXECUTION_SOURCES: &[&str] = &["request", "job", "job_attempt", "scheduled_task", "command"];
const CHILD_TYPES: &[&str] = &["query", "outgoing_request", "llm_call", "llm_tool"];
const ANOMALY_TARGET_KINDS: &[&str] = &["requests", "jobs", "queries", "outgoing_requests", "scheduled_tasks", "llm_calls"];
const ANOMALY_METRICS: &[&str] = &["p95", "avg", "count", "error_rate", "spend"];
const THRESHOLD_TARGET_KINDS: &[&str] = &["requests", "jobs", "commands", "queries", "scheduled_tasks", "outgoing_requests", "llm_calls", "llm_tools"];
const THRESHOLD_METRICS: &[&str] = &["p95", "max", "avg", "error_rate", "failure_rate", "missed", "spend", "tokens", "truncation_rate"];
const MAX_WINDOW_MINUTES: f64 = 1_440.0;
const THRESHOLD_MAX_LIMIT: f64 = 1_000_000_000.0;
const MIN_BASELINE_DAYS: f64 = 3.0;
const MAX_BASELINE_DAYS: f64 = 30.0;
const MAX_DEVIATION: f64 = 10.0;

fn target_kind_for(telemetry_type: &str) -> Option<&'static str> {
    Some(match telemetry_type {
        "request" => "requests",
        "job_attempt" => "jobs",
        "command" => "commands",
        "query" => "queries",
        "scheduled_task" => "scheduled_tasks",
        "outgoing_request" => "outgoing_requests",
        "llm_call" => "llm_calls",
        "llm_tool" => "llm_tools",
        _ => return None,
    })
}

/// What the presenter reads off the issue.
pub struct Presented<'a> {
    pub kind: &'a str,
    pub occurrences: i64,
    /// issues.sample, parsed.
    pub sample: &'a Value,
}

/// A raised KeyError / ArgumentError / TypeError: render unavailable_json.
struct Invalid;
type V<T> = Result<T, Invalid>;

fn check(ok: bool) -> V<()> {
    if ok { Ok(()) } else { Err(Invalid) }
}

/// `hash.fetch(key)`
fn fetch<'a>(h: &'a Value, key: &str) -> V<&'a Value> {
    h.as_object().ok_or(Invalid)?.get(key).ok_or(Invalid)
}

fn present_str(v: &Value) -> bool {
    v.as_str().is_some_and(|s| rb::present(Some(s)))
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|_| v.is_number())
}

fn is_integer(v: &Value) -> bool {
    v.is_i64() || v.is_u64()
}

/// `Time.iso8601(s)`, epoch microseconds; Err for what Ruby rejects.
fn parse_iso8601(s: &str) -> V<i64> {
    let b = s.as_bytes();
    check(b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':')?;
    let (body, offset) = if let Some(body) = s.strip_suffix('Z') {
        (body, 0i64)
    } else {
        check(b.len() >= 25)?;
        let (body, off) = s.split_at(s.len() - 6);
        let ob = off.as_bytes();
        check((ob[0] == b'+' || ob[0] == b'-') && ob[3] == b':')?;
        let h: i64 = off[1..3].parse().map_err(|_| Invalid)?;
        let m: i64 = off[4..6].parse().map_err(|_| Invalid)?;
        let sign = if ob[0] == b'-' { -1 } else { 1 };
        (body, sign * (h * 3600 + m * 60))
    };
    check(body.len() == 19 || (body.len() > 20 && body.as_bytes()[19] == b'.'))?;
    check(body[..19].bytes().enumerate().all(|(i, c)| [4, 7, 10, 13, 16].contains(&i) || c.is_ascii_digit()))?;
    check(body.len() == 19 || body[20..].bytes().all(|c| c.is_ascii_digit()))?;
    let micros = crate::time::parse_db_time(body).ok_or(Invalid)?;
    Ok(micros - offset * 1_000_000)
}

/// IssueDetectionPresenter#as_json. `now` is Time.current (epoch micros).
pub fn as_json(issue: &Presented<'_>, t: &Connection, now: i64) -> rusqlite::Result<Value> {
    if issue.kind != "performance" && issue.kind != "anomaly" {
        return Ok(Value::Null);
    }
    let details = match issue.sample.get("detection") {
        Some(d @ Value::Object(_)) => d,
        _ => return Ok(unavailable_json(issue)),
    };
    let Ok(window) = validate_details(issue, details) else { return Ok(unavailable_json(issue)) };
    let mut out = details.as_object().cloned().unwrap_or_default();
    let (event_count, trend, deploys) = live_details(details, window, t, now)?;
    let references = representative_references(details);
    let records = representatives(details, t)?;
    for (k, v) in [
        ("available", json!(true)),
        ("count_label", json!("Breached evaluation windows")),
        ("breached_windows", json!(issue.occurrences)),
        ("event_count", json!(event_count)),
        ("trend", trend),
        ("deploy_comparison", deploys),
        ("representative_references", references),
        ("representative_records", records),
    ] {
        out.insert(k.into(), v);
    }
    Ok(Value::Object(out))
}

/// IssueDetectionPresenter#unavailable_json
pub fn unavailable_json(issue: &Presented<'_>) -> Value {
    json!({
        "kind": issue.kind, "available": false,
        "count_label": "Breached evaluation windows", "breached_windows": issue.occurrences,
        "message": "Typed detector context was not captured for this historical issue.",
    })
}

/// validate_details!; Ok((from, to)) of the window.
fn validate_details(issue: &Presented<'_>, d: &Value) -> V<(i64, i64)> {
    check(fetch(d, "schema_version")?.as_i64() == Some(SCHEMA_VERSION) && is_integer(fetch(d, "schema_version")?))?;
    check(fetch(d, "kind")?.as_str() == Some(issue.kind))?;
    let telemetry_type = fetch(d, "telemetry_type")?.as_str().ok_or(Invalid)?;
    check(super::EXECUTION_TYPES.contains(&telemetry_type) || CHILD_TYPES.contains(&telemetry_type))?;
    let target_kind = target_kind_for(telemetry_type).ok_or(Invalid)?;
    let anomaly = issue.kind == "anomaly";
    check(if anomaly { ANOMALY_TARGET_KINDS } else { THRESHOLD_TARGET_KINDS }.contains(&target_kind))?;
    for key in ["telemetry_group_hash", "target", "metric", "unit"] {
        check(present_str(fetch(d, key)?))?;
    }
    let metric = fetch(d, "metric")?.as_str().ok_or(Invalid)?;
    check(if anomaly { ANOMALY_METRICS } else { THRESHOLD_METRICS }.contains(&metric))?;
    check(!(metric == "missed" && telemetry_type != "scheduled_task"))?;
    check(super::unit(metric).is_some_and(|u| fetch(d, "unit").ok().and_then(Value::as_str) == Some(u)))?;
    let window = validate_window(issue, d, metric)?;
    validate_numeric_object(d, "measurement")?;
    validate_numeric_object(d, "baseline")?;
    validate_rule(d, metric)?;
    validate_detector_shape(issue, d, metric, telemetry_type)?;
    validate_representatives(d, telemetry_type)?;
    Ok(window)
}

fn validate_window(issue: &Presented<'_>, d: &Value, metric: &str) -> V<(i64, i64)> {
    let window = fetch(d, "window")?;
    check(window.is_object())?;
    let from = parse_iso8601(fetch(window, "from")?.as_str().ok_or(Invalid)?)?;
    let to = parse_iso8601(fetch(window, "to")?.as_str().ok_or(Invalid)?)?;
    let minutes = number(fetch(window, "minutes")?).ok_or(Invalid)?;
    check(minutes > 0.0 && to > from)?;
    check((minutes - rb::round(rb::time_sub(to, from) / 60.0, 2)).abs() <= 0.01)?;
    if metric == "missed" {
        return Ok((from, to));
    }
    check(minutes == minutes.trunc())?;
    let _ = issue;
    check(minutes <= MAX_WINDOW_MINUTES)?;
    Ok((from, to))
}

fn validate_numeric_object(d: &Value, key: &str) -> V<()> {
    let Some(v) = d.get(key).filter(|v| !v.is_null()) else { return Ok(()) };
    let o = v.as_object().ok_or(Invalid)?;
    let fields: &[&str] = if key == "measurement" { &["value", "limit"] } else { &["mean", "stddev", "sigmas", "days", "deviation"] };
    for f in fields {
        if let Some(x) = o.get(*f) {
            check(number(x).is_some())?;
        }
    }
    let n = |f: &str| o.get(f).and_then(number);
    if key == "measurement" {
        for f in ["value", "limit"] {
            check(!n(f).is_some_and(|x| x < 0.0))?;
        }
        check(!o.contains_key("limit") || n("limit").is_some_and(|x| x > 0.0))?;
    } else {
        for f in ["mean", "stddev"] {
            check(!n(f).is_some_and(|x| x < 0.0))?;
        }
        check(!o.contains_key("days") || (o.get("days").is_some_and(is_integer) && n("days").is_some_and(|x| x > 0.0)))?;
        check(!o.contains_key("deviation") || n("deviation").is_some_and(|x| x > 0.0))?;
    }
    Ok(())
}

fn validate_rule(d: &Value, metric: &str) -> V<()> {
    let rule = match d.get("rule") {
        None | Some(Value::Null) => return check(metric != "missed"),
        Some(r) => r.as_object().ok_or(Invalid)?,
    };
    for f in RULE_STRING_FIELDS {
        if let Some(v) = rule.get(*f) {
            check(present_str(v))?;
        }
    }
    if let Some(id) = rule.get("id") {
        check(is_integer(id) && id.as_i64().is_some_and(|i| i > 0))?;
    }
    for f in ["last_run", "expected"] {
        if let Some(v) = rule.get(f) {
            parse_iso8601(v.as_str().ok_or(Invalid)?)?;
        }
    }
    if metric != "missed" {
        return Ok(());
    }
    check(rule.get("type").and_then(Value::as_str) == Some("schedule"))?;
    for f in ["task_key", "schedule", "last_run", "expected"] {
        check(rule.get(f).is_some_and(present_str))?;
    }
    Ok(())
}

fn require_numeric_fields<'a>(d: &'a Value, key: &str, fields: &[&str]) -> V<&'a Map<String, Value>> {
    let o = fetch(d, key)?.as_object().ok_or(Invalid)?;
    for f in fields {
        check(number(o.get(*f).ok_or(Invalid)?).is_some())?;
    }
    Ok(o)
}

fn require_detector_rule(d: &Value, kind: &str, telemetry_type: &str) -> V<()> {
    let rule = fetch(d, "rule")?;
    check(rule.is_object() && fetch(rule, "type")?.as_str() == Some(kind))?;
    let id = fetch(rule, "id")?;
    check(is_integer(id) && id.as_i64().is_some_and(|i| i > 0))?;
    check(present_str(fetch(rule, "target")?))?;
    check(fetch(rule, "target_kind")?.as_str() == target_kind_for(telemetry_type))
}

fn validate_detector_shape(issue: &Presented<'_>, d: &Value, metric: &str, telemetry_type: &str) -> V<()> {
    if issue.kind == "anomaly" {
        require_numeric_fields(d, "measurement", &["value"])?;
        let b = require_numeric_fields(d, "baseline", &["mean", "stddev", "sigmas", "days", "deviation"])?;
        let n = |f: &str| b.get(f).and_then(number).unwrap_or(0.0);
        check(n("sigmas") > 0.0)?;
        check((MIN_BASELINE_DAYS..=MAX_BASELINE_DAYS).contains(&n("days")))?;
        check(n("deviation") <= MAX_DEVIATION)?;
        require_detector_rule(d, "anomaly", telemetry_type)
    } else if metric != "missed" {
        let m = require_numeric_fields(d, "measurement", &["value", "limit"])?;
        let limit = m.get("limit").and_then(number).unwrap_or(0.0);
        check(limit <= THRESHOLD_MAX_LIMIT)?;
        check(!(metric.ends_with("rate") && limit > 100.0))?;
        require_detector_rule(d, "threshold", telemetry_type)
    } else {
        Ok(())
    }
}

fn validate_representatives(d: &Value, telemetry_type: &str) -> V<()> {
    let refs = match d.get("representative_records") {
        None => return Ok(()),
        Some(r) => r.as_array().ok_or(Invalid)?,
    };
    check(refs.len() <= super::REPRESENTATIVE_LIMIT)?;
    let group = fetch(d, "telemetry_group_hash")?;
    for r in refs {
        let o = r.as_object().ok_or(Invalid)?;
        let get = |k: &str| o.get(k).unwrap_or(&Value::Null);
        check(get("record_type").as_str() == Some(telemetry_type))?;
        check(is_integer(get("record_id")) && get("record_id").as_i64().is_some_and(|i| i > 0))?;
        check(get("group_hash") == group)?;
        let execution_id = get("execution_id");
        check(execution_id.is_null() || present_str(execution_id))?;
        check(telemetry_type == "query" || present_str(execution_id))?;
        let source = get("execution_source");
        if CHILD_TYPES.contains(&telemetry_type) {
            check(present_str(source) && EXECUTION_SOURCES.contains(&source.as_str().unwrap_or("")))?;
        } else {
            check(source.as_str() == Some(telemetry_type))?;
        }
    }
    Ok(())
}

fn rollups(t: &Connection, record_type: &str, group_hash: &str, from: i64, to: i64, ordered: bool) -> rusqlite::Result<Vec<summary::StoredRollup>> {
    let sql = format!(
        "SELECT {ROLLUP_COLUMNS} FROM rollups WHERE record_type = ? AND group_hash = ? AND bucket >= ? AND bucket <= ?{}",
        if ordered { " ORDER BY bucket ASC" } else { "" }
    );
    t.prepare(&sql)?
        .query_map(params![record_type, group_hash, crate::time::quoted_date_micros(rb::beginning_of_hour(from)), crate::time::quoted_date_micros(to)], summary::stored_from_row)?
        .collect()
}

fn milliseconds(v: Option<f64>) -> Value {
    jfo(v.map(|v| rb::round(v / 1000.0, 3)))
}

/// live_details: event_count, trend, deploy_comparison.
fn live_details(d: &Value, (from, to): (i64, i64), t: &Connection, now: i64) -> rusqlite::Result<(i64, Value, Value)> {
    let ty = d["telemetry_type"].as_str().unwrap_or("");
    let group = d["telemetry_group_hash"].as_str().unwrap_or("");
    let metric = d["metric"].as_str().unwrap_or("");
    let event_count = summary::summarize(&rollups(t, ty, group, from, to, false)?).count;
    let trend_from = rb::beginning_of_hour(now - TREND_DAYS * 86_400_000_000);
    let rows = rollups(t, ty, group, trend_from, now, true)?;
    let mut days: indexmap::IndexMap<i64, Vec<summary::StoredRollup>> = indexmap::IndexMap::new();
    for r in rows {
        days.entry(r.bucket.div_euclid(86_400)).or_default().push(r);
    }
    let trend: Vec<Value> = days
        .into_iter()
        .map(|(day, rows)| {
            let s = summary::summarize(&rows);
            let day_start = day * 86_400_000_000;
            let minutes = rb::time_sub((day_start + 86_400_000_000).min(now), day_start.max(trend_from)) / 60.0;
            json!({"day": &crate::time::utc_seconds_string(day * 86_400)[..10], "value": metric_value(metric, &s, minutes), "count": s.count})
        })
        .collect();
    Ok((event_count, Value::Array(trend), deploy_comparison(t, ty, group, now)?))
}

fn metric_value(metric: &str, s: &Summary, minutes: f64) -> Value {
    let ms = |v: i64| milliseconds(Some(v as f64));
    match metric {
        "p95" => ms(s.p95),
        "max" => ms(s.max),
        "avg" => ms(s.avg),
        "error_rate" | "failure_rate" => {
            if s.count == 0 { Value::Null } else { jf(rb::round(s.errors as f64 * 100.0 / s.count as f64, 2)) }
        }
        "count" => {
            if minutes > 0.0 { jf(rb::round(s.count as f64 / minutes, 2)) } else { Value::Null }
        }
        "spend" => jf(rb::round(summary::extra_i(&s.extra, "cost_nanos") as f64 / 1e9, 6)),
        "tokens" => json!(summary::extra_i(&s.extra, "input_tokens") + summary::extra_i(&s.extra, "output_tokens")),
        "truncation_rate" => {
            if s.count == 0 { Value::Null } else { jf(rb::round(summary::extra_i(&s.extra, "truncated") as f64 * 100.0 / s.count as f64, 2)) }
        }
        _ => Value::Null,
    }
}

/// record_scope's FROM/WHERE for `type`: (table, extra where, binds-first).
fn scope(ty: &str) -> Option<(&'static str, bool)> {
    super::table_for(ty)
}

fn deploy_comparison(t: &Connection, ty: &str, group: &str, now: i64) -> rusqlite::Result<Value> {
    let Some((table, execution)) = scope(ty) else { return Ok(json!([])) };
    let kind = if execution { " AND kind = ?3" } else { "" };
    let sql = format!(
        "SELECT deploy, COUNT(*), AVG(duration), MAX(duration) FROM \"{table}\" WHERE group_hash = ?1{kind} AND occurred_at >= ?2 AND deploy IS NOT NULL GROUP BY deploy"
    );
    let since = crate::time::quoted_date_micros(now - TREND_DAYS * 86_400_000_000);
    let row = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<i64>>(3)?));
    let mut s = t.prepare(&sql)?;
    let rows: Vec<(String, i64, Option<f64>, Option<i64>)> =
        if execution { s.query_map(params![group, since, ty], row)?.collect::<rusqlite::Result<_>>()? } else { s.query_map(params![group, since], row)?.collect::<rusqlite::Result<_>>()? };
    let sorted = rb::sort_by_num(rows, |r| rb::Num::I(-r.1));
    Ok(Value::Array(
        sorted
            .into_iter()
            .take(DEPLOY_LIMIT)
            .map(|(deploy, events, avg, max)| json!({"deploy": deploy, "events": events, "avg_ms": milliseconds(avg), "max_ms": milliseconds(max.map(|m| m as f64))}))
            .collect(),
    ))
}

fn drilldown(r: &Value) -> Value {
    if r["record_type"].as_str() == Some("query") {
        json!({"kind": "query", "group_hash": r["group_hash"]})
    } else if present_str(&r["execution_id"]) {
        let kind = if present_str(&r["execution_source"]) { r["execution_source"].clone() } else { r["record_type"].clone() };
        json!({"kind": kind, "execution_id": r["execution_id"]})
    } else {
        Value::Null
    }
}

fn reference_list(d: &Value) -> Vec<&Value> {
    match d.get("representative_records") {
        Some(Value::Array(a)) => a.iter().filter(|r| r.is_object()).collect(),
        Some(v) if v.is_object() => vec![v],
        _ => vec![],
    }
}

fn representative_references(d: &Value) -> Value {
    Value::Array(
        reference_list(d)
            .into_iter()
            .map(|r| {
                let mut o = Map::new();
                for k in ["record_type", "record_id", "group_hash", "execution_id", "execution_source"] {
                    if let Some(v) = r.get(k) {
                        o.insert(k.into(), v.clone());
                    }
                }
                o.insert("drilldown".into(), drilldown(r));
                Value::Object(o)
            })
            .collect(),
    )
}

fn representatives(d: &Value, t: &Connection) -> rusqlite::Result<Value> {
    let refs = reference_list(d);
    if refs.is_empty() {
        return Ok(json!([]));
    }
    let ty = d["telemetry_type"].as_str().unwrap_or("");
    let group = d["telemetry_group_hash"].as_str().unwrap_or("");
    let Some((table, execution)) = scope(ty) else { return Ok(json!([])) };
    let common = "id, group_hash, execution_id, execution_source, execution_preview, occurred_at, duration, deploy, user_ref, app_tenant";
    let (cols, kind) = match table {
        "executions" => ("name, status, outcome", " AND kind = ?3"),
        "queries" => ("COALESCE(NULLIF(queries.sql, ''), (SELECT query_shapes.sql FROM query_shapes WHERE query_shapes.group_hash = queries.group_hash), ''), source, connection, role", ""),
        "outgoing_requests" => ("method || ' ' || host, status_code, source", ""),
        _ => ("operation, tool_name, provider, model, status, finish_reason, input_tokens, output_tokens, cost_nanos, attachments, attachment_types", ""),
    };
    let sql = format!("SELECT {common}, {cols} FROM \"{table}\" WHERE group_hash = ?1 AND id = ?2{kind}");
    let mut s = t.prepare(&sql)?;
    let mut out = Vec::new();
    for r in refs {
        let Some(id) = r["record_id"].as_i64().filter(|_| is_integer(&r["record_id"])) else { continue };
        let row = |x: &rusqlite::Row<'_>| record_json(x, ty, table);
        let found = if execution { s.query_row(params![group, id, ty], row).optional()? } else { s.query_row(params![group, id], row).optional()? };
        if let Some(Value::Object(mut o)) = found {
            o.insert("drilldown".into(), drilldown(r));
            out.push(Value::Object(o));
        }
    }
    Ok(Value::Array(out))
}

fn record_json(x: &rusqlite::Row<'_>, ty: &str, table: &str) -> rusqlite::Result<Value> {
    let s = |i: usize| x.get::<_, Option<String>>(i).map(|v| v.map_or(Value::Null, Value::String));
    let i = |i: usize| x.get::<_, Option<i64>>(i).map(|v| v.map_or(Value::Null, Value::from));
    let at: String = x.get(5)?;
    let mut o = Map::new();
    o.insert("record_type".into(), json!(ty));
    o.insert("record_id".into(), json!(x.get::<_, i64>(0)?));
    o.insert("group_hash".into(), s(1)?);
    o.insert("execution_id".into(), s(2)?);
    o.insert("execution_source".into(), s(3)?);
    o.insert("execution_preview".into(), s(4)?);
    o.insert("occurred_at".into(), json!(rb::t_json(crate::time::parse_db_time(&at).unwrap_or(0))));
    o.insert("duration_ms".into(), milliseconds(x.get::<_, Option<i64>>(6)?.map(|d| d as f64)));
    o.insert("deploy".into(), s(7)?);
    o.insert("user_ref".into(), s(8)?);
    o.insert("tenant".into(), s(9)?);
    match table {
        "executions" => {
            o.insert("name".into(), s(10)?);
            o.insert("status".into(), i(11)?);
            o.insert("outcome".into(), s(12)?);
        }
        "queries" => {
            let sql: String = x.get(10)?;
            o.insert("name".into(), json!(rb::first_chars(&sql, 300)));
            o.insert("source".into(), s(11)?);
            o.insert("connection".into(), s(12)?);
            o.insert("role".into(), s(13)?);
        }
        "outgoing_requests" => {
            o.insert("name".into(), s(10)?);
            o.insert("status".into(), i(11)?);
            o.insert("source".into(), s(12)?);
        }
        _ => {
            let operation: String = x.get(10)?;
            let name = if operation == "tool" {
                x.get::<_, Option<String>>(11)?.unwrap_or_default()
            } else {
                format!("{}/{}", x.get::<_, Option<String>>(12)?.unwrap_or_default(), x.get::<_, Option<String>>(13)?.unwrap_or_default())
            };
            o.insert("name".into(), json!(name));
            o.insert("status".into(), s(14)?);
            o.insert("finish_reason".into(), s(15)?);
            o.insert("input_tokens".into(), i(16)?);
            o.insert("output_tokens".into(), i(17)?);
            o.insert("cost".into(), jfo(x.get::<_, Option<i64>>(18)?.map(|n| n as f64 / 1e9)));
            o.insert("attachments".into(), i(19)?);
            o.insert("attachment_types".into(), s(20)?);
        }
    }
    Ok(Value::Object(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_forms_ruby_accepts() {
        assert_eq!(parse_iso8601("2026-10-14T05:45:00Z").ok(), crate::time::parse_db_time("2026-10-14 05:45:00"));
        assert_eq!(parse_iso8601("2026-10-14T05:45:00.250000Z").ok(), crate::time::parse_db_time("2026-10-14 05:45:00.25"));
        assert_eq!(parse_iso8601("2026-10-14T07:45:00+02:00").ok(), crate::time::parse_db_time("2026-10-14 05:45:00"));
        assert!(parse_iso8601("2026-10-14 05:45:00").is_err());
        assert!(parse_iso8601("yesterday").is_err());
    }

    #[test]
    fn a_snapshot_failing_validation_renders_unavailable() {
        let t = Connection::open_in_memory().unwrap();
        let sample = json!({"detection": {"schema_version": 2}});
        let v = as_json(&Presented { kind: "performance", occurrences: 3, sample: &sample }, &t, 0).unwrap();
        assert_eq!(v["available"], json!(false));
        assert_eq!(v["breached_windows"], json!(3));
        let none = as_json(&Presented { kind: "exception", occurrences: 1, sample: &sample }, &t, 0).unwrap();
        assert!(none.is_null());
    }
}
