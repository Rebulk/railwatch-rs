//! WAL checkpoints off the ingest writer's thread (docs/PERF.md, "Ingest
//! floor"): SQLite's autocheckpoint runs inside the COMMIT that crosses 1000
//! WAL pages, with its fsync, and was 26% of a busy writer's time.
//!
//! An ingest write connection (`install`) gets a WAL hook in place of the
//! autocheckpoint. Past SOFT_PAGES it asks the one checkpointer thread to
//! checkpoint the file (PASSIVE, on a connection of its own, so it never
//! takes the writer's lock); the writer goes on committing meanwhile.
//! Past HARD_PAGES (the checkpointer is behind, or a reader pins old
//! frames) the writer checkpoints inline, as autocheckpoint would. With
//! autocheckpoint simply off, the WAL reached 5 GB in 30 s.
//!
//! A WAL only restarts from its start once every frame is checkpointed, and
//! journal_size_limit (64 MB, schema::CONNECTION_PRAGMAS) truncates it then.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};

use rusqlite::wal::CheckpointMode;
use rusqlite::{Connection, OpenFlags};

/// SQLite's default wal_autocheckpoint.
pub const SOFT_PAGES: i32 = 1000;
/// The writer checkpoints inline past this (32 MB of 4 KB pages).
pub const HARD_PAGES: i32 = 8000;

/// Checkpoints the thread has run, and those the writers ran inline.
pub static BACKGROUND: AtomicU64 = AtomicU64::new(0);
pub static INLINE: AtomicU64 = AtomicU64::new(0);
/// The minute an incomplete inline checkpoint was last logged.
static INCOMPLETE_WARNED: AtomicI64 = AtomicI64::new(-1);

struct Checkpointer {
    tx: Mutex<Sender<PathBuf>>,
    /// Files asked for and not yet done: one request per file at a time.
    pending: Mutex<HashSet<PathBuf>>,
}

fn checkpointer() -> &'static Checkpointer {
    static C: OnceLock<Checkpointer> = OnceLock::new();
    C.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        std::thread::Builder::new()
            .name("checkpointer".into())
            .spawn(move || {
                for path in rx {
                    if let Err(e) = checkpoint(&path) {
                        tracing::warn!(file = %path.display(), error = %e, "background checkpoint failed; the writer checkpoints inline past the cap");
                    }
                    checkpointer().pending.lock().unwrap().remove(&path);
                }
            })
            .expect("spawn checkpointer thread");
        Checkpointer { tx: Mutex::new(tx), pending: Mutex::new(HashSet::new()) }
    })
}

/// A connection of its own for each request, closed right after: kept
/// open, it would hold a day file that retention deletes. Never creates the
/// file (one deleted since the request is skipped).
fn checkpoint(path: &Path) -> rusqlite::Result<()> {
    let c = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX) {
        Ok(c) => c,
        Err(_) if !path.exists() => return Ok(()),
        Err(e) => return Err(e),
    };
    c.busy_timeout(std::time::Duration::from_millis(100))?;
    // Same durability as the writer's own checkpoints.
    c.execute_batch("PRAGMA synchronous = normal; PRAGMA cache_size = 16;")?;
    if std::env::var("RAILWATCH_SQLITE_SYNC").as_deref() == Ok("off") {
        c.execute_batch("PRAGMA synchronous = off;")?;
    }
    c.wal_checkpoint_v2(Some(rusqlite::MAIN_DB), CheckpointMode::PASSIVE).map(drop)?;
    BACKGROUND.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn request(path: &Path) {
    let c = checkpointer();
    if c.pending.lock().unwrap().insert(path.to_path_buf()) && c.tx.lock().unwrap().send(path.to_path_buf()).is_err() {
        c.pending.lock().unwrap().remove(path);
    }
}

