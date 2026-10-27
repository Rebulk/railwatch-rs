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
        && EXECUTION_KINDS.contains(&k.as_str())
        && has_column(table, "execution_source")
    {
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
                            && !groups.iter().any(|x| x == g)
                        {
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
    // A job's request is the one that enqueued it.
    let column = if matches!(resource, Resource::Jobs) { "parent_id" } else { "execution_id" };
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
pub const REQUEST_KEYS: [&str; 7] = ["ip", "ua", "path", "probe", "rate_limited", "halted", "unpermitted"];

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

/// Whether `p` has a request-detail key.
pub fn has_request_keys(p: &Parsed) -> bool {
    REQUEST_KEYS.iter().any(|k| p.field(k).is_some())
}

/// What is wrong with the request keys' values, for the caller to report
/// (the conditions themselves match nothing on a bad value).
pub fn check_request_keys(p: &Parsed) -> Option<String> {
    for k in ["probe", "rate_limited", "halted", "unpermitted"] {
        if let Some(v) = p.field(k)
            && v != "true"
            && v != "false"
        {
            return Some(format!("{k}: must be true or false, not {v:?}"));
        }
    }
    if let Some(ip) = p.field("ip") {
        for alt in ip.split('|') {
            if let Err(e) = ip_condition(alt) {
                return Some(format!("ip: {e}"));
            }
        }
    }
    None
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
    let flag = |c: &mut Cond, key: &str, set: String| {
        if let Some(v) = p.field(key) {
            c.and(if v == "true" { set } else { format!("NOT ({set})") }, vec![]);
        }
    };
    flag(c, "rate_limited", format!("COALESCE({} NOT IN ('null', 'false'), 0)", detail_fn(alias, "json_type", "$.rate_limited")));
    flag(c, "halted", format!("COALESCE({} != '', 0)", detail_sql(alias, "$.halted_callback")));
    flag(c, "unpermitted", format!("COALESCE({} > 0, 0)", detail_fn(alias, "json_array_length", "$.unpermitted_parameters")));
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
    let from = request_index(conn)?.unwrap_or_else(|| "\"executions\"".into());
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
    if let Some(m) = f("method") {
        c.eq("executions", "method", s(&m.to_uppercase()));
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
        let mut named = Cond::new();
        named_like(conn, &mut named, "request", &route, range, false)?;
        merge(&mut c, named);
    }
    if rb::present(Some(&p.text)) {
        let mut named = Cond::new();
        named_like(conn, &mut named, "request", &p.text, range, false)?;
        merge(&mut c, named);
    }
    if let Some(d) = f("deploy") {
        c.eq("executions", "deploy", s(&d));
    }
    if let Some(t) = f("tenant") {
        c.eq("executions", "app_tenant", s(&t));
    }
    if let Some(u) = f("user") {
        c.eq("executions", "user_ref", s(&u));
    }
    // status:401|403: any of them (an unreadable alternative is skipped,
    // as an unreadable status is).
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
    // The request keys read their fields from the request index; a route or
    // text filter has picked its own plan (named_like), and without request
    // keys a column filter (user:, deploy:) keeps the planner's index.
    if c.from.is_none() && has_request_keys(p) {
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

    #[test]
    fn fts_expression() {
        assert_eq!(match_expression("widg -boom \"exact phrase\"").unwrap(), "\"widg\"* \"exact phrase\" NOT \"boom\"*");
        assert_eq!(match_expression("-only").unwrap(), "\"only\"*");
        assert_eq!(match_expression("*** ::"), None);
        assert_eq!(match_expression("NEAR(a").unwrap(), "\"NEAR(a\"*");
    }
}
