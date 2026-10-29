//! The telemetry schema, exactly as the Rails app migrates it.
//!
//! `schema_export.json` was dumped from a tenant file railwatch-cloud @ 464f0fd
//! (gem 0.9.0) created and migrated (bench/export/export_schema.rb):
//! - `ddl`: sqlite_master rows in creation order, replayed verbatim here so a
//!   file we create is the file Rails would have created;
//! - `columns`: per table [name, AR type, limit, null, default], which is what
//!   Ingest::Mapper casts with (Mapper.columns_for);
//! - `text_limits`: Mapper::TEXT_LIMITS;
//! - `schema_migrations`: every version, so Rails sees nothing pending.

use std::collections::HashMap;
use std::sync::OnceLock;

use rusqlite::Connection;

const EXPORT: &str = include_str!("schema/schema_export.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    String,
    Text,
    Integer,
    Float,
    Datetime,
    Boolean,
    Json,
    Binary,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
    pub limit: Option<usize>,
    pub null: bool,
    /// The column default as Active Record casts it, pre-serialized for a
    /// bind: JSON defaults stay JSON text ("{}", "[]"), booleans 0/1.
    pub default: Option<DefaultValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

pub struct Schema {
    pub ddl: Vec<(String, String, String, Option<String>)>,
    pub tables: HashMap<String, Vec<Column>>,
    pub text_limits: HashMap<String, usize>,
    pub schema_migrations: Vec<String>,
}

pub fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(load)
}

fn load() -> Schema {
    let raw: serde_json::Value = serde_json::from_str(EXPORT).expect("embedded schema_export.json");
    let ddl = raw["ddl"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let r = r.as_array().unwrap();
            let s = |i: usize| r[i].as_str().map(str::to_string);
            (s(0).unwrap(), s(1).unwrap(), s(2).unwrap(), s(3))
        })
        .collect();
    let mut tables = HashMap::new();
    for (table, cols) in raw["columns"].as_object().unwrap() {
        let cols: Vec<Column> = cols
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let c = c.as_array().unwrap();
                let ty = match c[1].as_str().unwrap() {
                    "string" => ColType::String,
                    "text" => ColType::Text,
                    "integer" => ColType::Integer,
                    "float" => ColType::Float,
                    "datetime" => ColType::Datetime,
                    "boolean" => ColType::Boolean,
                    "json" => ColType::Json,
                    "binary" => ColType::Binary,
                    other => panic!("unknown column type {other}"),
                };
                let default = match &c[4] {
                    serde_json::Value::Null => None,
                    serde_json::Value::Bool(b) => Some(DefaultValue::Bool(*b)),
                    serde_json::Value::Number(n) if ty == ColType::Float => Some(DefaultValue::Float(n.as_f64().unwrap())),
                    serde_json::Value::Number(n) => Some(DefaultValue::Int(n.as_i64().unwrap())),
                    serde_json::Value::String(s) => Some(DefaultValue::Text(s.clone())),
                    other => panic!("unexpected default {other}"),
                };
                Column { name: c[0].as_str().unwrap().to_string(), ty, limit: c[2].as_u64().map(|l| l as usize), null: c[3].as_bool().unwrap(), default }
            })
            .collect();
        tables.insert(table.clone(), cols);
    }
    for (table, name, ty) in ADDED_COLUMNS {
        tables.get_mut(*table).expect("added column's table is exported").push(Column {
            name: name.to_string(),
            ty: *ty,
            limit: (*ty == ColType::String).then_some(ADDED_STRING_LIMIT),
            null: true,
            default: None,
        });
    }
    let text_limits = raw["text_limits"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap() as usize)).collect();
    let schema_migrations = raw["schema_migrations"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    Schema { ddl, tables, text_limits, schema_migrations }
}

impl Schema {
    pub fn columns(&self, table: &str) -> &[Column] {
        &self.tables[table]
    }

    pub fn column(&self, table: &str, name: &str) -> &Column {
        self.columns(table).iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no column {table}.{name}"))
    }
}

/// Columns this server adds to the exported Rails tables (docs/DIFFERENCES.md):
/// nullable, no default, appended with `ALTER TABLE ... ADD COLUMN`, so Rails
/// still opens the file and a row without them reads NULL. Every tenant file
/// must have all of them before a union reader attaches it: its views are
/// `SELECT *` over every day file, and SQLite refuses a UNION ALL whose
/// branches differ in column count. New files get them in `create_tenant`,
/// existing ones in `add_missing_columns` (every `tenants::open_tenant`, and
/// `tenants::add_missing_columns_everywhere` when the server starts).
///
/// visits: the initial load's TTFB split into phases (gem 1.2's browser
/// client). `redirect_count` is NULL when the browser showed no redirect,
/// which includes a cross-origin chain it hid; `redirect_ms` still holds it.
/// `session_id` and `tab_id`: the browser session the visit belongs to and
/// the tab it happened in (docs/sessions.md), NULL from a client without them.
pub const ADDED_COLUMNS: &[(&str, &str, ColType)] = &[
    ("visits", "redirect_ms", ColType::Integer),
    ("visits", "redirect_count", ColType::Integer),
    ("visits", "connect_ms", ColType::Integer),
    ("visits", "server_ms", ColType::Integer),
    ("visits", "session_id", ColType::String),
    ("visits", "tab_id", ColType::String),
];

