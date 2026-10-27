//! One pass over a window's requests with what the gem keeps of each in
//! `detail` (client address, user agent, path, the security flags), for
//! the questions rollups cannot answer: who sent them, from where, to which
//! path. Rows stream through a fold; nothing per row is kept.
//!
//! The fields are in `detail` JSON with no index, so a pass costs the
//! window's request count, not the matches: it reads at most `MAX_SCAN`
//! requests, newest first, and says when it stopped there (docs/PERF.md,
//! "Security filters").
//!
//! Paths, user agents and headers are written by whoever sent the request:
//! `untrusted` cuts them to a length and replaces control and bidi
//! characters before they go anywhere.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value};

use super::db::{self, jf};
use super::filter_query::{self, Cond, Parsed};
use super::rb;

/// Requests one pass reads at most (newest first).
pub const MAX_SCAN: usize = 200_000;
/// Characters kept of a path, user agent or header value.
pub const FIELD_CHARS: usize = 512;

/// `s` cut to `n` characters, control and bidi-override characters
/// replaced: text a client wrote, shown to people and agents.
pub fn untrusted(s: &str, n: usize) -> String {
    rb::first_chars(s, n)
        .chars()
        .map(|c| if c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') { '\u{FFFD}' } else { c })
        .collect()
}

/// One request as the fold sees it (borrowed from the statement's row).
pub struct Row<'a> {
    pub at: i64,
    pub execution_id: &'a str,
    pub name: &'a str,
    pub group_hash: &'a str,
    pub method: Option<&'a str>,
    pub status: Option<i64>,
    pub duration: i64,
    pub user: Option<&'a str>,
    pub tenant: Option<&'a str>,
    /// The class of the first exception (`exception_preview` up to ": ").
    pub exception: Option<&'a str>,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub path: Option<&'a str>,
    /// A Cloudflare `CF-IPCountry` header, when the app records one.
    pub country: Option<&'a str>,
    pub rate_limited: bool,
    pub halted: Option<&'a str>,
    pub unpermitted: bool,
}

/// What a pass read: rows folded, and whether it stopped at the cap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scanned {
    pub rows: usize,
    pub capped: bool,
    pub cap: usize,
}

impl Scanned {
    pub fn to_json(self) -> Value {
        let mut v = crate::obj! { "scanned" => self.rows as i64, "capped" => self.capped };
        if self.capped {
            db::merge_into(
                &mut v,
                "note",
                Value::from(format!("Stopped at the newest {} requests of the window; older ones are not counted. Narrow q or the window.", self.cap)),
            );
        }
        v
    }
}

/// Folds every request matching `cond` (filter_query::requests), newest
/// first, up to `cap`.
pub fn scan(conn: &Connection, cond: &Cond, cap: usize, mut f: impl FnMut(&Row<'_>)) -> rusqlite::Result<Scanned> {
    if cond.none {
        return Ok(Scanned { rows: 0, capped: false, cap });
    }
    let sql = scan_sql(conn, cond, cap)?;
    let mut stmt = db::prepare(conn, &sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(cond.binds.iter()))?;
    let mut n = 0;
    // A second's rows share their timestamp text: parse it once.
    let mut last: (Vec<u8>, i64) = (vec![], 0);
    while let Some(r) = rows.next()? {
        if n == cap {
            return Ok(Scanned { rows: n, capped: true, cap });
        }
        n += 1;
        let at = match r.get_ref(0)? {
            ValueRef::Text(t) if t == last.0.as_slice() => last.1,
            ValueRef::Text(t) => {
                let at = crate::time::parse_db_time(&String::from_utf8_lossy(t)).unwrap_or(0);
                last = (t.to_vec(), at);
                at
            }
            _ => 0,
        };
        let text = |i: usize| -> rusqlite::Result<Option<&str>> {
            Ok(match r.get_ref(i)? {
                ValueRef::Text(t) => std::str::from_utf8(t).ok().filter(|s| !s.is_empty()),
                _ => None,
            })
        };
        let int = |i: usize| -> rusqlite::Result<Option<i64>> {
            Ok(match r.get_ref(i)? {
                ValueRef::Integer(v) => Some(v),
                ValueRef::Real(v) => Some(v as i64),
                _ => None,
            })
        };
        let exception = text(9)?.map(|p| p.split_once(": ").map_or(p, |(c, _)| c));
        let row = Row {
            at,
            execution_id: text(1)?.unwrap_or(""),
            name: text(2)?.unwrap_or(""),
            group_hash: text(3)?.unwrap_or(""),
            method: text(4)?,
            status: int(5)?,
            duration: int(6)?.unwrap_or(0),
            user: text(7)?,
            tenant: text(8)?,
            exception,
            ip: text(10)?,
            user_agent: text(11)?,
            path: text(12)?,
            country: text(13)?,
            rate_limited: matches!(text(14)?, Some(t) if t != "null" && t != "false"),
            halted: text(15)?,
            unpermitted: int(16)?.unwrap_or(0) > 0,
        };
        f(&row);
    }
    Ok(Scanned { rows: n, capped: false, cap })
}

/// `scan`'s statement, newest first, `cap + 1` rows at most.
pub fn scan_sql(conn: &Connection, cond: &Cond, cap: usize) -> rusqlite::Result<String> {
    // Every field below is in the request index (schema::ADDED_INDEXES): with
    // it a pass reads no row's `detail`. A route or text filter brought its
    // own plan.
    let from = match &cond.from {
        Some(f) => f.clone(),
        None => filter_query::request_index(conn)?.unwrap_or_else(|| "\"executions\"".into()),
    };
    let d = |p: &str| filter_query::detail_sql("executions", p);
    let func = |f: &str, p: &str| filter_query::detail_fn("executions", f, p);
    Ok(format!(
        "SELECT \"executions\".\"occurred_at\", \"executions\".\"execution_id\", \"executions\".\"name\", \"executions\".\"group_hash\", \
         \"executions\".\"method\", \"executions\".\"status\", \"executions\".\"duration\", \"executions\".\"user_ref\", \"executions\".\"app_tenant\", \
         \"executions\".\"exception_preview\", {ip}, {ua}, {path}, {country}, {rl_type}, {halted}, {unpermitted} \
         FROM {from} WHERE {cond} ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {limit}",
        ip = d("$.ip"),
        ua = d("$.user_agent"),
        path = d("$.path"),
        country = d("$.headers.\"Cf-Ipcountry\""),
        rl_type = func("json_type", "$.rate_limited"),
        halted = d("$.halted_callback"),
        unpermitted = func("json_array_length", "$.unpermitted_parameters"),
        cond = cond.sql(),
        limit = cap + 1,
    ))
}

// -- query_requests: group, count, rank --------------------------------------

/// What a group can be keyed by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Ip,
    Path,
    Route,
    UserAgent,
    Status,
    StatusClass,
    Method,
    User,
    Tenant,
    Country,
    Exception,
    Minute,
    Hour,
    Day,
}

