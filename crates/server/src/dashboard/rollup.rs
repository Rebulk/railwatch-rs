//! Port of Railwatch::Telemetry::Rollup's read side: the rows, `summarize`,
//! `merged_percentiles` and `merge_extras`.

use rusqlite::Connection;
use serde_json::{Map, Value};

use super::db::{self, qt};
use super::rb;
use super::tdigest_decode;

#[derive(Debug, Clone)]
pub struct Rollup {
    pub group_hash: String,
    pub name: Option<String>,
    /// Epoch micros of the hour.
    pub bucket: i64,
    pub count: i64,
    pub error_count: i64,
    pub client_error_count: i64,
    pub duration_sum: i64,
    pub duration_max: i64,
    pub digest: Option<Vec<u8>>,
    pub extra: Map<String, Value>,
}

/// The SQL `Rollup.for_type(t).between(from, to)` produces; the extra
/// conditions follow it in the order Active Record appends them. Row order
/// is whatever SQLite's plan yields, as in Ruby (no ORDER BY): pages group
/// these rows by first appearance, so the statement must be the same one.
pub const BETWEEN: &str = r#"SELECT "rollups".* FROM "rollups" WHERE "rollups"."record_type" = ?1 AND "rollups"."bucket" BETWEEN ?2 AND ?3"#;

pub fn between(conn: &Connection, record_type: &str, from: i64, to: i64) -> rusqlite::Result<Vec<Rollup>> {
    load(conn, BETWEEN, rusqlite::params![record_type, qt(rb::beginning_of_hour(from)), qt(to)])
}

pub fn between_group(conn: &Connection, record_type: &str, from: i64, to: i64, group_hash: &str) -> rusqlite::Result<Vec<Rollup>> {
    let sql = format!(r#"{BETWEEN} AND "rollups"."group_hash" = ?4"#);
    load(conn, &sql, rusqlite::params![record_type, qt(rb::beginning_of_hour(from)), qt(to), group_hash])
}

/// `where(bucket: from_hour..to)` without the beginning_of_hour (deploys#show
/// passes an explicit bucket range).
pub fn bucket_range(conn: &Connection, record_type: &str, lo: i64, hi: i64) -> rusqlite::Result<Vec<Rollup>> {
    load(conn, BETWEEN, rusqlite::params![record_type, qt(lo), qt(hi)])
}

/// Decodes straight from the statement (the 7-day queries page reads ~30k
/// rollup rows; going through `db::Rec` copied every column into an owned
/// value first and cost more than the SQL). Same values as `Rec`'s
/// accessors: text columns lossily as UTF-8, numbers as `Rec::i`.
pub fn load<P: rusqlite::Params>(conn: &Connection, sql: &str, params: P) -> rusqlite::Result<Vec<Rollup>> {
    use rusqlite::types::ValueRef;
    let mut stmt = db::prepare(conn, sql)?;
    let col = |name: &str| stmt.column_index(name).ok();
    let [group_hash, name, bucket, count, error_count, client_error_count, duration_sum, duration_max, digest, extra] =
        ["group_hash", "name", "bucket", "count", "error_count", "client_error_count", "duration_sum", "duration_max", "digest", "extra"].map(col);
    fn text(v: ValueRef<'_>) -> Option<std::borrow::Cow<'_, str>> {
        match v {
            ValueRef::Text(t) => Some(String::from_utf8_lossy(t)),
            _ => None,
        }
    }
    let int = |v: ValueRef<'_>| match v {
        ValueRef::Integer(i) => i,
        ValueRef::Real(f) => f as i64,
        ValueRef::Text(t) => std::str::from_utf8(t).ok().and_then(|t| t.parse().ok()).unwrap_or(0),
        _ => 0,
    };
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let get = |i: Option<usize>| -> rusqlite::Result<ValueRef<'_>> { i.map_or(Ok(ValueRef::Null), |i| r.get_ref(i)) };
        out.push(Rollup {
            group_hash: text(get(group_hash)?).unwrap_or_default().into_owned(),
            name: text(get(name)?).map(|n| n.into_owned()),
            bucket: text(get(bucket)?).and_then(|b| crate::time::parse_db_time(&b)).unwrap_or(0),
            count: int(get(count)?),
            error_count: int(get(error_count)?),
            client_error_count: int(get(client_error_count)?),
            duration_sum: int(get(duration_sum)?),
            duration_max: int(get(duration_max)?),
            digest: match get(digest)? {
                ValueRef::Blob(b) | ValueRef::Text(b) => Some(b.to_vec()),
                _ => None,
            },
            extra: match text(get(extra)?) {
                Some(t) if t == "{}" => Map::new(),
                Some(t) => match serde_json::from_str(&t) {
                    Ok(Value::Object(m)) => m,
                    _ => Map::new(),
                },
                None => Map::new(),
            },
        });
    }
    Ok(out)
}

