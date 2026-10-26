//! Text for the client's commands: what matters first, one screen or less.
//! Every renderer reads the tool's JSON defensively (a field an older or
//! newer server lacks is skipped), and --json is there for everything else.

use serde_json::Value;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn n(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn short(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max { one_line } else { one_line.chars().take(max.saturating_sub(1)).collect::<String>() + "…" }
}

/// 12.345 -> "12 ms", 1234.5 -> "1.23 s".
fn ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else if v >= 10.0 {
        format!("{v:.0} ms")
    } else {
        format!("{v:.1} ms")
    }
}

fn plural(n: i64, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

fn push_url(out: &mut String, v: &Value) {
    if let Some(u) = s(v, "url") {
        out.push_str(&format!("{u}\n"));
    }
}

pub fn issues(v: &Value, status: &str) -> String {
    let rows = v.as_array().map_or(&[][..], Vec::as_slice);
    if rows.is_empty() {
        return format!("No {status} issues.");
    }
    let key_w = rows.iter().filter_map(|r| s(r, "key")).map(str::len).max().unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        let count = r.get("occurrences").and_then(Value::as_i64).unwrap_or(0);
        let last = s(r, "last_seen_at").unwrap_or("");
        out.push_str(&format!(
            "{:key_w$}  {:>6}×  {:<11}  {:<16}  {}\n",
            s(r, "key").unwrap_or("?"),
            count,
            s(r, "kind").unwrap_or(""),
            last.get(..16).unwrap_or(last),
            short(s(r, "title").unwrap_or(""), 90),
        ));
    }
    out.push_str(&format!("{}. `railwatch issue <key>` for one.", plural(rows.len() as i64, &format!("{status} issue"))));
    out
}

/// get_issue (a server without investigate_issue).
pub fn issue(v: &Value) -> String {
    let mut out = format!("{}  {}\n", s(v, "key").unwrap_or("?"), s(v, "title").unwrap_or(""));
    let mut facts = vec![];
    for k in ["kind", "status", "priority"] {
        if let Some(x) = s(v, k) {
            facts.push(x.to_string());
        }
    }
    if let Some(o) = v.get("occurrences").and_then(Value::as_i64) {
        facts.push(plural(o, "occurrence"));
    }
    if let Some(u) = v.get("affected_users").and_then(Value::as_i64).filter(|u| *u > 0) {
        facts.push(format!("{} affected", plural(u, "user")));
    }
    if let (Some(app), Some(env)) = (s(v, "application"), s(v, "environment")) {
        facts.push(format!("{app}/{env}"));
    }
    out.push_str(&facts.join(" · "));
    out.push('\n');
    if let Some(c) = s(v, "culprit") {
        out.push_str(&format!("culprit: {c}\n"));
    }
    out.push_str(&format!("first seen {}, last seen {}\n", s(v, "first_seen_at").unwrap_or("?"), s(v, "last_seen_at").unwrap_or("?")));
    if let Some(d) = v.get("sample").and_then(|x| s(x, "deploy")) {
        out.push_str(&format!("deploy: {d}\n"));
    }
    push_url(&mut out, v);
    if let Some(note) = s(v, "note") {
        out.push_str(&format!("\n{note}\n"));
    }
    let o = &v["sample_occurrence"];
    if o.is_object() {
        out.push_str(&format!("\n{}: {}\n", s(o, "class_name").unwrap_or("?"), short(s(o, "message").unwrap_or(""), 300)));
        frames(&mut out, arr(o, "frames"));
        if let Some(c) = o.get("context").filter(|c| c.as_object().is_some_and(|m| !m.is_empty())) {
            out.push_str(&format!("context: {}\n", short(&c.to_string(), 300)));
        }
        if let Some(p) = s(o, "execution_preview") {
            out.push_str(&format!("in: {p}"));
            if let Some(u) = s(o, "execution_url") {
                out.push_str(&format!("  {u}"));
            }
            out.push('\n');
        }
    }
    if let Some(d) = v.get("detection").filter(|d| d.is_object()) {
        out.push('\n');
        out.push_str(&generic(d));
    }
    out
}

const MAX_FRAMES: usize = 15;

