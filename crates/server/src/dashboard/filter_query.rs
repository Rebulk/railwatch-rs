//! Port of Railwatch::FilterQuery: the "key:value free text" grammar, the
//! per-resource `apply` (as SQL conditions with bound values), and
//! `filter_routes`.

use indexmap::IndexMap;
use rusqlite::types::Value as Sql;
use rusqlite::Connection;
use serde_json::Value;

use super::db::{self, qt};
use super::rb;
use super::window::parse_time;

pub const EXECUTION_KINDS: [&str; 4] = ["request", "job", "scheduled_task", "command"];
pub const LOG_LEVELS: [&str; 7] = ["debug", "info", "warn", "error", "fatal", "event", "unknown"];
pub const FRESH_NAMES: i64 = 5 * 60 * 1_000_000;
pub const DENSE_MATCHES: i64 = 1_000;

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub text: String,
    pub fields: IndexMap<String, String>,
}

impl Parsed {
    pub fn field(&self, k: &str) -> Option<&str> {
        self.fields.get(k).map(String::as_str).filter(|v| rb::present(Some(v)))
    }
}

/// `FilterQuery.parse`: TOKEN = /\A([a-z_]+):(\S+)\z/ per whitespace word.
pub fn parse(query: Option<&str>) -> Parsed {
    let mut p = Parsed::default();
    let mut text = Vec::new();
    for word in query.unwrap_or("").split(|c: char| c.is_ascii_whitespace()) {
        if word.is_empty() {
            continue;
        }
        if let Some((k, v)) = word.split_once(':')
            && !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') && !v.is_empty() {
                p.fields.insert(k.to_string(), v.to_string());
                continue;
            }
        if rb::present(Some(word)) {
            text.push(word);
        }
    }
    p.text = text.join(" ");
    p
}

/// "5xx" -> 500..=599, "404" -> 404..=404.
pub fn status_range(v: Option<&str>) -> Option<(i64, i64)> {
    let v = v?;
    let b = v.as_bytes();
    if b.len() == 3 && (b'2'..=b'5').contains(&b[0]) && b[1].eq_ignore_ascii_case(&b'x') && b[2].eq_ignore_ascii_case(&b'x') {
        let base = (b[0] - b'0') as i64 * 100;
        return Some((base, base + 99));
    }
    if !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()) {
        let n: i64 = v.parse().ok()?;
        return Some((n, n));
    }
    None
}

/// `FilterQuery.filter_routes` over grouped request rows.
pub fn filter_routes(routes: Vec<Value>, query: Option<&str>) -> Vec<Value> {
    let p = parse(query);
    let mut routes = routes;
    if let Some(m) = p.field("method") {
        let prefix = format!("{} ", m.to_uppercase());
        routes.retain(|r| r["name"].as_str().is_some_and(|n| n.starts_with(&prefix)));
    }
    if let Some(route) = p.field("route") {
        routes.retain(|r| r["name"].as_str().is_some_and(|n| n.contains(route)));
    }
    if let Some((lo, _)) = status_range(p.fields.get("status").map(String::as_str)) {
        routes.retain(|r| {
            let (c, e, ce) = (r["count"].as_i64().unwrap_or(0), r["errors"].as_i64().unwrap_or(0), r["client_errors"].as_i64().unwrap_or(0));
            match lo {
                500..=599 => e > 0,
                400..=499 => ce > 0,
                _ => c - e - ce > 0,
            }
        });
    }
    routes
}

