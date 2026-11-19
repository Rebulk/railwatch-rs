//! Read connections to tenant files, one small pool per tenant.
//!
//! Every connection is opened read-only (`SQLITE_OPEN_READ_ONLY` plus
//! `PRAGMA query_only`), so a dashboard page can never take the write lock
//! that ingest's writer thread holds; in WAL mode readers never block the
//! writer and the writer never blocks them. Prepared statements are cached
//! per connection (rusqlite's statement cache), so a page's SQL is parsed
//! once per connection, not per request.
//!
//! mmap_size is 16 GiB (a ceiling, not an allocation): page reads become
//! memory loads from the OS page cache instead of read() copies, which was
//! the largest single cost of the 7-day pages on a 2.4 GB tenant (perf:
//! _copy_to_iter 12%). Rails' tenant connections use 128 MiB.
//!
//! Each connection costs memory of its own (docs/PERF.md, "Memory bounded
//! by configuration"): a union reader over a 30-day tenant held ~7 MB of
//! page cache, prepared statements and parsed schema. So readers are capped
//! across all tenants (`max_open`, the `max_read_connections` setting), an
//! idle one is closed after IDLE_CLOSE (`close_idle`), and a tenant that needs
//! a reader while the cap is reached closes another tenant's idle one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a reader may sit idle in its pool before `close_idle` closes it.
pub const IDLE_CLOSE: Duration = Duration::from_secs(60);
/// Readers one tenant may have open at once.
pub const PER_TENANT: usize = 4;
/// Readers open across all tenants, by default (`max_read_connections`).
/// One overview page takes 8 at once (overview_show_parallel); at 8 in all,
/// pages over several tenants kept closing each other's readers, and a
/// union reader takes ~100 ms to open.
pub const MAX_OPEN: usize = 16;
/// How long one checkout (a page, an API call, an MCP tool) may run
/// queries on a pooled reader before SQLite interrupts it (GA review
/// 2026-11-19 T6): readers are shared across tenants, and one tenant's
/// unbounded query must not hold them. The slowest real page measured
/// 2.7 s on a large tenant.
pub const QUERY_LIMIT: Duration = Duration::from_secs(30);

/// Monotonic nanoseconds since the first call, for the deadlines.
fn now_nanos() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Has the connection interrupt its statement once `deadline` (now_nanos;
/// 0 is none) has passed. Checked every few thousand VM steps.
fn install_deadline(conn: &mut Connection) -> rusqlite::Result<Arc<std::sync::atomic::AtomicU64>> {
    let deadline = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let d = deadline.clone();
    conn.progress_handler(
        10_000,
        Some(move || {
            let at = d.load(std::sync::atomic::Ordering::Relaxed);
            at != 0 && now_nanos() > at
        }),
    )?;
    Ok(deadline)
}

use rusqlite::{Connection, OpenFlags};

pub fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI)?;
    conn.execute_batch("PRAGMA query_only = ON; PRAGMA mmap_size = 17179869184; PRAGMA cache_size = -16000; PRAGMA temp_store = MEMORY;")?;
    conn.set_prepared_statement_cache_capacity(256);
    Ok(conn)
}

/// A pooled connection: the single file, or a union reader over a
/// per-day directory with the list of files it has attached.
struct Pooled {
    conn: Connection,
    /// When this checkout's queries are interrupted (install_deadline).
    deadline: Arc<std::sync::atomic::AtomicU64>,
    days: Option<(PathBuf, crate::storage::reader::Attached)>,
    /// When it last went back to its pool.
    idle_since: Instant,
}

/// Opens a reader for whatever layout the environment has.
pub fn open_tenant_reader(tenants_dir: &Path, slug: &str) -> rusqlite::Result<Connection> {
    open_pooled(tenants_dir, slug).map(|p| {
        // Not pooled (background work): no time limit.
        let mut conn = p.conn;
        let _ = conn.progress_handler(0, None::<fn() -> bool>);
        conn
    })
}

fn open_pooled(tenants_dir: &Path, slug: &str) -> rusqlite::Result<Pooled> {
    let (mut conn, days) = match crate::storage::Layout::detect(tenants_dir, slug) {
        Some(crate::storage::Layout::Days(dir)) => {
            let (conn, attached) = crate::storage::reader::open(&dir)?;
            conn.set_prepared_statement_cache_capacity(crate::storage::reader::STATEMENT_CACHE);
            (conn, Some((dir, attached)))
        }
        _ => (open_reader(&tenants_dir.join(format!("{slug}.sqlite3")))?, None),
    };
    let deadline = install_deadline(&mut conn)?;
    Ok(Pooled { conn, deadline, days, idle_since: Instant::now() })
}