/// Rollup.summarize's hash.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub count: i64,
    pub errors: i64,
    pub client_errors: i64,
    pub avg: i64,
    pub p50: i64,
    pub p95: i64,
    pub p99: i64,
    pub max: i64,
    pub extra: Map<String, Value>,
}

impl Summary {
    pub fn to_json(&self) -> Value {
        crate::obj! {
            "count" => self.count, "errors" => self.errors, "client_errors" => self.client_errors,
            "avg" => self.avg, "p50" => self.p50, "p95" => self.p95, "p99" => self.p99, "max" => self.max,
            "extra" => Value::Object(self.extra.clone()),
        }
    }

    pub fn get(&self, key: &str) -> i64 {
        match key {
            "count" => self.count,
            "errors" => self.errors,
            "avg" => self.avg,
            "max" => self.max,
            "p50" => self.p50,
            "p95" => self.p95,
            "p99" => self.p99,
            _ => 0,
        }
    }
}

pub fn summarize(rows: &[&Rollup]) -> Summary {
    if rows.is_empty() {
        return Summary::default();
    }
    let ps = merged_percentiles(rows, &[0.5, 0.95, 0.99]);
    let count: i64 = rows.iter().map(|r| r.count).sum();
    Summary {
        count,
        errors: rows.iter().map(|r| r.error_count).sum(),
        client_errors: rows.iter().map(|r| r.client_error_count).sum(),
        avg: if count == 0 { 0 } else { rows.iter().map(|r| r.duration_sum).sum::<i64>().div_euclid(count) },
        p50: ps[0],
        p95: ps[1],
        p99: ps[2],
        max: rows.iter().map(|r| r.duration_max).max().unwrap_or(0),
        extra: merge_extras(rows),
    }
}

pub fn summarize_owned(rows: &[Rollup]) -> Summary {
    summarize(&rows.iter().collect::<Vec<_>>())
}

/// The digest-free half (Aggregations.cheap_summary).
pub fn cheap_summary(rows: &[&Rollup]) -> Summary {
    let count: i64 = rows.iter().map(|r| r.count).sum();
    Summary {
        count,
        errors: rows.iter().map(|r| r.error_count).sum(),
        avg: if count == 0 { 0 } else { rows.iter().map(|r| r.duration_sum).sum::<i64>().div_euclid(count) },
        max: rows.iter().map(|r| r.duration_max).max().unwrap_or(0),
        ..Summary::default()
    }
}

/// Nearest-rank percentiles of the union of every row's centroids.
pub fn merged_percentiles(rows: &[&Rollup], ps: &[f64]) -> Vec<i64> {
    let mut means = Vec::new();
    let mut counts = Vec::new();
    for row in rows {
        if let Some(d) = &row.digest {
            // Ruby raises ArgumentError on an unknown encoding; the page 500s.
            // A digest we cannot read is skipped here, and counted nowhere --
            // it cannot happen for rows the rollup job wrote.
            let _ = tdigest_decode::centroids(d, &mut means, &mut counts);
        }
    }
    if means.is_empty() {
        return ps.iter().map(|_| 0).collect();
    }
    // Sort (mean, weight) pairs by mean. Equal means are one value, so tie
    // order does not change the answer and an unstable sort is exact.
    let mut pairs: Vec<(f64, u64)> = means.into_iter().zip(counts).collect();
    pairs.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let total: u64 = pairs.iter().map(|p| p.1).sum();
    let targets: Vec<f64> = ps.iter().map(|p| total as f64 * p).collect();
    let mut found: Vec<f64> = Vec::with_capacity(ps.len());
    let mut seen: u64 = 0;
    for &(mean, count) in &pairs {
        seen += count;
        while found.len() < targets.len() && seen as f64 >= targets[found.len()] {
            found.push(mean);
        }
        if found.len() == targets.len() {
            break;
        }
    }
    while found.len() < targets.len() {
        found.push(pairs.last().unwrap().0);
    }
    found.into_iter().map(|f| f as i64).collect()
}

