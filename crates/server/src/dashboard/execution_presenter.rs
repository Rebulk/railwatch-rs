//! Port of Railwatch::ExecutionPresenter: the execution detail page.

use std::io::Read;

use rusqlite::Connection;
use serde_json::{Map, Value};

use super::control::ControlData;
use super::db::{self, Rec, jf, ji, js, jt};
use super::models::{self, dur, dur_ms};
use super::{PageContext, rb};

pub const INLINE_COLLAPSED_BYTES: usize = 512 * 1024;
pub const MAX_COLLAPSED_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_INGEST_BYTES: usize = 4 * 1024 * 1024;

fn children(conn: &Connection, table: &str, select: &str, eid: &str, order: &str, limit: Option<usize>) -> rusqlite::Result<Vec<Rec>> {
    let mut sql = format!("SELECT {select} FROM \"{table}\" WHERE \"{table}\".\"execution_id\" = ?1");
    if !order.is_empty() {
        sql.push_str(&format!(" ORDER BY \"{table}\".\"{order}\" ASC"));
    }
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    db::query(conn, &sql, [eid])
}

pub fn props(conn: &Connection, exe: &Rec, ctx: &PageContext, control: &dyn ControlData) -> rusqlite::Result<Value> {
    let eid = exe.s("execution_id").unwrap_or("").to_string();
    let start = exe.t("occurred_at").unwrap_or(0);
    let offset = |r: &Rec| rb::round(rb::time_sub(r.t("occurred_at").unwrap_or(0), start) * 1000.0, 3);

    let exceptions = children(conn, "exceptions", "\"exceptions\".*", &eid, "", None)?;
    let profile = db::query_one(
        conn,
        "SELECT \"profiles\".* FROM \"profiles\" WHERE \"profiles\".\"execution_id\" = ?1 ORDER BY \"profiles\".\"occurred_at\" DESC LIMIT 1",
        [&eid],
    )?;
    let attachments = children(
        conn,
        "attachments",
        "\"attachments\".\"id\", \"attachments\".\"name\", \"attachments\".\"content_type\", \"attachments\".\"bytes\", \"attachments\".\"truncated\", \"attachments\".\"occurred_at\", \"attachments\".\"execution_stage\"",
        &eid,
        "occurred_at",
        Some(50),
    )?;

    // base
    let mut stages = Map::new();
    if let Value::Object(m) = exe.json("stages") {
        for (k, v) in m {
            stages.insert(k, v.as_f64().map_or(Value::Null, |f| jf(rb::round(f / 1000.0, 3))));
        }
    }
    let ms1 = |v: Option<i64>| v.map_or(Value::Null, |q| jf(rb::round(q as f64 / 1000.0, 1)));
    let base = crate::obj! {
        "execution_id" => js(exe.s("execution_id")), "kind" => js(exe.s("kind")), "name" => js(exe.s("name")),
        "duration" => dur(exe, 3), "status" => ji(exe.i("status")), "outcome" => js(exe.s("outcome")),
        "method" => js(exe.s("method")), "route" => js(exe.s("route")), "controller" => js(exe.s("controller")), "action" => js(exe.s("action")),
        "queue" => js(exe.s("queue")), "attempt" => ji(exe.i("attempt")), "queue_latency" => ms1(exe.i("queue_latency")),
        "queue_time" => ms1(exe.i("queue_time")), "job_id" => js(exe.s("job_id")), "task_key" => js(exe.s("task_key")),
        "inertia_component" => js(exe.s("inertia_component")), "occurred_at" => jt(exe.t("occurred_at")), "deploy" => js(exe.s("deploy")),
        "server" => js(exe.s("server")), "user_ref" => js(exe.s("user_ref")), "tenant" => js(exe.s("app_tenant")),
        "trace_id" => js(exe.s("trace_id")), "allocations" => ji(exe.i("allocations")), "peak_memory" => ji(exe.i("peak_memory")),
        "exception_preview" => js(exe.s("exception_preview")), "profiled" => profile.is_some(),
        "stages" => Value::Object(stages), "counters" => exe.json("counters"), "detail" => exe.json("detail"),
    };

    let timeline = timeline(conn, &eid, start, &exceptions, &attachments)?;

    let logs: Vec<Value> = children(conn, "logs", "\"logs\".*", &eid, "occurred_at", Some(500))?
        .iter()
        .map(|l| {
            crate::obj! {
                "id" => ji(l.i("id")), "level" => js(l.s("level")), "message" => js(l.s("message")), "tags" => l.json("tags"),
                "occurred_at" => jt(l.t("occurred_at")), "stage" => js(l.s("execution_stage")),
            }
        })
        .collect();
    let queries: Vec<Value> = children(conn, "queries", models::QUERY_WITH_SQL, &eid, "occurred_at", Some(500))?
        .iter()
        .map(|q| {
            crate::obj! {
                "id" => ji(q.i("id")), "sql" => js(q.s("resolved_sql")), "duration" => dur(q, 3), "source" => js(q.s("source")),
                "group_hash" => js(q.s("group_hash")), "row_count" => ji(q.i("row_count")), "stage" => js(q.s("execution_stage")),
                "offset" => jf(offset(q)),
            }
        })
        .collect();
    let spans: Vec<Value> = children(conn, "spans", "\"spans\".*", &eid, "occurred_at", Some(200))?
        .iter()
        .map(|sp| {
            crate::obj! {
                "id" => ji(sp.i("id")), "name" => js(sp.s("name")), "duration" => dur(sp, 3), "status" => js(sp.s("status")),
                "attributes" => sp.json("payload"), "offset" => jf(offset(sp)), "stage" => js(sp.s("execution_stage")),
                "occurred_at" => jt(sp.t("occurred_at")),
            }
        })
        .collect();

    let profile_card = match &profile {
        None => Value::Null,
        Some(p) => match collapsed_capped(p.blob("stacks").unwrap_or(&[]), INLINE_COLLAPSED_BYTES) {
            Err(_) => Value::Null,
            Ok((collapsed, truncated)) => crate::obj! {
                "id" => ji(p.i("id")), "profiler" => js(p.s("profiler")), "mode" => js(p.s("mode")), "interval" => ji(p.i("interval")),
                "duration" => dur(p, 2), "samples" => ji(p.i("samples")), "stacks_bytes" => ji(p.i("stacks_bytes")),
                "collapsed" => collapsed, "truncated" => truncated,
            },
        },
    };

    let enqueued: Vec<Value> = {
        let mut out = vec![];
        for j in children(conn, "enqueued_jobs", "\"enqueued_jobs\".*", &eid, "", None)? {
            let attempt = db::query_one(
                conn,
                "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"kind\" = ?1 AND \"executions\".\"job_id\" = ?2 LIMIT 1",
                rusqlite::params!["job_attempt", j.s("job_id")],
            )?;
            out.push(crate::obj! {
                "id" => ji(j.i("id")), "name" => js(j.s("name")), "queue" => js(j.s("queue")), "job_id" => js(j.s("job_id")),
                "scheduled_at" => jt(j.t("scheduled_at")), "offset" => jf(offset(&j)),
                "attempt_execution_id" => attempt.as_ref().map_or(Value::Null, |a| js(a.s("execution_id"))),
                "attempt_outcome" => attempt.as_ref().map_or(Value::Null, |a| js(a.s("outcome"))),
            });
        }
        out
    };

    let trace_id = exe.s("trace_id");
    let parent = if exe.s("kind") == Some("job_attempt") && trace_id.is_some() {
        db::query_one(
            conn,
            "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"trace_id\" = ?1 AND \"executions\".\"execution_id\" != ?2 \
             ORDER BY \"executions\".\"occurred_at\" ASC LIMIT 1",
            rusqlite::params![trace_id, eid],
        )?
        .map_or(Value::Null, |p| crate::obj! { "execution_id" => js(p.s("execution_id")), "kind" => js(p.s("kind")), "name" => js(p.s("name")) })
    } else {
        Value::Null
    };
    // where(trace_id: nil) is "IS NULL": an execution without a trace lists
    // every other trace-less execution, exactly as Rails does.
    let trace_rows = match trace_id {
        Some(t) => db::query(
            conn,
            "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"trace_id\" = ?1 ORDER BY \"executions\".\"occurred_at\" ASC LIMIT 50",
            [t],
        )?,
        None => db::query(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"trace_id\" IS NULL ORDER BY \"executions\".\"occurred_at\" ASC LIMIT 50", [])?,
    };
    let trace: Vec<Value> = trace_rows
        .iter()
        .map(|e| {
            crate::obj! {
                "execution_id" => js(e.s("execution_id")), "kind" => js(e.s("kind")), "name" => js(e.s("name")), "duration" => dur(e, 1),
                "status" => ji(e.i("status")), "outcome" => js(e.s("outcome")), "occurred_at" => jt(e.t("occurred_at")),
                "parent_id" => js(e.s("parent_id")), "server" => js(e.s("server")), "current" => e.s("execution_id") == Some(eid.as_str()),
            }
        })
        .collect();
    let trace_url = match trace_id {
        Some(t) if trace.len() > 1 => Value::String(format!("{}/traces/{t}", ctx.param_s("_environment_path"))),
        _ => Value::Null,
    };

    let mut groups: Vec<&str> = vec![];
    for e in &exceptions {
        if let Some(g) = e.s("group_hash")
            && !groups.contains(&g) {
                groups.push(g);
            }
    }
    let issues = super::control::issues_in_groups(control, &groups);
    let person = match exe.s("user_ref") {
        None => Value::Null,
        Some(u) => models::person_by_ref(conn, u)?
            .map_or(Value::Null, |p| crate::obj! { "ref" => p.ref_.clone(), "name" => p.display_name(), "email" => js(p.email.as_deref()) }),
    };

    Ok(crate::obj! {
        "execution" => base,
        "timeline" => timeline,
        "exceptions" => exceptions.iter().map(exception_row).collect::<Vec<_>>(),
        "logs" => logs, "queries" => queries, "spans" => spans, "profile" => profile_card,
        "attachments" => attachments.iter().map(|a| crate::obj! {
            "id" => ji(a.i("id")), "name" => js(a.s("name")), "content_type" => js(a.s("content_type")), "bytes" => ji(a.i("bytes")),
            "truncated" => a.b("truncated").map_or(Value::Null, Value::Bool), "viewable" => viewable(a.s("content_type")),
            "occurred_at" => jt(a.t("occurred_at")),
        }).collect::<Vec<_>>(),
        "enqueued_jobs" => enqueued, "parent" => parent, "trace" => trace, "trace_url" => trace_url,
        "issues" => issues.iter().map(|i| crate::obj! {
            "id" => i.id, "key" => i.key.clone(), "title" => i.title.clone(), "status" => i.status.clone(), "group_hash" => i.group_hash.clone(),
        }).collect::<Vec<_>>(),
        "person" => person,
    })
}