pub const KEYS: [&str; 14] =
    ["ip", "path", "route", "user_agent", "status", "status_class", "method", "user", "tenant", "country", "exception", "minute", "hour", "day"];

impl Key {
    pub fn parse(s: &str) -> Option<Key> {
        Some(match s {
            "ip" => Key::Ip,
            "path" => Key::Path,
            "route" => Key::Route,
            "user_agent" | "ua" => Key::UserAgent,
            "status" => Key::Status,
            "status_class" => Key::StatusClass,
            "method" => Key::Method,
            "user" => Key::User,
            "tenant" => Key::Tenant,
            "country" => Key::Country,
            "exception" => Key::Exception,
            "minute" => Key::Minute,
            "hour" => Key::Hour,
            "day" => Key::Day,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Key::Ip => "ip",
            Key::Path => "path",
            Key::Route => "route",
            Key::UserAgent => "user_agent",
            Key::Status => "status",
            Key::StatusClass => "status_class",
            Key::Method => "method",
            Key::User => "user",
            Key::Tenant => "tenant",
            Key::Country => "country",
            Key::Exception => "exception",
            Key::Minute => "minute",
            Key::Hour => "hour",
            Key::Day => "day",
        }
    }

    /// Keys whose values are personal data (security access only).
    pub fn personal(self) -> bool {
        matches!(self, Key::Ip | Key::UserAgent)
    }

    fn bucket(self) -> Option<i64> {
        match self {
            Key::Minute => Some(60),
            Key::Hour => Some(3600),
            Key::Day => Some(86_400),
            _ => None,
        }
    }

    fn value(self, r: &Row<'_>) -> Option<String> {
        let cut = |s: Option<&str>| s.map(|s| untrusted(s, FIELD_CHARS));
        match self {
            Key::Ip => cut(r.ip),
            Key::Path => cut(r.path),
            Key::Route => Some(untrusted(r.name, FIELD_CHARS)),
            Key::UserAgent => cut(r.user_agent),
            Key::Status => r.status.map(|s| s.to_string()),
            Key::StatusClass => r.status.map(|s| format!("{}xx", s / 100)),
            Key::Method => cut(r.method),
            Key::User => cut(r.user),
            Key::Tenant => cut(r.tenant),
            Key::Country => cut(r.country),
            Key::Exception => cut(r.exception),
            Key::Minute | Key::Hour | Key::Day => {
                let step = self.bucket().expect("time key") * 1_000_000;
                Some(time_key(r.at.div_euclid(step) * step))
            }
        }
    }

    /// The value as JSON (status a number).
    fn json(self, v: &Option<String>) -> Value {
        match (self, v) {
            (_, None) => Value::Null,
            (Key::Status, Some(s)) => s.parse::<i64>().map_or(Value::Null, Value::from),
            (_, Some(s)) => Value::String(s.clone()),
        }
    }

    /// The filter token that selects this group's requests, when the
    /// grammar has one (substring keys select a superset).
    fn filter(self, v: &str) -> Vec<(&'static str, String)> {
        match self {
            Key::Ip => vec![("ip", v.to_string())],
            Key::Path => vec![("path", v.to_string())],
            Key::Route => vec![("route", v.to_string())],
            Key::UserAgent => vec![("ua", v.to_string())],
            Key::Status | Key::StatusClass => vec![("status", v.to_string())],
            Key::Method => vec![("method", v.to_string())],
            Key::User => vec![("user", v.to_string())],
            Key::Tenant => vec![("tenant", v.to_string())],
            Key::Country | Key::Exception => vec![],
            Key::Minute | Key::Hour | Key::Day => {
                let Some(at) = crate::time::parse_db_time(v) else { return vec![] };
                let end = at + self.bucket().expect("time key") * 1_000_000 - 1;
                vec![("after", time_key(at)), ("before", crate::time::iso8601_6(end))]
            }
        }
    }
}

/// "2026-10-27T15:04:00Z"
fn time_key(micros: i64) -> String {
    format!("{}Z", crate::time::utc_seconds_string(micros.div_euclid(1_000_000)).replace(' ', "T"))
}

/// A metric a group can report beyond its count.
pub const METRICS: [&str; 11] = [
    "errors",
    "client_errors",
    "statuses",
    "distinct_ips",
    "distinct_users",
    "distinct_paths",
    "distinct_user_agents",
    "exceptions",
    "duration",
    "first_seen",
    "last_seen",
];
pub const DEFAULT_METRICS: [&str; 6] = ["client_errors", "errors", "distinct_ips", "distinct_users", "first_seen", "last_seen"];
/// What a result can be sorted by: count, a metric that is a number, or the
/// group's key.
pub const SORTS: [&str; 11] =
    ["count", "errors", "client_errors", "distinct_ips", "distinct_users", "distinct_paths", "distinct_user_agents", "p95", "first_seen", "last_seen", "key"];

/// Distinct values a group counts exactly; past this it reports the bound
/// and says so (a scanner's paths are what would grow without end).
const DISTINCT_CAP: usize = 10_000;

#[derive(Default)]
struct Agg {
    count: i64,
    errors: i64,
    client_errors: i64,
    first: i64,
    last: i64,
    statuses: HashMap<i64, i64>,
    exceptions: HashMap<String, i64>,
    ips: HashSet<String>,
    users: HashSet<String>,
    paths: HashSet<String>,
    uas: HashSet<String>,
    durations: Vec<i64>,
    /// The newest request's execution_id (rows come newest first).
    sample: String,
}

fn add_distinct(set: &mut HashSet<String>, v: Option<&str>) {
    if let Some(v) = v
        && set.len() < DISTINCT_CAP
        && !set.contains(v)
    {
        set.insert(v.to_string());
    }
}

fn distinct(set: &HashSet<String>) -> Value {
    if set.len() >= DISTINCT_CAP { Value::from(format!(">={DISTINCT_CAP}")) } else { Value::from(set.len() as i64) }
}

/// A query_requests call, parsed.
pub struct Query {
    pub group_by: Vec<Key>,
    pub metrics: Vec<&'static str>,
    pub sort: &'static str,
    pub asc: bool,
    pub limit: usize,
    pub min_count: i64,
}

impl Query {
    fn wants(&self, m: &str) -> bool {
        self.metrics.contains(&m) || self.sort == m || (self.sort == "p95" && m == "duration")
    }
}

/// The groups of the requests `cond` selects, ranked; `parsed` is the
/// filter they came from (each group's `q` adds its own key to it).
pub fn query(conn: &Connection, cond: &Cond, parsed: &Parsed, q: &Query, cap: usize) -> rusqlite::Result<(Scanned, Value)> {
    let mut groups: HashMap<Vec<Option<String>>, Agg> = HashMap::new();
    let mut matched = 0i64;
    let scanned = scan(conn, cond, cap, |r| {
        matched += 1;
        let key: Vec<Option<String>> = q.group_by.iter().map(|k| k.value(r)).collect();
        let g = groups.entry(key).or_default();
        if g.count == 0 {
            g.last = r.at;
            g.first = r.at;
            if let Some(ex) = Some(r.execution_id).filter(|e| !e.is_empty()) {
                g.sample = ex.to_string();
            }
        }
        g.count += 1;
        g.first = g.first.min(r.at);
        g.last = g.last.max(r.at);
        match r.status {
            Some(500..=599) => g.errors += 1,
            Some(400..=499) => g.client_errors += 1,
            _ => {}
        }
        if q.wants("statuses")
            && let Some(s) = r.status
        {
            *g.statuses.entry(s).or_default() += 1;
        }
        if q.wants("exceptions")
            && let Some(e) = r.exception
            && (g.exceptions.len() < 100 || g.exceptions.contains_key(e))
        {
            *g.exceptions.entry(untrusted(e, 255)).or_default() += 1;
        }
        if q.wants("distinct_ips") {
            add_distinct(&mut g.ips, r.ip);
        }
        if q.wants("distinct_users") {
            add_distinct(&mut g.users, r.user);
        }
        if q.wants("distinct_paths") {
            add_distinct(&mut g.paths, r.path);
        }
        if q.wants("distinct_user_agents") {
            add_distinct(&mut g.uas, r.user_agent);
        }
        if q.wants("duration") {
            g.durations.push(r.duration);
        }
    })?;
    let total = groups.len();
    let mut ranked: Vec<(Vec<Option<String>>, Agg)> = groups.into_iter().filter(|(_, g)| g.count >= q.min_count).collect();
    let after_min = ranked.len();
    for (_, g) in ranked.iter_mut() {
        g.durations.sort_unstable();
    }
    let num = |g: &Agg| -> i64 {
        match q.sort {
            "errors" => g.errors,
            "client_errors" => g.client_errors,
            "distinct_ips" => g.ips.len() as i64,
            "distinct_users" => g.users.len() as i64,
            "distinct_paths" => g.paths.len() as i64,
            "distinct_user_agents" => g.uas.len() as i64,
            "p95" => percentile(&g.durations, 95),
            "first_seen" => g.first,
            "last_seen" => g.last,
            _ => g.count,
        }
    };
    // Ties by the key, then by count: the same order every call.
    ranked.sort_by(|(ka, a), (kb, b)| {
        let primary = if q.sort == "key" { ka.cmp(kb) } else { num(a).cmp(&num(b)) };
        let primary = if q.asc { primary } else { primary.reverse() };
        primary.then_with(|| b.count.cmp(&a.count)).then_with(|| ka.cmp(kb))
    });
    let rows: Vec<Value> = ranked.iter().take(q.limit).map(|(k, g)| group_json(q, parsed, k, g)).collect();
    let mut out = crate::obj! {
        "matched" => matched, "groups" => total as i64,
    };
    if q.min_count > 1 {
        db::merge_into(&mut out, "groups_with_min_count", Value::from(after_min as i64));
    }
    db::merge_into(&mut out, "rows", Value::Array(rows));
    Ok((scanned, out))
}

fn group_json(q: &Query, parsed: &Parsed, key: &[Option<String>], g: &Agg) -> Value {
    let mut m = Map::new();
    for (k, v) in q.group_by.iter().zip(key) {
        m.insert(k.name().into(), k.json(v));
    }
    m.insert("count".into(), Value::from(g.count));
    for metric in &q.metrics {
        let v = match *metric {
            "errors" => Value::from(g.errors),
            "client_errors" => Value::from(g.client_errors),
            "statuses" => {
                let mut s: Vec<(&i64, &i64)> = g.statuses.iter().collect();
                s.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                Value::Object(s.into_iter().take(8).map(|(k, v)| (k.to_string(), Value::from(*v))).collect())
            }
            "exceptions" => {
                let mut s: Vec<(&String, &i64)> = g.exceptions.iter().collect();
                s.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                Value::Object(s.into_iter().take(5).map(|(k, v)| (k.clone(), Value::from(*v))).collect())
            }
            "distinct_ips" => distinct(&g.ips),
            "distinct_users" => distinct(&g.users),
            "distinct_paths" => distinct(&g.paths),
            "distinct_user_agents" => distinct(&g.uas),
            "duration" => crate::obj! {
                "avg_ms" => jf(rb::round(g.durations.iter().sum::<i64>() as f64 / g.durations.len().max(1) as f64 / 1000.0, 2)),
                "p95_ms" => jf(rb::round(percentile(&g.durations, 95) as f64 / 1000.0, 2)),
                "max_ms" => jf(rb::round(g.durations.last().copied().unwrap_or(0) as f64 / 1000.0, 2)),
            },
            "first_seen" => Value::from(time_key(g.first)),
            "last_seen" => Value::from(time_key(g.last)),
            _ => continue,
        };
        m.insert((*metric).into(), v);
    }
    if !g.sample.is_empty() {
        m.insert("sample_execution_id".into(), Value::from(g.sample.clone()));
    }
    let mut drill = parsed.clone();
    for (k, v) in q.group_by.iter().zip(key) {
        if let Some(v) = v {
            for (fk, fv) in k.filter(v) {
                drill.fields.insert(fk.to_string(), fv);
            }
        }
    }
    m.insert("q".into(), Value::from(filter_query::unparse(&drill)));
    Value::Object(m)
}

/// The nearest-rank percentile of sorted values (0 for none).
pub fn percentile(sorted: &[i64], p: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted[rank - 1]
}

// -- The requests table, from raw rows ---------------------------------------

/// The grouped requests table (aggregations::grouped's rows) from the raw
/// requests `cond` selects, for a filter rollups cannot answer (ip:, path:,
/// ...). Percentiles are exact here (rollups' are t-digest estimates).
pub fn grouped(
    conn: &Connection,
    cond: &Cond,
    from: i64,
    to: i64,
    limit: usize,
    order: Option<&str>,
    dir: Option<&str>,
) -> rusqlite::Result<(Scanned, Vec<Value>)> {
    struct G {
        hash: String,
        name: String,
        count: i64,
        errors: i64,
        client_errors: i64,
        durations: Vec<i64>,
        times: Vec<i64>,
        order: usize,
    }
    let mut groups: HashMap<String, G> = HashMap::new();
    let scanned = scan(conn, cond, MAX_SCAN, |r| {
        let n = groups.len();
        let g = groups.entry(r.group_hash.to_string()).or_insert_with(|| G {
            hash: r.group_hash.to_string(),
            name: r.name.to_string(),
            count: 0,
            errors: 0,
            client_errors: 0,
            durations: vec![],
            times: vec![],
            order: n,
        });
        g.count += 1;
        match r.status {
            Some(500..=599) => g.errors += 1,
            Some(400..=499) => g.client_errors += 1,
            _ => {}
        }
        g.durations.push(r.duration);
        g.times.push(r.at);
    })?;
    let mut list: Vec<G> = groups.into_values().collect();
    list.sort_by_key(|g| g.order);
    let summaries: Vec<(String, G, super::rollup::Summary)> = list
        .into_iter()
        .map(|mut g| {
            g.durations.sort_unstable();
            let sum: i64 = g.durations.iter().sum();
            let s = super::rollup::Summary {
                count: g.count,
                errors: g.errors,
                client_errors: g.client_errors,
                avg: sum.div_euclid(g.count.max(1)),
                p50: percentile(&g.durations, 50),
                p95: percentile(&g.durations, 95),
                p99: percentile(&g.durations, 99),
                max: g.durations.last().copied().unwrap_or(0),
                extra: Map::new(),
            };
            (g.name.clone(), g, s)
        })
        .collect();
    let order = order.filter(|o| super::aggregations::GROUPED_SORT_FIELDS.contains(o)).unwrap_or("count");
    let sign: i64 = if dir == Some("asc") { 1 } else { -1 };
    let ranked = rb::sort_by_num(summaries, |(_, _, s)| rb::Num::I(s.get(order) * sign));
    let rows = ranked
        .into_iter()
        .take(limit)
        .map(|(name, g, s)| {
            let spark = super::aggregations::sparkline_counts(g.times.iter().map(|t| (*t, 1)), from, to);
            super::aggregations::grouped_row(&g.hash, Some(&name), &s, Value::from(spark))
        })
        .collect();
    Ok((scanned, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_is_cut_and_loses_control_and_bidi_characters() {
        assert_eq!(untrusted("ok\u{1b}[31mred\u{202E}x\n", 100), "ok\u{FFFD}[31mred\u{FFFD}x\u{FFFD}");
        assert_eq!(untrusted("ééé", 2), "éé");
    }

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<i64> = (1..=100).collect();
        assert_eq!((percentile(&v, 50), percentile(&v, 95), percentile(&v, 99)), (50, 95, 99));
        assert_eq!(percentile(&[7], 95), 7);
        assert_eq!(percentile(&[], 95), 0);
    }

    #[test]
    fn time_keys_drill_into_their_own_bucket() {
        let at = crate::time::parse_db_time("2026-10-27 15:04:31").unwrap();
        let r = |k: Key| {
            k.value(&Row {
                at,
                execution_id: "",
                name: "",
                group_hash: "",
                method: None,
                status: Some(401),
                duration: 0,
                user: None,
                tenant: None,
                exception: None,
                ip: None,
                user_agent: None,
                path: None,
                country: None,
                rate_limited: false,
                halted: None,
                unpermitted: false,
            })
        };
        assert_eq!(r(Key::Minute).unwrap(), "2026-10-27T15:04:00Z");
        assert_eq!(r(Key::Hour).unwrap(), "2026-10-27T15:00:00Z");
        assert_eq!(r(Key::StatusClass).unwrap(), "4xx");
        assert_eq!(
            Key::Minute.filter("2026-10-27T15:04:00Z"),
            vec![("after", "2026-10-27T15:04:00Z".to_string()), ("before", "2026-10-27T15:04:59.999999Z".to_string())]
        );
    }
}

#[cfg(test)]
mod per_day_tests {
    use super::*;
    use crate::dashboard::filter_query::{Resource, apply, parse, requests};

    /// The request keys, query_requests and the requests table read a
    /// per-day tenant (union views over day files) as they read the single
    /// file it was split from, including child records whose request is in
    /// another day's file.
    #[test]
    fn per_day_requests_equal_the_single_file() {
        let dir = std::env::temp_dir().join(format!("rw-scan-days-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.sqlite3");
        let t = crate::tenants::open_tenant(&file).unwrap();
        let now = crate::time::parse_db_time("2026-10-16 12:00:00").unwrap();
        let mut id = 0;
        for day in ["2026-10-13", "2026-10-14", "2026-10-15", "2026-10-16"] {
            for i in 0..12i64 {
                id += 1;
                let at = format!("{day} {:02}:59:59.{id:06}", i % 11);
                let ip = ["198.51.100.7", "203.0.113.9", "10.0.0.1"][(id % 3) as usize];
                let path = ["/.env", "/session", "/orders", "/wp-login.php"][(id % 4) as usize];
                let detail = serde_json::json!({"ip": ip, "path": path, "user_agent": format!("ua{}", id % 2), "rate_limited": if id % 5 == 0 { serde_json::json!({"name": "x"}) } else { Value::Null }});
                t.execute(
                    "INSERT INTO executions (id, execution_id, kind, name, method, group_hash, status, occurred_at, duration, user_ref, detail) \
                     VALUES (?1, ?2, 'request', ?3, 'GET', ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        id,
                        format!("e{id}"),
                        format!("GET {path}"),
                        format!("g{}", id % 4),
                        [200, 401, 404][(id % 3) as usize],
                        at,
                        1000 + id * 37,
                        format!("u{}", id % 4),
                        detail.to_string()
                    ],
                )
                .unwrap();
                // Its exception a second later: the next day's file for 23:59:59.
                let ex_at = crate::time::quoted_date_micros(crate::time::parse_db_time(&at).unwrap() + 1_000_000);
                t.execute(
                    "INSERT INTO exceptions (execution_id, execution_source, class_name, message, handled, occurred_at) VALUES (?1, 'request', 'Boom', 'x', 1, ?2)",
                    rusqlite::params![format!("e{id}"), ex_at],
                )
                .unwrap();
            }
        }
        drop(t);
        crate::storage::export::split(&file, &dir.join("t")).unwrap();
        let single = crate::dashboard::pool::open_reader(&file).unwrap();
        let (days, _) = crate::storage::reader::open(&dir.join("t")).unwrap();
        let (from, to) = (now - 7 * 86_400_000_000, now);
        for q in ["", "ip:198.51.100.7", "ip:203.0.113.0/24 status:401", "path:/session*", "probe:true", "ua:ua1 rate_limited:true", "probe:false ip:10.0.0.*"]
        {
            let p = parse(Some(q));
            let run = |c: &Connection| {
                let cond = requests(c, &p, from, to).unwrap();
                let query = Query { group_by: vec![Key::Ip, Key::Path], metrics: METRICS.to_vec(), sort: "count", asc: false, limit: 50, min_count: 1 };
                let (s, v) = super::query(c, &cond, &p, &query, MAX_SCAN).unwrap();
                let (_, g) = grouped(c, &cond, from, to, 50, Some("p95"), None).unwrap();
                let ex = apply(c, Resource::Exceptions, Some(q), from, to, &[]).unwrap();
                let n = db::count(c, &format!("SELECT COUNT(*) FROM \"exceptions\" WHERE {}", ex.sql()), rusqlite::params_from_iter(ex.binds.iter())).unwrap();
                (s.rows, v.to_string(), serde_json::to_string(&g).unwrap(), n)
            };
            let (a, b) = (run(&single), run(&days));
            assert_eq!(a, b, "{q}");
            assert!(a.0 > 0, "{q} matched something");
        }
        // A filter that matches no request (or an unusable text query) is an
        // empty page on a per-day tenant, not an error.
        for (res, table, q) in
            [(Resource::Exceptions, "exceptions", "ip:192.0.2.250"), (Resource::Logs, "logs", "***"), (Resource::Logs, "logs", "ip:192.0.2.250")]
        {
            let label = q;
            let cond = apply(&days, res, Some(q), from, to, &[]).unwrap();
            assert!(cond.none, "{label}");
            let select = format!("\"{table}\".*");
            let page = crate::dashboard::cursor_page::call(
                &days,
                &crate::dashboard::CursorKey(b"k".to_vec()),
                table,
                "M",
                &select,
                cond,
                None,
                None,
                crate::dashboard::cursor_page::Order::Recent,
                "c",
            )
            .unwrap()
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(page.rows.is_empty());
        }
        // The exceptions of 23:59:59 requests are in the next day's file.
        let ex = apply(&days, Resource::Exceptions, Some("ip:198.51.100.7"), from, to, &[]).unwrap();
        let n = db::count(&days, &format!("SELECT COUNT(*) FROM \"exceptions\" WHERE {}", ex.sql()), rusqlite::params_from_iter(ex.binds.iter())).unwrap();
        assert_eq!(n, 16, "every request of the address has its exception");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Requests with every field the request index holds, in all their
    /// shapes (missing, null, false, empty, named, unnamed, nested).
    fn varied_requests(t: &Connection, days: &[&str]) {
        let mut id = 0;
        for day in days {
            for i in 0..40i64 {
                id += 1;
                let ip = ["198.51.100.7", "203.0.113.9", "10.0.0.1", "2001:db8::1"][(id % 4) as usize];
                let path = ["/.env", "/session", "/orders", "/wp-login.php", "/config.php", "/admin/users/1"][(id % 6) as usize];
                let country = ["NL", "US"][(id % 2) as usize];
                let mut d = serde_json::json!({"ip": ip, "path": path, "user_agent": format!("ua{}", id % 3), "headers": {"Cf-Ipcountry": country}});
                match id % 7 {
                    0 => d["rate_limited"] = serde_json::json!({"name": "search", "count": 61, "to": 60}),
                    1 => d["rate_limited"] = serde_json::json!({"name": "", "count": 61, "to": 60}),
                    2 => d["rate_limited"] = Value::Bool(false),
                    _ => {}
                }
                match id % 5 {
                    0 => d["halted_callback"] = Value::from("authenticate"),
                    1 => d["halted_callback"] = Value::from("require_admin"),
                    2 => d["halted_callback"] = Value::from(""),
                    _ => {}
                }
                match id % 4 {
                    0 => d["unpermitted_parameters"] = serde_json::json!(["role", "admin"]),
                    1 => d["unpermitted_parameters"] = serde_json::json!([]),
                    _ => {}
                }
                match id % 6 {
                    0 => d["redirect_to"] = Value::from("https://a.example/sign_in?return_to=%2F"),
                    1 => d["redirect_to"] = Value::from("https://a.example/orders"),
                    2 => d["redirect_to"] = Value::Null,
                    _ => {}
                }
                d["response_size"] = Value::from(id * 13 % 900);
                if id % 11 == 0 {
                    d = serde_json::json!({});
                }
                t.execute(
                    "INSERT INTO executions (id, execution_id, kind, name, method, group_hash, status, occurred_at, duration, user_ref, detail) \
                     VALUES (?1, ?2, 'request', 'GET /x', 'GET', ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        id,
                        format!("r{id}"),
                        format!("g{}", id % 3),
                        [200, 302, 301, 401, 404, 500, 304][(id % 7) as usize],
                        format!("{day} {:02}:{:02}:00.{id:06}", i % 24, i % 60),
                        1000 + id,
                        (id % 3 == 0).then(|| format!("u{}", id % 4)),
                        d.to_string()
                    ],
                )
                .unwrap();
                t.execute(
                    "INSERT INTO logs (execution_id, level, message, occurred_at) VALUES (?1, 'info', 'x', ?2)",
                    rusqlite::params![format!("r{id}"), format!("{day} {:02}:{:02}:01", i % 24, i % 60)],
                )
                .unwrap();
            }
        }
    }

    /// Everything a security read returns for each query, as one string.
    fn security_reads(c: &Connection, from: i64, to: i64) -> Vec<String> {
        let queries = [
            "",
            "ip:198.51.100.7",
            "ip:203.0.113.0/24|2001:db8:*",
            "ua:ua1",
            "path:/session*",
            "probe:true",
            "probe:false",
            "rate_limited:true",
            "rate_limited:false",
            "halted:true",
            "halted:false",
            "unpermitted:true",
            "unpermitted:false",
        ];
        let all: Vec<Key> = KEYS.iter().filter_map(|k| Key::parse(k)).collect();
        let mut out = vec![];
        for q in queries {
            let p = parse(Some(q));
            let cond = requests(c, &p, from, to).unwrap();
            for pair in all.chunks(2) {
                let query = Query { group_by: pair.to_vec(), metrics: METRICS.to_vec(), sort: "key", asc: true, limit: 200, min_count: 1 };
                let (s, v) = super::query(c, &cond, &p, &query, MAX_SCAN).unwrap();
                out.push(format!("{q} {pair:?}: {} {v}", s.rows));
            }
            let mut rows = vec![];
            scan(c, &cond, MAX_SCAN, |r| {
                rows.push(format!(
                    "{} {:?} {:?} {:?} {:?} {:?} {} {:?} {}",
                    r.execution_id, r.ip, r.user_agent, r.path, r.country, r.status, r.rate_limited, r.halted, r.unpermitted
                ))
            })
            .unwrap();
            out.push(format!("{q} rows: {}", rows.join("; ")));
            let logs = apply(c, Resource::Logs, Some(q), from, to, &[]).unwrap();
            let n = db::count(c, &format!("SELECT COUNT(*) FROM \"logs\" WHERE {}", logs.sql()), rusqlite::params_from_iter(logs.binds.iter())).unwrap();
            out.push(format!("{q} logs: {n}"));
        }
        out
    }

    /// The request index changes plans, never answers: a per-day tenant
    /// whose files have it, one where none has it, and one where only some
    /// have it (the state between an upgrade and the background build)
    /// answer every security read as the single file without it does, and
    /// a single file with it answers the same too.
    #[test]
    fn the_request_index_gives_the_same_answers_as_json_with_some_files_without_it() {
        let dir = std::env::temp_dir().join(format!("rw-reqidx-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.sqlite3");
        let t = crate::tenants::open_tenant(&file).unwrap();
        // A file made before the index (an older server's).
        t.execute_batch(&format!("DROP INDEX {}", crate::schema::REQUEST_INDEX)).unwrap();
        t.execute_batch("BEGIN").unwrap();
        varied_requests(&t, &["2026-10-13", "2026-10-14", "2026-10-15", "2026-10-16"]);
        t.execute_batch("COMMIT").unwrap();
        drop(t);
        let now = crate::time::parse_db_time("2026-10-16 23:59:59").unwrap();
        let (from, to) = (now - 7 * 86_400_000_000, now);
        let single = crate::dashboard::pool::open_reader(&file).unwrap();
        assert!(filter_query::request_index(&single).unwrap().is_none(), "a single file without the index reads without the hint");
        let want = security_reads(&single, from, to);
        drop(single);
        let split = |name: &str, indexed: &[&str]| {
            let d = dir.join(name);
            crate::storage::export::split(&file, &d).unwrap();
            for day in crate::storage::layout::days(&d, crate::storage::layout::Tier::Core) {
                let p = crate::storage::layout::day_path(&d, crate::storage::layout::Tier::Core, &day);
                let c = rusqlite::Connection::open(&p).unwrap();
                let has = crate::schema::has_index(&c, "main", crate::schema::REQUEST_INDEX).unwrap();
                match (indexed.contains(&day.as_str()), has) {
                    (true, false) => assert_eq!(crate::schema::add_missing_indexes(&c).unwrap(), 1),
                    (false, true) => c.execute_batch(&format!("DROP INDEX {}", crate::schema::REQUEST_INDEX)).unwrap(),
                    _ => {}
                }
            }
            crate::storage::reader::open(&d).unwrap().0
        };
        let all = split("all", &["2026-10-13", "2026-10-14", "2026-10-15", "2026-10-16"]);
        let none = split("none", &[]);
        let some = split("some", &["2026-10-13", "2026-10-15"]);
        for (name, c) in [("indexed", &all), ("unindexed", &none), ("half", &some)] {
            let got = security_reads(c, from, to);
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(g, w, "{name}");
            }
        }
        // And the single file once it has the index.
        crate::schema::add_missing_indexes(&rusqlite::Connection::open(&file).unwrap()).unwrap();
        let single = crate::dashboard::pool::open_reader(&file).unwrap();
        assert!(filter_query::request_index(&single).unwrap().is_some());
        assert_eq!(security_reads(&single, from, to), want, "single file with the index");
        // The answers are not trivially empty.
        assert!(want.iter().any(|l| l.starts_with("probe:true rows: r")), "probes exist");
        assert!(want.iter().any(|l| l.starts_with("rate_limited:true rows: r")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scan reads every request field from the request index, none
    /// from the row's `detail`: an expression written differently from the
    /// index's (another alias form, another function) would make SQLite
    /// parse the JSON of every row again, silently.
    #[test]
    fn a_request_scan_reads_no_detail_where_the_index_exists() {
        let dir = std::env::temp_dir().join(format!("rw-reqplan-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        // The detail column's position in executions.
        let detail: i64 = t.query_row("SELECT cid FROM pragma_table_info('executions') WHERE name = 'detail'", [], |r| r.get(0)).unwrap();
        for q in ["", "ip:1.2.3.4", "ua:curl path:/admin* probe:true", "rate_limited:true halted:true unpermitted:false"] {
            let p = parse(Some(q));
            let cond = requests(&t, &p, 0, i64::MAX / 2).unwrap();
            // scan's own statement, as EXPLAIN: no Column read of the table's detail.
            let sql = crate::dashboard::request_scan::scan_sql(&t, &cond, 10).unwrap();
            assert!(sql.contains(&format!("INDEXED BY {}", crate::schema::REQUEST_INDEX)), "{q}: {sql}");
            let mut stmt = t.prepare(&format!("EXPLAIN {sql}")).unwrap();
            let ops: Vec<(String, i64, i64, String)> = stmt
                .query_map(rusqlite::params_from_iter(cond.binds.iter()), |r| {
                    Ok((r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<String>>(7)?.unwrap_or_default()))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let table_cursor = ops.iter().find(|o| o.0 == "OpenRead" && o.3.is_empty() && o.2 != 0).map(|o| o.1);
            let reads_detail = ops.iter().any(|o| o.0 == "Column" && Some(o.1) == table_cursor && o.2 == detail);
            assert!(!reads_detail, "{q}: {ops:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A new tenant file has the index; the background build adds it to a
    /// closed day file without it, never to today's or yesterday's (ingest
    /// writes those) nor to a single-file tenant.
    #[test]
    fn the_background_build_indexes_closed_days_only() {
        let dir = std::env::temp_dir().join(format!("rw-reqbuild-{}", crate::control::base58(8)));
        let tenants = dir.join("telemetry");
        let env = tenants.join("env");
        crate::storage::layout::ensure(&env).unwrap();
        let single = tenants.join("single.sqlite3");
        for (i, day) in ["2026-10-20", "2026-10-24", "2026-10-25", "2026-10-26"].iter().enumerate() {
            let p = crate::storage::layout::day_path(&env, crate::storage::layout::Tier::Core, day);
            let c = crate::storage::layout::open_or_create(&p).unwrap();
            assert!(crate::schema::has_index(&c, "main", crate::schema::REQUEST_INDEX).unwrap(), "a new day file has it");
            if i != 0 {
                c.execute_batch(&format!("DROP INDEX {}", crate::schema::REQUEST_INDEX)).unwrap();
            }
        }
        let s = crate::tenants::open_tenant(&single).unwrap();
        s.execute_batch(&format!("DROP INDEX {}", crate::schema::REQUEST_INDEX)).unwrap();
        drop(s);
        let now = crate::time::parse_db_time("2026-10-26 08:00:00").unwrap();
        // A union reader attached throughout, as the server's pools are.
        let (reader, _) = crate::storage::reader::open(&env).unwrap();
        let pass = crate::tenants::add_missing_indexes(&tenants, now, std::time::Duration::ZERO);
        assert_eq!((pass.built, pass.failed), (1, 0), "only 2026-10-24 was closed and without it");
        let has = |p: &std::path::Path| crate::schema::has_index(&rusqlite::Connection::open(p).unwrap(), "main", crate::schema::REQUEST_INDEX).unwrap();
        let day = |d: &str| crate::storage::layout::day_path(&env, crate::storage::layout::Tier::Core, d);
        // The build's WAL is checkpointed and emptied even with a reader holding the file open.
        let wal = |d: &str| std::fs::metadata(format!("{}-wal", day(d).display())).map_or(0, |m| m.len());
        assert_eq!(wal("2026-10-24"), 0, "checkpointed");
        assert!(has(&day("2026-10-24")));
        assert!(!has(&day("2026-10-25")) && !has(&day("2026-10-26")), "yesterday and today are left to ingest");
        assert!(!has(&single), "a single-file tenant is not altered");
        assert_eq!(crate::tenants::add_missing_indexes(&tenants, now, std::time::Duration::ZERO), crate::tenants::IndexPass::default(), "nothing left");
        drop(reader);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
