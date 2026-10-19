//! Reading a per-day tenant as if it were one file.
//!
//! The connection's main database is an empty in-memory one. meta and every
//! day file are ATTACHed read-only, and each gem table gets a TEMP view of
//! the same name that is the UNION ALL of that table in every file that can
//! hold its rows. The dashboard's SQL runs unchanged against the views:
//! SQLite pushes WHERE terms (and usually ORDER BY ... LIMIT) into each
//! branch, so every file uses its own indexes. `logs_fts` is a view too
//! (`rowid`, the hidden table column and `message`), which keeps MATCH,
//! `rowid IN (...)` and snippet() working.
//!
//! Detail tables read the detail files *and* the core files, which hold the
//! exemplars' child rows once a detail day is gone (exemplars.rs).
//!
//! INDEXED BY / NOT INDEXED cannot name a view; `strip_index_hints` removes
//! them, and each branch's planner picks its own index.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::layout::{self, Tier};

/// Which files a reader has attached, to notice a new or deleted day.
pub type Attached = Vec<PathBuf>;

pub fn open(dir: &Path) -> rusqlite::Result<(Connection, Attached)> {
    let conn = Connection::open_with_flags("file::memory:", OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA temp_store = MEMORY;")?;
    let attached = attach_all(&conn, dir)?;
    Ok((conn, attached))
}

/// The files a reader of `dir` should have attached right now.
pub fn wanted(dir: &Path) -> Attached {
    layout::all_files(dir)
}

/// Re-attaches if the directory's files changed since `attached`.
pub fn refresh(conn: &Connection, dir: &Path, attached: &mut Attached) -> rusqlite::Result<bool> {
    let now = wanted(dir);
    if now == *attached {
        return Ok(false);
    }
    conn.flush_prepared_statement_cache();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM pragma_database_list WHERE name NOT IN ('main', 'temp')")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let views: Vec<String> =
        conn.prepare("SELECT name FROM temp.sqlite_master WHERE type = 'view'")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for v in views {
        conn.execute_batch(&format!("DROP VIEW temp.\"{v}\""))?;
    }
    for n in names {
        conn.execute_batch(&format!("DETACH DATABASE \"{n}\""))?;
    }
    *attached = attach_all(conn, dir)?;
    Ok(true)
}

fn attach_all(conn: &Connection, dir: &Path) -> rusqlite::Result<Attached> {
    let meta = layout::meta_path(dir);
    let core = layout::days(dir, Tier::Core);
    let detail = layout::days(dir, Tier::Detail);
    let uri = |p: &Path| format!("file:{}?mode=ro", p.display().to_string().replace('?', "%3f").replace('#', "%23"));
    conn.execute("ATTACH DATABASE ?1 AS m", [uri(&meta)])?;
    let mut attached = vec![meta];
    let mut core_aliases = Vec::new();
    let mut detail_aliases = Vec::new();
    for (i, d) in core.iter().enumerate() {
        let p = layout::day_path(dir, Tier::Core, d);
        conn.execute(&format!("ATTACH DATABASE ?1 AS c{i}"), [uri(&p)])?;
        core_aliases.push(format!("c{i}"));
        attached.push(p);
    }
    for (i, d) in detail.iter().enumerate() {
        let p = layout::day_path(dir, Tier::Detail, d);
        conn.execute(&format!("ATTACH DATABASE ?1 AS d{i}"), [uri(&p)])?;
        detail_aliases.push(format!("d{i}"));
        attached.push(p);
    }
    // mmap per attached file, as the single-file reader does (pool.rs).
    for a in std::iter::once(&"m".to_string()).chain(&core_aliases).chain(&detail_aliases) {
        conn.execute_batch(&format!("PRAGMA \"{a}\".mmap_size = 17179869184; PRAGMA \"{a}\".cache_size = -4000;"))?;
    }
    for table in tables() {
        let sources: Vec<&String> = match Tier::of(table) {
            Tier::Meta => vec![],
            Tier::Core => core_aliases.iter().collect(),
            Tier::Detail => detail_aliases.iter().chain(&core_aliases).collect(),
        };
        let sql = if Tier::of(table) == Tier::Meta || sources.is_empty() {
            // No day file yet: the (empty) table in meta keeps the columns.
            format!("CREATE TEMP VIEW \"{table}\" AS SELECT * FROM m.\"{table}\"")
        } else {
            let parts: Vec<String> = sources.iter().map(|a| format!("SELECT * FROM {a}.\"{table}\"")).collect();
            format!("CREATE TEMP VIEW \"{table}\" AS {}", parts.join(" UNION ALL "))
        };
        conn.execute_batch(&sql)?;
    }
    // A hinted view per hint the dashboard uses: the same union with the
    // hint on every branch, so the per-day plan is the one the hint picks
    // in a single file (see `rewrite_index_hints`).
    for (table, hint) in HINTED {
        let sources: Vec<&String> = match Tier::of(table) {
            Tier::Meta => vec![],
            Tier::Core => core_aliases.iter().collect(),
            Tier::Detail => detail_aliases.iter().chain(&core_aliases).collect(),
        };
        let h = hint.map_or("NOT INDEXED".to_string(), |i| format!("INDEXED BY {i}"));
        let parts: Vec<String> = if sources.is_empty() {
            vec![format!("SELECT * FROM m.\"{table}\" {h}")]
        } else {
            sources.iter().map(|a| format!("SELECT * FROM {a}.\"{table}\" {h}")).collect()
        };
        conn.execute_batch(&format!("CREATE TEMP VIEW \"{}\" AS {}", hinted_view(table, *hint), parts.join(" UNION ALL ")))?;
    }
    let fts_sources: Vec<&String> = detail_aliases.iter().chain(&core_aliases).collect();
    let fts = if fts_sources.is_empty() {
        "SELECT rowid AS rowid, \"logs_fts\" AS \"logs_fts\", message FROM m.\"logs_fts\"".to_string()
    } else {
        fts_sources
            .iter()
            .map(|a| format!("SELECT rowid AS rowid, \"logs_fts\" AS \"logs_fts\", message FROM {a}.\"logs_fts\""))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
    };
    conn.execute_batch(&format!("CREATE TEMP VIEW \"logs_fts\" AS {fts}"))?;
    Ok(attached)
}

/// Every gem table a page can read (meta and day tables alike).
fn tables() -> Vec<&'static str> {
    let mut t: Vec<&'static str> = layout::META_TABLES.to_vec();
    t.extend(layout::day_tables());
    t
}

