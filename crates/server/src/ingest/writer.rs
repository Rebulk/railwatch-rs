//! Port of Railwatch::Ingest::Writer (gem app/models/railwatch/ingest/
//! writer.rb) plus the rest of the telemetry transaction in
//! Ingest::Batch#write! (link_profiles!, Person.touch_all, the ingest_batches
//! ledger row, and the receipt). Statement for statement in the Ruby order,
//! so autoincrement ids come out the same:
//!
//! 1. tables in first-appearance order of their record class in the batch,
//!    query_shapes last; within a table, rows grouped by their key set (in
//!    first-appearance order of the set), each group one
//!    `INSERT OR IGNORE INTO t (cols) VALUES (?, ...)` stepped per row;
//! 2. new log ids copied into logs_fts, 500 per statement;
//! 3. executions.profile_id linked for this batch's profiles;
//! 4. people upserted;
//! 5. the ingest_batches row; 6. the ingest_receipts row (receipted only).

use std::collections::HashMap;

use md5::{Digest, Md5};
use rusqlite::{Connection, ToSql, params_from_iter, types::ToSqlOutput, types::ValueRef};

use crate::ingest::batch::{self, Prepared};
use crate::ingest::mapper::{Bind, Row};
use crate::rjson::{self, Value, ruby};
use crate::time;

/// Receipts outlive their delivery: IngestReceipt::RETENTION and
/// ADMISSION_WINDOW, in microseconds.
const RECEIPT_RETENTION: i64 = 8 * 86_400 * 1_000_000;
const ADMISSION_WINDOW: i64 = (7 * 86_400 + 300) * 1_000_000;

#[derive(Debug)]
pub struct Written {
    pub accepted: i64,
    pub exception_ids: Vec<i64>,
    pub committed_at_micros: Option<i64>,
    /// Ids of rows this batch inserted into the tables rollups read
    /// (rollup::ROLLED_TABLES), for the incremental rollup (Phase 2).
    pub rolled_ids: Vec<(&'static str, Vec<i64>)>,
}

#[derive(Debug)]
pub enum WriteError {
    /// The receipt's (producer, delivery) already exists: a concurrent
    /// delivery of the same id committed first (RecordNotUnique in Ruby).
    DuplicateReceipt,
    /// Batch::Expired
    Expired,
    /// The tenant file's write lock was not available within busy_timeout
    /// (ActiveRecord::StatementTimeout -> 503 "database busy").
    Busy,
    /// Anything else; the request answers 500 as Ruby's rescue does.
    Internal(String),
}

impl From<rusqlite::Error> for WriteError {
    fn from(e: rusqlite::Error) -> Self {
        match &e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::DatabaseBusy => WriteError::Busy,
            _ => WriteError::Internal(e.to_string()),
        }
    }
}

impl ToSql for Bind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Bind::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Bind::Int(i) => ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            Bind::Float(f) => ToSqlOutput::Borrowed(ValueRef::Real(*f)),
            Bind::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
            Bind::Blob(b) => ToSqlOutput::Borrowed(ValueRef::Blob(b)),
            Bind::Unbindable(class) => {
                return Err(rusqlite::Error::ToSqlConversionFailure(format!("can't prepare {class}").into()));
            }
        })
    }
}

/// Where a batch's rows go: the one tenant file of the Rails layout
/// (`Single`), or an environment's per-day files (storage::writer).
///
/// Rows of one table bound for one file are inserted as a run;
/// `begin_run`/`end_run` bracket each run so a per-day target can keep the
/// file's AUTOINCREMENT counter in step with the environment's.
pub trait Target {
    /// The file a row of `table` belongs in.
    fn route(&mut self, table: &'static str, row: &Row) -> Result<usize, WriteError>;
    /// The file for a row of a day table stamped `at` (ingest_batches).
    fn route_at(&mut self, table: &'static str, at: &str) -> Result<usize, WriteError>;
    fn conn(&self, file: usize) -> &Connection;
    /// The file that holds the long-lived tables (people, receipts).
    fn meta(&self) -> &Connection;
    fn begin_run(&mut self, _file: usize, _table: &'static str) -> Result<(), WriteError> {
        Ok(())
    }
    fn end_run(&mut self, _file: usize, _table: &'static str) -> Result<(), WriteError> {
        Ok(())
    }
    /// Batch#link_profiles! for these execution ids.
    fn link_profiles(&mut self, execution_ids: &[&str]) -> Result<(), WriteError>;
}

/// The Rails layout: everything in one file.
pub struct Single<'a>(pub &'a Connection);

impl Target for Single<'_> {
    fn route(&mut self, _: &'static str, _: &Row) -> Result<usize, WriteError> {
        Ok(0)
    }
    fn route_at(&mut self, _: &'static str, _: &str) -> Result<usize, WriteError> {
        Ok(0)
    }
    fn conn(&self, _: usize) -> &Connection {
        self.0
    }
    fn meta(&self) -> &Connection {
        self.0
    }
    fn link_profiles(&mut self, ids: &[&str]) -> Result<(), WriteError> {
        link_profiles_in(self.0, ids)
    }
}