/// SHA256 of `[environment_id, resource, query.to_s, [from, to]].to_json`.
pub fn cursor_context(environment_id: i64, resource: &str, query: Option<&str>, from: i64, to: i64) -> String {
    use sha2::Digest;
    let json = serde_json::to_string(&serde_json::json!([
        environment_id,
        resource,
        query.unwrap_or(""),
        [crate::time::iso8601_6(from), crate::time::iso8601_6(to)]
    ]))
    .unwrap();
    let json = crate::rjson::escape_html(&json);
    sha2::Sha256::digest(json.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

// -- Conditions ---------------------------------------------------------------

/// A WHERE clause under construction: SQL fragments ANDed, binds in order
/// (fragments use `?`, numbered when rendered).
#[derive(Debug, Clone, Default)]
pub struct Cond {
    pub parts: Vec<String>,
    pub binds: Vec<Sql>,
    /// `none`: the relation can match nothing.
    pub none: bool,
    /// Overrides "FROM <table>" (INDEXED BY / NOT INDEXED hints).
    pub from: Option<String>,
}

impl Cond {
    pub fn new() -> Cond {
        Cond::default()
    }
    pub fn and(&mut self, sql: impl Into<String>, binds: Vec<Sql>) -> &mut Cond {
        self.parts.push(sql.into());
        self.binds.extend(binds);
        self
    }
    pub fn eq(&mut self, table: &str, col: &str, v: impl Into<Sql>) -> &mut Cond {
        self.and(format!("\"{table}\".\"{col}\" = ?"), vec![v.into()])
    }
    pub fn sql(&self) -> String {
        if self.none {
            return "1=0".into();
        }
        if self.parts.is_empty() {
            return "1=1".into();
        }
        self.parts.join(" AND ")
    }
}

pub fn s(v: &str) -> Sql {
    Sql::Text(v.to_string())
}

/// `bounded_time_range`: the window narrowed by after:/before:.
/// Returns (lower, upper, exclusive_empty).
pub fn bounded_time_range(p: &Parsed, from: i64, to: i64) -> (i64, i64, bool) {
    let after = p.fields.get("after").and_then(|v| parse_time(v));
    let before = p.fields.get("before").and_then(|v| parse_time(v));
    let lower = after.map_or(from, |a| a.max(from));
    let upper = before.map_or(to, |b| b.min(to));
    if lower <= upper { (lower, upper, false) } else { (lower, lower, true) }
}

fn time_range(c: &mut Cond, table: &str, (lo, hi, empty): (i64, i64, bool)) {
    if empty {
        c.and(format!("\"{table}\".\"occurred_at\" >= ? AND \"{table}\".\"occurred_at\" < ?"), vec![s(&qt(lo)), s(&qt(hi))]);
    } else {
        c.and(format!("\"{table}\".\"occurred_at\" BETWEEN ? AND ?"), vec![s(&qt(lo)), s(&qt(hi))]);
    }
}

pub enum Resource {
    Jobs,
    Exceptions,
    Logs,
    Queries,
}

impl Resource {
    fn table(&self) -> &'static str {
        match self {
            Resource::Jobs => "executions",
            Resource::Exceptions => "exceptions",
            Resource::Logs => "logs",
            Resource::Queries => "queries",
        }
    }
}

fn has_column(table: &str, col: &str) -> bool {
    crate::schema::schema().tables.get(table).is_some_and(|cs| cs.iter().any(|c| c.name == col))
}

/// `FilterQuery.apply`: the conditions (beyond the resource's base scope,
/// which is added here too for jobs) for `query` over [from, to].
pub fn apply(conn: &Connection, resource: Resource, query: Option<&str>, from: i64, to: i64, except: &[&str]) -> rusqlite::Result<Cond> {
    let table = resource.table();
    let mut parsed = parse(query);
    for e in except {
        parsed.fields.shift_remove(*e);
    }
    let range = bounded_time_range(&parsed, from, to);
    let mut c = Cond::new();
    if matches!(resource, Resource::Jobs) {
        c.eq(table, "kind", s("job_attempt"));
    }
    time_range(&mut c, table, range);
    // apply_common
    if let Some(v) = parsed.field("user") {
        c.eq(table, "user_ref", s(v));
    }
    if let Some(v) = parsed.field("tenant") {
        c.eq(table, "app_tenant", s(v));
    }
    if let Some(v) = parsed.field("deploy") {
        c.eq(table, "deploy", s(v));
    }
    if let Some(v) = parsed.field("source") {
        if EXECUTION_KINDS.contains(&v) && has_column(table, "execution_source") {
            c.eq(table, "execution_source", s(v));
        } else if has_column(table, "source") {
            c.and(format!("\"{table}\".\"source\" LIKE ?"), vec![s(&format!("{}%", rb::sanitize_sql_like(v)))]);
        }
    }
    if let Some(k) = parsed.fields.get("kind")
        && EXECUTION_KINDS.contains(&k.as_str()) && has_column(table, "execution_source") {
            c.eq(table, "execution_source", s(k));
        }
    let text = parsed.text.clone();
    match resource {
        Resource::Jobs => {
            if let Some(q) = parsed.field("queue") {
                c.eq(table, "queue", s(q));
            }
            if let Some(o) = parsed.field("outcome").or(parsed.field("status")) {
                c.eq(table, "outcome", s(o));
            }
            if let Some(name) = parsed.field("class") {
                let (lo, hi, _) = range;
                let rollup_cond = "\"rollups\".\"record_type\" = 'job_attempt' AND \"rollups\".\"bucket\" BETWEEN ?1 AND ?2 AND \"rollups\".\"name\" = ?3";
                let binds = rusqlite::params![qt(rb::beginning_of_hour(lo)), qt(hi), name];
                let sum = db::count(conn, &format!("SELECT SUM(\"rollups\".\"count\") FROM \"rollups\" WHERE {rollup_cond}"), binds)?;
                if sum >= DENSE_MATCHES {
                    c.eq(table, "name", s(name));
                } else {
                    let mut groups: Vec<String> = vec![md5_hex(name)];
                    for r in db::query(conn, &format!("SELECT DISTINCT \"rollups\".\"group_hash\" FROM \"rollups\" WHERE {rollup_cond}"), binds)? {
                        if let Some(g) = r.s("group_hash")
                            && !groups.iter().any(|x| x == g) {
                                groups.push(g.to_string());
                            }
                    }
                    in_groups_or_fresh(&mut c, "job_attempt", &groups, range);
                    c.eq(table, "name", s(name));
                }
            }
            if let Some(j) = parsed.field("job_id") {
                c.eq(table, "job_id", s(j));
            }
            if rb::present(Some(&text)) {
                named_like(conn, &mut c, "job_attempt", &text, range, true)?;
            }
        }
        Resource::Exceptions => {
            let handled = parsed.fields.get("handled").cloned().or_else(|| match parsed.fields.get("status").map(String::as_str) {
                Some("handled") => Some("true".into()),
                Some("unhandled") => Some("false".into()),
                _ => None,
            });
            if let Some(cl) = parsed.field("class") {
                c.eq(table, "class_name", s(cl));
            }
            if let Some(h) = handled.as_deref().filter(|h| *h == "true" || *h == "false") {
                c.eq(table, "handled", Sql::Integer((h == "true") as i64));
            }
            if let Some(sv) = parsed.field("severity") {
                c.eq(table, "severity", s(sv));
            }
            if rb::present(Some(&text)) {
                let pat = format!("%{}%", rb::sanitize_sql_like(&text));
                c.and("(message LIKE ? OR class_name LIKE ?)", vec![s(&pat), s(&pat)]);
            }
        }
        Resource::Logs => {
            let level = parsed.field("level").or(parsed.field("status"));
            if let Some(l) = level.filter(|l| LOG_LEVELS.contains(l)) {
                c.eq(table, "level", s(l));
            }
            if rb::present(Some(&text)) {
                match match_expression(&text) {
                    Some(m) => {
                        c.and("logs.id IN (SELECT rowid FROM logs_fts WHERE logs_fts MATCH ?)", vec![s(&m)]);
                    }
                    None => c.none = true,
                }
            }
        }
        Resource::Queries => {
            for f in ["adapter", "connection", "role"] {
                if let Some(v) = parsed.field(f) {
                    c.eq(table, f, s(v));
                }
            }
            if rb::present(Some(&text)) {
                let pat = format!("%{}%", rb::sanitize_sql_like(&text));
                c.and(
                    "(queries.sql LIKE ? ESCAPE '\\' OR queries.name LIKE ? ESCAPE '\\' OR queries.group_hash IN (SELECT group_hash FROM query_shapes WHERE sql LIKE ? ESCAPE '\\'))",
                    vec![s(&pat), s(&pat), s(&pat)],
                );
            }
        }
    }
    Ok(c)
}

pub fn md5_hex(s: &str) -> String {
    use md5::Digest;
    md5::Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn range_sql(col: &str, (lo, hi, empty): (i64, i64, bool)) -> (String, Vec<Sql>) {
    if empty {
        (format!("{col} >= ? AND {col} < ?"), vec![s(&qt(lo)), s(&qt(hi))])
    } else {
        (format!("{col} BETWEEN ? AND ?"), vec![s(&qt(lo)), s(&qt(hi))])
    }
}

/// `Execution.in_groups_or_fresh(kind, groups, range)`, as a condition.
pub fn in_groups_or_fresh(c: &mut Cond, kind: &str, groups: &[String], range: (i64, i64, bool)) {
    let (lo, hi, _) = range;
    let (rsql, rb_) = range_sql("e2.occurred_at", range);
    let ph = vec!["?"; groups.len()].join(", ");
    let mut binds = vec![s(kind)];
    binds.extend(rb_);
    binds.extend(groups.iter().map(|g| s(g)));
    binds.push(s(kind));
    binds.push(s(&qt(lo.max(hi - FRESH_NAMES))));
    binds.push(s(&qt(hi)));
    c.and(
        format!(
            "(\"executions\".id IN (SELECT e2.id FROM \"executions\" e2 INDEXED BY index_executions_on_group_hash_and_occurred_at \
             WHERE e2.kind = ? AND {rsql} AND e2.group_hash IN ({ph})) \
             OR \"executions\".id IN (SELECT e3.id FROM \"executions\" e3 WHERE e3.kind = ? AND e3.occurred_at BETWEEN ? AND ?))"
        ),
        binds,
    );
    c.from = Some("\"executions\" NOT INDEXED".into());
}

/// `Execution.named_like(kind, text, range, previews:)`, as a condition.
pub fn named_like(conn: &Connection, c: &mut Cond, kind: &str, text: &str, range: (i64, i64, bool), previews: bool) -> rusqlite::Result<()> {
    let (lo, hi, _) = range;
    let pat = format!("%{}%", rb::sanitize_sql_like(text));
    let groups_cond = "\"rollups\".\"record_type\" = ?1 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3 AND (name LIKE ?4 ESCAPE '\\')";
    let gbinds = rusqlite::params![kind, qt(rb::beginning_of_hour(lo)), qt(hi), pat];
    let sum = db::count(conn, &format!("SELECT SUM(\"rollups\".\"count\") FROM \"rollups\" WHERE {groups_cond}"), gbinds)?;
    if sum >= DENSE_MATCHES {
        if previews {
            c.and("(name LIKE ? ESCAPE '\\' OR exception_preview LIKE ? ESCAPE '\\')", vec![s(&pat), s(&pat)]);
        } else {
            c.and("(name LIKE ? ESCAPE '\\')", vec![s(&pat)]);
        }
        return Ok(());
    }
    let (rsql, rbinds) = range_sql("e2.occurred_at", range);
    let mut branches = vec![];
    let mut binds: Vec<Sql> = vec![];
    branches.push(format!(
        "\"executions\".id IN (SELECT e2.id FROM \"executions\" e2 INDEXED BY index_executions_on_group_hash_and_occurred_at \
         WHERE e2.group_hash IN (SELECT DISTINCT \"rollups\".\"group_hash\" FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ? \
         AND \"rollups\".\"bucket\" BETWEEN ? AND ? AND (name LIKE ? ESCAPE '\\')) AND {rsql} AND e2.kind = ?)"
    ));
    binds.extend([s(kind), s(&qt(rb::beginning_of_hour(lo))), s(&qt(hi)), s(&pat)]);
    binds.extend(rbinds.clone());
    binds.push(s(kind));
    branches.push(
        "\"executions\".id IN (SELECT e3.id FROM \"executions\" e3 WHERE e3.kind = ? AND e3.occurred_at BETWEEN ? AND ? AND (e3.name LIKE ? ESCAPE '\\'))".into(),
    );
    binds.extend([s(kind), s(&qt(lo.max(hi - FRESH_NAMES))), s(&qt(hi)), s(&pat)]);
    if previews {
        let (rsql4, rb4) = range_sql("e4.occurred_at", range);
        branches.push(format!(
            "\"executions\".id IN (SELECT e4.id FROM \"executions\" e4 INDEXED BY idx_executions_with_preview WHERE e4.kind = ? AND {rsql4} \
             AND e4.exception_preview IS NOT NULL AND (e4.exception_preview LIKE ? ESCAPE '\\'))"
        ));
        binds.push(s(kind));
        binds.extend(rb4);
        binds.push(s(&pat));
    }
    c.and(format!("({})", branches.join(" OR ")), binds);
    c.from = Some("\"executions\" NOT INDEXED".into());
    Ok(())
}

// -- Telemetry::Log FTS ---------------------------------------------------

pub const SNIPPET_MARK: &str = "\u{1F}";

/// `Log.match_expression`.
pub fn match_expression(text: &str) -> Option<String> {
    let mut terms: Vec<(bool, String)> = Vec::new();
    for term in scan_terms(text) {
        if let Some(t) = parse_term(&term) {
            terms.push(t);
        }
    }
    let mut positive: Vec<String> = terms.iter().filter(|t| !t.0).map(|t| t.1.clone()).collect();
    let mut negative: Vec<String> = terms.iter().filter(|t| t.0).map(|t| t.1.clone()).collect();
    if positive.is_empty() {
        positive = negative;
        negative = vec![];
    }
    if positive.is_empty() {
        return None;
    }
    let mut out = vec![positive.join(" ")];
    out.extend(negative.into_iter().map(|t| format!("NOT {t}")));
    Some(out.join(" "))
}

/// TERM = /-?"[^"]*"|\S+/
fn scan_terms(text: &str) -> Vec<String> {
    let cs: Vec<char> = text.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < cs.len() {
        if cs[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let q = if cs[i] == '-' && cs.get(i + 1) == Some(&'"') { i + 1 } else { i };
        if cs[q] == '"'
            && let Some(end) = cs[q + 1..].iter().position(|&c| c == '"') {
                let stop = q + 1 + end + 1;
                out.push(cs[start..stop].iter().collect());
                i = stop;
                continue;
            }
        while i < cs.len() && !cs[i].is_whitespace() {
            i += 1;
        }
        out.push(cs[start..i].iter().collect());
    }
    out
}

fn parse_term(term: &str) -> Option<(bool, String)> {
    let negated = term.starts_with('-') && term.chars().count() > 1;
    let body = if negated { &term[1..] } else { term };
    let phrase = body.starts_with('"') && body.ends_with('"') && body.chars().count() > 1;
    let body = if phrase { &body[1..body.len() - 1] } else { body };
    if !body.chars().any(char::is_alphanumeric) {
        return None;
    }
    let quoted = format!("\"{}\"", body.replace('"', "\"\""));
    Some((negated, if phrase { quoted } else { format!("{quoted}*") }))
}

/// `Log.fts_snippets(ids, text)`: id => snippet.
pub fn fts_snippets(conn: &Connection, ids: &[i64], text: &str) -> rusqlite::Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let Some(m) = match_expression(text) else { return Ok(out) };
    let ph = vec!["?"; ids.len()].join(",");
    let sql = format!("SELECT rowid, snippet(logs_fts, 0, ?, ?, ?, 24) FROM logs_fts WHERE logs_fts MATCH ? AND rowid IN ({ph})");
    let mut binds: Vec<Sql> = vec![s(SNIPPET_MARK), s(SNIPPET_MARK), s("…"), s(&m)];
    binds.extend(ids.iter().map(|i| Sql::Integer(*i)));
    for r in db::query(conn, &sql, rusqlite::params_from_iter(binds))? {
        out.insert(db::raw_json(r.at(0)).to_string(), db::raw_json(r.at(1)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar() {
        let p = parse(Some("status:5xx  route:/widgets slow Path"));
        assert_eq!(p.text, "slow Path");
        assert_eq!(p.fields.get("status").unwrap(), "5xx");
        // Uppercase keys and empty values are text.
        assert_eq!(parse(Some("Status:5xx a:")).text, "Status:5xx a:");
        assert_eq!(status_range(Some("4XX")), Some((400, 499)));
        assert_eq!(status_range(Some("404")), Some((404, 404)));
        assert_eq!(status_range(Some("1xx")), None);
    }

    #[test]
    fn fts_expression() {
        assert_eq!(match_expression("widg -boom \"exact phrase\"").unwrap(), "\"widg\"* \"exact phrase\" NOT \"boom\"*");
        assert_eq!(match_expression("-only").unwrap(), "\"only\"*");
        assert_eq!(match_expression("*** ::"), None);
        assert_eq!(match_expression("NEAR(a").unwrap(), "\"NEAR(a\"*");
    }
}