struct TenantPool {
    /// Most recently returned last.
    idle: Mutex<Vec<Pooled>>,
    open: Mutex<usize>,
    ready: Condvar,
    /// Readers open across every pool of this ReadPools.
    total: Arc<Mutex<usize>>,
    /// Its environment was deleted (`ReadPools::forget`): readers coming
    /// back are closed, not kept.
    forgotten: std::sync::atomic::AtomicBool,
}

impl TenantPool {
    /// One of this pool's readers was closed.
    fn closed(&self, n: usize) {
        *self.open.lock().unwrap() -= n;
        *self.total.lock().unwrap() -= n;
    }
}

/// A pool of read connections per tenant file, up to `per_tenant` each and
/// `max_open` in all.
#[derive(Clone)]
pub struct ReadPools {
    dir: PathBuf,
    per_tenant: usize,
    max_open: usize,
    total: Arc<Mutex<usize>>,
    pools: Arc<Mutex<HashMap<String, Arc<TenantPool>>>>,
    /// QUERY_LIMIT, unless `set_query_limit` changed it.
    query_limit: Duration,
}

/// A checked-out connection; goes back to its pool on drop.
pub struct Reader {
    conn: Option<Pooled>,
    pool: Arc<TenantPool>,
}

impl std::ops::Deref for Reader {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn.as_ref().unwrap().conn
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(mut c) = self.conn.take() {
            c.deadline.store(0, std::sync::atomic::Ordering::Relaxed);
            // A reader busy while its day files were deleted (retention, the
            // disk budget) is closed on return rather than kept idle: its
            // handles would hold the deleted files' space until its next
            // checkout. Checked under the idle lock, so it cannot slip past
            // a concurrent `release_deleted`.
            let mut idle = self.pool.idle.lock().unwrap();
            if c.attaches_deleted() || self.pool.forgotten.load(std::sync::atomic::Ordering::SeqCst) {
                drop(idle);
                drop(c);
                self.pool.closed(1);
            } else {
                c.idle_since = Instant::now();
                idle.push(c);
            }
            self.pool.ready.notify_one();
        }
    }
}

impl Pooled {
    fn attaches_deleted(&self) -> bool {
        self.days.as_ref().is_some_and(|(_, (attached, _))| attached.iter().any(|f| !f.exists()))
    }
}

impl ReadPools {
    /// Up to `per_tenant` readers per tenant, and as many in all.
    pub fn new(dir: PathBuf, per_tenant: usize) -> ReadPools {
        ReadPools::with_max(dir, per_tenant, per_tenant)
    }

    /// Up to `per_tenant` readers per tenant and `max_open` across tenants.
    pub fn with_max(dir: PathBuf, per_tenant: usize, max_open: usize) -> ReadPools {
        ReadPools {
            dir,
            per_tenant: per_tenant.max(1),
            max_open: max_open.max(1),
            total: Default::default(),
            pools: Default::default(),
            query_limit: QUERY_LIMIT,
        }
    }

    /// How long one checkout may run queries (QUERY_LIMIT by default).
    pub fn set_query_limit(&mut self, limit: Duration) {
        self.query_limit = limit;
    }

    /// Readers open now, idle or checked out, across tenants.
    pub fn open_count(&self) -> usize {
        *self.total.lock().unwrap()
    }

    /// Closes readers idle for longer than `after`, and forgets tenants
    /// with none open (a deleted environment's pool). Returns how many.
    pub fn close_idle(&self, after: Duration) -> usize {
        self.pools.lock().unwrap().retain(|_, p| *p.open.lock().unwrap() > 0 || Arc::strong_count(p) > 1);
        let pools: Vec<Arc<TenantPool>> = self.pools.lock().unwrap().values().cloned().collect();
        let mut closed = 0;
        for pool in pools {
            let mut idle = pool.idle.lock().unwrap();
            let (gone, kept): (Vec<Pooled>, Vec<Pooled>) = std::mem::take(&mut *idle).into_iter().partition(|c| c.idle_since.elapsed() >= after);
            *idle = kept;
            drop(idle);
            if !gone.is_empty() {
                pool.closed(gone.len());
                closed += gone.len();
                drop(gone);
                pool.ready.notify_all();
            }
        }
        closed
    }

    /// A thread that runs `close_idle(IDLE_CLOSE)` every few seconds, for as
    /// long as the process runs.
    pub fn spawn_idle_closer(&self) {
        let pools = self.clone();
        let _ = std::thread::Builder::new().name("idle-readers".into()).spawn(move || {
            loop {
                std::thread::sleep(IDLE_CLOSE / 6);
                pools.close_idle(IDLE_CLOSE);
            }
        });
    }

