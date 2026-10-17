//! The ingest writer's side of a per-day tenant: one transaction that spans
//! meta and whichever day files a coalesced group of batches touches, made
//! exactly-once without a multi-file atomic commit.
//!
//! **The commit point is meta.** Every day table's id high-water mark lives
//! in meta's own `sqlite_sequence` (meta has every gem table, so this is the
//! gem schema, not a new table). A transaction:
//!
//! 1. `BEGIN IMMEDIATE` on meta (every writer of the environment's files
//!    takes meta first, so holding it means no one else writes any of them);
//! 2. for each run of rows bound for a day file: `BEGIN IMMEDIATE` on that
//!    file the first time, set the file's `sqlite_sequence` for the table to
//!    the in-flight high-water mark, insert, read the advanced sequence back
//!    as the new mark. Ids are therefore unique across the environment and
//!    advance exactly as they would in one Rails file (ignored inserts burn
//!    an id in both);
//! 3. receipts, people, query shapes go to meta, inside its transaction;
//! 4. COMMIT every day file, then write the marks into meta's
//!    sqlite_sequence and COMMIT meta **last**.
//!
//! A row in a day file whose id is above meta's mark belongs to a
//! transaction whose meta commit never happened. `recover` deletes exactly
//! those rows (and their FTS entries, and the profile links that point at
//! them) before the writer takes its first batch, and again whenever a
//! commit fails half way. So whatever instant the process dies at:
//!
//! - before meta's COMMIT: the batch is not committed anywhere once
//!   recovery has run; no receipt exists, the 200 was never sent, the gem
//!   retries and it is written once;
//! - after meta's COMMIT: it is committed everywhere; a replay of a
//!   receipted delivery finds its receipt (already_committed).
//!
//! Power loss (synchronous=NORMAL) can additionally drop the WAL tail of any
//! one file. Losing a day file's tail while meta survives loses rows the
//! receipt claims (the same acked-batch loss a single Rails file has under
//! NORMAL); losing meta's tail while a day file survives leaves rows above
//! the mark, which recovery deletes. Neither double-counts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension};

use super::layout::{self, Tier, day_column, day_of};
use crate::ingest::mapper::{Bind, Row};
use crate::ingest::writer::{Target, WriteError};

/// Day files a writer keeps open between transactions (today and
/// yesterday in both tiers, plus a few late arrivals).
const OPEN_FILES: usize = 8;

struct DayFile {
    tier: Tier,
    day: String,
    conn: Connection,
    /// The file's sqlite_sequence as last read or set, per table.
    seq: HashMap<&'static str, i64>,
    last_used: u64,
}

pub struct DayWriter {
    dir: PathBuf,
    meta: Connection,
    /// Index i + 1 in Target terms (0 is meta).
    files: Vec<DayFile>,
    /// The environment's committed high-water marks (meta's sqlite_sequence).
    committed: HashMap<&'static str, i64>,
    /// In-flight marks, ahead of `committed` during a transaction.
    hwm: HashMap<&'static str, i64>,
    /// Files inside the open transaction (Target indices).
    in_txn: Vec<usize>,
    /// Files that hold the current batch's savepoint.
    in_savepoint: Vec<usize>,
    savepoint_hwm: HashMap<&'static str, i64>,
    /// Between savepoint() and release()/rollback_to().
    in_batch: bool,
    clock: u64,
    /// Day used for a row whose day column is missing (the batch's own day).
    fallback_day: String,
}

/// What recovery undid.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    pub rows: i64,
    pub files: usize,
}

impl DayWriter {
    pub fn open(dir: &Path) -> rusqlite::Result<DayWriter> {
        let meta = layout::open_or_create(&layout::meta_path(dir))?;
        let committed = read_marks(&meta)?;
        let mut w = DayWriter {
            dir: dir.to_path_buf(),
            meta,
            files: Vec::new(),
            hwm: committed.clone(),
            committed,
            in_txn: Vec::new(),
            in_savepoint: Vec::new(),
            savepoint_hwm: HashMap::new(),
            in_batch: false,
            clock: 0,
            fallback_day: today(),
        };
        let r = w.recover()?;
        if r.rows > 0 {
            tracing::warn!(dir = %dir.display(), rows = r.rows, files = r.files, "rolled back rows of an uncommitted transaction");
        }
        Ok(w)
    }