/// Writes one prepared batch. The caller owns the transaction (a savepoint
/// per batch inside the tenant writer's transaction).
pub fn write(conn: &Connection, p: &mut Prepared, gc_floor_micros: i64) -> Result<Written, WriteError> {
    write_to(&mut Single(conn), p, gc_floor_micros)
}

/// `write` through any target.
pub fn write_to(t: &mut dyn Target, p: &mut Prepared, gc_floor_micros: i64) -> Result<Written, WriteError> {
    // --- Ingest::Writer#write!
    let shapes = with_query_shapes(&mut p.tables);
    let mut exception_ids = Vec::new();
    let mut log_ids = Vec::new();
    let mut accepted: i64 = 0;
    let mut rolled_ids = Vec::new();
    for (table, rows) in &p.tables {
        accepted += rows.len() as i64;
        let mut ids = Vec::new();
        let track = crate::rollup::ROLLED_TABLES.contains(table);
        insert_table(t, table, rows, &mut exception_ids, &mut log_ids, track.then_some(&mut ids))?;
        if track && !ids.is_empty() {
            rolled_ids.push((*table, ids));
        }
    }
    if !shapes.is_empty() {
        let rows: Vec<Row> = shapes;
        insert_table(t, "query_shapes", &rows, &mut Vec::new(), &mut Vec::new(), None)?;
    }
    index_logs(t, &log_ids)?;

    // --- Batch#link_profiles!
    link_profiles(t, p)?;

    // --- Telemetry::Person.touch_all
    touch_people(t.meta(), p)?;
    accepted += p.people.len() as i64;

    // --- Telemetry::IngestBatch.create!
    let dropped = match &p.meta.dropped_by_client {
        Value::Int(i) => *i,
        other => {
            return Err(WriteError::Internal(format!(
                "{} is out of range for ActiveRecord::ConnectionAdapters::SQLite3Adapter::SQLite3Integer with limit 8 bytes",
                String::from_utf8_lossy(&ruby::to_s(other))
            )));
        }
    };
    let bytes = match &p.meta.bytes {
        Value::Int(i) => *i,
        _ => return Err(WriteError::Internal("bytes out of range".into())),
    };
    let counts = Value::Hash(p.counts.iter().map(|(k, v)| (k.clone(), Value::Int(*v))).collect());
    let counts_json = json(&counts)?;
    let rejections_json = json(&batch::rejections_value(&p.rejections, 20))?;
    let gem_version = p.meta.gem_version.as_ref().map(|v| String::from_utf8_lossy(v).into_owned());
    let received_at = time::quoted_date_micros(p.meta.received_at_micros);
    let file = t.route_at("ingest_batches", &received_at)?;
    t.begin_run(file, "ingest_batches")?;
    t.conn(file).prepare_cached(
        "INSERT INTO \"ingest_batches\" (\"accepted\", \"backpressure_factor\", \"batch_id\", \"bytes\", \"counts_by_type\", \"dropped_by_client\", \"export_disposition\", \"export_record_count\", \"followups\", \"gem_version\", \"received_at\", \"rejected\", \"rejections\") VALUES (?, ?, NULL, ?, ?, ?, NULL, NULL, NULL, ?, ?, ?, ?)",
    )?
    .execute(rusqlite::params![
        accepted,
        p.meta.backpressure_factor,
        bytes,
        counts_json,
        dropped,
        gem_version,
        received_at,
        p.rejections.len() as i64,
        rejections_json,
    ])?;
    t.end_run(file, "ingest_batches")?;

    // --- Batch#write_receipt!
    let committed_at_micros = match &p.receipt {
        None => None,
        Some(r) => {
            let created = r.delivery_created_at.0 * 1_000_000 + r.delivery_created_at.1 as i64;
            if created < gc_floor_micros {
                return Err(WriteError::Expired);
            }
            let committed = time::now_micros();
            let retain_until = (committed + RECEIPT_RETENTION).max(created + ADMISSION_WINDOW);
            let ack = crate::rhash! {
                "accepted" => accepted,
                "rejected" => p.rejections.len(),
                "rejections" => batch::rejections_value(&p.rejections, 10),
            };
            let followups = batch::followups_value(accepted, &exception_ids, &p.rollup_buckets, &p.session_buckets);
            let at = |f: Option<f64>| {
                f.map(|f| {
                    let (s, u) = ruby::time_at_float(f);
                    time::quoted_date(s, u)
                })
            };
            let inserted = t
                .meta()
                .prepare_cached(
                    "INSERT INTO \"ingest_receipts\" (\"ack\", \"body_sha256\", \"committed_at\", \"delivery_created_at\", \"delivery_id\", \"events_from\", \"events_to\", \"followups\", \"followups_completed_at\", \"metadata_sha256\", \"policy_version\", \"producer_id\", \"record_count\", \"retain_until\") VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?)",
                )?
                .execute(rusqlite::params![
                    json(&ack)?,
                    r.body_sha256,
                    time::quoted_date_micros(committed),
                    time::quoted_date(r.delivery_created_at.0, r.delivery_created_at.1),
                    r.delivery_id,
                    at(p.events_from),
                    at(p.events_to),
                    json(&followups)?,
                    r.metadata_sha256,
                    r.policy_version,
                    r.producer_id,
                    p.record_count as i64,
                    time::quoted_date_micros(retain_until),
                ]);
            match inserted {
                Ok(_) => Some(committed),
                Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                    return Err(WriteError::DuplicateReceipt);
                }
                Err(e) => return Err(e.into()),
            }
        }
    };

    Ok(Written { accepted, exception_ids, committed_at_micros, rolled_ids })
}

