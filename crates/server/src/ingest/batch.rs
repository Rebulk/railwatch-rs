//! Port of Ingest::Batch (railwatch-cloud app/models/ingest/batch.rb).
//!
//! The Ruby class maps and writes in one call. Here the two halves are split:
//! `Batch::map_all` runs on the request task (in parallel across requests)
//! and produces a `Prepared` batch; the tenant's writer thread later applies
//! it with `Writer::write` inside its transaction. What each half does, and
//! in which order, is unchanged.

use indexmap::{IndexMap, IndexSet};

use crate::ingest::mapper::{self, PersonRecord, Row};
use crate::rjson::{self, Value, ruby};

const MAX_PAST_AGE: f64 = 30.0 * 86_400.0;
const MAX_FUTURE_AGE: f64 = 3_600.0;

/// Ingest::Batch::ROLLED_UP
const ROLLED_UP: &[&str] = &[
    "request",
    "job_attempt",
    "scheduled_task",
    "command",
    "channel_action",
    "query",
    "outgoing_request",
    "cache_event",
    "mail",
    "visit",
    "notification",
    "span",
    "llm_call",
];

#[derive(Debug, Clone)]
pub struct Rejection {
    pub record_type: Option<Vec<u8>>,
    pub reason: String,
}

impl Rejection {
    /// `{type:, reason:}` as the Ruby hash serializes.
    pub fn to_value(&self) -> Value {
        let mut h = rjson::Hash::new();
        h.insert(b"type".to_vec(), self.record_type.clone().map(Value::Str).unwrap_or(Value::Nil));
        h.insert(b"reason".to_vec(), Value::from(self.reason.clone()));
        Value::Hash(h)
    }
}

pub fn rejections_value(rejections: &[Rejection], limit: usize) -> Value {
    Value::Array(rejections.iter().take(limit).map(Rejection::to_value).collect())
}

/// What the request told us about the delivery besides its records.
#[derive(Debug, Clone)]
pub struct Meta {
    /// `X-Railwatch-Dropped`.to_i -- may be a bignum, which fails the write.
    pub dropped_by_client: Value,
    pub backpressure_factor: f64,
    /// `request.content_length.to_i`
    pub bytes: Value,
    pub gem_version: Option<Vec<u8>>,
    /// Time.current when the batch was created, in microseconds (stored) and
    /// as a float (the timestamp window).
    pub received_at_micros: i64,
}

impl Meta {
    pub fn received_at(&self) -> f64 {
        self.received_at_micros as f64 / 1e6
    }
}

/// ReceiptHeaders#to_attributes.
#[derive(Debug, Clone)]
pub struct ReceiptAttrs {
    pub producer_id: String,
    pub delivery_id: String,
    pub body_sha256: String,
    pub metadata_sha256: String,
    pub policy_version: String,
    /// Time.at(ms / 1000.0): (seconds, usec)
    pub delivery_created_at: (i64, u32),
}

