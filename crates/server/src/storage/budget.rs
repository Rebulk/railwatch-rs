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
//! A pass measures and trims every 10 s. Between passes the ingest writers
//! add what they commit to a running estimate (`State::wrote`); once it is
//! over the budget the pass is woken at once, and ingest waits for it
//! (`State::admit`), so a burst overshoots by about one batch per request
//! in flight rather than by 10 s of writes. Over the budget with nothing
//! left to trim (only today's files), ingest answers 503 like the floor:
//! accepting more would grow today's files until the floor stops it.
//!
//! Deleting telemetry early is data loss, so every pass that deletes says
//! what it deleted, and crossing the floor or the budget is logged once
//! each way.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

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

/// How long an ingest request over the estimate waits for the pass it
/// woke before it is refused.
pub const TRIM_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// What the ingest path reads, written by the budget thread.
#[derive(Debug, Default)]
pub struct State {
    /// Free space is under the floor: ingest answers 503.
    low_space: AtomicBool,
    /// Still over max_data_bytes after deleting all it may.
    over_budget: AtomicBool,
    /// Ingest requests refused while low (logged on recovery).
    refused: AtomicU64,
    /// Ingest requests refused over max_data_bytes (logged by the pass).
    refused_over: AtomicU64,
    /// max_data_bytes as of the last pass (0: off).
    max_data_bytes: AtomicU64,
    /// Telemetry bytes the last pass measured...
    measured: AtomicU64,
    /// ...plus what the ingest writers committed since: the estimate.
    written: AtomicU64,
    /// Wakes the budget thread before its next tick, or stops it.
    signal: Mutex<Signal>,
    wake: Condvar,
    /// Notified at the end of each pass's trim.
    passed: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct Signal {
    woken: bool,
    stopped: bool,
}

/// Why ingest is refused (503 + Retry-After).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Under min_free_bytes.
    LowSpace,
    /// Over max_data_bytes with nothing left to trim, or a trim that did
    /// not finish within TRIM_WAIT.
    OverBudget,
}

impl State {
    /// Before an ingest write. Over max_data_bytes by the estimate, it
    /// wakes the budget pass and waits up to TRIM_WAIT for it to trim; a
    /// refusal is counted.
    pub async fn admit(&self) -> Result<(), Refusal> {
        let deadline = tokio::time::Instant::now() + TRIM_WAIT;
        while self.over_estimate() && !self.over_budget.load(Ordering::Relaxed) {
            // Notified by every pass that ends from here on, polled or not.
            let passed = self.passed.notified();
            self.wake();
            if tokio::time::timeout_at(deadline, passed).await.is_err() {
                break;
            }
        }
        if self.low_space.load(Ordering::Relaxed) {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(Refusal::LowSpace);
        }
        if self.over_budget.load(Ordering::Relaxed) || self.over_estimate() {
            self.refused_over.fetch_add(1, Ordering::Relaxed);
            return Err(Refusal::OverBudget);
        }
        Ok(())
    }

    /// The ingest writers, after each commit: `bytes` (see `take_written`)
    /// count against max_data_bytes until the next pass measures. Over it,
    /// the pass is woken now rather than at its next tick.
    pub fn wrote(&self, bytes: u64) {
        let max = self.max_data_bytes.load(Ordering::Relaxed);
        if max == 0 || bytes == 0 {
            return;
        }
        let written = self.written.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if self.measured.load(Ordering::Relaxed) + written > max {
            self.wake();
        }
    }

    fn over_estimate(&self) -> bool {
        let max = self.max_data_bytes.load(Ordering::Relaxed);
        max > 0 && self.measured.load(Ordering::Relaxed) + self.written.load(Ordering::Relaxed) > max
    }

    fn wake(&self) {
        self.signal.lock().unwrap().woken = true;
        self.wake.notify_one();
    }

    /// Stops the budget thread (`spawn`); join it after.
    pub fn stop(&self) {
        self.signal.lock().unwrap().stopped = true;
        self.wake.notify_one();
    }
}

