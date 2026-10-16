//! Port of railwatch-cloud's RollupJob (app/jobs/rollup_job.rb @ 464f0fd):
//! recompute one hour's rollups from raw rows, every source in SOURCES
//! order, each type's groups replaced in one transaction.
//!
//! The reads are the SELECTs Rails issues, with an explicit ORDER BY that
//! spells out the index order SQLite's plan already returns them in
//! (EXPLAIN QUERY PLAN in rw-baseline: every source is a range SEARCH on its
//! occurred_at index). Order matters: it decides each group's first row (its
//! name), view_render's most common kind on a tie, and the order durations
//! are pushed into the t-digest.

use std::collections::HashMap;

use indexmap::IndexMap;
use rusqlite::{Connection, params_from_iter};

use super::random::RubyRandom;
use super::sqlval::{Sql, row_values};
use super::tdigest::TDigest;
use crate::rjson::{self, Value};

/// SQLite binds at most 32,766 variables per statement; a rollup row is 14.
pub const INSERT_SLICE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Execution,
    Query,
    OutgoingRequest,
    CacheEvent,
    Mail,
    Visit,
    Span,
    Notification,
    ViewRender,
    Transaction,
    LlmCall,
}

pub struct Source {
    pub record_type: &'static str,
    pub table: Table,
    /// The relation's WHERE beyond the hour (the scope lambda).
    scope: &'static str,
}

/// RollupJob::SOURCES, in order.
pub const SOURCES: &[Source] = &[
    Source { record_type: "request", table: Table::Execution, scope: "\"executions\".\"kind\" = 'request' AND " },
    Source { record_type: "job_attempt", table: Table::Execution, scope: "\"executions\".\"kind\" = 'job_attempt' AND " },
    Source { record_type: "scheduled_task", table: Table::Execution, scope: "\"executions\".\"kind\" = 'scheduled_task' AND " },
    Source { record_type: "command", table: Table::Execution, scope: "\"executions\".\"kind\" = 'command' AND " },
    Source { record_type: "channel_action", table: Table::Execution, scope: "\"executions\".\"kind\" = 'channel_action' AND " },
    Source { record_type: "query", table: Table::Query, scope: "" },
    Source { record_type: "outgoing_request", table: Table::OutgoingRequest, scope: "" },
    Source { record_type: "cache_event", table: Table::CacheEvent, scope: "" },
    Source { record_type: "mail", table: Table::Mail, scope: "" },
    Source { record_type: "visit", table: Table::Visit, scope: "" },
    Source { record_type: "span", table: Table::Span, scope: "" },
    Source { record_type: "notification", table: Table::Notification, scope: "" },
    Source { record_type: "view_render", table: Table::ViewRender, scope: "" },
    Source { record_type: "transaction", table: Table::Transaction, scope: "" },
    Source { record_type: "llm_call", table: Table::LlmCall, scope: "\"llm_calls\".\"operation\" != 'tool' AND " },
    Source { record_type: "llm_tool", table: Table::LlmCall, scope: "\"llm_calls\".\"operation\" = 'tool' AND " },
];

impl Table {
    pub fn name(self) -> &'static str {
        match self {
            Table::Execution => "executions",
            Table::Query => "queries",
            Table::OutgoingRequest => "outgoing_requests",
            Table::CacheEvent => "cache_events",
            Table::Mail => "mails",
            Table::Visit => "visits",
            Table::Span => "spans",
            Table::Notification => "notifications",
            Table::ViewRender => "view_renders",
            Table::Transaction => "transactions",
            Table::LlmCall => "llm_calls",
        }
    }

    /// The per-class columns after id, group_hash, duration.
    pub fn columns(self) -> &'static [&'static str] {
        match self {
            Table::Execution => &["name", "status", "outcome", "kind"],
            Table::Query => &[],
            Table::OutgoingRequest => &["method", "host", "status_code"],
            Table::CacheEvent => &["store", "key", "type"],
            Table::Mail => &["mailer", "failed"],
            Table::Visit => &["component", "status"],
            Table::Notification => &["notifier", "delivery_method", "failed"],
            Table::ViewRender => &["identifier", "kind"],
            Table::Transaction => &["outcome", "connection"],
            Table::Span => &["name", "status"],
            Table::LlmCall => &[
                "operation", "model", "tool_name", "status", "input_tokens", "output_tokens", "cache_read_tokens",
                "cache_write_tokens", "cost_nanos", "finish_reason", "attachments",
            ],
        }
    }

    /// The index order the hour's range scan returns rows in.
    fn order(self) -> &'static str {
        match self {
            Table::Query => "\"queries\".\"occurred_at\", \"queries\".\"duration\", \"queries\".\"id\"",
            t => match t {
                Table::Execution => "\"executions\".\"occurred_at\", \"executions\".\"id\"",
                Table::OutgoingRequest => "\"outgoing_requests\".\"occurred_at\", \"outgoing_requests\".\"id\"",
                Table::CacheEvent => "\"cache_events\".\"occurred_at\", \"cache_events\".\"id\"",
                Table::Mail => "\"mails\".\"occurred_at\", \"mails\".\"id\"",
                Table::Visit => "\"visits\".\"occurred_at\", \"visits\".\"id\"",
                Table::Span => "\"spans\".\"occurred_at\", \"spans\".\"id\"",
                Table::Notification => "\"notifications\".\"occurred_at\", \"notifications\".\"id\"",
                Table::ViewRender => "\"view_renders\".\"occurred_at\", \"view_renders\".\"id\"",
                Table::Transaction => "\"transactions\".\"occurred_at\", \"transactions\".\"id\"",
                Table::LlmCall => "\"llm_calls\".\"occurred_at\", \"llm_calls\".\"id\"",
                Table::Query => unreachable!(),
            },
        }
    }
}