fn frames(out: &mut String, frames: &[Value]) {
    for f in frames.iter().take(MAX_FRAMES) {
        if let Some(n) = f.get("omitted_frames").and_then(Value::as_i64) {
            let gems: Vec<&str> = arr(f, "gems").iter().filter_map(Value::as_str).collect();
            out.push_str(&format!("    … {n} frames ({})\n", gems.join(", ")));
            continue;
        }
        let at = match (s(f, "file"), f.get("line").and_then(Value::as_i64)) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.to_string(),
            _ => "?".into(),
        };
        let mark = if f.get("in_app").and_then(Value::as_bool) == Some(true) { "→" } else { " " };
        out.push_str(&format!("  {mark} {at} in {}\n", s(f, "function").unwrap_or("?")));
        // The frame's own line of code, when the gem sent it.
        if let (Some(code), Some(line)) = (f.get("code").and_then(Value::as_object), f.get("line").and_then(Value::as_i64))
            && let Some(src) = code.get(&line.to_string()).and_then(Value::as_str)
        {
            out.push_str(&format!("        {}\n", short(src, 140)));
        }
    }
    if frames.len() > MAX_FRAMES {
        out.push_str(&format!("    … {} more (--json, or railwatch call get_issue --arg key=<KEY> --arg frames=all)\n", frames.len() - MAX_FRAMES));
    }
}

/// get_route (a server without explain_route).
pub fn route(v: &Value) -> String {
    let mut out = format!("{}\n", s(v, "route").unwrap_or("?"));
    let count = v.get("count").and_then(Value::as_i64).unwrap_or(0);
    let mut line = vec![plural(count, "request")];
    for (k, label) in [("p50_ms", "p50"), ("p95_ms", "p95"), ("p99_ms", "p99"), ("max_ms", "max")] {
        if let Some(x) = n(v, k) {
            line.push(format!("{label} {}", ms(x)));
        }
    }
    if let Some(e) = v.get("errors").and_then(Value::as_i64).filter(|e| *e > 0) {
        line.push(plural(e, "error"));
    }
    if let Some(w) = v.get("window") {
        line.push(format!("{} → {}", s(w, "from").unwrap_or("?"), s(w, "to").unwrap_or("?")));
    }
    out.push_str(&line.join(" · "));
    out.push('\n');
    push_url(&mut out, v);
    let n1 = arr(v, "n_plus_ones");
    if !n1.is_empty() {
        out.push_str("\nN+1s:\n");
        for p in n1.iter().take(5) {
            out.push_str(&format!(
                "  {} — {}× in {}: {}\n",
                s(p, "source").unwrap_or("?"),
                p.get("max_repeats").and_then(Value::as_i64).unwrap_or(0),
                plural(p.get("occurrences").and_then(Value::as_i64).unwrap_or(0), "request"),
                short(s(p, "sql").unwrap_or(""), 100)
            ));
            if let Some(fix) = p.get("suggestion").and_then(|x| s(x, "code")) {
                out.push_str(&format!("    fix: {}\n", short(fix, 140)));
            }
        }
    }
    let q = arr(v, "slow_queries");
    if !q.is_empty() {
        out.push_str("\nSlowest query shapes (total):\n");
        for x in q.iter().take(5) {
            out.push_str(&format!(
                "  {:>9}  {:>5}×  {}  {}\n",
                ms(n(x, "total_ms").unwrap_or(0.0)),
                x.get("count").and_then(Value::as_i64).unwrap_or(0),
                s(x, "source").unwrap_or("?"),
                short(s(x, "sql").unwrap_or(""), 80)
            ));
        }
    }
    let slow = arr(v, "slowest_requests");
    if !slow.is_empty() {
        out.push_str("\nSlowest requests:\n");
        for r in slow.iter().take(3) {
            out.push_str(&format!("  {}\n", request_line(r)));
        }
    }
    out
}

/// explain_route: its one-paragraph answer, then the evidence.
pub fn explain(v: &Value) -> String {
    let Some(answer) = s(v, "answer") else { return generic(v) };
    let mut out = String::new();
    if let Some(r) = s(v, "route") {
        out.push_str(&format!("{r}\n"));
    }
    out.push_str(answer);
    out.push('\n');
    push_url(&mut out, v);
    out.push('\n');
    out.push_str(&generic(&without(v, &["answer", "route", "url"])));
    out
}

fn without(v: &Value, keys: &[&str]) -> Value {
    let mut rest = v.clone();
    if let Some(m) = rest.as_object_mut() {
        for k in keys {
            m.remove(*k);
        }
    }
    rest
}