/// The length of an added string column (varchar), as the `sessions`
/// table's own `session_id` is: the gem sends ids of 64 characters at most.
pub const ADDED_STRING_LIMIT: usize = 64;

/// Adds every ADDED_COLUMNS column `conn`'s main database lacks. Returns
/// how many it added (0 on a file that already has them all).
pub fn add_missing_columns(conn: &Connection) -> rusqlite::Result<usize> {
    let mut added = 0;
    for (table, name, ty) in ADDED_COLUMNS {
        let present: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)", [*table, *name], |r| r.get(0))?;
        if !present {
            let sql_type = match ty {
                ColType::Integer => "integer".to_string(),
                ColType::String => format!("varchar({ADDED_STRING_LIMIT})"),
                other => unreachable!("no added column is {other:?}"),
            };
            conn.execute_batch(&format!("ALTER TABLE \"{table}\" ADD COLUMN \"{name}\" {sql_type}"))?;
            added += 1;
        }
    }
    Ok(added)
}

/// Indexes this server adds to the exported Rails tables (docs/DIFFERENCES.md),
/// created after Rails' own, so Rails opens the file with 0 pending migrations.
///
/// `idx_executions_requests`: what the gem keeps of a request in `detail`
/// (client address, user agent, path, the security flags, the redirect and
/// response size) as index expressions, beside the columns a request scan
/// reads, for requests only. The security reads (filter_query::requests,
/// request_scan::scan) name it with INDEXED BY and read every field from the
/// index, never parsing `detail` (docs/PERF.md, "Security reads"). The
/// expressions are the text those reads use: SQLite matches an expression
/// to an index column by its parse tree, so one written differently would
/// read the row's JSON again (request_scan's tests check none does).
///
/// `idx_executions_sessions` and `idx_visits_sessions`: one browser
/// session's requests and visits (docs/sessions.md), so a journey is an
/// index seek, not a scan of every day. Partial on the session id being
/// set: a request or visit without one (every row before the gem sent it,
/// a job, an API client without the cookie) is not in them at all. The
/// requests' carries the tab and what session discovery counts (status,
/// user), so listing a window's sessions reads only the index
/// (docs/PERF.md, "Session journeys").
///
/// New files get them in `create_tenant`. Existing ones get them from
/// `tenants::add_missing_indexes_in_background` after the server starts:
/// building one takes the file's write lock, and a file without it answers
/// the same through `json_extract` (storage::reader builds each day file's
/// branch of the hinted view with the hint only where the index exists).
pub const ADDED_INDEXES: &[(&str, &str)] = &[
    (
        "idx_executions_requests",
        "CREATE INDEX \"idx_executions_requests\" ON \"executions\" (\"occurred_at\", \"status\", \"method\", \"name\", \"group_hash\", \"duration\", \
         \"user_ref\", \"app_tenant\", \"execution_id\", \"exception_preview\", json_extract(\"detail\", '$.ip'), json_extract(\"detail\", '$.user_agent'), \
         json_extract(\"detail\", '$.path'), json_extract(\"detail\", '$.headers.\"Cf-Ipcountry\"'), json_type(\"detail\", '$.rate_limited'), \
         json_extract(\"detail\", '$.rate_limited.name'), json_extract(\"detail\", '$.halted_callback'), \
         json_array_length(\"detail\", '$.unpermitted_parameters'), json_extract(\"detail\", '$.unpermitted_parameters'), \
         json_extract(\"detail\", '$.redirect_to'), json_extract(\"detail\", '$.response_size')) WHERE kind = 'request'",
    ),
    (
        "idx_executions_sessions",
        "CREATE INDEX \"idx_executions_sessions\" ON \"executions\" (json_extract(\"detail\", '$.session_id'), \"occurred_at\", \
         json_extract(\"detail\", '$.tab_id'), \"status\", \"user_ref\") WHERE kind = 'request' AND json_extract(\"detail\", '$.session_id') IS NOT NULL",
    ),
    ("idx_visits_sessions", "CREATE INDEX \"idx_visits_sessions\" ON \"visits\" (\"session_id\", \"occurred_at\") WHERE session_id IS NOT NULL"),
];

/// The request index's name (ADDED_INDEXES).
pub const REQUEST_INDEX: &str = "idx_executions_requests";
/// The session indexes' names (ADDED_INDEXES).
pub const SESSION_INDEX: &str = "idx_executions_sessions";
pub const VISIT_SESSION_INDEX: &str = "idx_visits_sessions";