impl Source {
    /// The SELECT list Rails sends for this source.
    pub fn select_list(&self) -> String {
        let t = self.table.name();
        let mut cols = vec![format!("\"{t}\".\"id\""), format!("\"{t}\".\"group_hash\""), format!("\"{t}\".\"duration\"")];
        cols.extend(self.table.columns().iter().map(|c| format!("\"{t}\".\"{c}\"")));
        if self.table == Table::Query {
            cols.push("COALESCE(NULLIF(queries.sql, ''), (SELECT query_shapes.sql FROM query_shapes WHERE query_shapes.group_hash = queries.group_hash), '') AS resolved_sql".into());
        }
        cols.join(", ")
    }

    pub fn select_sql(&self) -> String {
        let t = self.table.name();
        format!(
            "SELECT {} FROM \"{t}\" WHERE {}\"{t}\".\"occurred_at\" >= ?1 AND \"{t}\".\"occurred_at\" < ?2 ORDER BY {}",
            self.select_list(),
            self.scope,
            self.table.order()
        )
    }
}

impl Source {
    /// The same columns plus occurred_at, for rows picked by id (the
    /// incremental path). The kind filter is written `+kind` so it is checked
    /// on each row instead of driving the plan: as `kind = ?` SQLite chose
    /// idx_executions_kind_outcome_cursor (kind=?) and walked every row of
    /// the kind per lookup, which grew with the file (docs/PERF.md #2).
    pub fn select_by_ids_sql(&self, n: usize) -> String {
        let t = self.table.name();
        let ph = vec!["?"; n].join(",");
        let scope = self.scope.replacen("\"executions\".\"kind\"", "+\"executions\".\"kind\"", 1);
        format!("SELECT {}, \"{t}\".\"occurred_at\" FROM \"{t}\" WHERE {scope}\"{t}\".\"id\" IN ({ph})", self.select_list())
    }

    pub fn width(&self) -> usize {
        3 + self.table.columns().len() + usize::from(self.table == Table::Query)
    }

    pub fn is_query(&self) -> bool {
        self.table == Table::Query
    }

    pub fn scope_sql(&self) -> &'static str {
        self.scope
    }
}

/// One raw row as RollupJob reads it: [id, group_hash, duration, ...columns].
pub type RawRow = Vec<Sql>;

/// A rollups row, ready for insert_all (column order is Rails').
#[derive(Debug, Clone, PartialEq)]
pub struct RollupRow {
    pub record_type: &'static str,
    pub group_hash: Vec<u8>,
    pub name: Vec<u8>,
    pub bucket: i64,
    pub count: i64,
    pub error_count: i64,
    pub client_error_count: i64,
    pub duration_sum: i64,
    pub duration_max: i64,
    pub p50: i64,
    pub p95: i64,
    pub p99: i64,
    pub digest: Vec<u8>,
    pub extra: Value,
}

/// Where compress!'s shuffles draw from.
pub enum Rng<'a> {
    /// One stream for the whole run, as Ruby's global generator is: the
    /// Rails-equivalence mode (seed both sides the same).
    Shared(&'a mut RubyRandom),
    /// A stream per (type, group, hour) seeded from those, so a group's digest
    /// depends only on its own rows. What the server uses (DIFFERENCES.md).
    PerGroup,
}