    pub fn meta_conn(&self) -> &Connection {
        &self.meta
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Deletes every day-file row above meta's committed mark. Runs with
    /// meta's write lock held, so nothing else writes meanwhile.
    pub fn recover(&mut self) -> rusqlite::Result<Recovered> {
        self.meta.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.committed = read_marks(&self.meta)?;
            self.hwm = self.committed.clone();
            let mut out = Recovered::default();
            let tables = layout::day_tables();
            let files = layout::all_files(&self.dir).into_iter().skip(1).collect::<Vec<_>>();
            let mut lost_profiles = false;
            for path in &files {
                let conn = crate::tenants::open_tenant(path)?;
                let mut over: Vec<(&'static str, i64)> = Vec::new();
                for t in &tables {
                    let max: Option<i64> = conn.query_row(&format!("SELECT MAX(id) FROM \"{t}\""), [], |r| r.get(0))?;
                    let mark = self.committed.get(t).copied().unwrap_or(0);
                    if max.is_some_and(|m| m > mark) {
                        over.push((t, mark));
                    }
                }
                if over.is_empty() {
                    continue;
                }
                conn.execute_batch("BEGIN IMMEDIATE")?;
                for (t, mark) in &over {
                    if *t == "logs" {
                        conn.execute("INSERT INTO logs_fts(logs_fts, rowid, message) SELECT 'delete', id, message FROM logs WHERE id > ?", [mark])?;
                    }
                    lost_profiles |= *t == "profiles";
                    out.rows += conn.execute(&format!("DELETE FROM \"{t}\" WHERE id > ?"), [mark])? as i64;
                    conn.execute(&format!("UPDATE sqlite_sequence SET seq = ?1 WHERE name = '{t}' AND seq > ?1"), [mark])?;
                }
                conn.execute_batch("COMMIT")?;
                out.files += 1;
            }
            // Profile links the lost transaction made point past the mark
            // (possibly from an older day's execution). Batch#link_profiles!
            // redoes them when the gem resends the batch.
            if lost_profiles {
                let mark = self.committed.get("profiles").copied().unwrap_or(0);
                for path in files.iter().filter(|p| p.parent().is_some_and(|d| d.ends_with(Tier::Core.dir()))) {
                    crate::tenants::open_tenant(path)?.execute("UPDATE executions SET profile_id = NULL WHERE profile_id > ?", [mark])?;
                }
            }
            Ok(out)
        })();
        match result {
            Ok(out) => {
                self.meta.execute_batch("COMMIT")?;
                for f in &mut self.files {
                    f.seq.clear();
                }
                Ok(out)
            }
            Err(e) => {
                let _ = self.meta.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn begin(&mut self) -> rusqlite::Result<()> {
        self.meta.execute_batch("BEGIN IMMEDIATE")?;
        self.in_txn.clear();
        self.hwm = self.committed.clone();
        self.fallback_day = today();
        Ok(())
    }

    pub fn savepoint(&mut self) -> rusqlite::Result<()> {
        self.meta.execute_batch("SAVEPOINT batch")?;
        self.in_savepoint.clear();
        for &i in &self.in_txn {
            self.files[i - 1].conn.execute_batch("SAVEPOINT batch")?;
            self.in_savepoint.push(i);
        }
        self.savepoint_hwm = self.hwm.clone();
        self.in_batch = true;
        Ok(())
    }

    pub fn release(&mut self) -> rusqlite::Result<()> {
        self.in_batch = false;
        for &i in &self.in_savepoint {
            self.files[i - 1].conn.execute_batch("RELEASE batch")?;
        }
        self.meta.execute_batch("RELEASE batch")
    }

    pub fn rollback_to(&mut self) -> rusqlite::Result<()> {
        self.in_batch = false;
        for &i in &self.in_savepoint {
            let f = &mut self.files[i - 1];
            f.conn.execute_batch("ROLLBACK TO batch; RELEASE batch")?;
            f.seq.clear();
        }
        self.hwm = std::mem::take(&mut self.savepoint_hwm);
        self.meta.execute_batch("ROLLBACK TO batch; RELEASE batch")
    }

    /// Day files first, meta (with the new marks) last.
    pub fn commit(&mut self) -> rusqlite::Result<()> {
        for k in 0..self.in_txn.len() {
            let i = self.in_txn[k];
            if let Err(e) = self.files[i - 1].conn.execute_batch("COMMIT") {
                self.abort_after_partial_commit();
                return Err(e);
            }
            crash_point("after_day_commit");
        }
        crash_point("before_meta_commit");
        let marks = (|| -> rusqlite::Result<()> {
            for (t, v) in &self.hwm {
                if self.committed.get(t) != Some(v) {
                    set_seq(&self.meta, t, *v)?;
                }
            }
            self.meta.execute_batch("COMMIT")
        })();
        if let Err(e) = marks {
            self.abort_after_partial_commit();
            return Err(e);
        }
        self.committed = self.hwm.clone();
        self.in_txn.clear();
        crash_point("after_meta_commit");
        self.close_extra_files();
        Ok(())
    }

    pub fn rollback(&mut self) {
        for &i in &self.in_txn {
            let f = &mut self.files[i - 1];
            if !f.conn.is_autocommit() {
                let _ = f.conn.execute_batch("ROLLBACK");
            }
            f.seq.clear();
        }
        self.in_txn.clear();
        if !self.meta.is_autocommit() {
            let _ = self.meta.execute_batch("ROLLBACK");
        }
        self.hwm = self.committed.clone();
    }

    /// Some day files committed, meta did not: undo them now rather than at
    /// the next start.
    fn abort_after_partial_commit(&mut self) {
        self.rollback();
        match self.recover() {
            Ok(r) => tracing::warn!(dir = %self.dir.display(), rows = r.rows, "commit failed part way; uncommitted rows removed"),
            Err(e) => tracing::error!(dir = %self.dir.display(), error = %e, "commit failed part way and recovery failed; it runs again at the next open"),
        }
    }

    fn file(&mut self, tier: Tier, day: &str) -> Result<usize, WriteError> {
        self.clock += 1;
        // Retention may have unlinked a cached file since the last
        // transaction; writing to it would lose the rows. Reopen it in
        // place (recreating it), which keeps every index stable.
        if let Some(i) = self.files.iter().position(|f| f.tier == tier && f.day == day)
            && !self.in_txn.contains(&(i + 1))
            && !layout::day_path(&self.dir, tier, day).exists()
        {
            self.files[i].conn = layout::open_or_create(&layout::day_path(&self.dir, tier, day))?;
            self.files[i].seq.clear();
        }
        let i = match self.files.iter().position(|f| f.tier == tier && f.day == day) {
            Some(i) => i + 1,
            None => {
                let conn = layout::open_or_create(&layout::day_path(&self.dir, tier, day))?;
                self.files.push(DayFile { tier, day: day.to_string(), conn, seq: HashMap::new(), last_used: 0 });
                self.files.len()
            }
        };
        self.files[i - 1].last_used = self.clock;
        if !self.in_txn.contains(&i) {
            let f = &self.files[i - 1];
            f.conn.execute_batch("BEGIN IMMEDIATE")?;
            if self.in_batch {
                f.conn.execute_batch("SAVEPOINT batch")?;
                self.in_savepoint.push(i);
            }
            self.in_txn.push(i);
        }
        Ok(i)
    }

    /// Keeps the OPEN_FILES most recently used day connections.
    fn close_extra_files(&mut self) {
        if self.files.len() <= OPEN_FILES {
            return;
        }
        self.files.sort_by_key(|f| std::cmp::Reverse(f.last_used));
        self.files.truncate(OPEN_FILES);
    }

    /// Closes every day connection (before retention unlinks files).
    pub fn close_files(&mut self) {
        debug_assert!(self.in_txn.is_empty());
        self.files.clear();
    }
}

impl Target for DayWriter {
    fn route(&mut self, table: &'static str, row: &Row) -> Result<usize, WriteError> {
        let tier = Tier::of(table);
        if tier == Tier::Meta {
            return Ok(0);
        }
        let col = day_column(table);
        let day = match row.iter().find(|(c, _)| *c == col) {
            Some((_, Bind::Text(s))) => day_of(s).map(str::to_string),
            _ => None,
        };
        let day = day.unwrap_or_else(|| self.fallback_day.clone());
        self.file(tier, &day)
    }

    fn route_at(&mut self, table: &'static str, at: &str) -> Result<usize, WriteError> {
        let day = day_of(at).map_or_else(|| self.fallback_day.clone(), str::to_string);
        self.file(Tier::of(table), &day)
    }

    fn conn(&self, file: usize) -> &Connection {
        if file == 0 { &self.meta } else { &self.files[file - 1].conn }
    }

    fn meta(&self) -> &Connection {
        &self.meta
    }

    fn begin_run(&mut self, file: usize, table: &'static str) -> Result<(), WriteError> {
        if file == 0 {
            return Ok(());
        }
        let mark = self.hwm.get(table).copied().unwrap_or(0);
        let f = &mut self.files[file - 1];
        let cur = match f.seq.get(table) {
            Some(v) => *v,
            None => get_seq(&f.conn, table)?,
        };
        if cur != mark {
            set_seq(&f.conn, table, mark)?;
        }
        f.seq.insert(table, mark);
        Ok(())
    }

    fn end_run(&mut self, file: usize, table: &'static str) -> Result<(), WriteError> {
        if file == 0 {
            return Ok(());
        }
        let f = &mut self.files[file - 1];
        let now = get_seq(&f.conn, table)?;
        f.seq.insert(table, now);
        let mark = self.hwm.entry(table).or_insert(0);
        *mark = (*mark).max(now);
        crash_point("mid_transaction");
        Ok(())
    }

    /// Profiles and executions are both core rows, of the days their
    /// occurred_at says, which are normally the same day. The newest
    /// profile per execution is looked up, and the executions updated, in
    /// the core files of the days this transaction writes plus the day
    /// before and after each (an execution further away than that from
    /// its profile is not linked; docs/DIFFERENCES.md).
    fn link_profiles(&mut self, ids: &[&str]) -> Result<(), WriteError> {
        let mut days: Vec<String> = Vec::new();
        for &i in &self.in_txn {
            let f = &self.files[i - 1];
            if f.tier == Tier::Core {
                for d in [-1, 0, 1] {
                    let day = shift_day(&f.day, d);
                    if !days.contains(&day) {
                        days.push(day);
                    }
                }
            }
        }
        let on_disk = layout::days(&self.dir, Tier::Core);
        let mut targets = Vec::new();
        for day in days {
            let open = self.files.iter().position(|f| f.tier == Tier::Core && f.day == day).is_some_and(|i| self.in_txn.contains(&(i + 1)));
            if open || on_disk.contains(&day) {
                targets.push(self.file(Tier::Core, &day)?);
            }
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let mut newest: HashMap<String, i64> = HashMap::new();
        for &i in &targets {
            let mut s = self.files[i - 1]
                .conn
                .prepare_cached(&format!("SELECT execution_id, MAX(id) FROM profiles WHERE execution_id IN ({placeholders}) GROUP BY execution_id"))?;
            for row in s.query_map(rusqlite::params_from_iter(ids), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (e, id) = row?;
                let v = newest.entry(e).or_insert(id);
                *v = (*v).max(id);
            }
        }
        for &i in &targets {
            let mut s = self.files[i - 1].conn.prepare_cached("UPDATE \"executions\" SET profile_id = ?1 WHERE \"executions\".\"execution_id\" = ?2")?;
            for id in ids {
                s.execute(rusqlite::params![newest.get(*id), id])?;
            }
        }
        Ok(())
    }
}

/// "YYYY-MM-DD" moved by `by` days.
pub fn shift_day(day: &str, by: i64) -> String {
    let t = crate::time::parse_db_time(&format!("{day} 00:00:00")).unwrap_or(0) / 1_000_000 + by * 86_400;
    crate::time::utc_seconds_string(t)[..10].to_string()
}

fn today() -> String {
    crate::time::utc_seconds_string(crate::time::now_micros() / 1_000_000)[..10].to_string()
}

pub fn read_marks(meta: &Connection) -> rusqlite::Result<HashMap<&'static str, i64>> {
    let mut out = HashMap::new();
    for t in layout::day_tables() {
        out.insert(t, get_seq(meta, t)?);
    }
    Ok(out)
}

fn get_seq(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    Ok(conn.prepare_cached("SELECT seq FROM sqlite_sequence WHERE name = ?")?.query_row([table], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0))
}

/// sqlite_sequence has no key on name: UPDATE, else INSERT.
pub fn set_seq(conn: &Connection, table: &str, seq: i64) -> rusqlite::Result<()> {
    if conn.prepare_cached("UPDATE sqlite_sequence SET seq = ?2 WHERE name = ?1")?.execute(rusqlite::params![table, seq])? == 0 {
        conn.prepare_cached("INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)")?.execute(rusqlite::params![table, seq])?;
    }
    Ok(())
}

/// Crash injection for tests/storage_crash.rs: `RAILWATCH_CRASH_AT=point:n`
/// aborts the process the n-th time `point` is reached. Unset in normal runs
/// (one OnceLock read per call).
pub fn crash_point(point: &str) {
    static AT: OnceLock<Option<(String, u64)>> = OnceLock::new();
    static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let at = AT.get_or_init(|| {
        let v = std::env::var("RAILWATCH_CRASH_AT").ok()?;
        let (p, n) = v.split_once(':').unwrap_or((&v, "1"));
        Some((p.to_string(), n.parse().unwrap_or(1)))
    });
    if let Some((p, n)) = at
        && p == point
        && HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == *n
    {
        eprintln!("railwatch: injected crash at {point}");
        std::process::abort();
    }
}
