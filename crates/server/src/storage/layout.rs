//! Where an environment's rows live: which file a table's row goes to, and
//! the directory of day files.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::schema;

/// The three kinds of file in an environment's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// Long-lived rows: rollups, receipts, people, query shapes.
    Meta,
    /// Executions, exceptions and the other per-request rows (30 days by default).
    Core,
    /// The bulky child rows (7 days by default; exemplars move to Core).
    Detail,
}

/// Child rows that make up most of a tenant file (queries and their four
/// indexes alone are ~50%, research/IMPROVEMENTS.md).
pub const DETAIL_TABLES: &[&str] = &["queries", "cache_events", "view_renders", "logs", "spans"];

/// Rows that must outlive any one day: the dedup state, the rollups, and
/// rows keyed by something other than time.
pub const META_TABLES: &[&str] = &["rollups", "release_health", "ingest_receipts", "people", "query_shapes", "export_destinations", "export_deliveries"];

impl Tier {
    pub fn of(table: &str) -> Tier {
        if META_TABLES.contains(&table) {
            Tier::Meta
        } else if DETAIL_TABLES.contains(&table) {
            Tier::Detail
        } else {
            Tier::Core
        }
    }

    pub fn dir(self) -> &'static str {
        match self {
            Tier::Meta => "",
            Tier::Core => "core",
            Tier::Detail => "detail",
        }
    }
}

/// The column that decides a day-file row's UTC day.
pub fn day_column(table: &str) -> &'static str {
    match table {
        "health_samples" => "sampled_at",
        "processes" => "booted_at",
        "ingest_batches" => "received_at",
        _ => "occurred_at",
    }
}

/// Every gem table whose rows go to day files, in schema order. These are
/// the tables whose id high-water marks meta's sqlite_sequence carries.
pub fn day_tables() -> Vec<&'static str> {
    static TABLES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    TABLES
        .get_or_init(|| {
            schema::schema()
                .ddl
                .iter()
                .filter(|(ty, name, _, sql)| ty == "table" && sql.is_some() && is_gem_table(name) && Tier::of(name) != Tier::Meta)
                .map(|(_, name, _, _)| &*Box::leak(name.clone().into_boxed_str()))
                .collect()
        })
        .clone()
}

fn is_gem_table(name: &str) -> bool {
    !(name.starts_with("sqlite_") || name.starts_with("logs_fts") || name == "schema_migrations" || name == "ar_internal_metadata")
}

/// "YYYY-MM-DD" from a stored datetime ("YYYY-MM-DD HH:MM:SS[.ffffff]"),
/// if it is one.
pub fn day_of(value: &str) -> Option<&str> {
    let d = value.get(0..10)?;
    let b = d.as_bytes();
    let ok = b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    ok.then_some(d)
}

/// An environment's storage on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// The Rails layout: one file (`telemetry/<slug>.sqlite3`).
    Single(PathBuf),
    /// Per-day files under `telemetry/<slug>/`.
    Days(PathBuf),
}

impl Layout {
    /// The directory wins over a single file of the same slug (a split
    /// leaves the old file behind until it is removed).
    pub fn detect(tenants_dir: &Path, slug: &str) -> Option<Layout> {
        let dir = tenants_dir.join(slug);
        if dir.join("meta.sqlite3").is_file() {
            return Some(Layout::Days(dir));
        }
        let file = tenants_dir.join(format!("{slug}.sqlite3"));
        file.exists().then_some(Layout::Single(file))
    }

    pub fn days(tenants_dir: &Path, slug: &str) -> Layout {
        Layout::Days(tenants_dir.join(slug))
    }

    /// Total bytes on disk (files plus their -wal).
    pub fn bytes(&self) -> u64 {
        let size = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
        match self {
            Layout::Single(f) => size(f) + size(&wal(f)),
            Layout::Days(d) => all_files(d).iter().map(|f| size(f) + size(&wal(f))).sum(),
        }
    }
}

fn wal(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", p.display()))
}

pub fn meta_path(dir: &Path) -> PathBuf {
    dir.join("meta.sqlite3")
}

pub fn day_path(dir: &Path, tier: Tier, day: &str) -> PathBuf {
    dir.join(tier.dir()).join(format!("{day}.sqlite3"))
}

/// The days that have a file in `tier`, oldest first.
pub fn days(dir: &Path, tier: Tier) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir.join(tier.dir()))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let day = name.strip_suffix(".sqlite3")?;
            (day.len() == 10 && day_of(day) == Some(day)).then(|| day.to_string())
        })
        .collect();
    out.sort();
    out
}

/// meta, then core days, then detail days (oldest first within a tier).
pub fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![meta_path(dir)];
    for tier in [Tier::Core, Tier::Detail] {
        out.extend(days(dir, tier).into_iter().map(|d| day_path(dir, tier, &d)));
    }
    out
}

/// Opens (creating if needed) a gem-schema file for writing. Creation goes
/// through a temporary name and a rename, so a reader listing the directory
/// never attaches a file whose schema is half written.
pub fn open_or_create(path: &Path) -> rusqlite::Result<Connection> {
    if !path.exists() {
        let parent = path.parent().expect("day file has a parent");
        std::fs::create_dir_all(parent).map_err(io)?;
        let tmp = parent.join(format!(".{}.creating", path.file_name().unwrap().to_string_lossy()));
        let _ = std::fs::remove_file(&tmp);
        {
            let mut c = Connection::open(&tmp)?;
            schema::configure(&c)?;
            let now = crate::time::quoted_date_micros(crate::time::now_micros());
            schema::create_tenant(&mut c, &now)?;
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        // Closing the last connection removed the -wal and -shm.
        std::fs::rename(&tmp, path).map_err(io)?;
        if let Ok(d) = std::fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    crate::tenants::open_tenant(path)
}

pub fn io(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR), Some(e.to_string()))
}

/// Ensures the directory and its meta file exist (a new environment).
pub fn ensure(dir: &Path) -> rusqlite::Result<()> {
    open_or_create(&meta_path(dir)).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_cover_every_gem_table_once() {
        let day = day_tables();
        assert!(day.contains(&"executions") && day.contains(&"queries") && day.contains(&"sessions"));
        for t in META_TABLES {
            assert!(!day.contains(t));
            assert!(schema::schema().tables.contains_key(*t), "{t}");
        }
        assert_eq!(Tier::of("queries"), Tier::Detail);
        assert_eq!(Tier::of("exceptions"), Tier::Core);
        assert_eq!(Tier::of("rollups"), Tier::Meta);
    }

    #[test]
    fn day_of_accepts_only_dates() {
        assert_eq!(day_of("2026-10-14 23:59:59.123456"), Some("2026-10-14"));
        assert_eq!(day_of("2026-10-1"), None);
        assert_eq!(day_of("../../etc/pas"), None);
    }
}