/// A union reader's main database is in memory (no file name).
pub fn is_union(conn: &Connection) -> bool {
    conn.path().is_none_or(str::is_empty)
}

/// One attached day file a table's rows can be in: its schema alias (`c3`,
/// `d0`) and its day (`YYYY-MM-DD`, from the file name).
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    pub alias: String,
    pub day: String,
    pub tier: Tier,
}

/// The attached day files `table` reads from, as its view unions them
/// (detail tables: the detail files, then the core files). Empty for a meta
/// table, or before any day file exists.
pub fn branches(conn: &Connection, table: &str) -> rusqlite::Result<Vec<Branch>> {
    let tier = Tier::of(table);
    if tier == Tier::Meta {
        return Ok(vec![]);
    }
    let mut files: Vec<Branch> = conn
        .prepare_cached("SELECT name, file FROM pragma_database_list")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| {
            let (alias, file) = match r {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let t = match alias.as_bytes().first() {
                Some(b'c') => Tier::Core,
                Some(b'd') => Tier::Detail,
                _ => return None,
            };
            if !alias[1..].bytes().all(|b| b.is_ascii_digit()) || (t == Tier::Detail && tier != Tier::Detail) {
                return None;
            }
            let day = Path::new(&file).file_stem()?.to_str()?.to_string();
            Some(Ok(Branch { alias, day, tier: t }))
        })
        .collect::<rusqlite::Result<_>>()?;
    files.sort_by_key(|b| (b.tier != Tier::Detail, b.alias[1..].parse::<usize>().unwrap_or(0)));
    Ok(files)
}