/// Replaces `conn`'s autocheckpoint with the background checkpointer.
pub fn install(conn: &mut Connection, path: &Path) -> rusqlite::Result<()> {
    let path = path.to_path_buf();
    conn.wal_hook(Some(move |wal: &mut rusqlite::hooks::Wal, pages: i32| {
        if pages >= HARD_PAGES {
            // Busy means the checkpointer is on this file right now: wait
            // for it (up to ~1 s), or the cap would not hold while it is slow.
            for _ in 0..100 {
                match wal.checkpoint_v2(CheckpointMode::PASSIVE) {
                    Ok((log, done)) => {
                        INLINE.fetch_add(1, Ordering::Relaxed);
                        // A reader still needs the rest: the cap does not
                        // hold until it ends. Said once a minute at most.
                        if done < log {
                            let now = crate::time::now_micros() / 60_000_000;
                            if INCOMPLETE_WARNED.swap(now, Ordering::Relaxed) != now {
                                tracing::warn!(wal_pages = log, checkpointed = done, "WAL over its cap: a reader holds frames back");
                            }
                        }
                        break;
                    }
                    Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "inline checkpoint failed");
                        break;
                    }
                }
            }
        } else if pages >= SOFT_PAGES {
            request(&path);
        }
        // The commit has happened; a failed checkpoint must not fail it.
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_bytes(path: &Path) -> u64 {
        std::fs::metadata(format!("{}-wal", path.display())).map(|m| m.len()).unwrap_or(0)
    }

    /// Commits of ~200 pages each: the WAL is checkpointed in the
    /// background and, whatever the thread's pace, never grows past the
    /// hard cap plus one commit (rounded up for the WAL's frame headers).
    #[test]
    fn the_wal_is_checkpointed_in_the_background_and_capped() {
        let dir = std::env::temp_dir().join(format!("rw-ckpt-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite3");
        let mut c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA journal_mode = wal; PRAGMA synchronous = off; CREATE TABLE t (x BLOB);").unwrap();
        install(&mut c, &path).unwrap();
        let before = BACKGROUND.load(Ordering::Relaxed) + INLINE.load(Ordering::Relaxed);
        let blob = vec![7u8; 4000];
        let mut max_wal = 0;
        for _ in 0..200 {
            c.execute_batch("BEGIN").unwrap();
            for _ in 0..200 {
                c.execute("INSERT INTO t VALUES (?)", [&blob]).unwrap();
            }
            c.execute_batch("COMMIT").unwrap();
            max_wal = max_wal.max(wal_bytes(&path));
        }
        let cap = (HARD_PAGES as u64 + 400) * (4096 + 24);
        assert!(max_wal <= cap, "WAL reached {max_wal} bytes, cap {cap}");
        assert!(BACKGROUND.load(Ordering::Relaxed) + INLINE.load(Ordering::Relaxed) > before, "something checkpointed");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 40_000, "every committed row is there");
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The thread's own work: a file over SOFT_PAGES is checkpointed without
    /// the writer reaching the hard cap.
    #[test]
    fn a_request_is_checkpointed_by_the_thread() {
        let dir = std::env::temp_dir().join(format!("rw-ckpt-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite3");
        let mut c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA journal_mode = wal; PRAGMA synchronous = off; CREATE TABLE t (x BLOB);").unwrap();
        install(&mut c, &path).unwrap();
        let blob = vec![7u8; 4000];
        c.execute_batch("BEGIN").unwrap();
        for _ in 0..(SOFT_PAGES + 100) {
            c.execute("INSERT INTO t VALUES (?)", [&blob]).unwrap();
        }
        let before = BACKGROUND.load(Ordering::Relaxed);
        c.execute_batch("COMMIT").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while checkpointer().pending.lock().unwrap().contains(&path) || BACKGROUND.load(Ordering::Relaxed) == before {
            assert!(std::time::Instant::now() < deadline, "not checkpointed within 10 s");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The thread copied every frame: a checkpoint now has nothing to do.
        let (log, done): (i64, i64) = c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| Ok((r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(log, done);
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