pub fn group_seed(record_type: &str, group_hash: &[u8], bucket: i64) -> u32 {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(record_type.as_bytes());
    h.update([0]);
    h.update(group_hash);
    h.update([0]);
    h.update(bucket.to_le_bytes());
    let d = h.finalize();
    u32::from_le_bytes([d[0], d[1], d[2], d[3]])
}

/// Reads one source's hour.
pub fn read_source(conn: &Connection, source: &Source, bucket: i64) -> rusqlite::Result<Vec<RawRow>> {
    let (from, to) = (crate::time::utc_seconds_string(bucket), crate::time::utc_seconds_string(bucket + 3600));
    let n = 3 + source.table.columns().len() + usize::from(source.table == Table::Query);
    let mut stmt = conn.prepare_cached(&source.select_sql())?;
    let rows = stmt.query_map([from, to], |r| row_values(r, n))?;
    rows.collect()
}

/// `rows.group_by(&:group_hash).except(nil)`, then fresh_attributes per group.
pub fn rollups_for(source: &Source, bucket: i64, rows: &[RawRow], rng: &mut Rng<'_>) -> Vec<RollupRow> {
    let mut groups: IndexMap<&[u8], Vec<&RawRow>> = IndexMap::new();
    for row in rows {
        if let Some(g) = row[1].text() {
            groups.entry(g).or_default().push(row);
        }
    }
    groups.into_iter().map(|(g, group)| fresh_attributes(source, g, bucket, &group, rng)).collect()
}

pub fn fresh_attributes(source: &Source, group_hash: &[u8], bucket: i64, group: &[&RawRow], rng: &mut Rng<'_>) -> RollupRow {
    let t = source.record_type;
    let mut own;
    let rng: &mut RubyRandom = match rng {
        Rng::Shared(r) => r,
        Rng::PerGroup => {
            own = RubyRandom::new(group_seed(t, group_hash, bucket));
            &mut own
        }
    };
    let mut digest = TDigest::new(0.01);
    let mut sum: i64 = 0;
    let mut max: Option<i64> = None;
    for r in group {
        let d = r[2].to_i();
        digest.push_int(d, rng);
        sum += d;
        max = Some(max.map_or(d, |m| m.max(d)));
    }
    digest.compress(rng);
    let p = |d: &mut TDigest, q: f64| d.percentile(q).map_or(0, |n| n.to_i());
    RollupRow {
        record_type: t,
        group_hash: group_hash.to_vec(),
        name: name_for(source, group[0]),
        bucket,
        count: group.len() as i64,
        error_count: group.iter().filter(|r| error(t, r)).count() as i64,
        client_error_count: if t == "request" {
            group.iter().filter(|r| (400..=499).contains(&col(source, r, "status").to_i())).count() as i64
        } else {
            0
        },
        duration_sum: sum,
        duration_max: max.unwrap_or(0),
        p50: p(&mut digest, 0.5),
        p95: p(&mut digest, 0.95),
        p99: p(&mut digest, 0.99),
        digest: digest.as_small_bytes(),
        extra: extra_for(source, group),
    }
}

fn col<'r>(source: &Source, row: &'r RawRow, name: &str) -> &'r Sql {
    let i = source.table.columns().iter().position(|c| *c == name).unwrap_or_else(|| panic!("{} has no {name}", source.record_type));
    &row[3 + i]
}

fn join(a: &[u8], sep: &str, b: &[u8]) -> Vec<u8> {
    let mut v = a.to_vec();
    v.extend_from_slice(sep.as_bytes());
    v.extend_from_slice(b);
    v
}

/// The namer lambda, `.to_s`.
fn name_for(source: &Source, row: &RawRow) -> Vec<u8> {
    let c = |n| col(source, row, n);
    match source.record_type {
        "request" | "job_attempt" | "scheduled_task" | "command" | "channel_action" | "span" => c("name").s().to_vec(),
        "query" => rjson::ruby::first_chars(row[3].s(), 255),
        "outgoing_request" => join(c("method").s(), " ", c("host").s()),
        "cache_event" => join(c("store").s(), " ", c("key").s()),
        "mail" => c("mailer").s().to_vec(),
        "visit" => c("component").s().to_vec(),
        // `row.notifier || row.delivery_method`: only nil falls through.
        "notification" => if c("notifier").is_null() { c("delivery_method").s().to_vec() } else { c("notifier").s().to_vec() },
        "view_render" => c("identifier").s().to_vec(),
        "transaction" => join(c("connection").s(), " · ", c("outcome").s()),
        "llm_call" => join(c("model").s(), " · ", c("operation").s()),
        "llm_tool" => c("tool_name").s().to_vec(),
        other => unreachable!("{other}"),
    }
}