/// At most how much `conn`'s files grew since the last call (as file_bytes
/// counts them), from a counter SQLite keeps, so no stat per batch: each
/// WAL frame written (page plus frame header) grows the WAL by at most its
/// size and the main file, once checkpointed, by at most a page, so it
/// counts twice. Erring high only wakes the pass early; the pass measures
/// before it deletes anything.
pub fn take_written(conn: &Connection) -> u64 {
    let (mut pages, mut high) = (0, 0);
    // SAFETY: the handle is live for `conn`'s lifetime; the out-params are
    // plain ints; resetting the counter is all this does.
    let rc = unsafe { rusqlite::ffi::sqlite3_db_status(conn.handle(), rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE, &mut pages, &mut high, 1) };
    if rc != rusqlite::ffi::SQLITE_OK || pages <= 0 {
        return 0;
    }
    let page: i64 = conn.prepare_cached("PRAGMA page_size").and_then(|mut q| q.query_row([], |r| r.get(0))).unwrap_or(4096);
    2 * pages as u64 * (page as u64 + 24)
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

/// A database file with its -wal and -shm. With a live WAL, the main file
/// counts at the size a checkpoint will grow it to: the WAL keeps its size
/// after one, so the file and its WAL hold those pages twice.
pub(super) fn file_bytes(path: &Path) -> u64 {
    let len = |s: &str| std::fs::metadata(format!("{}{s}", path.display())).map_or(0, |m| m.len());
    let (main, wal) = (len(""), len("-wal"));
    let main = if wal > 0 { main.max(wal_db_bytes(path).unwrap_or(0)) } else { main };
    main + wal + len("-shm")
}

/// The database size the WAL-index header (the first 48 bytes of -shm,
/// stored twice, native byte order; sqlite.org/walformat.html) records:
/// what the main file becomes once the WAL is checkpointed. None when the
/// two copies differ (a writer is updating it) or it is not initialised.
fn wal_db_bytes(path: &Path) -> Option<u64> {
    let mut h = [0u8; 96];
    std::io::Read::read_exact(&mut std::fs::File::open(format!("{}-shm", path.display())).ok()?, &mut h).ok()?;
    if h[..48] != h[48..] || h[12] == 0 {
        return None;
    }
    let page = match u16::from_ne_bytes([h[14], h[15]]) {
        1 => 65536,
        n => n as u64,
    };
    Some(u32::from_ne_bytes([h[20], h[21], h[22], h[23]]) as u64 * page)
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
            // `used` counts files as file_bytes does, which can be more
            // than their length today (a WAL not yet checkpointed).
            let detail = layout::day_path(dir, Tier::Detail, &day);
            let mut counted = file_bytes(&detail);
            let mut freed = super::retention::remove(&detail);
            if tier == Tier::Core {
                let core = layout::day_path(dir, Tier::Core, &day);
                counted += file_bytes(&core);
                freed += super::retention::remove(&core);
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
            used = used.saturating_sub(counted.max(freed));
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
    state.max_data_bytes.store(budget.max_data_bytes, Ordering::Relaxed);
    if budget.max_data_bytes > 0 {
        let today = &crate::time::utc_seconds_string(crate::time::now_micros() / 1_000_000)[..10];
        // Writes counted from here on may be in the measurement too: the
        // estimate errs high.
        let counted = state.written.load(Ordering::Relaxed);
        match trim(tenants_dir, budget.max_data_bytes, today) {
            Ok(t) => {
                let over = t.used > budget.max_data_bytes;
                state.measured.store(t.used, Ordering::Relaxed);
                state.written.fetch_sub(counted, Ordering::Relaxed);
                let was_over = state.over_budget.swap(over, Ordering::Relaxed);
                // Ingest waiting on this pass goes on now; closing handles
                // (on_trim) only matters to the floor.
                state.passed.notify_waiters();
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
                if over && !was_over {
                    tracing::error!(
                        used = t.used,
                        max_data_bytes = budget.max_data_bytes,
                        "telemetry is still over max_data_bytes with no older day file left to delete (today's files, or a single-file environment): refusing ingest (503) until it is within max_data_bytes"
                    );
                } else if !over {
                    let refused = state.refused_over.swap(0, Ordering::Relaxed);
                    if was_over {
                        tracing::warn!(
                            used = t.used,
                            max_data_bytes = budget.max_data_bytes,
                            refused,
                            "telemetry is within max_data_bytes again: accepting ingest again"
                        );
                    } else if refused > 0 {
                        tracing::warn!(
                            used = t.used,
                            max_data_bytes = budget.max_data_bytes,
                            refused,
                            "refused ingest (503) over max_data_bytes: the trim took longer than the requests wait for it"
                        );
                    }
                }
            }
            Err(e) => {
                state.passed.notify_waiters();
                tracing::error!(error = %e, "disk budget: could not delete day files");
            }
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

/// How often the budget thread checks when nothing wakes it.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// The budget thread (only started when a knob is set): a pass every
/// EVERY, and one at once when ingest crosses the estimate. It runs until
/// `State::stop`; join it before the work `on_trim` sends to is shut down
/// (its closure holds a sender).
pub fn spawn(
    budget: Budget,
    data_dir: PathBuf,
    tenants_dir: PathBuf,
    state: std::sync::Arc<State>,
    on_trim: Box<dyn Fn(&Trimmed) + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("disk-budget".into())
        .spawn(move || {
            loop {
                let signal = state.signal.lock().unwrap();
                let (mut signal, _) = state.wake.wait_timeout_while(signal, EVERY, |s| !s.woken && !s.stopped).unwrap();
                if signal.stopped {
                    return;
                }
                signal.woken = false;
                drop(signal);
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

    fn admit(state: &State) -> Result<(), Refusal> {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(state.admit())
    }

    fn today() -> String {
        crate::time::utc_seconds_string(crate::time::now_micros() / 1_000_000)[..10].to_string()
    }

    #[test]
    fn under_the_free_space_floor_ingest_is_refused_until_it_recovers() {
        let root = tenants("floor");
        let state = State::default();
        let free = free_bytes(&root).unwrap();
        let floor = Budget { max_data_bytes: 0, min_free_bytes: free + (1 << 40) };
        check(&floor, &root, &root, &state, &|_| {});
        assert_eq!(admit(&state), Err(Refusal::LowSpace));
        assert_eq!(admit(&state), Err(Refusal::LowSpace));
        assert_eq!(state.refused.load(Ordering::Relaxed), 2);
        check(&Budget { max_data_bytes: 0, min_free_bytes: 1 }, &root, &root, &state, &|_| {});
        assert_eq!(admit(&state), Ok(()));
        assert_eq!(state.refused.load(Ordering::Relaxed), 0, "reported and reset on recovery");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What a commit wrote is counted from SQLite's own page counter: at
    /// least what the file grew by as the budget measures it, and a
    /// checkpoint (which grows the main file by pages the WAL holds) adds
    /// nothing.
    #[test]
    fn take_written_counts_the_wal_frames_a_commit_wrote() {
        let root = tenants("written");
        let path = day_path(&root.join("a"), Tier::Core, "2026-10-20");
        let c = open_or_create(&path).unwrap();
        let before = file_bytes(&path);
        let _ = take_written(&c);
        c.execute_batch("CREATE TABLE more (b BLOB); INSERT INTO more VALUES (zeroblob(1000000));").unwrap();
        let wrote = take_written(&c);
        let grew = file_bytes(&path) - before;
        assert!(wrote >= grew && wrote <= grew + grew / 10, "wrote {wrote}, the files grew {grew} (WAL and main file to be)");
        assert_eq!(take_written(&c), 0, "reset by each call");
        // The main file grows at the checkpoint; it was counted already.
        let main = std::fs::metadata(&path).unwrap().len();
        c.execute_batch("PRAGMA wal_checkpoint(PASSIVE)").unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > main + 900_000, "the checkpoint grew the main file");
        assert_eq!(file_bytes(&path) - before, grew, "and file_bytes had counted it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Writes past the budget wake the pass at once (not at its 10 s tick),
    /// and the request over the estimate waits for it rather than writing.
    #[test]
    fn crossing_the_budget_wakes_the_pass_and_ingest_waits_for_its_trim() {
        let root = tenants("wake");
        let (total, _) = usage(&root);
        let state = std::sync::Arc::new(State::default());
        let budget = Budget { max_data_bytes: total, min_free_bytes: 0 };
        check(&budget, &root, &root, &state, &|_| {});
        assert_eq!(admit(&state), Ok(()), "within: no wait");
        let thread = spawn(budget, root.clone(), root.clone(), state.clone(), Box::new(|_| {}));
        // Today's file grows by ~2 day files, which the writer reports.
        let c = open_or_create(&day_path(&root.join("a"), Tier::Core, &today())).unwrap();
        let _ = take_written(&c);
        c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(400000));").unwrap();
        let started = std::time::Instant::now();
        state.wrote(take_written(&c));
        assert_eq!(admit(&state), Ok(()), "the pass trimmed old days back under the budget");
        assert!(started.elapsed() < TRIM_WAIT, "woken, not the 10 s tick: {:?}", started.elapsed());
        assert!(left(&root, "a", Tier::Detail).len() < 5, "old detail days went");
        assert_eq!(state.written.load(Ordering::Relaxed), 0, "re-synced by the measurement");
        state.stop();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only today's files left and still over: ingest answers 503 (it would
    /// otherwise grow today's files until the floor stops it), until a pass
    /// finds telemetry within the budget again.
    #[test]
    fn over_budget_with_only_today_left_ingest_is_refused_until_within() {
        let root = std::env::temp_dir().join(format!("rw-budget-today-{}", crate::control::base58(8)));
        let dir = root.join("a");
        open_or_create(&meta_path(&dir)).unwrap();
        for tier in [Tier::Core, Tier::Detail] {
            let c = open_or_create(&day_path(&dir, tier, &today())).unwrap();
            c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(200000)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
        let (total, _) = usage(&root);
        let state = State::default();
        check(&Budget { max_data_bytes: total / 2, min_free_bytes: 0 }, &root, &root, &state, &|_| {});
        assert_eq!(left(&root, "a", Tier::Core), vec![today()], "today's files are never trimmed");
        let waited = std::time::Instant::now();
        assert_eq!(admit(&state), Err(Refusal::OverBudget));
        assert!(waited.elapsed() < TRIM_WAIT / 2, "nothing to trim: refused without waiting");
        assert_eq!(admit(&state), Err(Refusal::OverBudget));
        assert_eq!(state.refused_over.load(Ordering::Relaxed), 2);
        // Within again (here a larger budget; in practice the next day).
        check(&Budget { max_data_bytes: total * 2, min_free_bytes: 0 }, &root, &root, &state, &|_| {});
        assert_eq!(admit(&state), Ok(()));
        assert_eq!(state.refused_over.load(Ordering::Relaxed), 0, "reported and reset on recovery");
        let _ = std::fs::remove_dir_all(&root);
    }
}
