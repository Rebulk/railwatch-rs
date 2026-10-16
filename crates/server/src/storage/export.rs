//! Moving between the two layouts.
//!
//! - `split`: a single Rails-layout tenant file -> a per-day directory
//!   (`railwatch split`, also after `import-rails`). Rows keep their ids;
//!   meta's sqlite_sequence takes the file's, so new rows continue after them.
//! - `export`: a per-day directory -> one Rails-compatible file
//!   (`railwatch export`). Ids are unique across the directory, so rows are
//!   copied as they are; the sequence is meta's (the high-water marks).
//!
//! Both build the output under a temporary name and rename it into place,
//! so an interrupted run leaves nothing half made.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::layout::{self, Tier, day_column};

#[derive(Debug, Default)]
pub struct Moved {
    pub rows: i64,
    pub files: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

fn columns(conn: &Connection, db: &str, table: &str) -> rusqlite::Result<String> {
    let names: Vec<String> = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}', '{db}')"))?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", "))
}

fn uri(p: &Path) -> String {
    format!("file:{}?mode=ro", p.display().to_string().replace('?', "%3f").replace('#', "%23"))
}

fn size(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

fn tmp_dir_for(dir: &Path) -> PathBuf {
    dir.with_file_name(format!(".{}.splitting", dir.file_name().unwrap().to_string_lossy()))
}

/// Splits `file` (a gem-schema tenant) into a per-day directory `dir`.
pub fn split(file: &Path, dir: &Path) -> Result<Moved, String> {
    let e = |e: rusqlite::Error| e.to_string();
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let tmp = tmp_dir_for(dir);
    let _ = std::fs::remove_dir_all(&tmp);
    let mut out = Moved { bytes_in: size(file), ..Default::default() };

    let meta = layout::open_or_create(&layout::meta_path(&tmp)).map_err(e)?;
    meta.execute("ATTACH DATABASE ?1 AS src", [uri(file)]).map_err(e)?;
    meta.execute_batch("BEGIN").map_err(e)?;
    for t in layout::META_TABLES {
        let cols = columns(&meta, "src", t).map_err(e)?;
        out.rows += meta.execute(&format!("INSERT INTO main.\"{t}\" ({cols}) SELECT {cols} FROM src.\"{t}\""), []).map_err(e)? as i64;
    }
    // Every table's high-water mark: day tables' marks are meta's
    // sqlite_sequence (writer.rs), meta tables keep their own.
    meta.execute("DELETE FROM main.sqlite_sequence", []).map_err(e)?;
    meta.execute("INSERT INTO main.sqlite_sequence (name, seq) SELECT name, seq FROM src.sqlite_sequence", []).map_err(e)?;
    meta.execute_batch("COMMIT").map_err(e)?;

    // Which days hold rows, per tier.
    let mut days: Vec<(Tier, String)> = Vec::new();
    for t in layout::day_tables() {
        let col = day_column(t);
        let mut s = meta.prepare(&format!("SELECT DISTINCT substr({col}, 1, 10) FROM src.\"{t}\" WHERE {col} IS NOT NULL")).map_err(e)?;
        for d in s.query_map([], |r| r.get::<_, String>(0)).map_err(e)? {
            let d = d.map_err(e)?;
            if layout::day_of(&d) == Some(d.as_str()) && !days.iter().any(|(tt, dd)| *tt == Tier::of(t) && *dd == d) {
                days.push((Tier::of(t), d));
            }
        }
    }
    days.sort();
    let marks = super::writer::read_marks(&meta).map_err(e)?;
    drop(meta);

    for (tier, day) in &days {
        let path = layout::day_path(&tmp, *tier, day);
        let c = layout::open_or_create(&path).map_err(e)?;
        c.execute("ATTACH DATABASE ?1 AS src", [uri(file)]).map_err(e)?;
        c.execute_batch("BEGIN").map_err(e)?;
        let next = super::writer::shift_day(day, 1);
        for t in layout::day_tables().into_iter().filter(|t| Tier::of(t) == *tier) {
            let col = day_column(t);
            let cols = columns(&c, "src", t).map_err(e)?;
            out.rows += c
                .execute(
                    &format!("INSERT INTO main.\"{t}\" ({cols}) SELECT {cols} FROM src.\"{t}\" WHERE {col} >= ?1 AND {col} < ?2 ORDER BY id"),
                    [day, &next],
                )
                .map_err(e)? as i64;
            super::writer::set_seq(&c, t, marks.get(t).copied().unwrap_or(0)).map_err(e)?;
        }
        if *tier == Tier::Detail {
            c.execute("INSERT INTO logs_fts(rowid, message) SELECT id, message FROM logs", []).map_err(e)?;
        }
        c.execute_batch("COMMIT").map_err(e)?;
        c.execute("DETACH DATABASE src", []).map_err(e)?;
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").map_err(e)?;
        out.files += 1;
    }
    // Rows whose day column is NULL or not a date have no day file; count
    // them so the caller can tell (none exist in gem-written files).
    let check = Connection::open_with_flags(file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(e)?;
    let mut total = 0i64;
    for t in layout::META_TABLES.iter().copied().chain(layout::day_tables()) {
        total += check.query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get::<_, i64>(0)).map_err(e)?;
    }
    if total != out.rows {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("split copied {} of {total} rows (rows without a valid day?); nothing was written", out.rows));
    }
    std::fs::rename(&tmp, dir).map_err(|e| e.to_string())?;
    out.files += 1;
    out.bytes_out = layout::Layout::Days(dir.to_path_buf()).bytes();
    Ok(out)
}