pub fn merge_extras(rows: &[&Rollup]) -> Map<String, Value> {
    let mut out = Map::new();
    for row in rows {
        for (k, v) in &row.extra {
            let merged = match out.get(k) {
                Some(existing) if existing.is_number() && v.is_number() => num_add(existing, v),
                _ => v.clone(),
            };
            out.insert(k.clone(), merged);
        }
    }
    out
}

/// Integer + Integer stays an Integer; anything with a Float is a Float.
pub fn num_add(a: &Value, b: &Value) -> Value {
    match (a.as_i64(), b.as_i64()) {
        (Some(x), Some(y)) if is_int(a) && is_int(b) => Value::from(x + y),
        _ => db::jf(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)),
    }
}

fn is_int(v: &Value) -> bool {
    v.as_number().is_some_and(|n| {
        let s = n.to_string();
        !s.contains(['.', 'e', 'E'])
    })
}

/// `r.extra[key].to_i`
pub fn extra_i(r: &Rollup, key: &str) -> i64 {
    match r.extra.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(_)) | Some(Value::Null) | None => 0,
        _ => 0,
    }
}

/// Groups rows by group_hash in order of first appearance (Ruby's group_by).
pub fn group_by_hash(rows: &[Rollup]) -> Vec<(String, Vec<&Rollup>)> {
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut out: Vec<(String, Vec<&Rollup>)> = Vec::new();
    for r in rows {
        match index.get(r.group_hash.as_str()) {
            Some(&i) => out[i].1.push(r),
            None => {
                index.insert(&r.group_hash, out.len());
                out.push((r.group_hash.clone(), vec![r]));
            }
        }
    }
    out
}

/// `group_rows.max_by(&:bucket).name`
pub fn latest_name(rows: &[&Rollup]) -> Option<String> {
    rb::max_by(rows, |r| r.bucket).and_then(|r| r.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(digest: Vec<u8>) -> Rollup {
        Rollup {
            group_hash: "g".into(),
            name: None,
            bucket: 0,
            count: 0,
            error_count: 0,
            client_error_count: 0,
            duration_sum: 0,
            duration_max: 0,
            digest: Some(digest),
            extra: Map::new(),
        }
    }

    fn small(means: &[f32], weights: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(2u32.to_le_bytes());
        b.extend(0.01f64.to_le_bytes());
        b.extend((means.len() as u32).to_le_bytes());
        let mut prev = 0.0f32;
        for m in means {
            b.extend((m - prev).to_le_bytes());
            prev = *m;
        }
        b.extend(weights);
        b
    }

    #[test]
    fn nearest_rank_not_midpoint() {
        // The comment in rollup.rb: the median of 1,000 sevens and 300 nines is 7.
        let a = row(small(&[7.0], &[0xE8, 0x07])); // 1000 as a varint
        let b = row(small(&[9.0], &[0xAC, 0x02])); // 300
        assert_eq!(merged_percentiles(&[&a, &b], &[0.5, 0.95, 0.99]), vec![7, 9, 9]);
    }

    #[test]
    fn extras_add_numbers_and_keep_last_strings() {
        let mut a = row(vec![]);
        a.extra = serde_json::from_str(r#"{"hits":2,"kind":"partial","cost":1.5}"#).unwrap();
        let mut b = row(vec![]);
        b.extra = serde_json::from_str(r#"{"hits":3,"kind":"template","cost":1}"#).unwrap();
        let m = merge_extras(&[&a, &b]);
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"hits":5,"kind":"template","cost":2.5}"#);
    }
}
