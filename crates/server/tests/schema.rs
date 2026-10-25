//! A tenant file we create must be the file Rails creates: same sqlite_master
//! rows in the same order (Rails opens it, sees no pending migrations),
//! except the two `queries` indexes in schema::PARTIAL_INDEXES, which are
//! the same index made partial, and the nullable columns in
//! schema::ADDED_COLUMNS, appended the way ALTER TABLE ADD COLUMN appends
//! them (docs/DIFFERENCES.md).

use std::path::{Path, PathBuf};

use railwatch_server::schema;
use railwatch_server::storage::layout::{Tier, day_path, meta_path, open_or_create};

/// The DDL text SQLite keeps for `table` once ADDED_COLUMNS were appended.
fn with_added_columns(table: &str, sql: &str) -> String {
    let added: Vec<String> = schema::ADDED_COLUMNS.iter().filter(|(t, _, _)| *t == table).map(|(_, name, _)| format!(", \"{name}\" integer")).collect();
    format!("{}{})", sql.strip_suffix(')').expect("CREATE TABLE ends with )"), added.concat())
}

#[test]
fn created_tenant_has_the_exported_sqlite_master() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::create_tenant(&mut conn, "2026-10-13 00:00:00.000000").unwrap();
    let mut stmt = conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid").unwrap();
    let rows: Vec<(String, String, String, Option<String>)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect();
    let mut expected = schema::schema().ddl.clone();
    let (mut partial, mut widened) = (0, 0);
    for (ty, name, _, sql) in &mut expected {
        if let Some((_, clause)) = schema::PARTIAL_INDEXES.iter().find(|(n, _)| n == name) {
            *sql = sql.as_ref().map(|s| format!("{s}{clause}"));
            partial += 1;
        }
        if ty == "table" && schema::ADDED_COLUMNS.iter().any(|(t, _, _)| t == name) {
            *sql = sql.as_ref().map(|s| with_added_columns(name, s));
            widened += 1;
        }
    }
    assert_eq!(partial, 2, "both partial indexes exist in the exported Rails DDL");
    assert_eq!(widened, 1, "the added columns all belong to visits");
    assert_eq!(rows, expected, "every other row is Rails' own, in Rails' order");
    let versions: i64 = conn.query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0)).unwrap();
    assert_eq!(versions as usize, schema::schema().schema_migrations.len());
    let env: String = conn.query_row("SELECT value FROM ar_internal_metadata WHERE key = 'environment'", [], |r| r.get(0)).unwrap();
    assert_eq!(env, "production");
}

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("rw-schema-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tenant file as every release before ADDED_COLUMNS made it (the
/// exported Rails DDL, nothing appended), holding one visit.
fn old_tenant(path: &Path) {
    let mut c = rusqlite::Connection::open(path).unwrap();
    schema::create_tenant(&mut c, "2026-10-13 00:00:00.000000").unwrap();
    for (table, name, _) in schema::ADDED_COLUMNS {
        c.execute_batch(&format!("ALTER TABLE \"{table}\" DROP COLUMN \"{name}\"")).unwrap();
    }
    c.execute(
        "INSERT INTO visits (component, url, method, duration, status, occurred_at, ttfb) VALUES ('home/show', '/', 'GET', 1000, 'success', ?1, 900)",
        ["2026-10-13 12:00:00.000000"],
    )
    .unwrap();
}

fn visit_columns(c: &rusqlite::Connection) -> Vec<String> {
    c.prepare("SELECT name FROM pragma_table_info('visits')").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

#[test]
fn opening_a_file_from_an_older_release_adds_the_columns_and_keeps_its_rows() {
    let dir = Scratch::new("open");
    let path = dir.0.join("t.sqlite3");
    old_tenant(&path);
    assert!(!visit_columns(&rusqlite::Connection::open(&path).unwrap()).iter().any(|c| c == "redirect_ms"), "the old file lacks them");

    let c = railwatch_server::tenants::open_tenant(&path).unwrap();
    let cols = visit_columns(&c);
    for (_, name, _) in schema::ADDED_COLUMNS {
        assert!(cols.iter().any(|c| c == name), "{name} added: {cols:?}");
    }
    let (ttfb, redirect): (i64, Option<i64>) = c.query_row("SELECT ttfb, redirect_ms FROM visits", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((ttfb, redirect), (900, None), "an old row reads NULL in the new columns");
    assert_eq!(schema::add_missing_columns(&c).unwrap(), 0, "a second pass adds nothing");
}

/// The union reader's views are `SELECT *` over every day file, and SQLite
/// refuses a UNION ALL whose branches differ in width. Days written before
/// the upgrade sit beside days written after it, so the startup sweep must
/// widen every file before a reader attaches them.
#[test]
fn startup_widens_every_day_file_so_the_union_reader_still_reads_old_days() {
    let tenants = Scratch::new("days");
    let dir = tenants.0.join("shop-production");
    std::fs::create_dir_all(dir.join(Tier::Core.dir())).unwrap();
    old_tenant(&meta_path(&dir));
    old_tenant(&day_path(&dir, Tier::Core, "2026-10-13"));
    // A day the new release wrote: already widened.
    drop(open_or_create(&day_path(&dir, Tier::Core, "2026-10-14")).unwrap());

    let before = railwatch_server::dashboard::pool::open_tenant_reader(&tenants.0, "shop-production").unwrap();
    assert!(before.query_row("SELECT count(*) FROM visits", [], |r| r.get::<_, i64>(0)).is_err(), "mixed widths break the union: what the sweep prevents");
    drop(before);

    assert_eq!(railwatch_server::tenants::add_missing_columns_everywhere(&tenants.0), 2, "meta and the old day");
    assert_eq!(railwatch_server::tenants::add_missing_columns_everywhere(&tenants.0), 0, "idempotent");

    let reader = railwatch_server::dashboard::pool::open_tenant_reader(&tenants.0, "shop-production").unwrap();
    let n: i64 = reader.query_row("SELECT count(*) FROM visits WHERE redirect_ms IS NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "the old day's visit reads through the union, NULL in the new column");
}
