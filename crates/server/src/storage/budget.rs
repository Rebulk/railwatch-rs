//! A size budget for the data dir, for a server that shares its disk with
//! the app it monitors (the railwatch-server gem runs it on the app's own
//! volume). Retention bounds telemetry by time only, and a full volume
//! breaks the app: sign-in and `db:prepare` fail on SQLITE_FULL.
//!
//! Two knobs, both off by default (a server on its own disk keeps today's
//! behaviour):
//!
//! - `max_data_bytes`: telemetry may use this much. Over it, whole day files
//!   go, oldest first: detail days (queries, logs, cache, views, spans) of
//!   every environment, then core days (executions, exceptions, ...). Never
//!   today's files, meta (rollups, receipts) or the control DB, so charts,
//!   issues and alerts outlive the raw rows. No exemplars are copied: the
//!   point is to free space.
//! - `min_free_bytes`: below this much free space on the data dir's
//!   filesystem, ingest answers 503 + Retry-After (the gem keeps its buffer
//!   and backs off) instead of writing. The budget is enforced first, so a
//!   budget that fits the disk never reaches the floor; the floor covers
//!   what the budget cannot (the app itself filling the volume).
//!
//! Deleting telemetry early is data loss, so every pass that deletes says
//! what it deleted, and crossing the floor or the budget is logged once
//! each way.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusqlite::Connection;

use super::layout::{self, Tier};

/// `max_data_bytes` / `min_free_bytes`; 0 is off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budget {
    pub max_data_bytes: u64,
    pub min_free_bytes: u64,
}

impl Budget {
    pub fn is_off(&self) -> bool {
        self.max_data_bytes == 0 && self.min_free_bytes == 0
    }
}

/// What the ingest path reads, written by the budget thread.
#[derive(Debug, Default)]
pub struct State {
    /// Free space is under the floor: ingest answers 503.
    low_space: AtomicBool,
    /// Still over max_data_bytes after deleting all it may.
    over_budget: AtomicBool,
    /// Ingest requests refused while low (logged on recovery).
    refused: AtomicU64,
}