/// Creates every ADDED_INDEXES index `conn`'s main database lacks. Returns
/// how many it created.
pub fn add_missing_indexes(conn: &Connection) -> rusqlite::Result<usize> {
    let mut added = 0;
    for (name, sql) in ADDED_INDEXES {
        if !has_index(conn, "main", name)? {
            conn.execute_batch(sql)?;
            added += 1;
        }
    }
    Ok(added)
}

/// Whether schema `db` (main, or an attached alias) has index `name`.
pub fn has_index(conn: &Connection, db: &str, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(&format!("SELECT EXISTS (SELECT 1 FROM \"{db}\".sqlite_master WHERE type = 'index' AND name = ?1)"), [name], |r| r.get(0))
}

/// ar_internal_metadata.schema_sha1 in a tenant Rails created (railwatch-cloud @ 464f0fd).
pub const SCHEMA_SHA1: &str = "753f328f08a840b845caf27796ec5dd56d02db42";

/// The pragmas database.yml sets on every telemetry connection.
pub const CONNECTION_PRAGMAS: &str = "PRAGMA journal_mode = wal;
PRAGMA synchronous = normal;
PRAGMA mmap_size = 134217728;
PRAGMA journal_size_limit = 67108864;
PRAGMA cache_size = 2000;
PRAGMA foreign_keys = ON;";

pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.execute_batch(CONNECTION_PRAGMAS)?;
    // Bench knob, unset by default (Rails parity): RAILWATCH_SQLITE_CACHE_KIB overrides cache_size.
    if let Some(kib) = std::env::var("RAILWATCH_SQLITE_CACHE_KIB").ok().and_then(|v| v.parse::<i64>().ok()) {
        conn.execute_batch(&format!("PRAGMA cache_size = -{kib};"))?;
    }
    // Bench diagnostic only, never for real data: RAILWATCH_SQLITE_SYNC=off isolates fsync cost.
    if std::env::var("RAILWATCH_SQLITE_SYNC").as_deref() == Ok("off") {
        conn.execute_batch("PRAGMA synchronous = off;")?;
    }
    Ok(())
}

/// The one deliberate departure from the exported DDL, for NEW tenant
/// files only (docs/DIFFERENCES.md, research/IMPROVEMENTS.md 1(iii)): the
/// two `queries` indexes no gem 0.9.1 read uses are created partial, `WHERE
/// app_tenant IS NOT NULL`. Same names and columns, so Rails opens the file
/// and the gem's plans are unchanged; they cost ~3% of a busy tenant's
/// file and a B-tree insert per query row for nothing. Files that already
/// exist (Rails-made, imported, older Rust ones) are never altered.
pub const PARTIAL_INDEXES: [(&str, &str); 2] =
    [("idx_queries_tenant_duration_cursor", " WHERE app_tenant IS NOT NULL"), ("idx_queries_duration_cursor", " WHERE app_tenant IS NOT NULL")];

/// Creates the telemetry tables in an empty database the way the Rails
/// migrations leave them: every sqlite_master row replayed in creation order
/// (the implicit ones -- sqlite_sequence, FTS5 shadow tables, autoindexes --
/// come into being on their own), then schema_migrations and the
/// ar_internal_metadata environment row. PARTIAL_INDEXES are the exception.
pub fn create_tenant(conn: &mut Connection, now: &str) -> rusqlite::Result<()> {
    let s = schema();
    let tx = conn.transaction()?;
    let implicit = |name: &str| name.starts_with("sqlite_") || (name.starts_with("logs_fts_") && name != "logs_fts");
    for (_ty, name, _tbl, sql) in &s.ddl {
        let Some(sql) = sql else { continue };
        if implicit(name) {
            continue;
        }
        match PARTIAL_INDEXES.iter().find(|(n, _)| n == name) {
            Some((_, clause)) => tx.execute_batch(&format!("{sql}{clause}"))?,
            None => tx.execute_batch(sql)?,
        }
    }
    {
        let mut stmt = tx.prepare("INSERT INTO schema_migrations (version) VALUES (?)")?;
        for v in &s.schema_migrations {
            stmt.execute([v])?;
        }
    }
    // Rails' internal metadata, as a migrated Rails tenant has it. Rails
    // reads `environment` (protected-environment checks); `schema_sha1` is
    // the digest of the schema file the gem's tenants are created from, the
    // value every tenant made by railwatch-cloud @ 464f0fd carries.
    tx.execute(
        "INSERT INTO ar_internal_metadata (key, value, created_at, updated_at) VALUES ('environment', 'production', ?1, ?1), ('schema_sha1', ?2, ?1, ?1)",
        [now, SCHEMA_SHA1],
    )?;
    // A fresh Rails tenant carries this sqlite_sequence row: a migration
    // inserted into and emptied export_deliveries.
    tx.execute("INSERT INTO sqlite_sequence (name, seq) VALUES ('export_deliveries', 0)", [])?;
    add_missing_columns(&tx)?;
    add_missing_indexes(&tx)?;
    tx.commit()
}