    /// Closes the reader idle the longest in any pool but `except`, to make
    /// room under `max_open`. False when no other pool has one idle.
    fn close_oldest_idle(&self, except: &Arc<TenantPool>) -> bool {
        let pools: Vec<Arc<TenantPool>> = self.pools.lock().unwrap().values().filter(|p| !Arc::ptr_eq(p, except)).cloned().collect();
        let oldest = pools.iter().filter_map(|p| p.idle.lock().unwrap().first().map(|c| (c.idle_since, p.clone()))).min_by_key(|(at, _)| *at);
        let Some((_, pool)) = oldest else { return false };
        let mut idle = pool.idle.lock().unwrap();
        if idle.is_empty() {
            // Taken meanwhile: the caller tries again.
            return true;
        }
        let c = idle.remove(0);
        drop(idle);
        drop(c);
        pool.closed(1);
        true
    }

    /// Closes idle union readers that attach a deleted day file (retention
    /// or the disk budget just deleted it): an open handle keeps an unlinked
    /// file's blocks allocated, and an idle reader would only re-attach on
    /// its next checkout. Readers checked out now close on return (Drop).
    pub fn release_deleted(&self) -> usize {
        let pools: Vec<Arc<TenantPool>> = self.pools.lock().unwrap().values().cloned().collect();
        let mut closed = 0;
        for pool in pools {
            let mut idle = pool.idle.lock().unwrap();
            let (gone, kept): (Vec<Pooled>, Vec<Pooled>) = std::mem::take(&mut *idle).into_iter().partition(Pooled::attaches_deleted);
            *idle = kept;
            drop(idle);
            pool.closed(gone.len());
            closed += gone.len();
            drop(gone);
            pool.ready.notify_all();
        }
        closed
    }

    /// The environment `slug` is being deleted: its idle readers close now,
    /// and readers checked out close when they come back (their pool is no
    /// longer the slug's). Call before its files are removed, so no handle
    /// to them outlives the delete. Returns how many closed now.
    pub fn forget(&self, slug: &str) -> usize {
        let Some(pool) = self.pools.lock().unwrap().remove(slug) else { return 0 };
        pool.forgotten.store(true, std::sync::atomic::Ordering::SeqCst);
        let gone = std::mem::take(&mut *pool.idle.lock().unwrap());
        let n = gone.len();
        drop(gone);
        pool.closed(n);
        pool.ready.notify_all();
        n
    }