pub fn viewable(ct: Option<&str>) -> bool {
    let ct = ct.unwrap_or("");
    ct.starts_with("text/") || ct.split(';').next() == Some("application/json")
}

fn exception_row(e: &Rec) -> Value {
    crate::obj! {
        "id" => ji(e.i("id")), "class_name" => js(e.s("class_name")), "message" => js(e.s("message")),
        "handled" => e.b("handled").map_or(Value::Null, Value::Bool), "severity" => js(e.s("severity")), "source" => js(e.s("source")),
        "file" => js(e.s("file")), "line" => ji(e.i("line")), "frames" => e.json("frames"), "cause" => e.json("cause"),
        "locals" => e.json("locals"), "context" => js(e.s("context")), "occurred_at" => jt(e.t("occurred_at")), "group_hash" => js(e.s("group_hash")),
    }
}

fn entry(type_: &str, r: &Rec, start: i64, duration: Option<f64>, label: String, source: Value, detail: Value) -> (f64, Value) {
    entry_sql(type_, r, start, duration, label, None, source, detail)
}

/// The query entry carries `sql` between `label` and `source`.
#[allow(clippy::too_many_arguments)]
fn entry_sql(type_: &str, r: &Rec, start: i64, duration: Option<f64>, label: String, sql: Option<&str>, source: Value, detail: Value) -> (f64, Value) {
    let off = rb::round(rb::time_sub(r.t("occurred_at").unwrap_or(0), start) * 1000.0, 3);
    let mut m = Map::new();
    m.insert("type".into(), Value::from(type_));
    m.insert("id".into(), ji(r.i("id")));
    m.insert("offset".into(), jf(off));
    m.insert("duration".into(), duration.map_or(Value::Null, |d| jf(rb::round(d, 3))));
    m.insert("stage".into(), js(r.s("execution_stage")));
    m.insert("label".into(), Value::String(label));
    if let Some(sql) = sql {
        m.insert("sql".into(), Value::String(sql.to_string()));
    }
    m.insert("source".into(), source);
    m.insert("detail".into(), detail);
    (off, Value::Object(m))
}

