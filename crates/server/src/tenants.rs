//! One writer per tenant file. Requests map and validate on their own tasks
//! (in parallel), then hand the prepared batch to the tenant's writer over a
//! channel. The writer owns the only write connection to the file. It
//! coalesces whatever is queued (up to MAX_COALESCE_BATCHES batches or
//! MAX_COALESCE_ROWS rows) into one `BEGIN IMMEDIATE` transaction, with a
//! SAVEPOINT per batch so a batch that fails rolls back alone, commits, and
//! only then answers each request.
//!
//! The writer is a dedicated OS thread (rusqlite is blocking); it is opened
//! lazily on the first batch and closes itself after `idle` without work.
//! Reads that the ingest path needs (a receipt lookup) also go through it, so
//! they see exactly what has committed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};
use tokio::sync::oneshot;

use crate::followups::{Followup, FollowupSender};
use crate::ingest::batch::Prepared;
use crate::ingest::writer::{WriteError, Written};
use crate::schema;
use crate::storage::Layout;
use crate::storage::store::Store;

pub const MAX_COALESCE_BATCHES: usize = 64;
pub const MAX_COALESCE_ROWS: usize = 20_000;

/// A stored receipt, as Telemetry::IngestReceipt.lookup returns it.
#[derive(Debug, Clone)]
pub struct StoredReceipt {
    pub body_sha256: String,
    pub metadata_sha256: String,
    pub ack: String,
    pub committed_at: String,
}

pub enum Job {
    Write {
        batch: Box<Prepared>,
        /// Environment#ingest_followup_gc_before, re-read by the caller
        /// just before sending (Batch#write_receipt! reads it in the txn).
        gc_floor_micros: i64,
        reply: oneshot::Sender<Result<Written, WriteError>>,
    },
    LookupReceipt {
        producer_id: String,
        delivery_id: String,
        reply: oneshot::Sender<Result<Option<StoredReceipt>, String>>,
    },
    /// Close cached day files that were deleted (Tenants::release_deleted).
    Release {
        reply: std::sync::mpsc::Sender<()>,
    },
}

#[derive(Clone)]
pub struct Tenants {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    idle: Duration,
    /// slug -> (generation, sender); the generation tells a writer that
    /// exits on idle whether the map still points at it.
    writers: Mutex<HashMap<String, (u64, Sender<Job>)>>,
    generation: std::sync::atomic::AtomicU64,
    followups: Mutex<Option<FollowupSender>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// The disk budget's estimate, which every commit adds to.
    disk: Arc<crate::storage::budget::State>,
}

impl Tenants {
    pub fn new(dir: PathBuf, idle: Duration, followups: FollowupSender) -> Tenants {
        std::fs::create_dir_all(&dir).expect("create telemetry dir");
        Tenants {
            inner: Arc::new(Inner {
                dir,
                idle,
                writers: Mutex::new(HashMap::new()),
                generation: Default::default(),
                followups: Mutex::new(Some(followups)),
                threads: Mutex::new(Vec::new()),
                disk: Default::default(),
            }),
        }
    }

    /// The disk budget's state (storage::budget): the writers count what
    /// they commit into it, ingest reads it.
    pub fn disk(&self) -> Arc<crate::storage::budget::State> {
        self.inner.disk.clone()
    }

    pub fn dir(&self) -> PathBuf {
        self.inner.dir.clone()
    }

    /// The environment's single file (the Rails layout).
    pub fn path(&self, slug: &str) -> PathBuf {
        self.inner.dir.join(format!("{slug}.sqlite3"))
    }

    /// Where the environment's telemetry is, if it has any yet.
    pub fn layout(&self, slug: &str) -> Option<Layout> {
        Layout::detect(&self.inner.dir, slug)
    }

