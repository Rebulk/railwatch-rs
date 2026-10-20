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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

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
    days: Option<(PathBuf, crate::storage::reader::Attached)>,
}

/// Opens a reader for whatever layout the environment has.
pub fn open_tenant_reader(tenants_dir: &Path, slug: &str) -> rusqlite::Result<Connection> {
    open_pooled(tenants_dir, slug).map(|p| p.conn)
}

fn open_pooled(tenants_dir: &Path, slug: &str) -> rusqlite::Result<Pooled> {
    match crate::storage::Layout::detect(tenants_dir, slug) {
        Some(crate::storage::Layout::Days(dir)) => {
            let (conn, attached) = crate::storage::reader::open(&dir)?;
            conn.set_prepared_statement_cache_capacity(crate::storage::reader::STATEMENT_CACHE);
            Ok(Pooled { conn, days: Some((dir, attached)) })
        }
        _ => Ok(Pooled { conn: open_reader(&tenants_dir.join(format!("{slug}.sqlite3")))?, days: None }),
    }
}

struct TenantPool {
    idle: Mutex<Vec<Pooled>>,
    open: Mutex<usize>,
    ready: Condvar,
}

/// A pool of read connections per tenant file, up to `per_tenant` each.
#[derive(Clone)]
pub struct ReadPools {
    dir: PathBuf,
    per_tenant: usize,
    pools: Arc<Mutex<HashMap<String, Arc<TenantPool>>>>,
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
        if let Some(c) = self.conn.take() {
            self.pool.idle.lock().unwrap().push(c);
            self.pool.ready.notify_one();
        }
    }
}

impl ReadPools {
    pub fn new(dir: PathBuf, per_tenant: usize) -> ReadPools {
        ReadPools { dir, per_tenant: per_tenant.max(1), pools: Default::default() }
    }

    pub fn get(&self, slug: &str) -> rusqlite::Result<Reader> {
        let pool = self
            .pools
            .lock()
            .unwrap()
            .entry(slug.to_string())
            .or_insert_with(|| Arc::new(TenantPool { idle: Mutex::new(vec![]), open: Mutex::new(0), ready: Condvar::new() }))
            .clone();
        loop {
            let popped = pool.idle.lock().unwrap().pop();
            if let Some(mut c) = popped {
                // A day file was created or deleted since this reader attached.
                if let Some((dir, attached)) = c.days.as_mut()
                    && let Err(e) = crate::storage::reader::refresh(&c.conn, dir, attached)
                {
                    *pool.open.lock().unwrap() -= 1;
                    return Err(e);
                }
                return Ok(Reader { conn: Some(c), pool });
            }
            {
                let mut open = pool.open.lock().unwrap();
                if *open < self.per_tenant {
                    *open += 1;
                    drop(open);
                    return match open_pooled(&self.dir, slug) {
                        Ok(c) => Ok(Reader { conn: Some(c), pool }),
                        Err(e) => {
                            *pool.open.lock().unwrap() -= 1;
                            Err(e)
                        }
                    };
                }
            }
            let idle = pool.idle.lock().unwrap();
            let _unused = pool.ready.wait_timeout(idle, std::time::Duration::from_millis(50)).unwrap();
        }
    }
}