/// `cond_sql` for one day file: `logs_fts` (filter_query's full-text term)
/// names that file's own index. A log's FTS entry is always in the file that
/// holds the log (ingest, exemplars and export index per file), so the
/// pairing returns the same rows, and the whole union of FTS indexes is not
/// re-run inside every file's branch.
pub fn pair_fts(cond_sql: &str, alias: &str) -> String {
    cond_sql.replace("FROM logs_fts WHERE", &format!("FROM {alias}.logs_fts WHERE"))
}

/// A statement over `table` as one branch per day file it reads,
/// `(b1) UNION ALL (b2) ...`: `branch(file, covered, cond)` writes each, with
/// `cond` the WHERE clause whose FTS term is paired to that file and
/// `covered` when [lo, hi] (epoch micros, the statement's own occurred_at
/// bounds) spans the file's whole day. `binds` repeat per branch. None when
/// there is no day file (the view, over meta's empty table, is as cheap).
///
/// Every file is read, so the rows are the view's rows whatever a file
/// holds; what this buys is that a page's ORDER BY ... LIMIT and GROUP BY run
/// inside each file on its own indexes, instead of over the union's rows.
/// `covered` only picks a plan (an index hint); it never drops a row.
pub fn per_file(
    conn: &Connection,
    table: &str,
    (lo, hi): (i64, i64),
    cond_sql: &str,
    binds: &[rusqlite::types::Value],
    branch: impl Fn(&Branch, bool, &str) -> String,
) -> rusqlite::Result<Option<(String, Vec<rusqlite::types::Value>)>> {
    let files = branches(conn, table)?;
    if files.is_empty() {
        return Ok(None);
    }
    let (from_day, to_day) = (crate::time::quoted_date_micros(lo)[..10].to_string(), crate::time::quoted_date_micros(hi)[..10].to_string());
    let mut all = Vec::with_capacity(binds.len() * files.len());
    let parts: Vec<String> = files
        .iter()
        .map(|f| {
            all.extend_from_slice(binds);
            format!("SELECT * FROM ({})", branch(f, f.day > from_day && f.day < to_day, &pair_fts(cond_sql, &f.alias)))
        })
        .collect();
    Ok(Some((parts.join(" UNION ALL "), all)))
}

/// The index hints the dashboard's SQL carries (dashboard::filter_query,
/// tenant): (table, Some(index)) for `INDEXED BY index`, None for `NOT
/// INDEXED`. Each gets a hinted view in attach_all. cursor_page's
/// `idx_queries_slowest` is left out on purpose: it walks a whole file's
/// queries by duration to find a window, which pays in one big file but, in
/// every day file, costs more than each file's own occurred_at plan (queries@1h
/// 1.9 -> 9.9 ms with it).
const HINTED: &[(&str, Option<&str>)] = &[
    ("executions", Some("index_executions_on_group_hash_and_occurred_at")),
    ("executions", Some("idx_executions_with_preview")),
    ("executions", Some("idx_executions_tenant_summary")),
    ("executions", None),
];

fn hinted_view(table: &str, hint: Option<&str>) -> String {
    format!("{table}__{}", hint.unwrap_or("not_indexed"))
}

/// Turns `"t" [alias] INDEXED BY i` / `"t" [alias] NOT INDEXED` into the
/// hinted view `"t__i" AS alias` (alias defaults to "t", so qualified column
/// names still resolve). Views cannot take the hint themselves; stripping it
/// instead left SQLite free to pick another index in every day file, which
/// made the hinted pages 3-11x slower than on a single file (docs/PERF.md
/// #5). A hint with no hinted view is stripped.
pub fn rewrite_index_hints(sql: &str) -> std::borrow::Cow<'_, str> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#""(\w+)"(\s+\w+)?\s+(?:INDEXED BY (\w+)|NOT INDEXED)"#).unwrap());
    if !sql.contains("INDEXED") {
        return sql.into();
    }
    let rewritten = RE.replace_all(sql, |c: &regex::Captures<'_>| {
        let table = &c[1];
        let hint = c.get(3).map(|m| m.as_str());
        let alias = c.get(2).map_or_else(|| format!("\"{table}\""), |a| a.as_str().trim().to_string());
        if HINTED.iter().any(|(t, h)| *t == table && *h == hint) {
            format!("\"{}\" AS {alias}", hinted_view(table, hint))
        } else {
            format!("\"{table}\"{}", c.get(2).map_or("", |a| a.as_str()))
        }
    });
    strip_index_hints(&rewritten).into_owned().into()
}