    /// Creates the environment's storage if it does not exist yet
    /// (Environment's after_create_commit): a per-day directory.
    pub fn ensure(&self, slug: &str) -> rusqlite::Result<()> {
        match self.layout(slug) {
            Some(_) => Ok(()),
            None => crate::storage::layout::ensure(&self.inner.dir.join(slug)),
        }
    }

    /// Hands a job to the tenant's writer, starting one if none is running.
    pub fn send(&self, env_id: i64, slug: &str, mut job: Job) {
        loop {
            let (generation, tx) = {
                let mut writers = self.inner.writers.lock().unwrap();
                writers.entry(slug.to_string()).or_insert_with(|| self.spawn(env_id, slug)).clone()
            };
            match tx.send(job) {
                Ok(()) => return,
                // The writer exited on idle between our lookup and the send;
                // forget it and start another.
                Err(mpsc::SendError(j)) => {
                    job = j;
                    let mut writers = self.inner.writers.lock().unwrap();
                    if writers.get(slug).is_some_and(|(g, _)| *g == generation) {
                        writers.remove(slug);
                    }
                }
            }
        }
    }

    fn spawn(&self, env_id: i64, slug: &str) -> (u64, Sender<Job>) {
        let (tx, rx) = mpsc::channel();
        let dir = self.inner.dir.clone();
        let inner = self.inner.clone();
        let slug = slug.to_string();
        let generation = self.inner.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let followups = self.inner.followups.lock().unwrap().clone().expect("tenants already shut down");
        let handle = std::thread::Builder::new()
            .name(format!("writer-{slug}"))
            .spawn(move || {
                run_writer(env_id, &slug, &dir, rx, inner.idle, &followups, &inner.disk);
                drop(followups);
                // Unregister, unless a newer writer already took our place.
                let mut writers = inner.writers.lock().unwrap();
                if writers.get(&slug).is_some_and(|(g, _)| *g == generation) {
                    writers.remove(&slug);
                }
            })
            .expect("spawn writer thread");
        let mut threads = self.inner.threads.lock().unwrap();
        threads.retain(|h| !h.is_finished());
        threads.push(handle);
        (generation, tx)
    }

