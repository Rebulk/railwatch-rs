//! A tenant file we create must be the file Rails creates: same sqlite_master
//! rows in the same order (Rails opens it, sees no pending migrations),
//! except the two `queries` indexes in schema::PARTIAL_INDEXES, which are
//! the same index made partial (docs/DIFFERENCES.md).

use railwatch_server::schema;

#[test]
fn created_tenant_has_the_exported_sqlite_master() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::create_tenant(&mut conn, "2026-10-13 00:00:00.000000").unwrap();
    let mut stmt = conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid").unwrap();
    let rows: Vec<(String, String, String, Option<String>)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(Result::unwrap).collect();
    let mut expected = schema::schema().ddl.clone();
    let mut partial = 0;
    for (_, name, _, sql) in &mut expected {
        if let Some((_, clause)) = schema::PARTIAL_INDEXES.iter().find(|(n, _)| n == name) {
            *sql = sql.as_ref().map(|s| format!("{s}{clause}"));
            partial += 1;
        }
    }
    assert_eq!(partial, 2, "both partial indexes exist in the exported Rails DDL");
    assert_eq!(rows, expected, "every other row is Rails' own, in Rails' order");
    let versions: i64 = conn.query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0)).unwrap();
    assert_eq!(versions as usize, schema::schema().schema_migrations.len());
    let env: String = conn.query_row("SELECT value FROM ar_internal_metadata WHERE key = 'environment'", [], |r| r.get(0)).unwrap();
    assert_eq!(env, "production");
}