/// What Active Record's json type stores: the json gem's output, unescaped.
fn json(v: &Value) -> Result<String, WriteError> {
    rjson::generate(v).map_err(|e| WriteError::Internal(format!("JSON::GeneratorError: {}", e.0)))
}

/// Writer#with_query_shapes: a query whose text digests to its group files
/// the text once in query_shapes and stores "" on the row.
fn with_query_shapes(tables: &mut [(&'static str, Vec<Row>)]) -> Vec<Row> {
    let mut shapes: Vec<Row> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    let Some((_, rows)) = tables.iter_mut().find(|(t, _)| *t == "queries") else { return shapes };
    for row in rows.iter_mut() {
        let get = |row: &Row, k: &str| row.iter().find(|(c, _)| *c == k).map(|(_, v)| v.clone());
        let Some(Bind::Text(group_hash)) = get(row, "group_hash") else { continue };
        let Some(Bind::Text(sql)) = get(row, "sql") else { continue };
        if ruby::blank(sql.as_bytes()) {
            continue;
        }
        // Railwatch::Record.group_hash(connection, sql): MD5 of parts.join(",").
        let connection = match get(row, "connection") {
            Some(Bind::Text(c)) => c,
            _ => String::new(),
        };
        let digest = Md5::digest(format!("{connection},{sql}").as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if hex != group_hash {
            continue;
        }
        if seen.insert(group_hash.clone(), ()).is_none() {
            shapes.push(vec![("group_hash", Bind::Text(group_hash.clone())), ("sql", Bind::Text(sql.clone()))]);
        }
        for (c, v) in row.iter_mut() {
            if *c == "sql" {
                *v = Bind::Text(String::new());
            }
        }
    }
    shapes
}

fn insert_table(
    t: &mut dyn Target,
    table: &'static str,
    rows: &[Row],
    exception_ids: &mut Vec<i64>,
    log_ids: &mut Vec<(usize, i64)>,
    mut rolled: Option<&mut Vec<i64>>,
) -> Result<(), WriteError> {
    if rows.is_empty() {
        return Ok(());
    }
    // rows.group_by(&:keys): groups in first-appearance order.
    let mut groups: Vec<(Vec<&'static str>, Vec<&Row>)> = Vec::new();
    for row in rows {
        let keys: Vec<&'static str> = row.iter().map(|(k, _)| *k).collect();
        match groups.iter_mut().find(|(k, _)| *k == keys) {
            Some((_, g)) => g.push(row),
            None => groups.push((keys, vec![row])),
        }
    }
    for (columns, group) in groups {
        let column_sql = columns.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
        let placeholders = vec!["?"; columns.len()].join(",");
        let sql = format!("INSERT OR IGNORE INTO \"{table}\" ({column_sql}) VALUES ({placeholders})");
        // One run per destination file, files in first-appearance order
        // (a single file for the Rails layout).
        let mut runs: Vec<(usize, Vec<&Row>)> = Vec::new();
        for row in group {
            let file = t.route(table, row)?;
            match runs.iter_mut().find(|(f, _)| *f == file) {
                Some((_, r)) => r.push(row),
                None => runs.push((file, vec![row])),
            }
        }
        for (file, run) in runs {
            t.begin_run(file, table)?;
            let conn = t.conn(file);
            let mut stmt = conn.prepare_cached(&sql)?;
            for row in run {
                let changed = stmt
                    .execute(params_from_iter(row.iter().map(|(_, v)| v)))
                    .map_err(|e| match e {
                        rusqlite::Error::ToSqlConversionFailure(m) => WriteError::Internal(m.to_string()),
                        e => e.into(),
                    })?;
                if changed > 0 {
                    let id = conn.last_insert_rowid();
                    match table {
                        "exceptions" => exception_ids.push(id),
                        "logs" => log_ids.push((file, id)),
                        _ => {}
                    }
                    if let Some(r) = rolled.as_deref_mut() {
                        r.push(id);
                    }
                }
            }
            drop(stmt);
            t.end_run(file, table)?;
        }
    }
    Ok(())
}

/// Writer#index_logs: logs_fts is external-content without triggers.
fn index_logs(t: &mut dyn Target, ids: &[(usize, i64)]) -> Result<(), WriteError> {
    let mut files: Vec<usize> = ids.iter().map(|(f, _)| *f).collect();
    files.dedup();
    files.sort_unstable();
    files.dedup();
    for file in files {
        let ids: Vec<i64> = ids.iter().filter(|(f, _)| *f == file).map(|(_, id)| *id).collect();
        for slice in ids.chunks(500) {
            let placeholders = vec!["?"; slice.len()].join(",");
            t.conn(file)
                .prepare_cached(&format!("INSERT INTO logs_fts(rowid, message) SELECT id, message FROM logs WHERE id IN ({placeholders})"))?
                .execute(params_from_iter(slice))?;
        }
    }
    Ok(())
}

fn link_profiles(t: &mut dyn Target, p: &Prepared) -> Result<(), WriteError> {
    let Some((_, rows)) = p.tables.iter().find(|(t, _)| *t == "profiles") else { return Ok(()) };
    let mut ids: Vec<&str> = Vec::new();
    for row in rows {
        if let Some((_, Bind::Text(id))) = row.iter().find(|(c, _)| *c == "execution_id")
            && !ids.contains(&id.as_str()) {
                ids.push(id);
            }
    }
    if ids.is_empty() {
        return Ok(());
    }
    t.link_profiles(&ids)
}

/// The Rails statement: each execution's profile_id = its newest profile.
pub fn link_profiles_in(conn: &Connection, ids: &[&str]) -> Result<(), WriteError> {
    let placeholders = vec!["?"; ids.len()].join(", ");
    conn.prepare_cached(&format!(
        "UPDATE \"executions\" SET profile_id = (SELECT MAX(profiles.id) FROM profiles WHERE profiles.execution_id = executions.execution_id) WHERE \"executions\".\"execution_id\" IN ({placeholders})"
    ))?
    .execute(params_from_iter(ids))?;
    Ok(())
}

/// Person.touch_all: one upsert, refs deduped keeping the latest timestamp
/// (the first of equal ones), blank strings stored as NULL.
fn touch_people(conn: &Connection, p: &Prepared) -> Result<(), WriteError> {
    struct P {
        r#ref: String,
        name: Option<String>,
        email: Option<String>,
        tenant: Option<String>,
        at: (i64, u32),
    }
    let presence = |s: &Option<String>| s.as_ref().filter(|v| !ruby::blank(v.as_bytes())).cloned();
    let mut rows: Vec<P> = Vec::new();
    for (person, at) in &p.people {
        let r#ref = person.id.clone().unwrap_or_default();
        if r#ref.is_empty() {
            continue;
        }
        let row = P { r#ref, name: presence(&person.name), email: presence(&person.email), tenant: presence(&person.tenant), at: *at };
        match rows.iter_mut().find(|r| r.r#ref == row.r#ref) {
            Some(existing) if row.at > existing.at => *existing = row,
            Some(_) => {}
            None => rows.push(row),
        }
    }
    for chunk in rows.chunks(1000) {
        let values = vec!["(?, ?, ?, ?, ?, ?, 0, 0)"; chunk.len()].join(", ");
        let sql = format!(
            "INSERT INTO \"people\" (\"ref\",\"name\",\"email\",\"app_tenant\",\"first_seen_at\",\"last_seen_at\",\"requests_count\",\"exceptions_count\") VALUES {values} ON CONFLICT (\"ref\") DO UPDATE SET name = COALESCE(excluded.name, people.name), email = COALESCE(excluded.email, people.email), app_tenant = COALESCE(excluded.app_tenant, people.app_tenant), first_seen_at = COALESCE(people.first_seen_at, excluded.first_seen_at), last_seen_at = MAX(people.last_seen_at, excluded.last_seen_at)"
        );
        let mut binds: Vec<Option<String>> = Vec::with_capacity(chunk.len() * 6);
        for r in chunk {
            let at = time::quoted_date(r.at.0, r.at.1);
            binds.extend([Some(r.r#ref.clone()), r.name.clone(), r.email.clone(), r.tenant.clone(), Some(at.clone()), Some(at)]);
        }
        conn.prepare_cached(&sql)?.execute(params_from_iter(binds))?;
    }
    Ok(())
}
