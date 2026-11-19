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
/// slug -> account id, for `State::set_owners`.
pub type Owners = Box<dyn Fn() -> std::collections::HashMap<String, i64> + Send + Sync>;

#[derive(Default)]
pub struct State {
    /// Free space is under the floor: ingest answers 503.
    low_space: AtomicBool,
    /// Still over max_data_bytes after deleting all it may: every account
    /// (`None`, a server whose environments' owners are unknown) or the
    /// accounts over their share (`Some`).
    over_budget: Mutex<Option<std::collections::HashSet<i64>>>,
    /// Whose environment each telemetry directory is (slug -> account id),
    /// read at each pass; unset, every directory is one account.
    owners: Mutex<Option<Owners>>,
    /// The last pass ended with telemetry still over max_data_bytes.
    last_pass_over: AtomicBool,
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
    /// Before an ingest write by `account`. Over max_data_bytes by the
    /// estimate, it wakes the budget pass and waits up to TRIM_WAIT for it
    /// to trim; it is then refused only when the last pass found its
    /// account over its share (every account, when owners are unknown). A
    /// refusal is counted.
    pub async fn admit(&self, account: i64) -> Result<(), Refusal> {
        let deadline = tokio::time::Instant::now() + TRIM_WAIT;
        while self.over_estimate() && !self.refuses(account) {
            // The last pass was itself over the budget and did not name this
            // account: another account is over its share (only today's files
            // left), and another pass would say the same. Waiting for it made
            // every innocent request take TRIM_WAIT and kept the budget thread
            // running back to back (PR #107 review M2).
            if self.cleared(account) {
                break;
            }
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
        // Owners unknown (one account): over the estimate after the wait is
        // refused too, as before. With owners, only accounts the pass found
        // over their share are, so a burst from one account never refuses
        // another's ingest.
        let unknown = self.owners.lock().unwrap().is_none();
        if self.refuses(account) || (unknown && self.over_estimate()) {
            self.refused_over.fetch_add(1, Ordering::Relaxed);
            return Err(Refusal::OverBudget);
        }
        Ok(())
    }

    /// Did the last pass, finding telemetry over the budget, leave `account`
    /// out of the accounts over their share? Only with owners known.
    fn cleared(&self, account: i64) -> bool {
        self.owners.lock().unwrap().is_some()
            && self.last_pass_over.load(Ordering::Relaxed)
            && self.over_budget.lock().unwrap().as_ref().is_some_and(|over| !over.contains(&account))
    }

    fn refuses(&self, account: i64) -> bool {
        match &*self.over_budget.lock().unwrap() {
            None => false,
            Some(over) if self.owners.lock().unwrap().is_none() => !over.is_empty(),
            Some(over) => over.contains(&account),
        }
    }

    /// How the budget pass learns which account each environment directory
    /// belongs to (serve: from the control DB). Unset, the whole server is
    /// one account: oldest days go first across every environment.
    pub fn set_owners(&self, owners: Owners) {
        *self.owners.lock().unwrap() = Some(owners);
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
    /// Still over max_data_bytes: the accounts over their fair share
    /// (0 is every directory when owners are unknown, -1 telemetry no
    /// environment owns). Their ingest is refused until a pass finds them
    /// within it; other accounts' is not.
    pub over: Vec<i64>,
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
/// (`railwatch split` it). Source maps (the `sourcemaps/` directory beside
/// `tenants_dir`) count too; they are bounded per environment instead of
/// trimmed (storage::sourcemaps).
pub fn usage(tenants_dir: &Path) -> (u64, Vec<PathBuf>) {
    let per = usage_by_slug(tenants_dir);
    let total = per.iter().map(|(_, b, _)| b).sum();
    let mut dirs: Vec<PathBuf> = per.into_iter().filter_map(|(_, _, d)| d).collect();
    dirs.sort();
    (total, dirs)
}

/// Bytes per environment slug: (slug, bytes, its per-day directory).
fn usage_by_slug(tenants_dir: &Path) -> Vec<(String, u64, Option<PathBuf>)> {
    let mut out: std::collections::BTreeMap<String, (u64, Option<PathBuf>)> = Default::default();
    for e in std::fs::read_dir(tenants_dir).into_iter().flatten().flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if path.is_dir() && layout::meta_path(&path).is_file() {
            let entry = out.entry(name).or_default();
            entry.0 += layout::all_files(&path).iter().map(|f| file_bytes(f)).sum::<u64>();
            entry.1 = Some(path);
        } else if let Some(slug) = name.strip_suffix(".sqlite3") {
            out.entry(slug.to_string()).or_default().0 += file_bytes(&path);
        }
    }
    let maps = sourcemaps_dir(tenants_dir);
    for e in (tenants_dir.file_name() == Some(std::ffi::OsStr::new("telemetry"))).then(|| std::fs::read_dir(&maps)).into_iter().flatten().flatten().flatten() {
        let slug = e.file_name().to_string_lossy().into_owned();
        out.entry(slug.clone()).or_default().0 += super::sourcemaps::env_bytes(&maps, &slug);
    }
    out.into_iter().map(|(s, (b, d))| (s, b, d)).collect()
}

/// `<data_dir>/sourcemaps`, beside `<data_dir>/telemetry`.
fn sourcemaps_dir(tenants_dir: &Path) -> PathBuf {
    tenants_dir.with_file_name("sourcemaps")
}

/// The fair share each account may keep when `max_bytes` must be split
/// among accounts using `used` bytes each (max-min fairness): an account
/// under the level keeps all it has, the rest share what is left equally.
fn water_level(used: &[u64], max_bytes: u64) -> u64 {
    let mut sorted = used.to_vec();
    sorted.sort_unstable();
    let mut left = max_bytes;
    for (i, u) in sorted.iter().enumerate() {
        let level = left / (sorted.len() - i) as u64;
        if *u > level {
            return level;
        }
        left -= u;
    }
    u64::MAX
}

/// Deletes the oldest day files until the telemetry under `tenants_dir`
/// fits `max_bytes`. Days on or after `today` stay. Files go while holding
/// their meta file's write lock, as retention deletes them, so no batch is
/// writing into one.
///
/// `owners` (slug -> account) makes it fair between accounts: each gets
/// the max-min fair share of `max_bytes`, only accounts over it lose days
/// (the one furthest over first, its own oldest detail day, then its oldest
/// core day with its detail day), and an account under it never loses
/// anything to make room for another's growth. Accounts still over their
/// share afterwards (only today's files left) are in `Trimmed::over`.
/// Without owners every directory is one account: oldest detail day across
/// environments first, then core days.
pub fn trim(tenants_dir: &Path, max_bytes: u64, today: &str) -> rusqlite::Result<Trimmed> {
    trim_fair(tenants_dir, max_bytes, today, None)
}

pub fn trim_fair(tenants_dir: &Path, max_bytes: u64, today: &str, owners: Option<&std::collections::HashMap<String, i64>>) -> rusqlite::Result<Trimmed> {
    let per = usage_by_slug(tenants_dir);
    let mut used: u64 = per.iter().map(|(_, b, _)| b).sum();
    let mut out = Trimmed { used, ..Default::default() };
    if used <= max_bytes {
        return Ok(out);
    }
    // Account of each slug: unknown slugs (no environment) are one account
    // of their own, never charged to anyone else's share.
    let account = |slug: &str| owners.map_or(0, |o| o.get(slug).copied().unwrap_or(-1));
    let mut by_account: std::collections::BTreeMap<i64, u64> = Default::default();
    for (slug, b, _) in &per {
        *by_account.entry(account(slug)).or_default() += b;
    }
    let level = water_level(&by_account.values().copied().collect::<Vec<_>>(), max_bytes);
    // Each account's deletable days, in deletion order: detail days oldest
    // first across its environments, then core days.
    let mut queue: std::collections::BTreeMap<i64, std::collections::VecDeque<(Tier, String, PathBuf)>> = Default::default();
    for tier in [Tier::Detail, Tier::Core] {
        let mut days: Vec<(i64, String, PathBuf)> = Vec::new();
        for (slug, _, dir) in &per {
            let Some(dir) = dir else { continue };
            days.extend(layout::days(dir, tier).into_iter().filter(|d| d.as_str() < today).map(|d| (account(slug), d, dir.clone())));
        }
        days.sort();
        for (acct, day, dir) in days {
            queue.entry(acct).or_default().push_back((tier, day, dir));
        }
    }
    while used > max_bytes {
        // The account furthest over its share that still has a day to give.
        let Some(acct) = by_account
            .iter()
            .filter(|(a, u)| **u > level && queue.get(a).is_some_and(|q| !q.is_empty()))
            .max_by_key(|(a, u)| (**u - level, std::cmp::Reverse(**a)))
            .map(|(a, _)| *a)
        else {
            break;
        };
        let (tier, day, dir) = queue.get_mut(&acct).unwrap().pop_front().unwrap();
        // A core day goes with its detail day, which may be gone already.
        let detail = layout::day_path(&dir, Tier::Detail, &day);
        if tier == Tier::Core || detail.exists() {
            let meta: Connection = crate::tenants::open_tenant(&layout::meta_path(&dir))?;
            meta.execute_batch("BEGIN IMMEDIATE")?;
            // `used` counts files as file_bytes does, which can be more
            // than their length today (a WAL not yet checkpointed).
            let mut counted = file_bytes(&detail);
            let mut freed = super::retention::remove(&detail);
            if tier == Tier::Core {
                let core = layout::day_path(&dir, Tier::Core, &day);
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
            let gone = counted.max(freed);
            used = used.saturating_sub(gone);
            let u = by_account.get_mut(&acct).unwrap();
            *u = u.saturating_sub(gone);
        }
    }
    out.used = used;
    if used > max_bytes {
        out.over = by_account.into_iter().filter(|(_, u)| *u > level).map(|(a, _)| a).collect();
    }
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
        let owners = state.owners.lock().unwrap().as_ref().map(|f| f());
        match trim_fair(tenants_dir, budget.max_data_bytes, today, owners.as_ref()) {
            Ok(t) => {
                let over = t.used > budget.max_data_bytes;
                state.measured.store(t.used, Ordering::Relaxed);
                state.written.fetch_sub(counted, Ordering::Relaxed);
                let was_over = {
                    let mut o = state.over_budget.lock().unwrap();
                    let was = o.as_ref().is_some_and(|s| !s.is_empty());
                    *o = Some(if over { t.over.iter().copied().collect() } else { Default::default() });
                    was
                };
                state.last_pass_over.store(over, Ordering::Relaxed);
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
                        accounts = ?t.over,
                        "telemetry is still over max_data_bytes with no older day file left to delete (today's files, or a single-file environment): refusing ingest (503) from the accounts over their share until it is within max_data_bytes"
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
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(state.admit(1))
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

    /// GA review 2026-11-19 T2: one account's growth never deletes another
    /// account's history or refuses its ingest. Account 2 fills today's
    /// files far past the budget; account 1, within its fair share, keeps
    /// every day and may still ingest, while account 2 loses its own old
    /// days first and is the only one refused.
    #[test]
    fn one_accounts_growth_never_trims_or_refuses_another_account() {
        let root = tenants("fair");
        let today = "2026-10-14";
        let big = open_or_create(&day_path(&root.join("b"), Tier::Detail, today)).unwrap();
        big.execute_batch("CREATE TABLE more (b BLOB); INSERT INTO more VALUES (zeroblob(12000000)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let a_bytes = usage_by_slug(&root).into_iter().find(|(s, _, _)| s == "a").unwrap().1;
        let owners: std::collections::HashMap<String, i64> = [("a".to_string(), 1), ("b".to_string(), 2)].into();
        // Room for all of a's data and a bit more: b alone is over.
        let budget = a_bytes * 2;
        let t = trim_fair(&root, budget, today, Some(&owners)).unwrap();
        assert_eq!(left(&root, "a", Tier::Detail).len(), 5, "account 1 keeps every detail day");
        assert_eq!(left(&root, "a", Tier::Core).len(), 5, "and every core day");
        assert_eq!(left(&root, "b", Tier::Detail), vec![today.to_string()], "account 2 lost its own old days");
        assert_eq!(left(&root, "b", Tier::Core), vec![today.to_string()]);
        assert_eq!(t.over, vec![2], "only account 2 is still over its share");

        let state = State::default();
        let owners2 = owners.clone();
        state.set_owners(Box::new(move || owners2.clone()));
        // check() uses the real today; a fresh tree with today's date works the same way.
        let root2 = std::env::temp_dir().join(format!("rw-budget-fair2-{}", crate::control::base58(8)));
        for slug in ["a", "b"] {
            let c = open_or_create(&day_path(&root2.join(slug), Tier::Core, &today_now())).unwrap();
            c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(200000)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
            open_or_create(&meta_path(&root2.join(slug))).unwrap();
        }
        let c = open_or_create(&day_path(&root2.join("b"), Tier::Detail, &today_now())).unwrap();
        c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(3000000)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let (total, _) = usage(&root2);
        check(&Budget { max_data_bytes: total / 2, min_free_bytes: 0 }, &root2, &root2, &state, &|_| {});
        let admit_as = |acct: i64| tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(state.admit(acct));
        assert_eq!(admit_as(1), Ok(()), "the account within its share keeps ingesting");
        assert_eq!(admit_as(2), Err(Refusal::OverBudget), "the one over it is refused");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    fn today_now() -> String {
        today()
    }

    #[test]
    fn the_fair_share_is_max_min() {
        assert_eq!(water_level(&[10, 100], 60), 50, "the small account keeps all, the large one gets the rest");
        assert_eq!(water_level(&[10, 20], 60), u64::MAX, "everyone fits");
        assert_eq!(water_level(&[100, 100], 60), 30);
    }

    /// PR #107 review M2: account 2 is over its share with only today's
    /// files left. Account 1's ingest is admitted at once, not after waiting
    /// TRIM_WAIT for a pass that cannot change the verdict; account 2 is
    /// still refused.
    #[test]
    fn an_account_within_its_share_is_admitted_without_waiting_while_another_is_over() {
        let root = std::env::temp_dir().join(format!("rw-budget-m2-{}", crate::control::base58(8)));
        for (slug, pad) in [("a", 200_000), ("b", 3_000_000)] {
            open_or_create(&meta_path(&root.join(slug))).unwrap();
            let c = open_or_create(&day_path(&root.join(slug), Tier::Core, &today())).unwrap();
            c.execute_batch(&format!("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob({pad})); PRAGMA wal_checkpoint(TRUNCATE);")).unwrap();
        }
        let state = State::default();
        state.set_owners(Box::new(|| [("a".to_string(), 1), ("b".to_string(), 2)].into()));
        // Room for twice a's bytes: a is within its share, b far over it.
        let a_bytes = usage_by_slug(&root).into_iter().find(|(s, _, _)| s == "a").unwrap().1;
        check(&Budget { max_data_bytes: a_bytes * 2, min_free_bytes: 0 }, &root, &root, &state, &|_| {});
        assert_eq!(state.over_budget.lock().unwrap().clone(), Some([2].into()));
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        for _ in 0..3 {
            let started = std::time::Instant::now();
            assert_eq!(rt.block_on(state.admit(1)), Ok(()));
            assert!(started.elapsed() < TRIM_WAIT / 4, "admitted without waiting: {:?}", started.elapsed());
        }
        assert_eq!(rt.block_on(state.admit(2)), Err(Refusal::OverBudget));
        let _ = std::fs::remove_dir_all(&root);
    }
}