/// RollupJob#error?
fn error(t: &str, row: &RawRow) -> bool {
    let source = SOURCES.iter().find(|s| s.record_type == t).unwrap();
    let c = |n| col(source, row, n);
    match t {
        "request" => c("status").to_i() >= 500,
        "job_attempt" | "scheduled_task" | "channel_action" => c("outcome").eq_str("failed"),
        "command" => c("status").to_i() != 0,
        "outgoing_request" => {
            let s = c("status_code").to_i();
            s >= 500 || s == 0
        }
        "mail" | "notification" => c("failed").truthy_bool(),
        "visit" => c("status").eq_str("error"),
        "transaction" => c("outcome").eq_str("rollback"),
        "span" | "llm_call" | "llm_tool" => c("status").eq_str("failed"),
        _ => false,
    }
}

/// RollupJob#extra_for
fn extra_for(source: &Source, group: &[&RawRow]) -> Value {
    let c = |r: &RawRow, n: &str| col(source, r, n).clone();
    match source.record_type {
        "cache_event" => crate::rhash! {
            "hits" => group.iter().filter(|r| c(r, "type").eq_str("hit")).count() as i64,
            "misses" => group.iter().filter(|r| { let t = c(r, "type"); t.eq_str("miss") || t.eq_str("generate") }).count() as i64,
        },
        "view_render" => {
            // group_by(&:kind).max_by { size }&.first: the first kind to
            // reach the largest count, nil included.
            let mut kinds: IndexMap<Option<Vec<u8>>, usize> = IndexMap::new();
            for r in group {
                *kinds.entry(c(r, "kind").text().map(<[u8]>::to_vec)).or_default() += 1;
            }
            let mut best: Option<(&Option<Vec<u8>>, usize)> = None;
            for (k, n) in &kinds {
                if best.is_none_or(|(_, b)| *n > b) {
                    best = Some((k, *n));
                }
            }
            let kind = best.and_then(|(k, _)| k.clone()).map_or(Value::Nil, Value::Str);
            crate::rhash! { "kind" => kind }
        }
        "llm_call" => {
            let sum = |n: &str| group.iter().map(|r| c(r, n).to_i()).sum::<i64>();
            crate::rhash! {
                "input_tokens" => sum("input_tokens"),
                "output_tokens" => sum("output_tokens"),
                "cache_read_tokens" => sum("cache_read_tokens"),
                "cache_write_tokens" => sum("cache_write_tokens"),
                "cost_nanos" => sum("cost_nanos"),
                "priced" => group.iter().filter(|r| !c(r, "cost_nanos").is_null()).count() as i64,
                "unpriced" => group.iter().filter(|r| c(r, "cost_nanos").is_null() && !c(r, "status").eq_str("failed")).count() as i64,
                "truncated" => group.iter().filter(|r| c(r, "finish_reason").eq_str("max_tokens")).count() as i64,
                "with_attachments" => group.iter().filter(|r| c(r, "attachments").to_i() > 0).count() as i64,
            }
        }
        _ => Value::Hash(rjson::Hash::new()),
    }
}

/// Reads and aggregates the whole hour, every source in order.
pub fn recompute(conn: &Connection, bucket: i64, rng: &mut Rng<'_>) -> rusqlite::Result<Vec<(&'static Source, Vec<RollupRow>)>> {
    let mut out = Vec::new();
    for source in SOURCES {
        let rows = read_source(conn, source, bucket)?;
        let rollups = rollups_for(source, bucket, &rows, rng);
        if !rollups.is_empty() {
            out.push((source, rollups));
        }
    }
    Ok(out)
}