/// last_execution / get_execution: the summary, not the timeline.
pub fn execution(v: &Value) -> String {
    let mut head = vec![s(v, "name").unwrap_or("?").to_string()];
    if let Some(p) = s(v, "path") {
        head.push(p.to_string());
    }
    if let Some(st) = v.get("status").and_then(Value::as_i64) {
        head.push(st.to_string());
    } else if let Some(o) = s(v, "outcome") {
        head.push(o.to_string());
    }
    if let Some(d) = n(v, "duration_ms") {
        head.push(ms(d));
    }
    let mut out = head.join("  ");
    out.push('\n');
    out.push_str(&format!("{} {}\n", s(v, "occurred_at").unwrap_or(""), s(v, "execution_id").unwrap_or("")));
    push_url(&mut out, v);
    let sm = &v["summary"];
    if let Some(t) = sm.get("time_ms").and_then(Value::as_object) {
        let parts: Vec<String> = ["sql", "transactions", "view", "external", "cache", "other", "untracked"]
            .iter()
            .filter_map(|k| t.get(*k).and_then(Value::as_f64).filter(|x| *x > 0.0).map(|x| format!("{k} {}", ms(x))))
            .collect();
        if !parts.is_empty() {
            out.push_str(&format!("time: {}\n", parts.join(" · ")));
        }
    }
    if let Some(c) = sm.get("counts").and_then(Value::as_object).filter(|c| !c.is_empty()) {
        let parts: Vec<String> = c.iter().map(|(k, x)| format!("{x} {k}")).collect();
        let shapes = sm.get("distinct_query_shapes").and_then(Value::as_i64).map(|d| format!(" ({d} query shapes)")).unwrap_or_default();
        out.push_str(&format!("records: {}{shapes}\n", parts.join(", ")));
    }
    for note in arr(sm, "notes").iter().filter_map(Value::as_str) {
        out.push_str(&format!("note: {note}\n"));
    }
    let n1 = arr(sm, "n_plus_ones");
    if !n1.is_empty() {
        out.push_str("\nN+1s:\n");
        for p in n1 {
            out.push_str(&format!(
                "  {} — {}×: {}\n",
                s(p, "source").unwrap_or("?"),
                p.get("repeats").and_then(Value::as_i64).unwrap_or(0),
                short(s(p, "sql").unwrap_or(""), 100)
            ));
            if let Some(fix) = s(p, "fix") {
                out.push_str(&format!("    fix: {}\n", short(fix, 140)));
            }
        }
    }
    let rep = arr(sm, "repeated_queries");
    if !rep.is_empty() {
        out.push_str("\nRepeated queries:\n");
        for q in rep.iter().take(5) {
            out.push_str(&format!(
                "  {:>4}×  {:>9}  {}  {}\n",
                q.get("count").and_then(Value::as_i64).unwrap_or(0),
                ms(n(q, "total_ms").unwrap_or(0.0)),
                s(q, "source").unwrap_or("?"),
                short(s(q, "sql").unwrap_or(""), 80)
            ));
        }
    }
    let slow = arr(sm, "slowest");
    if !slow.is_empty() {
        out.push_str("\nSlowest records:\n");
        for r in slow.iter().take(5) {
            out.push_str(&format!(
                "  {:>9}  {:<16} {}{}\n",
                ms(n(r, "duration").unwrap_or(0.0)),
                s(r, "type").unwrap_or(""),
                short(s(r, "label").unwrap_or(""), 80),
                s(r, "source").map(|x| format!("  @ {x}")).unwrap_or_default()
            ));
        }
    }
    for (title, key) in [("Exceptions", "exceptions"), ("Warnings", "warnings")] {
        let rows = arr(sm, key);
        if !rows.is_empty() {
            out.push_str(&format!("\n{title}:\n"));
            for r in rows {
                let text = s(r, "label").or_else(|| s(r, "message")).unwrap_or("");
                out.push_str(&format!("  {}\n", short(text, 160)));
            }
        }
    }
    out
}

/// compare: the verdict and the deltas.
pub fn compare(v: &Value) -> String {
    let mut out = String::new();
    if let Some(verdict) = s(v, "verdict") {
        out.push_str(verdict);
        out.push('\n');
    }
    if let Some(w) = n(v, "worst_regression_pct") {
        out.push_str(&format!("worst regression: {w}%"));
        if v.get("low_samples").and_then(Value::as_bool) == Some(true) {
            out.push_str(" (low samples: too few executions on one side to trust)");
        }
        out.push('\n');
    }
    push_url(&mut out, v);
    out.push('\n');
    out.push_str(&generic(&without(v, &["verdict", "worst_regression_pct", "low_samples", "url"])));
    out
}

/// One search_requests row.
pub fn request_line(r: &Value) -> String {
    let status = r.get("status").and_then(Value::as_i64).map_or_else(|| s(r, "outcome").unwrap_or("-").to_string(), |x| x.to_string());
    let queries = r.get("queries").and_then(Value::as_i64).map(|q| format!("{q}q")).unwrap_or_default();
    format!(
        "{}  {:>3}  {:>9}  {:>5}  {}  {}",
        s(r, "occurred_at").unwrap_or("").trim_end_matches(" UTC"),
        status,
        ms(n(r, "duration").unwrap_or(0.0)),
        queries,
        s(r, "name").unwrap_or("?"),
        s(r, "execution_id").unwrap_or("")
    )
}