/// Removes `INDEXED BY <name>` and `NOT INDEXED`, which cannot name a view.
pub fn strip_index_hints(sql: &str) -> std::borrow::Cow<'_, str> {
    if !sql.contains("INDEXED") {
        return sql.into();
    }
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(i) = rest.find("INDEXED") {
        let before = &rest[..i];
        let after = &rest[i + "INDEXED".len()..];
        if let Some(stripped) = before.strip_suffix("NOT ") {
            out.push_str(stripped.trim_end());
            rest = after;
        } else if let Some(name) = after.strip_prefix(" BY ") {
            out.push_str(before.trim_end());
            let end = name.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(name.len());
            rest = &name[end..];
        } else {
            out.push_str(before);
            out.push_str("INDEXED");
            rest = after;
        }
    }
    out.push_str(rest);
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::layout::{day_path, meta_path, open_or_create};

    #[test]
    fn strips_hints() {
        assert_eq!(strip_index_hints("FROM \"queries\" INDEXED BY idx_queries_slowest WHERE x"), "FROM \"queries\" WHERE x");
        assert_eq!(strip_index_hints("FROM \"executions\" NOT INDEXED WHERE"), "FROM \"executions\" WHERE");
        assert_eq!(strip_index_hints("e2 INDEXED BY index_a_b\n WHERE"), "e2\n WHERE");
    }

    #[test]
    fn rewrites_hints_to_hinted_views() {
        assert_eq!(rewrite_index_hints("FROM \"queries\" INDEXED BY idx_queries_slowest WHERE x"), "FROM \"queries\" WHERE x");
        assert_eq!(rewrite_index_hints("FROM \"executions\" NOT INDEXED WHERE"), "FROM \"executions__not_indexed\" AS \"executions\" WHERE");
        assert_eq!(
            rewrite_index_hints("FROM \"executions\" e2 INDEXED BY index_executions_on_group_hash_and_occurred_at\n WHERE"),
            "FROM \"executions__index_executions_on_group_hash_and_occurred_at\" AS e2\n WHERE"
        );
    }

    #[test]
    fn unions_days_and_sees_new_files() {
        let dir = std::env::temp_dir().join(format!("rw-reader-{}", crate::control::base58(8)));
        open_or_create(&meta_path(&dir)).unwrap();
        for (day, id) in [("2026-10-13", 1), ("2026-10-14", 2)] {
            let c = open_or_create(&day_path(&dir, Tier::Core, day)).unwrap();
            c.execute(
                "INSERT INTO executions (id, execution_id, kind, name, occurred_at, duration) VALUES (?, ?, 'request', 'GET /', ?, 1.0)",
                rusqlite::params![id, format!("e{id}"), format!("{day} 12:00:00")],
            )
            .unwrap();
            let d = open_or_create(&day_path(&dir, Tier::Detail, day)).unwrap();
            d.execute(
                "INSERT INTO logs (id, level, message, occurred_at) VALUES (?, 'info', ?, ?)",
                rusqlite::params![id, format!("hello {day}"), format!("{day} 12:00:00")],
            )
            .unwrap();
            d.execute("INSERT INTO logs_fts (rowid, message) SELECT id, message FROM logs", []).unwrap();
        }
        let (conn, mut attached) = open(&dir).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM executions WHERE occurred_at >= '2026-10-13'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let ids: Vec<i64> = conn
            .prepare("SELECT logs.id FROM logs WHERE logs.id IN (SELECT rowid FROM logs_fts WHERE logs_fts MATCH '\"hello\"*') ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(!refresh(&conn, &dir, &mut attached).unwrap());
        open_or_create(&day_path(&dir, Tier::Core, "2026-10-15")).unwrap();
        assert!(refresh(&conn, &dir, &mut attached).unwrap());
        assert_eq!(attached.len(), 6);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
