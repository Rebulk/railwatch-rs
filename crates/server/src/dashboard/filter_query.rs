//! Port of Railwatch::FilterQuery: the "key:value free text" grammar, the
//! per-resource `apply` (as SQL conditions with bound values), and
//! `filter_routes`.

use indexmap::IndexMap;
use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use serde_json::Value;

use super::db::{self, qt};
use super::rb;
use super::window::parse_time;

/// What a record ran in (`kind:`, and `source:` naming one of them): the
/// gem's execution sources.
pub const EXECUTION_KINDS: [&str; 5] = ["request", "job", "scheduled_task", "command", "channel_action"];
pub const LOG_LEVELS: [&str; 7] = ["debug", "info", "warn", "error", "fatal", "event", "unknown"];
/// A job attempt's outcome (`outcome:`/`status:` on jobs), as the gem
/// records it.
pub const JOB_OUTCOMES: [&str; 4] = ["processed", "failed", "released", "aborted"];
/// An exception's severity (Rails.error's and Railwatch.report's).
pub const SEVERITIES: [&str; 4] = ["fatal", "error", "warning", "info"];
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
/// A value may be quoted to hold spaces (`ua:"Mozilla/5.0 (X11"`): from
/// `key:"` to the next `"`, or to the end without one.
pub fn parse(query: Option<&str>) -> Parsed {
    let mut p = Parsed::default();
    let mut text = Vec::new();
    let mut rest = query.unwrap_or("");
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| c.is_ascii_whitespace()).unwrap_or(rest.len());
        let word = &rest[..end];
        if let Some((k, v)) = word.split_once(':')
            && !k.is_empty()
            && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            && !v.is_empty()
        {
            if let Some(after) = rest[k.len() + 1..].strip_prefix('"') {
                let close = after.find('"').unwrap_or(after.len());
                if close > 0 {
                    p.fields.insert(k.to_string(), after[..close].to_string());
                }
                rest = after.get(close + 1..).unwrap_or("");
                continue;
            }
            p.fields.insert(k.to_string(), v.to_string());
            rest = &rest[end..];
            continue;
        }
        if rb::present(Some(word)) {
            text.push(word);
        }
        rest = &rest[end..];
    }
    p.text = text.join(" ");
    p
}

/// `parse`'s inverse for one field: quoted when the value holds whitespace
/// (a value with a `"` in it cannot be written; it is cut there).
pub fn token(key: &str, value: &str) -> String {
    if value.contains(|c: char| c.is_ascii_whitespace()) { format!("{key}:\"{}\"", value.split('"').next().unwrap_or("")) } else { format!("{key}:{value}") }
}

/// A query from fields and free text, in `parse`'s grammar.
pub fn unparse(p: &Parsed) -> String {
    let mut out: Vec<String> = p.fields.iter().map(|(k, v)| token(k, v)).collect();
    if !p.text.is_empty() {
        out.push(p.text.clone());
    }
    out.join(" ")
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
        let prefixes: Vec<String> = alternatives(m).map(|m| format!("{} ", m.to_uppercase())).collect();
        routes.retain(|r| r["name"].as_str().is_some_and(|n| prefixes.iter().any(|p| n.starts_with(p))));
    }
    if let Some(route) = p.field("route") {
        routes.retain(|r| r["name"].as_str().is_some_and(|n| alternatives(route).any(|a| n.contains(a))));
    }
    let statuses: Vec<i64> = p.fields.get("status").map_or(vec![], |v| alternatives(v).filter_map(|x| status_range(Some(x)).map(|r| r.0)).collect());
    if !statuses.is_empty() {
        routes.retain(|r| {
            let (c, e, ce) = (r["count"].as_i64().unwrap_or(0), r["errors"].as_i64().unwrap_or(0), r["client_errors"].as_i64().unwrap_or(0));
            statuses.iter().any(|lo| match lo {
                500..=599 => e > 0,
                400..=499 => ce > 0,
                _ => c - e - ce > 0,
            })
        });
    }
    routes
}