fn first120(s: &str) -> String {
    rb::first_chars(s, 120).to_string()
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn timeline(conn: &Connection, eid: &str, start: i64, exceptions: &[Rec], attachments: &[Rec]) -> rusqlite::Result<Vec<Value>> {
    let mut entries: Vec<(f64, Value)> = vec![];
    let all = |t: &str| children(conn, t, &format!("\"{t}\".*"), eid, "", None);
    for q in children(conn, "queries", models::QUERY_WITH_SQL, eid, "", None)? {
        let sql = q.s("resolved_sql").unwrap_or("");
        entries.push(entry_sql("query", &q, start, dur_ms(&q), first120(sql), Some(sql), js(q.s("source")), obj(vec![("rows", ji(q.i("row_count"))), ("connection", js(q.s("connection")))])));
    }
    for c in all("cache_events")? {
        let label = first120(&format!("{} {}", c.s("type").unwrap_or(""), c.s("key").unwrap_or("")));
        entries.push(entry("cache_event", &c, start, dur_ms(&c), label, Value::Null, obj(vec![
            ("key", js(c.s("key"))), ("op", js(c.s("type"))), ("store", js(c.s("store"))), ("hits", ji(c.i("hits"))), ("ttl", ji(c.i("ttl"))),
        ])));
    }
    for o in all("outgoing_requests")? {
        entries.push(entry("outgoing_request", &o, start, dur_ms(&o), first120(o.s("url").unwrap_or("")), js(o.s("source")), obj(vec![
            ("method", js(o.s("method"))), ("url", js(o.s("url"))), ("status", ji(o.i("status_code"))), ("host", js(o.s("host"))),
            ("error", js(o.s("error"))), ("body", js(o.s("response_body"))),
        ])));
    }
    for m in all("mails")? {
        entries.push(entry("mail", &m, start, dur_ms(&m), first120(m.s("mailer").unwrap_or("")), Value::Null, obj(vec![
            ("subject", js(m.s("subject"))), ("mailer", js(m.s("mailer"))), ("to", ji(m.i("to"))),
        ])));
    }
    for l in all("logs")? {
        let msg = l.s("message").unwrap_or("");
        // Log has no duration column: duration_ms is nil.
        entries.push(entry("log", &l, start, None, first120(msg), js(l.s("source")), obj(vec![
            ("level", js(l.s("level"))), ("message", Value::String(rb::first_chars(msg, 500).to_string())),
        ])));
    }
    for x in exceptions {
        let msg = x.s("message").unwrap_or("");
        let source = match x.s("file") {
            Some(f) => Value::String(format!("{f}:{}", x.i("line").map_or(String::new(), |l| l.to_string()))),
            None => Value::Null,
        };
        entries.push(entry("exception", x, start, None, format!("{}: {}", x.s("class_name").unwrap_or(""), rb::first_chars(msg, 100)), source, obj(vec![
            ("class", js(x.s("class_name"))), ("message", Value::String(rb::first_chars(msg, 300).to_string())),
            ("handled", x.b("handled").map_or(Value::Null, Value::Bool)),
        ])));
    }
    for b in all("broadcasts")? {
        entries.push(entry("broadcast", &b, start, dur_ms(&b), first120(b.s("channel").unwrap_or("")), Value::Null, obj(vec![
            ("channel", js(b.s("channel"))), ("action", js(b.s("action"))), ("stream", js(b.s("stream"))),
        ])));
    }
    for s in all("storage_ops")? {
        let label = first120(&format!("{} {}", s.s("op").unwrap_or(""), s.s("key").unwrap_or("")));
        entries.push(entry("storage_op", &s, start, dur_ms(&s), label, Value::Null, obj(vec![
            ("op", js(s.s("op"))), ("key", js(s.s("key"))), ("service", js(s.s("service"))),
        ])));
    }
    for v in all("view_renders")? {
        entries.push(entry("view_render", &v, start, dur_ms(&v), first120(v.s("identifier").unwrap_or("")), Value::Null, obj(vec![
            ("count", ji(v.i("count"))), ("cache_hits", ji(v.i("cache_hits"))),
        ])));
    }
    for j in all("enqueued_jobs")? {
        entries.push(entry("enqueued_job", &j, start, dur_ms(&j), first120(j.s("name").unwrap_or("")), Value::Null, obj(vec![
            ("queue", js(j.s("queue"))), ("job_id", js(j.s("job_id"))),
        ])));
    }
    for s in all("spans")? {
        let mut d = obj(vec![("status", js(s.s("status")))]);
        if let Value::Object(p) = s.json("payload") {
            for (k, v) in p {
                db::merge_into(&mut d, &k, v);
            }
        }
        entries.push(entry("span", &s, start, dur_ms(&s), first120(s.s("name").unwrap_or("")), Value::Null, d));
    }
    for t in all("transactions")? {
        entries.push(entry("transaction", &t, start, dur_ms(&t), first120(t.s("outcome").unwrap_or("")), Value::Null, obj(vec![
            ("outcome", js(t.s("outcome"))), ("connection", js(t.s("connection"))),
        ])));
    }
    for c in all("llm_calls")? {
        let label = if c.s("operation") == Some("tool") {
            c.s("tool_name").unwrap_or("").to_string()
        } else {
            format!("{}/{}", c.s("provider").unwrap_or(""), c.s("model").unwrap_or(""))
        };
        entries.push(entry("llm_call", &c, start, dur_ms(&c), first120(&label), Value::Null, obj(vec![
            ("operation", js(c.s("operation"))), ("provider", js(c.s("provider"))), ("model", js(c.s("model"))), ("tool", js(c.s("tool_name"))),
            ("input_tokens", ji(c.i("input_tokens"))), ("output_tokens", ji(c.i("output_tokens"))),
            ("cost", c.i("cost_nanos").map_or(Value::Null, |n| jf(n as f64 / 1e9))), ("status", js(c.s("status"))), ("error", js(c.s("error"))),
            ("workflow", js(c.s("workflow_name"))), ("step", js(c.s("workflow_step_name"))),
        ])));
    }
    for a in attachments {
        entries.push(entry("attachment", a, start, None, first120(a.s("name").unwrap_or("")), Value::Null, obj(vec![
            ("name", js(a.s("name"))), ("content_type", js(a.s("content_type"))), ("bytes", ji(a.i("bytes"))),
            ("truncated", a.b("truncated").map_or(Value::Null, Value::Bool)),
        ])));
    }
    // sort_by { offset }: every key a Float -> MRI's uniform introsort.
    let sorted = rb::sort_by_num(entries, |(o, _)| rb::Num::F(*o));
    Ok(sorted.into_iter().map(|(_, v)| v).collect())
}

/// `Profile#collapsed_capped(limit)`: inflate at most `limit` bytes; when
/// cut, drop the partial last line. Err on a malformed stream or non-UTF-8.
pub fn collapsed_capped(stacks: &[u8], limit: usize) -> Result<(String, bool), String> {
    let limit = limit.min(MAX_INGEST_BYTES);
    let mut dec = flate2::read::GzDecoder::new(stacks);
    let mut out = Vec::with_capacity(limit.min(1 << 16));
    let mut buf = vec![0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = dec.read(&mut buf).map_err(|e| format!("invalid gzip stream: {e}"))?;
        if n == 0 {
            break;
        }
        if out.len() + n > limit {
            out.extend_from_slice(&buf[..limit - out.len()]);
            truncated = true;
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    if truncated {
        match out.iter().rposition(|&b| b == b'\n') {
            Some(i) => out.truncate(i + 1),
            None => out.clear(),
        }
    }
    String::from_utf8(out).map(|s| (s, truncated)).map_err(|_| "decompressed content is not valid UTF-8".into())
}