impl State {
    /// Whether to refuse an ingest request; counts the refusal.
    pub fn refuse_ingest(&self) -> bool {
        let low = self.low_space.load(Ordering::Relaxed);
        if low {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        low
    }
}

/// Free bytes for an unprivileged user on the filesystem holding `dir`.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs is plain data; zeroed is a valid value for it.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: path is a valid C string, s a properly sized out-param.
    (unsafe { libc::statvfs(path.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

/// What a pass deleted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Trimmed {
    pub detail_files: usize,
    pub core_files: usize,
    pub bytes: u64,
    /// Per environment directory name (its slug): the newest core day
    /// deleted, everything up to whose end is gone.
    pub core_through: Vec<(String, String)>,
    /// Telemetry bytes after the pass.
    pub used: u64,
}

fn file_bytes(path: &Path) -> u64 {
    ["", "-wal", "-shm"].iter().map(|s| std::fs::metadata(format!("{}{s}", path.display())).map_or(0, |m| m.len())).sum()
}

/// Bytes under `tenants_dir`, and its per-day environments. A single-file
/// (Rails layout) environment counts but cannot be trimmed by file
/// (`railwatch split` it).
pub fn usage(tenants_dir: &Path) -> (u64, Vec<PathBuf>) {
    let mut total = 0;
    let mut dirs = Vec::new();
    for e in std::fs::read_dir(tenants_dir).into_iter().flatten().flatten() {
        let path = e.path();
        if path.is_dir() && layout::meta_path(&path).is_file() {
            total += layout::all_files(&path).iter().map(|f| file_bytes(f)).sum::<u64>();
            dirs.push(path);
        } else if path.extension().is_some_and(|x| x == "sqlite3") {
            total += file_bytes(&path);
        }
    }
    dirs.sort();
    (total, dirs)
}

/// Deletes the oldest day files until the telemetry under `tenants_dir`
/// fits `max_bytes`: detail days oldest first across environments, then
/// core days (each with its detail day). Days on or after `today` stay.
/// Files go while holding their meta file's write lock, as retention
/// deletes them, so no batch is writing into one.
pub fn trim(tenants_dir: &Path, max_bytes: u64, today: &str) -> rusqlite::Result<Trimmed> {
    let (mut used, dirs) = usage(tenants_dir);
    let mut out = Trimmed { used, ..Default::default() };
    if used <= max_bytes {
        return Ok(out);
    }
    for tier in [Tier::Detail, Tier::Core] {
        let mut days: Vec<(String, &PathBuf)> = Vec::new();
        for dir in &dirs {
            days.extend(layout::days(dir, tier).into_iter().filter(|d| d.as_str() < today).map(|d| (d, dir)));
        }
        days.sort();
        for (day, dir) in days {
            if used <= max_bytes {
                break;
            }
            let meta: Connection = crate::tenants::open_tenant(&layout::meta_path(dir))?;
            meta.execute_batch("BEGIN IMMEDIATE")?;
            let mut freed = super::retention::remove(&layout::day_path(dir, Tier::Detail, &day));
            if tier == Tier::Core {
                freed += super::retention::remove(&layout::day_path(dir, Tier::Core, &day));
                let slug = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
                match out.core_through.iter_mut().find(|(s, _)| *s == slug) {
                    Some(e) => e.1 = day.clone(),
                    None => out.core_through.push((slug, day.clone())),
                }
                out.core_files += 1;
            } else {
                out.detail_files += 1;
            }
            meta.execute_batch("COMMIT")?;
            out.bytes += freed;
            used = used.saturating_sub(freed);
        }
    }
    out.used = used;
    Ok(out)
}

/// One pass: trim to the budget, then set the free-space state. `on_trim`
/// runs after a pass that deleted files, before free space is read (the
/// caller closes idle connections to the deleted files, whose open handles
/// keep the space allocated, and records the new cutoffs). The ingest
/// writer drops its own at its next transaction (DayWriter::begin).
pub fn check(budget: &Budget, data_dir: &Path, tenants_dir: &Path, state: &State, on_trim: &dyn Fn(&Trimmed)) {
    check_with(budget, data_dir, tenants_dir, state, on_trim, &free_bytes);
}

/// `check` with the free-space reading passed in (tests).
fn check_with(budget: &Budget, data_dir: &Path, tenants_dir: &Path, state: &State, on_trim: &dyn Fn(&Trimmed), free_bytes: &dyn Fn(&Path) -> Option<u64>) {
    if budget.max_data_bytes > 0 {
        let today = &crate::time::utc_seconds_string(crate::time::now_micros() / 1_000_000)[..10];
        match trim(tenants_dir, budget.max_data_bytes, today) {
            Ok(t) => {
                if t.detail_files + t.core_files > 0 {
                    tracing::warn!(
                        max_data_bytes = budget.max_data_bytes,
                        detail_files = t.detail_files,
                        core_files = t.core_files,
                        bytes = t.bytes,
                        used = t.used,
                        "telemetry over max_data_bytes: deleted the oldest day files"
                    );
                    on_trim(&t);
                }
                let over = t.used > budget.max_data_bytes;
                if state.over_budget.swap(over, Ordering::Relaxed) != over {
                    if over {
                        tracing::error!(
                            used = t.used,
                            max_data_bytes = budget.max_data_bytes,
                            "telemetry is still over max_data_bytes with no older day file left to delete (today's files, or a single-file environment)"
                        );
                    } else {
                        tracing::info!(used = t.used, max_data_bytes = budget.max_data_bytes, "telemetry is within max_data_bytes again");
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "disk budget: could not delete day files"),
        }
    }
    if budget.min_free_bytes > 0 {
        let free = free_bytes(data_dir);
        let low = free.is_some_and(|f| f < budget.min_free_bytes);
        if state.low_space.swap(low, Ordering::Relaxed) != low {
            if low {
                tracing::error!(
                    free_bytes = free.unwrap_or(0),
                    min_free_bytes = budget.min_free_bytes,
                    data_dir = %data_dir.display(),
                    "disk almost full: refusing ingest (503) until free space is back above min_free_bytes"
                );
            } else {
                // Outside the macro: tracing skips a field's expression when
                // nothing listens at that level.
                let refused = state.refused.swap(0, Ordering::Relaxed);
                tracing::warn!(
                    free_bytes = free.unwrap_or(0),
                    min_free_bytes = budget.min_free_bytes,
                    refused,
                    "free space is back above min_free_bytes: accepting ingest again"
                );
            }
        }
    }
}

/// How often the budget thread checks.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// The budget thread (only started when a knob is set). It runs until
/// `stop`'s sender is dropped; join it before the work `on_trim` sends to
/// is shut down (its closure holds a sender).
pub fn spawn(
    budget: Budget,
    data_dir: PathBuf,
    tenants_dir: PathBuf,
    state: std::sync::Arc<State>,
    on_trim: Box<dyn Fn(&Trimmed) + Send>,
    stop: std::sync::mpsc::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("disk-budget".into())
        .spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(EVERY) {
                check(&budget, &data_dir, &tenants_dir, &state, &*on_trim);
            }
        })
        .expect("spawn disk budget thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::layout::{day_path, meta_path, open_or_create};

    /// Two environments, five days each, every file padded to a known size.
    fn tenants(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rw-budget-{name}-{}", crate::control::base58(8)));
        for slug in ["a", "b"] {
            let dir = root.join(slug);
            open_or_create(&meta_path(&dir)).unwrap();
            for day in ["2026-10-10", "2026-10-11", "2026-10-12", "2026-10-13", "2026-10-14"] {
                for tier in [Tier::Core, Tier::Detail] {
                    let c = open_or_create(&day_path(&dir, tier, day)).unwrap();
                    c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(200000)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
                }
            }
        }
        root
    }

    fn left(root: &Path, slug: &str, tier: Tier) -> Vec<String> {
        layout::days(&root.join(slug), tier)
    }

    #[test]
    fn over_budget_goes_oldest_detail_first_then_oldest_core() {
        let root = tenants("order");
        let (total, _) = usage(&root);
        let file = file_bytes(&day_path(&root.join("a"), Tier::Detail, "2026-10-10"));
        // Room for all but 2.5 day files: the three oldest detail days go,
        // across both environments, before any core day.
        let t = trim(&root, total - 3 * file + file / 2, "2026-10-14").unwrap();
        assert_eq!((t.detail_files, t.core_files), (3, 0));
        // By day, then environment: a 10-10, b 10-10, a 10-11.
        assert_eq!(left(&root, "a", Tier::Detail), vec!["2026-10-12", "2026-10-13", "2026-10-14"]);
        assert_eq!(left(&root, "b", Tier::Detail), vec!["2026-10-11", "2026-10-12", "2026-10-13", "2026-10-14"]);
        assert_eq!(left(&root, "a", Tier::Core).len(), 5);
        assert!(t.core_through.is_empty());

        // Far under: every detail day before today goes, then core days
        // oldest first; today's files and meta never do.
        let t = trim(&root, 1, "2026-10-14").unwrap();
        for slug in ["a", "b"] {
            assert_eq!(left(&root, slug, Tier::Detail), vec!["2026-10-14"]);
            assert_eq!(left(&root, slug, Tier::Core), vec!["2026-10-14"]);
            assert!(meta_path(&root.join(slug)).is_file());
        }
        assert_eq!(t.core_files, 8);
        assert_eq!(t.core_through, vec![("a".to_string(), "2026-10-13".to_string()), ("b".to_string(), "2026-10-13".to_string())]);
        assert!(t.used > 1, "still over: today's files are kept");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Deleted day files still open somewhere keep their disk blocks.
    fn open_deleted(root: &Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_link(e.path()).ok())
            .filter(|t| t.starts_with(root) && t.to_string_lossy().ends_with(" (deleted)"))
            .count()
    }

    /// After a trim, every handle to the deleted files is closed before free
    /// space is read: a dashboard reader idle in its pool, one checked out
    /// during the trim (closed when it comes back), and the ingest writer's
    /// cached day file. Each would otherwise keep a deleted file's blocks
    /// allocated, and the floor would see no space come back.
    #[test]
    fn trimmed_files_are_closed_before_free_space_is_read() {
        let root = tenants("handles");
        let pools = crate::dashboard::pool::ReadPools::new(root.clone(), 2);
        drop(pools.get("a").unwrap());
        let busy = pools.get("a").unwrap();
        let attached = |c: &Connection| c.query_row("SELECT count(*) FROM pragma_database_list", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(attached(&busy), 2 + 1 + 10, "main, temp, meta and ten day files");
        let mut writer = crate::storage::writer::DayWriter::open(&root.join("a")).unwrap();
        writer.begin().unwrap();
        // Caches a day file that the trim deletes.
        crate::ingest::writer::Target::route_at(&mut writer, "ingest_batches", "2026-10-10 12:00:00").unwrap();
        writer.commit().unwrap();
        let writer = std::sync::Mutex::new(writer);
        let held_during_trim = std::cell::Cell::new(0);
        let held_at_free = std::cell::Cell::new(usize::MAX);
        let on_trim = |_: &Trimmed| {
            held_during_trim.set(open_deleted(&root));
            pools.release_deleted();
            writer.lock().unwrap().drop_deleted();
        };
        let free = |_: &Path| {
            held_at_free.set(open_deleted(&root));
            Some(u64::MAX)
        };
        let budget = Budget { max_data_bytes: 1, min_free_bytes: 1 };
        // Trims to today's files only ("today" is the real date, after them).
        check_with(&budget, &root, &root, &State::default(), &on_trim, &free);
        assert!(held_during_trim.get() > 0, "the readers and the writer held the deleted files");
        // Only the reader still checked out holds them when space is read...
        let busy_handles = held_at_free.get();
        assert!(busy_handles > 0);
        drop(busy);
        // ...and it lets go when it comes back to the pool.
        assert_eq!(open_deleted(&root), 0);
        let r = pools.get("a").unwrap();
        assert_eq!(attached(&r), 2 + 1, "a fresh reader over what is left: meta only");
        drop(r);
        // Without the busy reader, nothing is held when space is read.
        for tier in [Tier::Core, Tier::Detail] {
            let c = layout::open_or_create(&layout::day_path(&root.join("a"), tier, "2026-10-15")).unwrap();
            c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(200000));").unwrap();
        }
        drop(pools.get("a").unwrap());
        check_with(&budget, &root, &root, &State::default(), &on_trim, &free);
        assert_eq!(held_at_free.get(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn within_budget_deletes_nothing() {
        let root = tenants("within");
        let (total, _) = usage(&root);
        let t = trim(&root, total, "2026-10-14").unwrap();
        assert_eq!(t, Trimmed { used: total, ..Default::default() });
        assert_eq!(left(&root, "a", Tier::Detail).len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn under_the_free_space_floor_ingest_is_refused_until_it_recovers() {
        let root = tenants("floor");
        let state = State::default();
        let free = free_bytes(&root).unwrap();
        let floor = Budget { max_data_bytes: 0, min_free_bytes: free + (1 << 40) };
        check(&floor, &root, &root, &state, &|_| {});
        assert!(state.refuse_ingest());
        assert!(state.refuse_ingest());
        assert_eq!(state.refused.load(Ordering::Relaxed), 2);
        check(&Budget { max_data_bytes: 0, min_free_bytes: 1 }, &root, &root, &state, &|_| {});
        assert!(!state.refuse_ingest());
        assert_eq!(state.refused.load(Ordering::Relaxed), 0, "reported and reset on recovery");
        let _ = std::fs::remove_dir_all(&root);
    }
}