    /// Has every running writer close its cached day files that were
    /// deleted (retention, the disk budget), so their space comes back now
    /// rather than at that writer's next batch. Waits up to `wait` in all;
    /// a writer that is mid-batch answers after it (and drops them anyway
    /// at its next transaction).
    pub fn release_deleted(&self, wait: Duration) {
        let senders: Vec<Sender<Job>> = self.inner.writers.lock().unwrap().values().map(|(_, tx)| tx.clone()).collect();
        let deadline = Instant::now() + wait;
        let replies: Vec<_> = senders
            .into_iter()
            .filter_map(|tx| {
                let (reply, rx) = std::sync::mpsc::channel();
                tx.send(Job::Release { reply }).ok().map(|_| rx)
            })
            .collect();
        for rx in replies {
            let _ = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }

    /// Graceful shutdown: close every channel, then wait for the writers to
    /// drain what was already queued.
    pub fn shutdown(&self) {
        self.inner.writers.lock().unwrap().clear();
        self.inner.followups.lock().unwrap().take();
        let handles: Vec<_> = std::mem::take(&mut *self.inner.threads.lock().unwrap());
        for h in handles {
            let _ = h.join();
        }
    }
}

pub fn open_tenant(path: &Path) -> rusqlite::Result<Connection> {
    let fresh = !path.exists();
    let mut conn = Connection::open(path)?;
    schema::configure(&conn)?;
    // The per-batch SAVEPOINT makes every statement keep a statement journal;
    // with the default temp_store those are files in /var/tmp (etilqs_*), and
    // writing them was ~35% of the server's CPU under load (perf, 2026-10-13).
    // A journal only lives for one batch's savepoint, so memory is cheap.
    conn.execute_batch("PRAGMA temp_store = MEMORY;")?;
    // A batch dirties more pages than cache_size holds (one leaf per random
    // index key); with spilling on, SQLite wrote them to the WAL mid-
    // transaction and re-read them at COMMIT to rewrite checksums, ~30% of
    // the writer once the file outgrew the cache (docs/PERF.md #3). Only the
    // committing frames count either way, so durability is unchanged; the
    // cost is a transaction's dirty pages held in memory until commit.
    conn.execute_batch("PRAGMA cache_spill = OFF;")?;
    let has_schema: bool =
        conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if fresh || !has_schema {
        let now = crate::time::quoted_date_micros(crate::time::now_micros());
        schema::create_tenant(&mut conn, &now)?;
    } else {
        schema::add_missing_columns(&conn)?;
    }
    conn.set_prepared_statement_cache_capacity(256);
    Ok(conn)
}

/// Every tenant file under `tenants_dir` (single files and every file of a
/// per-day directory) gets the schema::ADDED_COLUMNS it lacks. Runs once at
/// startup, before any reader attaches: a union reader's views read every
/// day file, and one file short a column fails every page of that
/// environment. Returns the files changed.
///
/// A missing `tenants_dir` is a fresh install. Any other failure (a
/// directory or entry it cannot read, a file it cannot open) is logged and
/// skipped, so that environment's pages fail until it is fixed while the
/// rest are unaffected; it is never silently passed over. Only regular files
/// and directories are followed: the layout (storage/layout.rs) has no
/// symlinks, and following one could widen a file outside `tenants_dir`.
pub fn add_missing_columns_everywhere(tenants_dir: &Path) -> usize {
    let mut files = Vec::new();
    for (path, kind) in regular_entries(tenants_dir) {
        match kind {
            Kind::Dir => per_day_files(&path, &mut files),
            Kind::File if path.extension().is_some_and(|x| x == "sqlite3") => files.push(path),
            Kind::File => {}
        }
    }
    let widen = |path: &Path| {
        Connection::open(path).and_then(|c| {
            schema::configure(&c)?;
            schema::add_missing_columns(&c)
        })
    };
    let mut changed = 0;
    let mut failed = Vec::new();
    for path in files {
        match widen(&path) {
            Ok(0) => {}
            Ok(_) => changed += 1,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "cannot add the new telemetry columns yet; retrying");
                failed.push(path);
            }
        }
    }
    // A per-day file left narrow fails every visits page of its environment
    // until it is widened, and nothing else widens an old day file. So a
    // file that failed (typically a lock held past busy_timeout) gets more
    // tries before the server takes traffic.
    for _ in 0..STARTUP_WIDEN_RETRIES {
        if failed.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        let mut still = Vec::new();
        for path in failed {
            match widen(&path) {
                Ok(n) => changed += usize::from(n > 0),
                Err(_) => still.push(path),
            }
        }
        failed = still;
    }
    // Still narrow: start serving anyway (other environments must not wait
    // on one locked file) and keep trying in the background, so the
    // environment's visits pages come back on their own once the lock clears.
    if !failed.is_empty() {
        for path in &failed {
            tracing::error!(file = %path.display(), "cannot add the new telemetry columns; that environment's visits fail to read until a background retry succeeds");
        }
        let _ = std::thread::Builder::new().name("widen-retry".into()).spawn(|| widen_until_done(failed, BACKGROUND_WIDEN_EVERY));
    }
    changed
}

/// Further passes over files the startup sweep could not widen, before the
/// server starts serving.
const STARTUP_WIDEN_RETRIES: u32 = 3;
/// How often a file still narrow after startup is tried again.
const BACKGROUND_WIDEN_EVERY: Duration = Duration::from_secs(30);