/// Merges a per-day directory into one gem-schema file at `file`.
pub fn export(dir: &Path, file: &Path) -> Result<Moved, String> {
    let e = |e: rusqlite::Error| e.to_string();
    if file.exists() {
        return Err(format!("{} already exists", file.display()));
    }
    let tmp = file.with_file_name(format!(".{}.exporting", file.file_name().unwrap().to_string_lossy()));
    let _ = std::fs::remove_file(&tmp);
    let mut out = Moved::default();
    let c = layout::open_or_create(&tmp).map_err(e)?;
    // A single file is append-heavy and read by Rails: plain WAL as Rails
    // makes it. Bulk load without fsyncs; the rename publishes it.
    c.execute_batch("PRAGMA synchronous = OFF;").map_err(e)?;
    let meta = layout::meta_path(dir);
    out.bytes_in = layout::Layout::Days(dir.to_path_buf()).bytes();
    c.execute("ATTACH DATABASE ?1 AS src", [uri(&meta)]).map_err(e)?;
    c.execute_batch("BEGIN").map_err(e)?;
    for t in layout::META_TABLES {
        let cols = columns(&c, "src", t).map_err(e)?;
        out.rows += c.execute(&format!("INSERT INTO main.\"{t}\" ({cols}) SELECT {cols} FROM src.\"{t}\""), []).map_err(e)? as i64;
    }
    c.execute_batch("COMMIT").map_err(e)?;
    c.execute("DETACH DATABASE src", []).map_err(e)?;
    out.files += 1;
    for path in layout::all_files(dir).into_iter().skip(1) {
        let tier = if path.parent().is_some_and(|p| p.ends_with(Tier::Core.dir())) { Tier::Core } else { Tier::Detail };
        c.execute("ATTACH DATABASE ?1 AS src", [uri(&path)]).map_err(e)?;
        c.execute_batch("BEGIN").map_err(e)?;
        // Core files also hold exemplar child rows (detail tables).
        for t in layout::day_tables() {
            if tier == Tier::Detail && Tier::of(t) != Tier::Detail {
                continue;
            }
            let cols = columns(&c, "src", t).map_err(e)?;
            // OR IGNORE: an exemplar row copied to core while its detail day
            // still exists is the same row (same id) twice.
            out.rows += c.execute(&format!("INSERT OR IGNORE INTO main.\"{t}\" ({cols}) SELECT {cols} FROM src.\"{t}\""), []).map_err(e)? as i64;
        }
        c.execute_batch("COMMIT").map_err(e)?;
        c.execute("DETACH DATABASE src", []).map_err(e)?;
        out.files += 1;
    }
    c.execute_batch("BEGIN; INSERT INTO logs_fts(rowid, message) SELECT id, message FROM logs;").map_err(e)?;
    // Last, so inserts above cannot add rows: meta's marks, minus the 0
    // marks the writer seeds for every day table it touched (a Rails file
    // only has rows for tables that took an insert, plus the
    // export_deliveries row its migration leaves).
    c.execute("ATTACH DATABASE ?1 AS src", [uri(&meta)]).map_err(e)?;
    c.execute_batch(
        "DELETE FROM main.sqlite_sequence;
         INSERT INTO main.sqlite_sequence (name, seq) SELECT name, seq FROM src.sqlite_sequence WHERE seq > 0 OR name = 'export_deliveries';
         COMMIT; DETACH DATABASE src;",
    )
    .map_err(e)?;
    c.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA wal_checkpoint(TRUNCATE);").map_err(e)?;
    drop(c);
    std::fs::rename(&tmp, file).map_err(|e| e.to_string())?;
    out.bytes_out = size(file);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(c: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let n = c.prepare(&format!("SELECT * FROM \"{table}\" LIMIT 0")).unwrap().column_count();
        c.prepare(&format!("SELECT * FROM \"{table}\" ORDER BY 1"))
            .unwrap()
            .query_map([], |r| (0..n).map(|i| r.get(i)).collect())
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn split_then_export_round_trips_every_row() {
        let root = std::env::temp_dir().join(format!("rw-split-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&root).unwrap();
        let single = root.join("t.sqlite3");
        let c = crate::tenants::open_tenant(&single).unwrap();
        for (i, at) in ["2026-10-12 23:59:59.5", "2026-10-13 00:00:00", "2026-10-14 08:00:00"].iter().enumerate() {
            let id = i as i64 + 1;
            c.execute("INSERT INTO executions (execution_id, kind, name, occurred_at, duration) VALUES (?, 'request', 'GET /', ?, 1.5)", rusqlite::params![format!("e{id}"), at]).unwrap();
            c.execute("INSERT INTO queries (execution_id, sql, occurred_at, duration) VALUES (?, 'SELECT 1', ?, 0.5)", rusqlite::params![format!("e{id}"), at]).unwrap();
            c.execute("INSERT INTO logs (level, message, occurred_at) VALUES ('info', ?, ?)", rusqlite::params![format!("line {id}"), at]).unwrap();
        }
        c.execute("INSERT INTO logs_fts(rowid, message) SELECT id, message FROM logs", []).unwrap();
        c.execute("INSERT INTO rollups (record_type, group_hash, bucket, name, count) VALUES ('request', 'g', '2026-10-13 00:00:00', 'GET /', 1)", []).unwrap();
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        drop(c);

        let dir = root.join("t");
        let s = split(&single, &dir).unwrap();
        assert_eq!(s.rows, 10);
        assert_eq!(layout::days(&dir, Tier::Core), vec!["2026-10-12", "2026-10-13", "2026-10-14"]);
        // A new row continues after the single file's ids.
        let w = super::super::writer::DayWriter::open(&dir).unwrap();
        assert_eq!(super::super::writer::read_marks(w.meta_conn()).unwrap()["executions"], 3);
        drop(w);

        let back = root.join("back.sqlite3");
        let x = export(&dir, &back).unwrap();
        assert_eq!(x.rows, 10);
        let (a, b) = (Connection::open(&single).unwrap(), Connection::open(&back).unwrap());
        for t in ["executions", "queries", "logs", "rollups", "sqlite_sequence"] {
            assert_eq!(dump(&a, t), dump(&b, t), "{t}");
        }
        let hit: i64 = b.query_row("SELECT COUNT(*) FROM logs_fts WHERE logs_fts MATCH 'line'", [], |r| r.get(0)).unwrap();
        assert_eq!(hit, 3);
        let _ = std::fs::remove_dir_all(&root);
    }
}