    /// A reader handed out, its deadline armed for this checkout.
    fn checked_out(&self, c: Pooled, pool: Arc<TenantPool>) -> Reader {
        c.deadline.store(now_nanos() + self.query_limit.as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        Reader { conn: Some(c), pool }
    }

    pub fn get(&self, slug: &str) -> rusqlite::Result<Reader> {
        let pool = self
            .pools
            .lock()
            .unwrap()
            .entry(slug.to_string())
            .or_insert_with(|| {
                Arc::new(TenantPool {
                    idle: Mutex::new(vec![]),
                    open: Mutex::new(0),
                    ready: Condvar::new(),
                    total: self.total.clone(),
                    forgotten: Default::default(),
                })
            })
            .clone();
        loop {
            let popped = pool.idle.lock().unwrap().pop();
            if let Some(mut c) = popped {
                // A day file was created or deleted since this reader attached.
                if let Some((dir, attached)) = c.days.as_mut()
                    && let Err(e) = crate::storage::reader::refresh(&c.conn, dir, attached)
                {
                    drop(c);
                    pool.closed(1);
                    return Err(e);
                }
                return Ok(self.checked_out(c, pool));
            }
            {
                let mut open = pool.open.lock().unwrap();
                if *open < self.per_tenant {
                    let mut total = self.total.lock().unwrap();
                    if *total < self.max_open {
                        *open += 1;
                        *total += 1;
                        drop((open, total));
                        return match open_pooled(&self.dir, slug) {
                            Ok(c) => Ok(self.checked_out(c, pool)),
                            Err(e) => {
                                pool.closed(1);
                                Err(e)
                            }
                        };
                    }
                    drop((open, total));
                    // At the cap: another tenant's idle reader makes room.
                    if self.close_oldest_idle(&pool) {
                        continue;
                    }
                }
            }
            let idle = pool.idle.lock().unwrap();
            let _unused = pool.ready.wait_timeout(idle, std::time::Duration::from_millis(50)).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::layout::{Tier, day_path, meta_path, open_or_create};

    fn tenants(name: &str, slugs: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rw-pool-{name}-{}", crate::control::base58(8)));
        for slug in slugs {
            let dir = root.join(slug);
            open_or_create(&meta_path(&dir)).unwrap();
            open_or_create(&day_path(&dir, Tier::Core, "2026-10-14")).unwrap();
        }
        root
    }

    fn sqlite_fds(root: &Path) -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().flatten().filter_map(|e| std::fs::read_link(e.path()).ok()).filter(|t| t.starts_with(root)).count()
    }

    /// A reader idle past the timeout is closed, with its file handles, and
    /// a later checkout opens a fresh one; a busy reader is never closed.
    #[test]
    fn idle_readers_are_closed_and_their_files_released() {
        let root = tenants("idle", &["a"]);
        let pools = ReadPools::with_max(root.clone(), 2, 8);
        let busy = pools.get("a").unwrap();
        drop(pools.get("a").unwrap());
        assert_eq!(pools.open_count(), 2);
        assert!(sqlite_fds(&root) > 0);
        assert_eq!(pools.close_idle(Duration::from_secs(3600)), 0, "nothing idle that long");
        assert_eq!(pools.close_idle(Duration::ZERO), 1, "the idle one, not the busy one");
        assert_eq!(pools.open_count(), 1);
        drop(busy);
        assert_eq!(pools.close_idle(Duration::ZERO), 1);
        assert_eq!((pools.open_count(), sqlite_fds(&root)), (0, 0), "no handle left to any tenant file");
        let n: i64 = pools.get("a").unwrap().query_row("SELECT count(*) FROM executions", [], |r| r.get(0)).unwrap();
        assert_eq!((n, pools.open_count()), (0, 1), "reopened on demand");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Readers across tenants never exceed max_open: a tenant at the cap
    /// takes the place of another tenant's idle reader, and waits while every
    /// reader is checked out.
    #[test]
    fn readers_are_capped_across_tenants() {
        let root = tenants("cap", &["a", "b", "c"]);
        let pools = ReadPools::with_max(root.clone(), 4, 2);
        drop(pools.get("a").unwrap());
        drop(pools.get("b").unwrap());
        assert_eq!(pools.open_count(), 2);
        let c = pools.get("c").unwrap();
        assert_eq!(pools.open_count(), 2, "a's idle reader (the oldest) made room");
        let b = pools.get("b").unwrap();
        // Both checked out: a third tenant waits until one comes back.
        let (tx, rx) = std::sync::mpsc::channel();
        let p2 = pools.clone();
        let waiter = std::thread::spawn(move || {
            let r = p2.get("a").unwrap();
            tx.send(()).unwrap();
            drop(r);
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "waits at the cap");
        assert_eq!(pools.open_count(), 2);
        drop(b);
        rx.recv_timeout(Duration::from_secs(5)).expect("gets a reader once one is idle");
        waiter.join().unwrap();
        assert!(pools.open_count() <= 2);
        drop(c);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tenant with no reader left is forgotten (its environment may be
    /// gone); one with a reader checked out is kept.
    #[test]
    fn empty_pools_are_forgotten() {
        let root = tenants("forget", &["a", "b"]);
        let pools = ReadPools::with_max(root.clone(), 2, 8);
        drop(pools.get("a").unwrap());
        let b = pools.get("b").unwrap();
        pools.close_idle(Duration::ZERO);
        pools.close_idle(Duration::ZERO);
        assert_eq!(pools.pools.lock().unwrap().keys().cloned().collect::<Vec<_>>(), vec!["b".to_string()]);
        drop(b);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One checkout cannot hold a shared reader past the time limit: a
    /// runaway query is interrupted, the reader goes back to the pool
    /// usable, and the next checkout gets a fresh allowance (T6).
    #[test]
    fn a_query_past_the_time_limit_is_interrupted_and_the_reader_stays_usable() {
        let root = tenants("deadline", &["a"]);
        let mut pools = ReadPools::with_max(root.clone(), 1, 1);
        pools.set_query_limit(Duration::from_millis(200));
        let runaway = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";
        let started = Instant::now();
        let r = pools.get("a").unwrap().query_row(runaway, [], |r| r.get::<_, i64>(0));
        assert!(matches!(r, Err(rusqlite::Error::SqliteFailure(ref f, _)) if f.code == rusqlite::ErrorCode::OperationInterrupted), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "interrupted near the limit: {:?}", started.elapsed());
        let n: i64 = pools.get("a").unwrap().query_row("SELECT count(*) FROM executions", [], |r| r.get(0)).unwrap();
        assert_eq!((n, pools.open_count()), (0, 1), "the same reader, usable again");
        let _ = std::fs::remove_dir_all(&root);
    }
}