pub fn tools(list: &Value) -> String {
    let rows = list.as_array().map_or(&[][..], Vec::as_slice);
    let w = rows.iter().filter_map(|t| s(t, "name")).map(str::len).max().unwrap_or(0);
    let mut out = String::new();
    for t in rows {
        let desc = s(t, "description").unwrap_or("");
        // Some descriptions lead with an input note.
        let first = desc.split("\n\n").find(|p| !p.starts_with("Input constraint")).unwrap_or(desc);
        out.push_str(&format!("{:w$}  {}\n", s(t, "name").unwrap_or("?"), short(first.split(". ").next().unwrap_or(first), 100)));
    }
    out
}

/// Any JSON as indented `key: value` lines: long strings and arrays cut,
/// nulls and empty values left out.
pub fn generic(v: &Value) -> String {
    let mut out = String::new();
    write(&mut out, v, 0);
    out
}

const MAX_ITEMS: usize = 10;
const MAX_STRING: usize = 300;
const MAX_DEPTH: usize = 4;

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(short(s, MAX_STRING)),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        _ => None,
    }
}

fn write(out: &mut String, v: &Value, depth: usize) {
    let pad = "  ".repeat(depth);
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter().filter(|(_, x)| !empty(x)) {
                if let Some(sc) = scalar(x) {
                    out.push_str(&format!("{pad}{k}: {sc}\n"));
                } else if depth >= MAX_DEPTH {
                    out.push_str(&format!("{pad}{k}: {}\n", short(&x.to_string(), 120)));
                } else {
                    out.push_str(&format!("{pad}{k}:\n"));
                    write(out, x, depth + 1);
                }
            }
        }
        Value::Array(a) => {
            for x in a.iter().take(MAX_ITEMS) {
                match scalar(x) {
                    Some(sc) => out.push_str(&format!("{pad}- {sc}\n")),
                    None if depth >= MAX_DEPTH => out.push_str(&format!("{pad}- {}\n", short(&x.to_string(), 120))),
                    None => {
                        let mut inner = String::new();
                        write(&mut inner, x, depth + 1);
                        out.push_str(&format!("{pad}- {}", inner.trim_start_matches(' ')));
                    }
                }
            }
            if a.len() > MAX_ITEMS {
                out.push_str(&format!("{pad}… {} more (--json for all)\n", a.len() - MAX_ITEMS));
            }
        }
        other => {
            if let Some(sc) = scalar(other) {
                out.push_str(&format!("{pad}{sc}\n"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn issues_are_one_line_each() {
        let v = json!([{"key": "SHOP-12", "title": "NoMethodError: undefined method `x' for nil", "kind": "exception", "occurrences": 3,
                        "last_seen_at": "2026-10-25 23:03:45 UTC"}]);
        let t = issues(&v, "open");
        assert!(t.starts_with("SHOP-12       3×  exception    2026-10-25 23:03  NoMethodError"), "{t}");
        assert!(t.ends_with("1 open issue. `railwatch issue <key>` for one."));
        assert_eq!(issues(&json!([]), "resolved"), "No resolved issues.");
    }

    #[test]
    fn generic_skips_empties_and_cuts_long_arrays() {
        let v = json!({"a": null, "b": "", "c": 1, "d": {"e": [1, 2]}, "f": (0..12).collect::<Vec<_>>()});
        let t = generic(&v);
        assert!(!t.contains("a:") && !t.contains("b:"));
        assert!(t.contains("c: 1\nd:\n  e:\n    - 1\n    - 2\nf:\n"), "{t}");
        assert!(t.contains("… 2 more (--json for all)"));
    }

    #[test]
    fn an_execution_leads_with_where_the_time_went() {
        let v = json!({"name": "GET /orders", "path": "/orders", "status": 200, "duration_ms": 3670.0, "execution_id": "e1",
            "summary": {"time_ms": {"sql": 83.6, "untracked": 3586.4, "view": 0.0}, "counts": {"query": 826},
                        "n_plus_ones": [{"source": "app/x.rb:511", "repeats": 60, "sql": "SELECT 1", "fix": "includes(:y)"}],
                        "notes": ["98% of the time is Ruby."]}});
        let t = execution(&v);
        assert!(t.starts_with("GET /orders  /orders  200  3.67 s\n"), "{t}");
        assert!(t.contains("time: sql 84 ms · untracked 3.59 s\n"), "{t}");
        assert!(t.contains("app/x.rb:511 — 60×: SELECT 1\n    fix: includes(:y)"), "{t}");
    }
}