/// SHA256 of `[environment_id, resource, query.to_s, [from, to]].to_json`.
pub fn cursor_context(environment_id: i64, resource: &str, query: Option<&str>, from: i64, to: i64) -> String {
    use sha2::Digest;
    let json =
        serde_json::to_string(&serde_json::json!([environment_id, resource, query.unwrap_or(""), [crate::time::iso8601_6(from), crate::time::iso8601_6(to)]]))
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
    /// The occurred_at range `apply` filtered on (epoch micros), so a
    /// per-day read can tell which day files it covers whole.
    pub range: Option<(i64, i64)>,
    /// A request-key filter on child records kept only the newest
    /// MAX_REQUEST_IDS matching requests.
    pub capped: bool,
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
    /// `col` equal to any of `a|b|c` (`= ?` for one value). No value at all
    /// (`level:|`) matches nothing; `check` reports it.
    pub fn any_of(&mut self, table: &str, col: &str, v: &str) -> &mut Cond {
        let alts: Vec<&str> = alternatives(v).collect();
        match alts.len() {
            0 => {
                self.none = true;
                self
            }
            1 => self.eq(table, col, s(alts[0])),
            n => self.and(format!("\"{table}\".\"{col}\" IN ({})", vec!["?"; n].join(", ")), alts.into_iter().map(s).collect()),
        }
    }
    /// The WHERE clause. A `none` relation keeps its parts (their `?`s are
    /// what `binds` fills) and adds `1=0`: an empty `1=0` with binds left
    /// over failed as "wrong number of parameters" (search_logs with an
    /// unusable text query, a request key matching no request).
    pub fn sql(&self) -> String {
        if self.parts.is_empty() {
            return if self.none { "1=0" } else { "1=1" }.into();
        }
        let all = self.parts.join(" AND ");
        if self.none { format!("1=0 AND {all}") } else { all }
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
    c.range = Some((lo, hi));
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
    /// The resource's name, as `check` and the MCP tools take it.
    pub fn name(&self) -> &'static str {
        match self {
            Resource::Jobs => "jobs",
            Resource::Exceptions => "exceptions",
            Resource::Logs => "logs",
            Resource::Queries => "queries",
        }
    }

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
    // Callers report what is wrong (check); a query they did not check
    // matches nothing rather than less than it says.
    if check(resource.name(), &parsed).is_some() {
        c.none = true;
        return Ok(c);
    }
    // apply_common
    if let Some(v) = parsed.field("user") {
        c.any_of(table, "user_ref", v);
    }
    if let Some(v) = parsed.field("tenant") {
        c.any_of(table, "app_tenant", v);
    }
    if let Some(v) = parsed.field("deploy") {
        c.any_of(table, "deploy", v);
    }
    if let Some(v) = parsed.field("source") {
        // An execution kind is the record's execution_source; anything else
        // a prefix of its source file.
        let mut ors = vec![];
        let mut binds = vec![];
        for alt in alternatives(v) {
            if EXECUTION_KINDS.contains(&alt) && has_column(table, "execution_source") {
                ors.push(format!("\"{table}\".\"execution_source\" = ?"));
                binds.push(s(alt));
            } else if has_column(table, "source") {
                ors.push(format!("\"{table}\".\"source\" LIKE ?"));
                binds.push(s(&format!("{}%", rb::sanitize_sql_like(alt))));
            }
        }
        match ors.len() {
            0 => {}
            1 => {
                c.and(ors.remove(0), binds);
            }
            _ => {
                c.and(format!("({})", ors.join(" OR ")), binds);
            }
        }
    }
    if let Some(k) = parsed.field("kind")
        && has_column(table, "execution_source")
    {
        c.any_of(table, "execution_source", k);
    }
    let text = parsed.text.clone();
    match resource {
        Resource::Jobs => {
            if let Some(q) = parsed.field("queue") {
                c.any_of(table, "queue", q);
            }
            if let Some(o) = parsed.field("outcome").or(parsed.field("status")) {
                c.any_of(table, "outcome", o);
            }
            if let Some(class) = parsed.field("class") {
                let names: Vec<&str> = alternatives(class).collect();
                let (lo, hi, _) = range;
                let rollup_cond = format!(
                    "\"rollups\".\"record_type\" = 'job_attempt' AND \"rollups\".\"bucket\" BETWEEN ? AND ? AND \"rollups\".\"name\" IN ({})",
                    vec!["?"; names.len()].join(", ")
                );
                let mut binds = vec![s(&qt(rb::beginning_of_hour(lo))), s(&qt(hi))];
                binds.extend(names.iter().map(|n| s(n)));
                let binds = rusqlite::params_from_iter(binds.iter());
                let sum = db::count(conn, &format!("SELECT SUM(\"rollups\".\"count\") FROM \"rollups\" WHERE {rollup_cond}"), binds.clone())?;
                if sum >= DENSE_MATCHES {
                    c.any_of(table, "name", class);
                } else {
                    let mut groups: Vec<String> = names.iter().map(|n| md5_hex(n)).collect();
                    for r in db::query(conn, &format!("SELECT DISTINCT \"rollups\".\"group_hash\" FROM \"rollups\" WHERE {rollup_cond}"), binds)? {
                        if let Some(g) = r.s("group_hash")
                            && !groups.iter().any(|x| x == g)
                        {
                            groups.push(g.to_string());
                        }
                    }
                    in_groups_or_fresh(&mut c, "job_attempt", &groups, range);
                    c.any_of(table, "name", class);
                }
            }
            if let Some(j) = parsed.field("job_id") {
                c.any_of(table, "job_id", j);
            }
            if rb::present(Some(&text)) {
                named_like(conn, &mut c, "job_attempt", &text, range, true)?;
            }
        }
        Resource::Exceptions => {
            // handled:true|false, or status:handled|unhandled: both is all.
            let handled: Vec<bool> = match parsed.field("handled") {
                Some(h) => alternatives(h).map(|h| h == "true").collect(),
                None => parsed.field("status").map_or(vec![], |st| alternatives(st).map(|st| st == "handled").collect()),
            };
            if let Some(cl) = parsed.field("class") {
                c.any_of(table, "class_name", cl);
            }
            if let Some(h) = handled.first()
                && handled.iter().all(|x| x == h)
            {
                c.eq(table, "handled", Sql::Integer(*h as i64));
            }
            if let Some(sv) = parsed.field("severity") {
                c.any_of(table, "severity", sv);
            }
            if rb::present(Some(&text)) {
                let pat = format!("%{}%", rb::sanitize_sql_like(&text));
                c.and("(message LIKE ? OR class_name LIKE ?)", vec![s(&pat), s(&pat)]);
            }
        }
        Resource::Logs => {
            if let Some(l) = parsed.field("level").or(parsed.field("status")) {
                c.any_of(table, "level", l);
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
                    c.any_of(table, f, v);
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
    // A job's request is the one that enqueued it; a job's outcome: is its own.
    let (column, parsed) = if matches!(resource, Resource::Jobs) { ("parent_id", without_job_keys(&parsed)) } else { ("execution_id", parsed) };
    request_detail_of(conn, &mut c, table, column, &parsed, range)?;
    Ok(c)
}

// -- Request fields (detail JSON) -------------------------------------------
//
// What the gem records of a request beyond the executions columns (path,
// ip, user agent, headers, the security flags) is in its `detail` JSON, with
// no index: these keys narrow the window's requests row by row
// (json_extract), so their cost is the window's request count, not the
// matches (docs/PERF.md, "Security filters").

/// Keys that read a request's detail. On jobs, exceptions, logs and queries
/// they select the records of matching requests (the request's
/// execution_id; a job's parent_id).
pub const REQUEST_KEYS: [&str; 10] = ["ip", "ua", "path", "probe", "rate_limited", "halted", "unpermitted", "outcome", "session", "tab"];

/// `p` as a job query reads it: `outcome:` is the job's own (processed,
/// failed), not its request's.
pub fn without_job_keys(p: &Parsed) -> Parsed {
    let mut p = p.clone();
    p.fields.shift_remove("outcome");
    p
}

/// Keys that read personal data (the client's address and browser): only
/// with security access (a token's `security` scope, a dashboard session).
pub const PERSONAL_KEYS: [&str; 2] = ["ip", "ua"];

/// Path fragments of scanners and probes: dotfiles, other stacks' admin
/// pages, traversal, shells. Matched case-insensitively anywhere in the
/// recorded path (percent-encoded as the client sent it).
pub const PROBE_PATTERNS: [&str; 20] = [
    ".env",
    ".git",
    ".svn",
    ".htaccess",
    ".htpasswd",
    ".aws",
    ".ssh",
    ".ds_store",
    "/wp-",
    "wordpress",
    "xmlrpc",
    "phpmyadmin",
    ".php",
    "/cgi-bin",
    "../",
    "..%2f",
    "%2e%2e",
    "/etc/passwd",
    "/actuator",
    "/server-status",
];

/// `json_extract` of a request field (`$.ip`, `$.headers."Referer"`).
pub fn detail_sql(alias: &str, path: &str) -> String {
    detail_fn(alias, "json_extract", path)
}

/// `f("alias"."detail", 'path')`: a JSON function of a request field, in the
/// form the request index holds it (schema::ADDED_INDEXES). SQLite reads an
/// expression from the index only when it is the index's own expression
/// (the table alias aside), so every request field read goes through here.
pub fn detail_fn(alias: &str, f: &str, path: &str) -> String {
    format!("{f}(\"{alias}\".\"detail\", '{path}')")
}

/// The keys of `p` that need security access.
pub fn personal_keys(p: &Parsed) -> Vec<&'static str> {
    PERSONAL_KEYS.iter().copied().filter(|k| p.field(k).is_some()).collect()
}

/// The keys every record resource (jobs, exceptions, logs, queries) takes.
const COMMON_KEYS: [&str; 6] = ["after", "before", "user", "tenant", "deploy", "kind"];

/// The keys `resource` (requests, jobs, exceptions, logs, queries) reads:
/// any other is an error, not a filter quietly left out.
pub fn keys_of(resource: &str) -> Vec<&'static str> {
    let own: &[&'static str] = match resource {
        "requests" => &["after", "before", "user", "tenant", "deploy", "method", "route", "status", "min_ms"],
        "jobs" => &["class", "job_id", "outcome", "queue", "status"],
        "exceptions" => &["source", "class", "handled", "severity", "status"],
        "logs" => &["source", "level", "status"],
        "queries" => &["source", "adapter", "connection", "role"],
        _ => &[],
    };
    let mut keys: Vec<&'static str> = if resource == "requests" { vec![] } else { COMMON_KEYS.to_vec() };
    keys.extend(own);
    for k in REQUEST_KEYS {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

/// What is wrong with `p` as a query of `resource`, for the caller to
/// report: a key the resource does not read, a value an enum-like key does
/// not take (with the values it does), a key with no value, a time or a
/// number that does not parse, and the request keys' own checks. `apply`
/// and `requests` match nothing on such a query; nothing is dropped.
pub fn check(resource: &str, p: &Parsed) -> Option<String> {
    let keys = keys_of(resource);
    if let Some(k) = p.fields.keys().find(|k| !keys.contains(&k.as_str())) {
        return Some(format!("{k}: is not a key of {resource} (its keys: {})", keys.join(", ")));
    }
    for (k, v) in &p.fields {
        if alternatives(v).next().is_none() {
            return Some(format!("{k}: has no value"));
        }
    }
    let one_of = |k: &str, valid: &[&str]| -> Option<String> {
        let v = p.field(k)?;
        let bad = alternatives(v).find(|a| !valid.contains(a))?;
        Some(format!("{k}: {bad:?} is not one of {}", valid.join(", ")))
    };
    // status: is the resource's own word for its outcome (on requests, a
    // code or class, below); outcome: on anything but a job is the request
    // key (check_request_keys).
    let (status, outcome): (&[&str], &[&str]) = match resource {
        "jobs" => (&JOB_OUTCOMES, &JOB_OUTCOMES),
        "exceptions" => (&["handled", "unhandled"], &[]),
        "logs" => (&LOG_LEVELS, &[]),
        _ => (&[], &[]),
    };
    let enums: [(&str, &[&str]); 6] = [
        ("kind", &EXECUTION_KINDS),
        ("level", &LOG_LEVELS),
        ("severity", &SEVERITIES),
        ("handled", &["true", "false"]),
        ("status", status),
        ("outcome", outcome),
    ];
    if let Some(bad) = enums.iter().filter(|(_, valid)| !valid.is_empty()).find_map(|(k, valid)| one_of(k, valid)) {
        return Some(bad);
    }
    if resource == "requests"
        && let Some(v) = p.field("status")
        && let Some(bad) = alternatives(v).find(|a| status_range(Some(a)).is_none())
    {
        return Some(format!("status: {bad:?} is not a status (a code, 404, or a class, 4xx)"));
    }
    if let Some(v) = p.field("min_ms")
        && v.parse::<f64>().ok().filter(|ms| ms.is_finite() && *ms >= 0.0).is_none()
    {
        return Some(format!("min_ms: {v:?} is not a number of milliseconds"));
    }
    for k in ["after", "before"] {
        if let Some(v) = p.field(k)
            && parse_time(v).is_none()
        {
            return Some(format!("{k}: {v:?} is not a time (2026-10-29, 2026-10-29T14:00:00Z)"));
        }
    }
    // A job's outcome: is its own (processed, failed), not its request's.
    if resource == "jobs" { check_request_keys(&without_job_keys(p)) } else { check_request_keys(p) }
}

/// Whether the requests table for `p` needs the raw requests: the route
/// rollups (filter_routes) hold only the method, route and status class, so
/// any other key (a request key, user:, tenant:, deploy:, min_ms:, after:,
/// before:) or free text is read from the requests themselves rather than
/// dropped.
pub fn needs_raw_requests(p: &Parsed) -> bool {
    rb::present(Some(&p.text)) || p.fields.keys().any(|k| !["method", "route", "status"].contains(&k.as_str()))
}

/// Whether `p` has a request-detail key.
pub fn has_request_keys(p: &Parsed) -> bool {
    REQUEST_KEYS.iter().any(|k| p.field(k).is_some())
}

/// What is wrong with the request keys' values, for the caller to report
/// (the conditions themselves match nothing on a bad value).
pub fn check_request_keys(p: &Parsed) -> Option<String> {
    if let Some(v) = p.field("probe")
        && v != "true"
        && v != "false"
    {
        return Some(format!("probe: must be true or false, not {v:?}"));
    }
    // true, false, or names (halted:require_admin|authenticate).
    for k in ["rate_limited", "halted", "unpermitted"] {
        if let Some(v) = p.field(k)
            && v.split('|').any(|a| (a == "true" || a == "false") && a != v)
        {
            return Some(format!("{k}: true or false stands alone, not among names ({v:?})"));
        }
    }
    if let Some(v) = p.field("outcome")
        && let Some(bad) = v.split('|').find(|a| !super::request_scan::OUTCOMES.contains(a))
    {
        return Some(format!("outcome: {bad:?} is not one of {}", super::request_scan::OUTCOMES.join(", ")));
    }
    if let Some(ip) = p.field("ip") {
        for alt in ip.split('|') {
            if let Err(e) = ip_condition(alt) {
                return Some(format!("ip: {e}"));
            }
        }
    }
    for k in ["session", "tab"] {
        if let Some(v) = p.field(k)
            && !(k == "tab" && v == "none")
            && let Some(bad) = v.split('|').find(|id| !valid_session_id(id))
        {
            return Some(format!("{k}: {bad:?} is not a {k} id (letters, digits, - and _, 64 at most)"));
        }
    }
    None
}

/// A session or tab id as the gem sends one: 1-64 of [A-Za-z0-9_-].
pub fn valid_session_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One `ip:` alternative as (exact addresses, LIKE patterns): exact,
/// `prefix*`, or an IPv4 CIDR as the dotted prefixes it covers (a /20 is
/// 16 `a.b.c.%` patterns, a /28 the 16 addresses).
fn ip_condition(v: &str) -> Result<(Vec<String>, Vec<String>), String> {
    if let Some(prefix) = v.strip_suffix('*') {
        return Ok((vec![], vec![format!("{}%", rb::sanitize_sql_like(prefix))]));
    }
    let Some((net, bits)) = v.split_once('/') else { return Ok((vec![v.to_string()], vec![])) };
    let addr: std::net::Ipv4Addr = net.parse().map_err(|_| format!("{v:?} is not an IPv4 CIDR (for IPv6 use a prefix, e.g. ip:2001:db8:*)"))?;
    let bits: u32 = bits.parse().ok().filter(|b| (8..=32).contains(b)).ok_or_else(|| format!("{v:?}: the prefix length must be 8-32"))?;
    let o = addr.octets();
    let whole = (bits / 8) as usize;
    let kept: Vec<String> = o[..whole].iter().map(u8::to_string).collect();
    if whole == 4 {
        return Ok((vec![kept.join(".")], vec![]));
    }
    // The partial octet's values (one when the length is a whole octet).
    let spare = 8 - bits % 8;
    let base = (o[whole] as u16) & !((1u16 << spare) - 1);
    let values: Vec<u16> = if bits.is_multiple_of(8) { vec![] } else { (base..base + (1 << spare)).collect() };
    if values.is_empty() {
        return Ok((vec![], vec![format!("{}.%", kept.join("."))]));
    }
    let with = |x: u16| {
        let mut parts = kept.clone();
        parts.push(x.to_string());
        parts.join(".")
    };
    if whole == 3 {
        return Ok((values.into_iter().map(with).collect(), vec![]));
    }
    Ok((vec![], values.into_iter().map(|x| format!("{}.%", with(x))).collect()))
}

/// `a|b|c`: alternatives of one key, ORed.
fn alternatives(v: &str) -> impl Iterator<Item = &str> {
    v.split('|').filter(|a| !a.is_empty())
}

/// The request-detail keys of `p` as conditions on `alias` (an executions
/// row of kind request). A bad value matches nothing (check_request_keys
/// says why).
pub fn request_detail(c: &mut Cond, alias: &str, p: &Parsed) {
    // Only the request keys' own checks: a child-record caller has checked
    // the rest against its resource.
    if check_request_keys(p).is_some() {
        c.none = true;
        return;
    }
    if let Some(ip) = p.field("ip") {
        let col = detail_sql(alias, "$.ip");
        let mut ors = vec![];
        let mut binds = vec![];
        for alt in alternatives(ip) {
            let (exact, likes) = ip_condition(alt).expect("checked");
            for e in exact {
                ors.push(format!("{col} = ?"));
                binds.push(s(&e));
            }
            for l in likes {
                ors.push(format!("{col} LIKE ? ESCAPE '\\'"));
                binds.push(s(&l));
            }
        }
        if ors.is_empty() {
            c.none = true;
        } else {
            c.and(format!("({})", ors.join(" OR ")), binds);
        }
    }
    for (key, path) in [("ua", "$.user_agent"), ("path", "$.path")] {
        let Some(v) = p.field(key) else { continue };
        let col = detail_sql(alias, path);
        let mut ors = vec![];
        let mut binds = vec![];
        for alt in alternatives(v) {
            // path:/admin* is a prefix; anything else a substring.
            let pat = match alt.strip_suffix('*') {
                Some(prefix) if key == "path" => format!("{}%", rb::sanitize_sql_like(prefix)),
                _ => format!("%{}%", rb::sanitize_sql_like(alt)),
            };
            ors.push(format!("{col} LIKE ? ESCAPE '\\'"));
            binds.push(s(&pat));
        }
        if ors.is_empty() {
            c.none = true;
        } else {
            c.and(format!("({})", ors.join(" OR ")), binds);
        }
    }
    if let Some(v) = p.field("probe") {
        let col = detail_sql(alias, "$.path");
        let ors = vec![format!("{col} LIKE ? ESCAPE '\\'"); PROBE_PATTERNS.len()].join(" OR ");
        let binds = PROBE_PATTERNS.iter().map(|x| s(&format!("%{}%", rb::sanitize_sql_like(x)))).collect();
        c.and(if v == "true" { format!("({ors})") } else { format!("(NOT ({ors}) OR {col} IS NULL)") }, binds);
    }
    // true / false, or the names (any of them with |): which rate_limit,
    // which before_action, which dropped parameter.
    let flag = |c: &mut Cond, key: &str, set: String, named: &dyn Fn(usize) -> String| {
        let Some(v) = p.field(key) else { return };
        match v {
            "true" => c.and(set, vec![]),
            "false" => c.and(format!("NOT ({set})"), vec![]),
            names => {
                let names: Vec<&str> = alternatives(names).collect();
                c.and(format!("({set} AND {})", named(names.len())), names.into_iter().map(s).collect())
            }
        };
    };
    let rl = detail_sql(alias, "$.rate_limited.name");
    flag(c, "rate_limited", format!("COALESCE({} NOT IN ('null', 'false'), 0)", detail_fn(alias, "json_type", "$.rate_limited")), &|n| {
        format!("{rl} IN ({})", vec!["?"; n].join(", "))
    });
    let halted = detail_sql(alias, "$.halted_callback");
    flag(c, "halted", format!("COALESCE({halted} != '', 0)"), &|n| format!("{halted} IN ({})", vec!["?"; n].join(", ")));
    let keys = detail_sql(alias, "$.unpermitted_parameters");
    flag(c, "unpermitted", format!("COALESCE({} > 0, 0)", detail_fn(alias, "json_array_length", "$.unpermitted_parameters")), &|n| {
        format!("EXISTS (SELECT 1 FROM json_each({keys}) WHERE value IN ({}))", vec!["?"; n].join(", "))
    });
    if let Some(v) = p.field("outcome") {
        let outcomes: Vec<&str> = alternatives(v).collect();
        c.and(format!("{} IN ({})", super::request_scan::outcome_sql(alias), vec!["?"; outcomes.len()].join(", ")), outcomes.into_iter().map(s).collect());
    }
    // The browser session and tab (docs/sessions.md), exact ids; tab:none
    // is a request without a tab (a full page load sends the session
    // cookie, not the tab header). A session journey's expand names both.
    if let Some(v) = p.field("session") {
        let ids: Vec<&str> = alternatives(v).collect();
        c.and(format!("{} IN ({})", detail_sql(alias, "$.session_id"), vec!["?"; ids.len()].join(", ")), ids.into_iter().map(s).collect());
    }
    if let Some(v) = p.field("tab") {
        let col = detail_sql(alias, "$.tab_id");
        if v == "none" {
            c.and(format!("{col} IS NULL"), vec![]);
        } else {
            let ids: Vec<&str> = alternatives(v).collect();
            c.and(format!("{col} IN ({})", vec!["?"; ids.len()].join(", ")), ids.into_iter().map(s).collect());
        }
    }
}

/// Matching requests a child-record filter resolves at most: their ids are
/// the filter, as a list of values.
pub const MAX_REQUEST_IDS: usize = 20_000;

/// The request keys of `p` on a child record (or job): its execution_id
/// (parent_id) among the matching requests' within an hour before the window
/// (a request starts before the records it makes). The requests are read
/// first and their ids bound as a list: as a subquery the per-day views
/// ran it once per day file (exceptions by ip:, 30 days: 8.3 s -> see
/// docs/PERF.md, "Security filters"). Past MAX_REQUEST_IDS matching
/// requests, the newest are kept and `capped` is set.
fn request_detail_of(conn: &Connection, c: &mut Cond, table: &str, column: &str, p: &Parsed, (lo, hi, _): (i64, i64, bool)) -> rusqlite::Result<()> {
    if !has_request_keys(p) {
        return Ok(());
    }
    let mut inner = Cond::new();
    inner.and("\"executions\".\"kind\" = 'request' AND \"executions\".\"occurred_at\" BETWEEN ? AND ?", vec![s(&qt(lo - 3_600_000_000)), s(&qt(hi))]);
    request_detail(&mut inner, "executions", p);
    if inner.none {
        c.none = true;
        return Ok(());
    }
    let from = if p.field("session").is_some() { session_index(conn)? } else { request_index(conn)? }.unwrap_or_else(|| "\"executions\"".into());
    let rows = db::query(
        conn,
        &format!(
            "SELECT \"executions\".\"execution_id\" FROM {from} WHERE {} AND \"executions\".\"execution_id\" IS NOT NULL \
             ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {}",
            inner.sql(),
            MAX_REQUEST_IDS + 1
        ),
        rusqlite::params_from_iter(inner.binds.iter()),
    )?;
    if rows.len() > MAX_REQUEST_IDS {
        c.capped = true;
    }
    let ids: Vec<Sql> = rows.iter().take(MAX_REQUEST_IDS).filter_map(|r| r.s("execution_id").map(s)).collect();
    if ids.is_empty() {
        c.none = true;
        return Ok(());
    }
    // json_each over one bound array: one statement whatever the count
    // (SQLite caps bound parameters at 32,766).
    let list = serde_json::to_string(&ids.iter().map(|v| if let Sql::Text(t) = v { t.as_str() } else { "" }).collect::<Vec<_>>()).expect("json");
    c.and(format!("\"{table}\".\"{column}\" IN (SELECT value FROM json_each(?))"), vec![s(&list)]);
    Ok(())
}

/// search_requests' conditions: requests in [from, to] (narrowed by
/// after:/before:) matching the query's method, route, status (`401|403`
/// for either), deploy, tenant, user, min_ms, request keys and free text
/// (the route name).
pub fn requests(conn: &Connection, p: &Parsed, from: i64, to: i64) -> rusqlite::Result<Cond> {
    let f = |k: &str| p.field(k).map(str::to_string);
    // The where(...) calls in Active Record's order: method first (it
    // precedes the merged scopes), then requests.between, then the
    // merges, then the rest.
    let mut c = Cond::new();
    // Callers report what is wrong (check); an unchecked bad query matches
    // nothing rather than more than it says.
    c.none = check("requests", p).is_some();
    if let Some(m) = f("method") {
        c.any_of("executions", "method", &m.to_uppercase());
    }
    // A literal, so the planner can prove the request index's `WHERE kind =
    // 'request'` (a bound kind cannot use a partial index).
    c.and("\"executions\".\"kind\" = 'request'", vec![]);
    let range = bounded_time_range(p, from, to);
    let (lo, hi, _) = range;
    if range.2 {
        c.and("\"executions\".\"occurred_at\" >= ? AND \"executions\".\"occurred_at\" < ?", vec![s(&qt(lo)), s(&qt(hi))]);
    } else {
        c.and("\"executions\".\"occurred_at\" BETWEEN ? AND ?", vec![s(&qt(lo)), s(&qt(hi))]);
    }
    c.range = Some((lo, hi));
    if let Some(route) = f("route") {
        // route:a|b: a route name containing either.
        let mut either = Cond::new();
        let mut parts = vec![];
        for alt in alternatives(&route) {
            let mut named = Cond::new();
            named_like(conn, &mut named, "request", alt, range, false)?;
            parts.push(named.parts.join(" AND "));
            either.binds.extend(named.binds);
            either.from = named.from.or(either.from);
        }
        if parts.len() == 1 {
            either.parts = parts;
        } else if !parts.is_empty() {
            either.parts = vec![format!("(({}))", parts.join(") OR ("))];
        }
        merge(&mut c, either);
    }
    if rb::present(Some(&p.text)) {
        let mut named = Cond::new();
        named_like(conn, &mut named, "request", &p.text, range, false)?;
        merge(&mut c, named);
    }
    if let Some(d) = f("deploy") {
        c.any_of("executions", "deploy", &d);
    }
    if let Some(t) = f("tenant") {
        c.any_of("executions", "app_tenant", &t);
    }
    if let Some(u) = f("user") {
        c.any_of("executions", "user_ref", &u);
    }
    // status:401|403: any of them (check rejects an unreadable one).
    let ranges: Vec<(i64, i64)> = p.fields.get("status").map_or(vec![], |v| v.split('|').filter_map(|x| status_range(Some(x))).collect());
    if !ranges.is_empty() {
        let ors = vec!["\"executions\".\"status\" BETWEEN ? AND ?"; ranges.len()].join(" OR ");
        let sql = if ranges.len() == 1 { ors } else { format!("({ors})") };
        c.and(sql, ranges.iter().flat_map(|(lo, hi)| [Sql::from(*lo), Sql::from(*hi)]).collect());
    }
    if let Some(m) = f("min_ms") {
        let us = rb::round0(rb::string_to_f(&m) * 1000.0);
        c.and("\"executions\".\"duration\" >= ?", vec![us.into()]);
    }
    request_detail(&mut c, "executions", p);
    // session: is a seek on the session index, whatever else the query
    // has. Otherwise the request keys read their fields from the request
    // index; a route or text filter has picked its own plan (named_like),
    // and without request keys a column filter (user:, deploy:) keeps the
    // planner's index.
    if p.field("session").is_some() && !c.none {
        c.from = session_index(conn)?;
    } else if c.from.is_none() && has_request_keys(p) {
        c.from = request_index(conn)?;
    }
    Ok(c)
}

/// `"executions" INDEXED BY idx_executions_requests` where the reader can
/// use it: a per-day reader always (its hinted view leaves the hint off the
/// day files that lack the index), a single file once it has the index.
/// With it a request read takes every field it filters on or returns from
/// the index, without parsing `detail` (schema::ADDED_INDEXES).
pub fn request_index(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let usable = crate::storage::reader::is_union(conn) || crate::schema::has_index(conn, "main", crate::schema::REQUEST_INDEX)?;
    Ok(usable.then(|| format!("\"executions\" INDEXED BY {}", crate::schema::REQUEST_INDEX)))
}

/// `"executions" INDEXED BY idx_executions_sessions` where the reader can
/// use it, as `request_index`: one session's requests are a seek on it
/// (schema::ADDED_INDEXES). The hint is only for a statement that has
/// `filter_query`'s session condition, which the index's WHERE needs.
pub fn session_index(conn: &Connection) -> rusqlite::Result<Option<String>> {
    added_index(conn, "executions", crate::schema::SESSION_INDEX)
}

/// `"visits" INDEXED BY idx_visits_sessions`, as `session_index`.
pub fn visit_session_index(conn: &Connection) -> rusqlite::Result<Option<String>> {
    added_index(conn, "visits", crate::schema::VISIT_SESSION_INDEX)
}

fn added_index(conn: &Connection, table: &str, index: &str) -> rusqlite::Result<Option<String>> {
    let usable = crate::storage::reader::is_union(conn) || crate::schema::has_index(conn, "main", index)?;
    Ok(usable.then(|| format!("\"{table}\" INDEXED BY {index}")))
}

fn merge(c: &mut Cond, other: Cond) {
    c.parts.extend(other.parts);
    c.binds.extend(other.binds);
    if other.from.is_some() {
        c.from = other.from;
    }
    c.none |= other.none;
}

pub fn md5_hex(s: &str) -> String {
    use md5::Digest;
    md5::Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn range_sql(col: &str, (lo, hi, empty): (i64, i64, bool)) -> (String, Vec<Sql>) {
    if empty { (format!("{col} >= ? AND {col} < ?"), vec![s(&qt(lo)), s(&qt(hi))]) } else { (format!("{col} BETWEEN ? AND ?"), vec![s(&qt(lo)), s(&qt(hi))]) }
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
        "\"executions\".id IN (SELECT e3.id FROM \"executions\" e3 WHERE e3.kind = ? AND e3.occurred_at BETWEEN ? AND ? AND (e3.name LIKE ? ESCAPE '\\'))"
            .into(),
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
            && let Some(end) = cs[q + 1..].iter().position(|&c| c == '"')
        {
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
    // `rowid IN (...)` alone makes FTS5 run the whole MATCH once per id (50
    // ids: 120 ms on 139k logs). A rowid range is one MATCH pass bounded to
    // the page's ids, and `+rowid IN` filters it: the same rows in the same
    // (rowid) order, 3 ms (docs/PERF.md, "Slow dashboard pages").
    let ph = vec!["?"; ids.len()].join(",");
    let sql = format!("SELECT rowid, snippet(logs_fts, 0, ?, ?, ?, 24) FROM logs_fts WHERE logs_fts MATCH ? AND rowid BETWEEN ? AND ? AND +rowid IN ({ph})");
    let (lo, hi) = (ids.iter().min().unwrap(), ids.iter().max().unwrap());
    let mut binds: Vec<Sql> = vec![s(SNIPPET_MARK), s(SNIPPET_MARK), s("…"), s(&m), Sql::Integer(*lo), Sql::Integer(*hi)];
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

    /// The rowid-range form returns exactly the page's matching ids (not
    /// the other matches inside the range, not the page's non-matching ids).
    #[test]
    fn fts_snippets_only_for_the_given_ids() {
        let dir = std::env::temp_dir().join(format!("rw-snip-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        for (id, m) in [(1, "Completed 200"), (2, "Completed 500"), (3, "Started GET"), (4, "Completed 302"), (5, "Completed 404")] {
            t.execute("INSERT INTO logs (id, level, message, occurred_at) VALUES (?, 'info', ?, '2026-10-16 12:00:00')", rusqlite::params![id, m]).unwrap();
        }
        t.execute("INSERT INTO logs_fts (rowid, message) SELECT id, message FROM logs", []).unwrap();
        let got = fts_snippets(&t, &[4, 1, 3], "complet").unwrap();
        assert_eq!(got.keys().collect::<Vec<_>>(), vec!["1", "4"]);
        assert_eq!(got["4"], Value::String(format!("{SNIPPET_MARK}Completed{SNIPPET_MARK} 302")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past MAX_REQUEST_IDS matching requests a child-record filter keeps
    /// the newest ones and says so; at the limit it does not.
    #[test]
    fn child_records_of_too_many_requests_keep_the_newest_and_say_so() {
        let dir = std::env::temp_dir().join(format!("rw-reqids-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        let now = crate::time::parse_db_time("2026-10-16 12:00:00").unwrap();
        t.execute_batch("BEGIN").unwrap();
        for i in 0..=MAX_REQUEST_IDS as i64 {
            let at = qt(now - 1_000_000 - i * 1_000);
            t.execute(
                "INSERT INTO executions (execution_id, kind, name, occurred_at, duration, detail) VALUES (?1, 'request', 'GET /', ?2, 1, '{\"ip\":\"1.2.3.4\"}')",
                rusqlite::params![format!("e{i}"), at],
            )
            .unwrap();
            t.execute("INSERT INTO logs (execution_id, level, message, occurred_at) VALUES (?1, 'info', 'x', ?2)", rusqlite::params![format!("e{i}"), at])
                .unwrap();
        }
        t.execute_batch("COMMIT").unwrap();
        let count = |c: &Cond| db::count(&t, &format!("SELECT COUNT(*) FROM \"logs\" WHERE {}", c.sql()), rusqlite::params_from_iter(c.binds.iter())).unwrap();
        let c = apply(&t, Resource::Logs, Some("ip:1.2.3.4"), now - 3_600_000_000, now, &[]).unwrap();
        assert!(c.capped);
        assert_eq!(count(&c), MAX_REQUEST_IDS as i64);
        let oldest = db::count(
            &t,
            &format!("SELECT COUNT(*) FROM \"logs\" WHERE {} AND execution_id = 'e{MAX_REQUEST_IDS}'", c.sql()),
            rusqlite::params_from_iter(c.binds.iter()),
        )
        .unwrap();
        assert_eq!(oldest, 0, "the oldest request is the one left out");
        t.execute("DELETE FROM executions WHERE execution_id = ?1", [format!("e{MAX_REQUEST_IDS}")]).unwrap();
        let c = apply(&t, Resource::Logs, Some("ip:1.2.3.4"), now - 3_600_000_000, now, &[]).unwrap();
        assert!(!c.capped, "exactly at the limit");
        assert_eq!(count(&c), MAX_REQUEST_IDS as i64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tenant with records of every resource: logs at three levels,
    /// exceptions of three classes and two severities, jobs on three
    /// queues with two outcomes, queries on two connections.
    fn mixed(name: &str) -> (std::path::PathBuf, Connection, i64) {
        let dir = std::env::temp_dir().join(format!("rw-alts-{name}-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        let now = crate::time::parse_db_time("2026-10-16 12:00:00").unwrap();
        let at = qt(now - 60_000_000);
        for (level, n) in [("error", 3), ("warn", 2), ("info", 7)] {
            for _ in 0..n {
                t.execute("INSERT INTO logs (level, message, occurred_at) VALUES (?1, 'x', ?2)", rusqlite::params![level, at]).unwrap();
            }
        }
        for (class, severity, handled, n) in [("A::Hit", "warning", 1, 1), ("Inertia", "error", 0, 2), ("Other", "error", 0, 4)] {
            for _ in 0..n {
                t.execute(
                    "INSERT INTO exceptions (class_name, message, severity, handled, occurred_at) VALUES (?1, 'm', ?2, ?3, ?4)",
                    rusqlite::params![class, severity, handled, at],
                )
                .unwrap();
            }
        }
        for (i, (queue, outcome)) in [("default", "processed"), ("mailers", "processed"), ("mailers", "failed"), ("low", "processed")].iter().enumerate() {
            t.execute(
                "INSERT INTO executions (execution_id, kind, name, queue, outcome, occurred_at, duration) VALUES (?1, 'job_attempt', ?2, ?3, ?4, ?5, 1)",
                rusqlite::params![format!("j{i}"), format!("Job{i}"), queue, outcome, at],
            )
            .unwrap();
        }
        for (conn_name, n) in [("primary", 2), ("cache", 3), ("queue", 1)] {
            for _ in 0..n {
                t.execute(
                    "INSERT INTO queries (sql, name, connection, occurred_at, duration) VALUES ('SELECT 1', 'q', ?1, ?2, 1)",
                    rusqlite::params![conn_name, at],
                )
                .unwrap();
            }
        }
        (dir, t, now)
    }

    fn matched(t: &Connection, resource: Resource, q: &str, now: i64) -> i64 {
        let table = resource.table();
        let c = apply(t, resource, Some(q), now - 3_600_000_000, now, &[]).unwrap();
        let from = c.from.clone().unwrap_or_else(|| format!("\"{table}\""));
        db::count(t, &format!("SELECT COUNT(*) FROM {from} WHERE {}", c.sql()), rusqlite::params_from_iter(c.binds.iter())).unwrap()
    }

    /// `a|b` on an exact or enum key is either value, on every resource:
    /// level:error|warn is the errors and the warnings (it used to be
    /// every log), class:A|B both classes (it used to be none).
    #[test]
    fn alternatives_match_any_of_their_values_on_every_resource() {
        let (dir, t, now) = mixed("any");
        for (resource, q, n) in [
            (Resource::Logs, "level:error", 3),
            (Resource::Logs, "level:error|warn", 5),
            (Resource::Logs, "status:warn|info", 9),
            (Resource::Exceptions, "class:A::Hit|Inertia", 3),
            (Resource::Exceptions, "severity:error|warning", 7),
            (Resource::Exceptions, "severity:warning", 1),
            (Resource::Exceptions, "handled:true|false", 7),
            (Resource::Exceptions, "status:unhandled", 6),
            (Resource::Jobs, "queue:default|mailers", 3),
            (Resource::Jobs, "outcome:processed|failed queue:mailers", 2),
            (Resource::Jobs, "class:Job0|Job3", 2),
            (Resource::Queries, "connection:primary|queue", 3),
        ] {
            assert_eq!(matched(&t, resource, q, now), n, "{q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A value an enum key does not take is an error naming the ones it
    /// does; `apply` matches nothing on it rather than everything.
    #[test]
    fn an_unknown_enum_value_is_an_error_listing_the_valid_ones() {
        let (dir, t, now) = mixed("enum");
        let cases = [
            ("logs", "level:bogus", "level: \"bogus\" is not one of debug, info, warn, error, fatal, event, unknown"),
            ("logs", "level:error|warning", "level: \"warning\" is not one of debug, info, warn, error, fatal, event, unknown"),
            ("logs", "status:failed", "status: \"failed\" is not one of debug, info, warn, error, fatal, event, unknown"),
            ("exceptions", "severity:warn", "severity: \"warn\" is not one of fatal, error, warning, info"),
            ("exceptions", "handled:yes", "handled: \"yes\" is not one of true, false"),
            ("exceptions", "status:open", "status: \"open\" is not one of handled, unhandled"),
            ("jobs", "outcome:ok", "outcome: \"ok\" is not one of processed, failed, released, aborted"),
            ("jobs", "kind:mail", "kind: \"mail\" is not one of request, job, scheduled_task, command, channel_action"),
            ("requests", "status:oops", "status: \"oops\" is not a status (a code, 404, or a class, 4xx)"),
            ("requests", "outcome:fine", "outcome: \"fine\" is not one of served, redirected_to_auth, redirected, refused, error"),
            ("logs", "level:|", "level: has no value"),
            ("logs", "after:yesterday", "after: \"yesterday\" is not a time (2026-10-29, 2026-10-29T14:00:00Z)"),
            ("requests", "min_ms:fast", "min_ms: \"fast\" is not a number of milliseconds"),
        ];
        for (resource, q, err) in cases {
            assert_eq!(check(resource, &parse(Some(q))).as_deref(), Some(err), "{resource} {q}");
        }
        assert_eq!(matched(&t, Resource::Logs, "level:bogus", now), 0, "nothing, not every log");
        assert_eq!(matched(&t, Resource::Logs, "", now), 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key the resource does not read is an error, not a filter left out
    /// (queue: on logs used to match every log).
    #[test]
    fn a_key_the_resource_does_not_read_is_an_error() {
        let (dir, t, now) = mixed("keys");
        let err = check("logs", &parse(Some("queue:x"))).unwrap();
        assert!(err.starts_with("queue: is not a key of logs (its keys: after, before, user, tenant, deploy, kind, source, level, status, ip,"), "{err}");
        for (resource, q) in
            [("exceptions", "level:error"), ("jobs", "severity:error"), ("queries", "class:X"), ("requests", "level:error"), ("requests", "queue:x")]
        {
            assert!(check(resource, &parse(Some(q))).unwrap().contains(&format!("is not a key of {resource}")), "{resource} {q}");
        }
        assert_eq!(matched(&t, Resource::Logs, "queue:x", now), 0);
        // Every key a resource reads passes, request keys included.
        for (resource, q) in [
            ("logs", "after:2026-10-16 before:2026-10-16T12:00:00Z user:u tenant:t deploy:d kind:job source:app/ level:info status:warn tab:none"),
            ("exceptions", "class:A severity:error handled:true status:handled source:job path:/x"),
            ("jobs", "class:A job_id:1 outcome:failed queue:q status:processed ip:192.0.2.1"),
            ("queries", "source:app/models adapter:sqlite connection:primary role:reading session:s1"),
            ("requests", "method:get|post route:/a|/b status:401|5xx min_ms:250 deploy:d outcome:served|refused probe:true"),
        ] {
            assert_eq!(check(resource, &parse(Some(q))), None, "{resource} {q}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The requests page's grouped table (filter_routes) and the raw reads
    /// (requests) take alternatives too.
    #[test]
    fn route_rows_and_requests_take_alternatives() {
        let rows = vec![
            serde_json::json!({"name": "GET /a", "count": 3, "errors": 0, "client_errors": 1}),
            serde_json::json!({"name": "POST /b", "count": 2, "errors": 1, "client_errors": 0}),
            serde_json::json!({"name": "DELETE /c", "count": 1, "errors": 0, "client_errors": 0}),
        ];
        let names = |q: &str| filter_routes(rows.clone(), Some(q)).iter().map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names("method:get|post"), ["GET /a", "POST /b"]);
        assert_eq!(names("route:/b|/c"), ["POST /b", "DELETE /c"]);
        assert_eq!(names("status:4xx|5xx"), ["GET /a", "POST /b"]);

        let (dir, t, now) = mixed("reqs");
        for (i, (m, user)) in [("GET", "ada"), ("POST", "bob"), ("DELETE", "cy")].iter().enumerate() {
            t.execute(
                "INSERT INTO executions (execution_id, kind, name, method, user_ref, status, occurred_at, duration) VALUES (?1, 'request', ?2, ?3, ?4, 200, ?5, 1)",
                rusqlite::params![format!("r{i}"), format!("{m} /r"), m, user, qt(now - 60_000_000)],
            )
            .unwrap();
        }
        let n = |q: &str| {
            let c = requests(&t, &parse(Some(q)), now - 3_600_000_000, now).unwrap();
            db::count(&t, &format!("SELECT COUNT(*) FROM \"executions\" WHERE {}", c.sql()), rusqlite::params_from_iter(c.binds.iter())).unwrap()
        };
        assert_eq!((n("method:get|post"), n("user:ada|cy"), n("method:get user:bob|cy")), (2, 2, 0));
        assert_eq!(n("level:error"), 0, "a key requests do not read matches nothing");
        // Only method, route and status are in the route rollups.
        let raw = |q: &str| needs_raw_requests(&parse(Some(q)));
        assert!(!raw("method:get|post route:/a status:5xx"));
        assert!(raw("user:ada") && raw("min_ms:5") && raw("after:2026-10-16") && raw("deploy:d") && raw("widgets") && raw("ip:192.0.2.1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fts_expression() {
        assert_eq!(match_expression("widg -boom \"exact phrase\"").unwrap(), "\"widg\"* \"exact phrase\" NOT \"boom\"*");
        assert_eq!(match_expression("-only").unwrap(), "\"only\"*");
        assert_eq!(match_expression("*** ::"), None);
        assert_eq!(match_expression("NEAR(a").unwrap(), "\"NEAR(a\"*");
    }
}
