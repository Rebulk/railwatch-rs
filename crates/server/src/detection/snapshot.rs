//! IssueDetectionSnapshot.capture and DetectionSnapshotting#carry_detection
//! (railwatch-cloud app/models/issue_detection_snapshot.rb,
//! app/models/concerns/detection_snapshotting.rb).

use rusqlite::{Connection, params};

use crate::dashboard::rb;
use crate::rjson::{self, Value};

/// What a detector knows about one breach.
pub struct Capture<'a> {
    pub kind: &'a str,
    pub telemetry_type: &'a str,
    pub telemetry_group_hash: &'a str,
    pub target: &'a str,
    pub metric: &'a str,
    /// Window bounds, epoch microseconds.
    pub from: i64,
    pub to: i64,
    pub value: Value,
    pub limit: Value,
    /// A Hash, or Nil when the detector has no baseline.
    pub baseline: Value,
    /// A Hash.
    pub rule: Value,
    pub representative_from: Option<i64>,
}

/// `Hash#compact`
fn compact(v: Value) -> Value {
    match v {
        Value::Hash(h) => Value::Hash(h.into_iter().filter(|(_, v)| !v.is_nil()).collect()),
        other => other,
    }
}

/// IssueDetectionSnapshot#capture: the snapshot Hash.
pub fn capture(t: &Connection, c: &Capture<'_>) -> rusqlite::Result<Value> {
    let representatives = representatives(t, c)?;
    let measurement = compact(crate::rhash! { "value" => c.value.clone(), "limit" => c.limit.clone() });
    let minutes = rb::round(rb::time_sub(c.to, c.from) / 60.0, 2);
    let snapshot = crate::rhash! {
        "schema_version" => 1i64, "kind" => c.kind, "telemetry_type" => c.telemetry_type,
        "telemetry_group_hash" => c.telemetry_group_hash, "target" => c.target, "metric" => c.metric,
        "unit" => super::unit(c.metric).unwrap_or("value"),
        "window" => crate::rhash! { "from" => rb::iso8601_6(c.from), "to" => rb::iso8601_6(c.to), "minutes" => minutes },
        "measurement" => measurement,
        "baseline" => compact(c.baseline.clone()),
        "rule" => compact(c.rule.clone()),
        "representative_records" => Value::Array(representatives),
    };
    Ok(compact(snapshot))
}

/// representative_scope(...).order(duration: :desc).limit(REPRESENTATIVE_LIMIT),
/// as record_json references. The statement is Active Record's, so ties in
/// duration come out in the order SQLite gives Rails.
fn representatives(t: &Connection, c: &Capture<'_>) -> rusqlite::Result<Vec<Value>> {
    let Some((table, execution)) = super::table_for(c.telemetry_type) else {
        return Err(rusqlite::Error::InvalidParameterName(format!("unsupported detection telemetry type: {}", c.telemetry_type)));
    };
    let from = c.representative_from.unwrap_or(c.from);
    // A frozen whole-second upper bound serializes without the fraction;
    // `...(to + 1.second)` keeps a record at that exact instant.
    let to = c.to + 1_000_000;
    let source = if execution { "kind" } else { "execution_source" };
    let kind = if execution { format!(" AND \"{table}\".\"kind\" = ?4") } else { String::new() };
    let sql = format!(
        "SELECT \"{table}\".\"id\", \"{table}\".\"group_hash\", \"{table}\".\"execution_id\", \"{table}\".\"{source}\" FROM \"{table}\" \
         WHERE \"{table}\".\"group_hash\" = ?1 AND \"{table}\".\"occurred_at\" >= ?2 AND \"{table}\".\"occurred_at\" < ?3{kind} \
         ORDER BY \"{table}\".\"duration\" DESC LIMIT {}",
        super::REPRESENTATIVE_LIMIT
    );
    let mut s = t.prepare(&crate::storage::reader::strip_index_hints(&sql))?;
    let row = |r: &rusqlite::Row<'_>| {
        Ok(crate::rhash! {
            "record_type" => c.telemetry_type, "record_id" => r.get::<_, i64>(0)?, "group_hash" => r.get::<_, Option<String>>(1)?,
            "execution_id" => r.get::<_, Option<String>>(2)?, "execution_source" => r.get::<_, Option<String>>(3)?,
        })
    };
    let (f, tt) = (crate::time::quoted_date_micros(from), crate::time::quoted_date_micros(to));
    if execution {
        s.query_map(params![c.telemetry_group_hash, f, tt, c.telemetry_type], row)?.collect()
    } else {
        s.query_map(params![c.telemetry_group_hash, f, tt], row)?.collect()
    }
}

/// `issue.update!(sample: issue.sample.merge("detection" => detection))`.
pub fn store(c: &Connection, issue_id: i64, sample: &Value, detection: Value) -> rusqlite::Result<()> {
    let mut merged = match sample {
        Value::Hash(h) => h.clone(),
        _ => rjson::Hash::new(),
    };
    merged.insert(b"detection".to_vec(), detection);
    c.prepare_cached("UPDATE issues SET sample = ?, updated_at = ? WHERE id = ?")?.execute(params![
        rjson::generate(&Value::Hash(merged)).unwrap_or_else(|_| "{}".into()),
        crate::grouping::issue::now_db(),
        issue_id
    ])?;
    Ok(())
}

/// DetectionSnapshotting#carry_detection: a newly open issue gets a fresh
/// snapshot; otherwise the one the previous sample held (record_occurrence!
/// replaced the sample wholesale) is carried forward. `previous` is the
/// issue's sample before record_occurrence!.
pub fn carry(
    c: &Connection,
    issue: &crate::grouping::issue::Issue,
    newly_open: bool,
    previous: Option<&Value>,
    fresh: impl FnOnce() -> rusqlite::Result<Value>,
) -> rusqlite::Result<()> {
    let detection = if newly_open { fresh()? } else { previous.map(|p| p.get("detection").clone()).unwrap_or(Value::Nil) };
    if detection.truthy() {
        store(c, issue.id, &issue.sample, detection)?;
    }
    Ok(())
}