/// Widens every file in `paths`, trying the ones that fail again every
/// `every`, until none is left. A file that disappears (retention, the disk
/// budget) is dropped, not retried.
pub fn widen_until_done(mut paths: Vec<PathBuf>, every: Duration) {
    while !paths.is_empty() {
        std::thread::sleep(every);
        paths.retain(|path| {
            if !path.exists() {
                return false;
            }
            let result = Connection::open(path).and_then(|c| {
                schema::configure(&c)?;
                schema::add_missing_columns(&c)
            });
            match result {
                Ok(_) => {
                    tracing::info!(file = %path.display(), "added the new telemetry columns");
                    false
                }
                Err(_) => true,
            }
        });
    }
}

enum Kind {
    File,
    Dir,
}

/// The regular files and directories directly in `dir`, by `DirEntry::file_type`
/// (which does not follow symlinks). A missing `dir` is empty; every other
/// error is logged.
fn regular_entries(dir: &Path) -> Vec<(PathBuf, Kind)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::error!(dir = %dir.display(), error = %e, "cannot list tenant files for the new telemetry columns");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::error!(dir = %dir.display(), error = %e, "cannot read a tenant directory entry");
                continue;
            }
        };
        match entry.file_type() {
            Ok(t) if t.is_file() => out.push((entry.path(), Kind::File)),
            Ok(t) if t.is_dir() => out.push((entry.path(), Kind::Dir)),
            Ok(_) => tracing::warn!(path = %entry.path().display(), "not a regular file or directory: skipped"),
            Err(e) => tracing::error!(path = %entry.path().display(), error = %e, "cannot read a tenant entry's type"),
        }
    }
    out
}

/// A per-day tenant directory's meta file and day files, as
/// storage::layout::all_files lists them, but through regular_entries.
fn per_day_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let meta = crate::storage::layout::meta_path(dir);
    match std::fs::symlink_metadata(&meta) {
        Ok(m) if m.file_type().is_file() => files.push(meta),
        // Not a per-day tenant (no meta file), or meta is not a regular file.
        Ok(_) => return,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::error!(file = %meta.display(), error = %e, "cannot read a tenant's meta file");
            return;
        }
    }
    for tier in [crate::storage::layout::Tier::Core, crate::storage::layout::Tier::Detail] {
        let tier_dir = dir.join(tier.dir());
        // read_dir follows a symlinked tier directory, so check it first.
        match std::fs::symlink_metadata(&tier_dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                tracing::warn!(path = %tier_dir.display(), "not a regular directory: skipped");
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::error!(path = %tier_dir.display(), error = %e, "cannot read a tenant's tier directory");
                continue;
            }
        }
        let mut days: Vec<PathBuf> = regular_entries(&tier_dir)
            .into_iter()
            .filter_map(|(path, kind)| {
                let day = path.file_name()?.to_str()?.strip_suffix(".sqlite3")?;
                (matches!(kind, Kind::File) && day.len() == 10 && crate::storage::layout::day_of(day) == Some(day)).then_some(path)
            })
            .collect();
        days.sort();
        files.extend(days);
    }
}

fn run_writer(env_id: i64, slug: &str, dir: &Path, rx: Receiver<Job>, idle: Duration, followups: &FollowupSender, disk: &crate::storage::budget::State) {
    let mut store = match Store::open(dir, slug) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(tenant = slug, error = %e, "cannot open tenant file");
            // Answer what is already queued, then exit: the spawn wrapper
            // unregisters this writer, so the next job starts a new one that
            // opens the file again. Answering until the senders went away
            // never ended (the registry holds a sender), so one transient
            // failure (a lock held past busy_timeout while the new columns
            // were added) refused that tenant's ingest until a restart.
            while let Ok(job) = rx.try_recv() {
                fail(job, format!("cannot open tenant: {e}"));
            }
            return;
        }
    };
    tracing::debug!(tenant = slug, "writer opened");
    loop {
        let first = match rx.recv_timeout(idle) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                tracing::debug!(tenant = slug, "writer idle, closing");
                return;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut jobs = vec![first];
        let mut rows = row_count(&jobs[0]);
        while jobs.len() < MAX_COALESCE_BATCHES && rows < MAX_COALESCE_ROWS {
            match rx.try_recv() {
                Ok(job) => {
                    rows += row_count(&job);
                    jobs.push(job);
                }
                Err(_) => break,
            }
        }
        process(env_id, slug, &mut store, jobs, followups);
        disk.wrote(store.take_written());
    }
}