/// A mapped batch, ready for the writer.
pub struct Prepared {
    /// Rows by table, tables in first-appearance order (Ruby's
    /// `@rows_by_class` hash order).
    pub tables: Vec<(&'static str, Vec<Row>)>,
    pub people: Vec<(PersonRecord, (i64, u32))>,
    pub rejections: Vec<Rejection>,
    pub counts: IndexMap<Vec<u8>, i64>,
    pub truncations: Vec<String>,
    /// Hour buckets as epoch seconds, insertion-ordered sets.
    pub rollup_buckets: IndexSet<i64>,
    pub session_buckets: IndexSet<i64>,
    pub events_from: Option<f64>,
    pub events_to: Option<f64>,
    pub record_count: usize,
    pub meta: Meta,
    pub receipt: Option<ReceiptAttrs>,
}

impl Prepared {
    pub fn row_count(&self) -> usize {
        self.tables.iter().map(|(_, rows)| rows.len()).sum::<usize>() + self.people.len()
    }
}

/// Batch.new(...).map_all. `ingest_floor` is Environment#ingest_floor in
/// epoch seconds (`[now - retention_days, raw_pruned_through].max`).
pub fn map_all(records: Vec<Value>, ingest_floor: f64, meta: Meta, receipt: Option<ReceiptAttrs>) -> Prepared {
    let received_at = meta.received_at();
    // retention_floor: beginning of the floor's hour, plus one hour.
    let floor_secs = ingest_floor.floor() as i64;
    let retention_floor = (floor_secs - floor_secs.rem_euclid(3600) + 3600) as f64;
    let (min, max) = (received_at - MAX_PAST_AGE, received_at + MAX_FUTURE_AGE);

    let mut p = Prepared {
        tables: Vec::new(),
        people: Vec::new(),
        rejections: Vec::new(),
        counts: IndexMap::new(),
        truncations: Vec::new(),
        rollup_buckets: IndexSet::new(),
        session_buckets: IndexSet::new(),
        events_from: None,
        events_to: None,
        record_count: records.len(),
        meta,
        receipt,
    };

    for rec in &records {
        if let Err(e) = map_one(rec, &mut p, min, max, retention_floor) {
            reject(&mut p, rec, e.rejection_reason());
        }
    }
    p
}

fn map_one(rec: &Value, p: &mut Prepared, min: f64, max: f64, retention_floor: f64) -> Result<(), mapper::MapError> {
    mapper::validate_record(rec)?;
    let ts_value = rec.get("timestamp");
    let ts = timestamp_in_range(ts_value)?;
    if !(ts >= min && ts <= max) {
        reject(p, rec, "timestamp out of range".into());
        return Ok(());
    }
    if ts < retention_floor {
        reject(p, rec, "outside retained window".into());
        return Ok(());
    }
    let Value::Str(t) = rec.get("t") else { unreachable!("validated") };
    if t == b"user" {
        let person = mapper::person_record(rec)?;
        let at = ruby::time_at_float(ts);
        p.people.push((person, at));
        *p.counts.entry(t.clone()).or_insert(0) += 1;
        return Ok(());
    }
    let Some(mapped) = mapper::row_for(rec, &mut p.truncations)? else {
        reject(p, rec, "unknown type".into());
        return Ok(());
    };
    match p.tables.iter_mut().find(|(table, _)| *table == mapped.table) {
        Some((_, rows)) => rows.push(mapped.row),
        None => p.tables.push((mapped.table, vec![mapped.row])),
    }
    *p.counts.entry(t.clone()).or_insert(0) += 1;
    p.events_from = Some(p.events_from.map_or(ts, |f| f.min(ts)));
    p.events_to = Some(p.events_to.map_or(ts, |f| f.max(ts)));
    let t = std::str::from_utf8(t).unwrap_or("");
    if ROLLED_UP.contains(&t) {
        p.rollup_buckets.insert(bucket_for(ts_value));
    } else if t == "session" {
        p.session_buckets.insert(bucket_for(ts_value));
    }
    Ok(())
}

/// Batch#timestamp_in_range? up to the comparison.
fn timestamp_in_range(value: &Value) -> Result<f64, mapper::MapError> {
    if !value.is_numeric() {
        return Err(mapper::MapError::new("TypeError", "timestamp must be a number"));
    }
    let f = value.to_f().unwrap();
    if !f.is_finite() {
        return Err(mapper::MapError::new("RangeError", "timestamp is not finite"));
    }
    Ok(f)
}

/// Batch#bucket_for: the hour, as epoch seconds.
fn bucket_for(value: &Value) -> i64 {
    let micros = (value.to_f().unwrap() * 1_000_000.0).round() as i64;
    let seconds = micros.div_euclid(1_000_000);
    seconds - seconds.rem_euclid(3600)
}

fn reject(p: &mut Prepared, rec: &Value, reason: String) {
    // `rec["t"].scrub.first(64)`: an invalid UTF-8 type is stored with
    // U+FFFD for each invalid sequence (from_utf8_lossy is String#scrub),
    // so encoding the rejections cannot fail the batch.
    let record_type = match rec.get("t") {
        Value::Str(t) if matches!(rec, Value::Hash(_)) => Some(ruby::first_chars(String::from_utf8_lossy(t).as_bytes(), 64)),
        _ => None,
    };
    let reason = String::from_utf8_lossy(&ruby::first_chars(reason.as_bytes(), 200)).into_owned();
    p.rejections.push(Rejection { record_type, reason });
}

/// Ingest::Followups.build
pub fn followups_value(accepted: i64, exception_ids: &[i64], rollup: &IndexSet<i64>, sessions: &IndexSet<i64>) -> Value {
    let buckets = |set: &IndexSet<i64>| Value::Array(set.iter().map(|s| Value::from(crate::time::iso8601(*s))).collect());
    crate::rhash! {
        "version" => 1i64,
        "usage" => crate::rhash! { "events" => accepted },
        "group_exception_ids" => Value::Array(exception_ids.iter().map(|i| Value::Int(*i)).collect()),
        "rollup_buckets" => buckets(rollup),
        "session_buckets" => buckets(sessions),
    }
}

/// Batch#normalize_backpressure_factor
pub fn normalize_backpressure_factor(header: Option<&[u8]>) -> f64 {
    let Some(h) = header else { return 1.0 };
    match ruby::float(&Value::Str(h.to_vec())) {
        Ok(f) if f.is_finite() => f.max(1.0),
        _ => 1.0,
    }
}

/// String#to_i (base 10): leading whitespace, sign, digits with single
/// underscores; anything else ends the number.
pub fn string_to_i(s: &[u8]) -> Value {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && matches!(s[i], b'+' | b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut digits = String::new();
    while i < s.len() {
        match s[i] {
            b'0'..=b'9' => digits.push(s[i] as char),
            b'_' if !digits.is_empty() && s.get(i + 1).is_some_and(u8::is_ascii_digit) => {}
            _ => break,
        }
        i += 1;
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Value::Int(0);
    }
    let text = if neg { format!("-{digits}") } else { digits.to_string() };
    match text.parse::<i64>() {
        Ok(v) => Value::Int(v),
        Err(_) => Value::BigInt(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_i_matches_ruby() {
        let cases: &[(&str, Value)] = &[
            ("12abc", Value::Int(12)),
            (" 12", Value::Int(12)),
            ("-5", Value::Int(-5)),
            ("1_000", Value::Int(1000)),
            ("abc", Value::Int(0)),
            ("", Value::Int(0)),
            ("0x10", Value::Int(0)),
            ("+7", Value::Int(7)),
            ("1e3", Value::Int(1)),
            ("  -3x", Value::Int(-3)),
            ("1__0", Value::Int(1)),
            ("_1", Value::Int(0)),
            ("1_", Value::Int(1)),
            ("\n5", Value::Int(5)),
            ("99999999999999999999", Value::BigInt("99999999999999999999".into())),
        ];
        for (s, want) in cases {
            assert_eq!(&string_to_i(s.as_bytes()), want, "{s:?}.to_i");
        }
    }

    #[test]
    fn backpressure_factor() {
        for (h, want) in [("2.5", 2.5), ("abc", 1.0), ("Infinity", 1.0), ("0.5", 1.0), ("1e400", 1.0), ("", 1.0), (" 3 ", 3.0)] {
            assert_eq!(normalize_backpressure_factor(Some(h.as_bytes())), want, "{h:?}");
        }
        assert_eq!(normalize_backpressure_factor(None), 1.0);
    }
}