/// Writes one type's groups for the hour: one IMMEDIATE transaction holding
/// a DELETE of the groups and INSERTs in slices, as Rails does.
pub fn write_type(conn: &Connection, record_type: &str, bucket: i64, rows: &[RollupRow]) -> rusqlite::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let b = crate::time::utc_seconds_string(bucket);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        // Chunked under SQLite's bind limit; Rails sends one IN list.
        for chunk in rows.chunks(30_000) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "DELETE FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ? AND \"rollups\".\"group_hash\" IN ({placeholders}) AND \"rollups\".\"bucket\" = ?"
            );
            let mut binds: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() + 2);
            binds.push(record_type.to_string().into());
            binds.extend(chunk.iter().map(|r| rusqlite::types::Value::Text(String::from_utf8_lossy(&r.group_hash).into_owned())));
            binds.push(b.clone().into());
            conn.prepare_cached(&sql)?.execute(params_from_iter(binds))?;
        }
        for slice in rows.chunks(INSERT_SLICE) {
            insert_slice(conn, slice)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// A bind value: Ruby strings are bytes tagged UTF-8, and the sqlite3 gem
/// binds them as TEXT byte for byte, valid UTF-8 or not. rusqlite's
/// `ValueRef::Text` takes bytes, so this needs no `String` (and no unsafe).
enum Bind<'a> {
    Text(&'a [u8]),
    Value(rusqlite::types::Value),
}

impl rusqlite::ToSql for Bind<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(match self {
            Bind::Text(b) => rusqlite::types::ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Text(b)),
            Bind::Value(v) => rusqlite::types::ToSqlOutput::Borrowed(v.into()),
        })
    }
}

fn text(b: &[u8]) -> Bind<'_> {
    Bind::Text(b)
}

fn value(v: impl Into<rusqlite::types::Value>) -> Bind<'static> {
    Bind::Value(v.into())
}

fn insert_slice(conn: &Connection, rows: &[RollupRow]) -> rusqlite::Result<()> {
    let values = vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"; rows.len()].join(", ");
    let sql = format!(
        "INSERT INTO \"rollups\" (\"record_type\",\"group_hash\",\"name\",\"bucket\",\"count\",\"error_count\",\"client_error_count\",\"duration_sum\",\"duration_max\",\"p50\",\"p95\",\"p99\",\"digest\",\"extra\") VALUES {values} ON CONFLICT  DO NOTHING"
    );
    let mut binds: Vec<Bind<'_>> = Vec::with_capacity(rows.len() * 14);
    for r in rows {
        binds.push(value(r.record_type.to_string()));
        binds.push(text(&r.group_hash));
        binds.push(text(&r.name));
        binds.push(value(crate::time::utc_seconds_string(r.bucket)));
        for n in [r.count, r.error_count, r.client_error_count, r.duration_sum, r.duration_max, r.p50, r.p95, r.p99] {
            binds.push(value(n));
        }
        binds.push(value(rusqlite::types::Value::Blob(r.digest.clone())));
        binds.push(value(rjson::generate(&r.extra).unwrap_or_else(|_| "{}".into())));
    }
    conn.prepare_cached(&sql)?.execute(params_from_iter(binds))?;
    Ok(())
}

/// Recompute and write the hour, one transaction per type.
pub fn perform(read: &Connection, write: &Connection, bucket: i64, rng: &mut Rng<'_>) -> rusqlite::Result<usize> {
    let mut n = 0;
    for (source, rows) in recompute(read, bucket, rng)? {
        write_type(write, source.record_type, bucket, &rows)?;
        n += rows.len();
    }
    Ok(n)
}

/// Hours that hold rolled-up raw rows (for a full rebuild).
pub fn hours_with_rows(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut hours = std::collections::BTreeSet::new();
    let mut seen = HashMap::new();
    for s in SOURCES {
        if seen.insert(s.table.name(), ()).is_some() {
            continue;
        }
        let sql = format!("SELECT DISTINCT strftime('%s', strftime('%Y-%m-%d %H:00:00', occurred_at)) FROM \"{}\"", s.table.name());
        let mut stmt = conn.prepare(&sql)?;
        for h in stmt.query_map([], |r| r.get::<_, Option<String>>(0))? {
            if let Some(h) = h?.and_then(|h| h.parse::<i64>().ok()) {
                hours.insert(h);
            }
        }
    }
    Ok(hours.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invalid UTF-8 in a name is stored as TEXT byte for byte (as the
    /// sqlite3 gem binds a Ruby string), without an unchecked String.
    #[test]
    fn text_binds_keep_invalid_utf8_bytes() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
        let raw: &[u8] = b"GET /caf\xe9 \xff\xfe";
        c.execute("INSERT INTO t VALUES (?)", [text(raw)]).unwrap();
        let (ty, bytes): (String, Vec<u8>) = c.query_row("SELECT typeof(v), CAST(v AS BLOB) FROM t", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(ty, "text");
        assert_eq!(bytes, raw);
    }
}