fn row_count(job: &Job) -> usize {
    match job {
        Job::Write { batch, .. } => batch.row_count().max(1),
        Job::LookupReceipt { .. } | Job::Release { .. } => 0,
    }
}

fn fail(job: Job, message: String) {
    match job {
        Job::Write { reply, .. } => {
            let _ = reply.send(Err(WriteError::Internal(message)));
        }
        Job::LookupReceipt { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        Job::Release { reply } => {
            let _ = reply.send(());
        }
    }
}

type Pending = (oneshot::Sender<Result<Written, WriteError>>, Result<Written, WriteError>, Option<Followup>);

fn process(env_id: i64, slug: &str, store: &mut Store, jobs: Vec<Job>, followups: &FollowupSender) {
    // Lookups first: they read committed state, which is all there is
    // between transactions.
    let mut writes = Vec::new();
    for job in jobs {
        match job {
            Job::LookupReceipt { producer_id, delivery_id, reply } => {
                let _ = reply.send(lookup_receipt(store.meta(), &producer_id, &delivery_id).map_err(|e| e.to_string()));
            }
            Job::Release { reply } => {
                store.drop_deleted();
                let _ = reply.send(());
            }
            w @ Job::Write { .. } => writes.push(w),
        }
    }
    if writes.is_empty() {
        return;
    }
    let started = Instant::now();
    let begin = store.begin();
    let lock_ms = started.elapsed().as_millis() as u64;
    if let Err(e) = begin {
        let busy = matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::DatabaseBusy);
        for job in writes {
            match job {
                Job::Write { reply, .. } => {
                    let _ = reply.send(Err(if busy { WriteError::Busy } else { WriteError::Internal(e.to_string()) }));
                }
                Job::LookupReceipt { .. } | Job::Release { .. } => unreachable!(),
            }
        }
        return;
    }
    let mut pending: Vec<Pending> = Vec::new();
    for job in writes {
        let Job::Write { mut batch, gc_floor_micros, reply } = job else { unreachable!() };
        let result = store.write(&mut batch, gc_floor_micros);
        let followup = result.as_ref().ok().map(|w| Followup::from_batch(env_id, slug, &batch, w));
        tracing::debug!(tenant = slug, records = batch.record_count, ok = result.is_ok(), "batch written");
        pending.push((reply, result, followup));
    }
    let n = pending.len();
    match store.commit() {
        Ok(()) => {
            for (reply, result, followup) in pending {
                if let Some(f) = followup {
                    followups.send(f);
                }
                let _ = reply.send(result);
            }
        }
        Err(e) => {
            store.rollback();
            tracing::error!(tenant = slug, error = %e, "commit failed");
            for (reply, _, _) in pending {
                let _ = reply.send(Err(WriteError::Internal(format!("commit failed: {e}"))));
            }
        }
    }
    tracing::debug!(tenant = slug, batches = n, lock_ms, ms = started.elapsed().as_millis() as u64, "transaction committed");
}

fn lookup_receipt(conn: &Connection, producer_id: &str, delivery_id: &str) -> rusqlite::Result<Option<StoredReceipt>> {
    conn.prepare_cached("SELECT body_sha256, metadata_sha256, ack, committed_at FROM ingest_receipts WHERE producer_id = ? AND delivery_id = ? LIMIT 1")?
        .query_row([producer_id, delivery_id], |r| {
            Ok(StoredReceipt { body_sha256: r.get(0)?, metadata_sha256: r.get(1)?, ack: r.get(2)?, committed_at: r.get(3)? })
        })
        .optional()
}
