//! Telemetry::Rollup.summarize and .merged_percentiles (gem
//! app/models/railwatch/telemetry/rollup.rb): window summaries read straight
//! out of stored rollup rows.

use crate::rjson::{self, Value};

use super::tdigest;

/// The columns of one stored rollup row that summarize reads.
#[derive(Debug, Clone)]
pub struct StoredRollup {
    pub group_hash: String,
    pub name: String,
    pub bucket: i64,
    pub count: i64,
    pub error_count: i64,
    pub client_error_count: i64,
    pub duration_sum: i64,
    pub duration_max: i64,
    pub digest: Option<Vec<u8>>,
    pub extra: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub count: i64,
    pub errors: i64,
    pub client_errors: i64,
    /// Integer division, as Ruby's `duration_sum / count`.
    pub avg: i64,
    pub p50: i64,
    pub p95: i64,
    pub p99: i64,
    pub max: i64,
    pub extra: Value,
}

pub const ROLLUP_COLUMNS: &str = "group_hash, name, bucket, count, error_count, client_error_count, duration_sum, duration_max, digest, extra";

pub fn stored_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRollup> {
    let bucket: String = r.get(2)?;
    let extra: Option<String> = r.get(9)?;
    Ok(StoredRollup {
        group_hash: r.get(0)?,
        name: r.get(1)?,
        bucket: crate::time::parse_db_time(&bucket).map_or(0, |m| m / 1_000_000),
        count: r.get(3)?,
        error_count: r.get(4)?,
        client_error_count: r.get(5)?,
        duration_sum: r.get(6)?,
        duration_max: r.get(7)?,
        digest: r.get(8)?,
        extra: extra.and_then(|e| rjson::parse(e.as_bytes()).ok()).unwrap_or(Value::Hash(rjson::Hash::new())),
    })
}

pub fn summarize(rows: &[StoredRollup]) -> Summary {
    if rows.is_empty() {
        return Summary { count: 0, errors: 0, client_errors: 0, avg: 0, p50: 0, p95: 0, p99: 0, max: 0, extra: Value::Hash(rjson::Hash::new()) };
    }
    let ps = merged_percentiles(rows, &[0.5, 0.95, 0.99]);
    let count: i64 = rows.iter().map(|r| r.count).sum();
    let sum: i64 = rows.iter().map(|r| r.duration_sum).sum();
    Summary {
        count,
        errors: rows.iter().map(|r| r.error_count).sum(),
        client_errors: rows.iter().map(|r| r.client_error_count).sum(),
        avg: if count == 0 { 0 } else { sum.div_euclid(count) },
        p50: ps[0],
        p95: ps[1],
        p99: ps[2],
        max: rows.iter().map(|r| r.duration_max).max().unwrap_or(0),
        extra: merge_extras(rows),
    }
}

/// Nearest rank over the union of every row's centroids.
pub fn merged_percentiles(rows: &[StoredRollup], ps: &[f64]) -> Vec<i64> {
    let mut means: Vec<f64> = Vec::new();
    let mut counts: Vec<i64> = Vec::new();
    for r in rows {
        if let Some(d) = &r.digest {
            // Rails raises on an unknown encoding; a corrupt row is skipped
            // here rather than failing the whole window.
            if let Ok((m, c, _)) = tdigest::decode(d) {
                means.extend(m);
                counts.extend(c);
            }
        }
    }
    if means.is_empty() {
        return ps.iter().map(|_| 0).collect();
    }
    let mut order: Vec<usize> = (0..means.len()).collect();
    // sort_by is not stable in Ruby, but ties share a mean, so the answer
    // (a mean) is the same whichever tied index comes first.
    order.sort_by(|a, b| means[*a].partial_cmp(&means[*b]).unwrap());
    let total: i64 = counts.iter().sum();
    let targets: Vec<f64> = ps.iter().map(|p| total as f64 * p).collect();
    let mut found: Vec<f64> = Vec::new();
    let mut seen: i64 = 0;
    for i in &order {
        seen += counts[*i];
        while found.len() < targets.len() && seen as f64 >= targets[found.len()] {
            found.push(means[*i]);
        }
        if found.len() == targets.len() {
            break;
        }
    }
    while found.len() < targets.len() {
        found.push(means[*order.last().unwrap()]);
    }
    found.into_iter().map(|f| f as i64).collect()
}

/// Numbers add up, anything else is last-wins.
pub fn merge_extras(rows: &[StoredRollup]) -> Value {
    let mut out = rjson::Hash::new();
    for r in rows {
        if let Value::Hash(h) = &r.extra {
            for (k, v) in h {
                let merged = match (out.get(k), v) {
                    (Some(Value::Int(a)), Value::Int(b)) => Value::Int(a + b),
                    (Some(a), b) if a.is_numeric() && b.is_numeric() => Value::Float(a.to_f().unwrap() + b.to_f().unwrap()),
                    _ => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
        }
    }
    Value::Hash(out)
}

/// The integer at `extra[key]`, `.to_i`.
pub fn extra_i(extra: &Value, key: &str) -> i64 {
    match extra.get(key) {
        Value::Int(i) => *i,
        Value::Float(f) => *f as i64,
        Value::Str(s) => match crate::ingest::batch::string_to_i(s) {
            Value::Int(i) => i,
            _ => 0,
        },
        _ => 0,
    }
}
