//! One background thread that owns, per tenant, a read-only connection, a
//! write connection for rollups, and the incremental accumulator. It folds
//! committed batches in, groups direct-path exceptions, flushes rollups every
//! ROLLUP_FLUSH, and runs the per-tenant jobs the scheduler hands it
//! (catch-up recompute, receipt drain, prune), so no two of them ever race
//! on the same tenant's rollups.
//!
//! Reads (the hour, the group, the receipts) go through the read-only
//! connection: under WAL they never wait for, or block, the ingest writer.
//! Rollup writes take the tenant's write lock only for each type's DELETE +
//! INSERTs, as RollupJob does.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

use super::TenantWork;
use crate::control::Control;
use crate::followups::{Followup, ROLLUP_FLUSH};
use crate::grouping;
use crate::rollup::incremental::{self, Accumulator, MAX_BYTES};
use crate::rollup::{release_health_rollup_job, rollup_job};

struct Tenant {
    read: Connection,
    /// The file rollups, release_health and receipts are written to: the
    /// single file, or a per-day tenant's meta.
    write: Connection,
    /// Per-day tenants: the directory and the files `read` has attached.
    days: Option<(PathBuf, crate::storage::reader::Attached)>,
    acc: Accumulator,
    /// Hours whose sessions changed since the last flush.
    session_buckets: HashSet<i64>,
    /// Hours to recompute in full at the next flush.
    overflow_buckets: HashSet<i64>,
    /// Hours the accumulator gave up on at its memory cap (MAX_BYTES):
    /// their rows are not accumulated; each flush recomputes them.
    full_hours: HashSet<i64>,
    users: grouping::AffectedUsers,
}

pub struct Worker {
    pub control: Arc<Control>,
    pub dir: PathBuf,
    pub tiers: crate::storage::retention::Tiers,
    /// The accumulator's memory cap per tenant (MAX_BYTES; tests lower it).
    pub acc_max_bytes: usize,
    tenants: HashMap<i64, (String, Tenant)>,
}

pub fn open_read(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI)?;
    c.busy_timeout(Duration::from_secs(5))?;
    c.execute_batch("PRAGMA cache_size = 2000; PRAGMA temp_store = MEMORY;")?;
    Ok(c)
}

impl Worker {
    pub fn new(control: Arc<Control>, dir: PathBuf) -> Worker {
        Worker { control, dir, tiers: Default::default(), acc_max_bytes: MAX_BYTES, tenants: HashMap::new() }
    }

    fn tenant(&mut self, env: i64, slug: &str) -> rusqlite::Result<&mut Tenant> {
        if !self.tenants.contains_key(&env) {
            let (read, write, days) = match crate::storage::Layout::detect(&self.dir, slug) {
                Some(crate::storage::Layout::Days(dir)) => {
                    let write = crate::tenants::open_tenant(&crate::storage::layout::meta_path(&dir))?;
                    let (read, attached) = crate::storage::reader::open(&dir)?;
                    (read, write, Some((dir, attached)))
                }
                _ => {
                    let path = self.dir.join(format!("{slug}.sqlite3"));
                    let write = crate::tenants::open_tenant(&path)?;
                    (open_read(&path)?, write, None)
                }
            };
            self.tenants.insert(
                env,
                (
                    slug.to_string(),
                    Tenant {
                        read,
                        write,
                        days,
                        acc: Accumulator::default(),
                        session_buckets: HashSet::new(),
                        overflow_buckets: HashSet::new(),
                        full_hours: HashSet::new(),
                        users: Default::default(),
                    },
                ),
            );
        }
        let t = &mut self.tenants.get_mut(&env).unwrap().1;
        if let Some((dir, attached)) = t.days.as_mut() {
            crate::storage::reader::refresh(&t.read, dir, attached)?;
        }
        Ok(t)
    }

    /// Drops a tenant's connections (its files are about to be deleted or
    /// replaced).
    pub fn forget(&mut self, env: i64) {
        self.tenants.remove(&env);
    }

    pub fn run(mut self, rx: Receiver<TenantWork>) {
        let mut last_flush = Instant::now();
        loop {
            match rx.recv_timeout(ROLLUP_FLUSH.saturating_sub(last_flush.elapsed()).max(Duration::from_millis(50))) {
                Ok(w) => self.handle(w),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush_all();
                    return;
                }
            }
            if last_flush.elapsed() >= ROLLUP_FLUSH {
                self.flush_all();
                last_flush = Instant::now();
            }
        }
    }

    fn handle(&mut self, w: TenantWork) {
        let started = Instant::now();
        let what = w.name();
        if let Err(e) = self.handle_inner(w) {
            tracing::error!(work = what, error = %e, "background work failed");
        }
        tracing::debug!(work = what, ms = started.elapsed().as_millis() as u64, "background work");
    }

    fn handle_inner(&mut self, w: TenantWork) -> Result<(), String> {
        let e = |e: rusqlite::Error| e.to_string();
        match w {
            TenantWork::Committed(f) => self.committed(f),
            TenantWork::Recompute { env, slug, buckets } => {
                let t = self.tenant(env, &slug).map_err(e)?;
                for b in buckets {
                    rollup_job::perform(&t.read, &t.write, b, &mut rollup_job::Rng::PerGroup).map_err(e)?;
                }
                Ok(())
            }
            TenantWork::RecomputeSessions { env, slug, buckets, retention_floor } => {
                let t = self.tenant(env, &slug).map_err(e)?;
                for b in buckets {
                    release_health_rollup_job::perform(&t.read, &t.write, b, retention_floor).map_err(e)?;
                }
                Ok(())
            }
            TenantWork::Drain { env, slug } => {
                let control = self.control.clone();
                let t = self.tenant(env, &slug).map_err(e)?;
                super::drain::drain(&control, (&t.read, &t.write), env, &mut t.users).map(|_| ())
            }
            TenantWork::Prune { env, slug } => {
                let control = self.control.clone();
                let tiers = self.tiers;
                let t = self.tenant(env, &slug).map_err(e)?;
                t.acc.clear();
                t.users.clear();
                match t.days.as_ref().map(|(d, _)| d.clone()) {
                    Some(dir) => super::prune::prune_days(&control, &dir, &t.write, env, tiers).map(|_| ()),
                    None => super::prune::prune(&control, &t.write, env).map(|_| ()),
                }
            }
            TenantWork::Trimmed { env, slug } => {
                // Exceptions went with the files: the kept affected-user
                // sets would overcount, as after a prune.
                self.tenant(env, &slug).map_err(e)?.users.clear();
                Ok(())
            }
            TenantWork::With { env, slug, f } => {
                let t = self.tenant(env, &slug).map_err(e)?;
                f(&t.read, &t.write);
                Ok(())
            }
        }
    }

    fn committed(&mut self, f: Followup) -> Result<(), String> {
        let e = |e: rusqlite::Error| e.to_string();
        let control = self.control.clone();
        let max_bytes = self.acc_max_bytes;
        let t = self.tenant(f.env_id, &f.slug).map_err(e)?;
        // Direct path: GroupExceptionsJob. Receipted: the drain, under claims.
        if !f.exception_ids.is_empty() && !f.receipted {
            let rows = grouping::load_exceptions(&t.read, &f.exception_ids).map_err(e)?;
            let conn = control.conn();
            grouping::group_with(&conn, &t.read, f.env_id, rows, None, &mut t.users)?;
        }
        if t.acc.bytes() > max_bytes {
            // The hours it holds are too big to keep in memory: they leave
            // the accumulator for good and are recomputed in full at each
            // flush while rows arrive (RollupJob, same output), until they
            // age out like any hour. Reloading them group by group instead
            // would refill it with the same hour on the next batch.
            let hours = t.acc.touched_buckets();
            tracing::info!(
                tenant = f.slug,
                rows = t.acc.row_count(),
                bytes = t.acc.bytes(),
                hours = hours.len(),
                "rollup accumulator at its memory cap; recomputing its hours in full"
            );
            t.overflow_buckets.extend(hours.iter().copied());
            t.full_hours.extend(hours);
            t.acc.clear();
        }
        for (table, ids) in &f.rolled_ids {
            if *table != "sessions" {
                t.acc.ingest_except(&t.read, table, ids, &t.full_hours).map_err(e)?;
            }
        }
        t.overflow_buckets.extend(f.rollup_buckets.iter().filter(|b| t.full_hours.contains(b)));
        t.session_buckets.extend(f.session_buckets.iter().copied());
        Ok(())
    }

    fn flush_all(&mut self) {
        let started = Instant::now();
        let now = crate::time::now_micros() / 1_000_000;
        for (env, (slug, t)) in self.tenants.iter_mut() {
            let r = (|| -> rusqlite::Result<()> {
                incremental::flush(&mut t.acc, &t.write)?;
                for b in std::mem::take(&mut t.overflow_buckets) {
                    rollup_job::perform(&t.read, &t.write, b, &mut rollup_job::Rng::PerGroup)?;
                }
                for b in std::mem::take(&mut t.session_buckets) {
                    release_health_rollup_job::perform(&t.read, &t.write, b, i64::MIN)?;
                }
                // Hours older than the catch-up window leave memory.
                let before = now - now.rem_euclid(3600) - 3600;
                t.acc.evict_before(before);
                t.full_hours.retain(|h| h + 3600 > before);
                Ok(())
            })();
            if let Err(e) = r {
                tracing::error!(environment = env, tenant = slug, error = %e, "rollup flush failed");
            }
        }
        if !self.tenants.is_empty() {
            tracing::debug!(ms = started.elapsed().as_millis() as u64, "rollup flush");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At its memory cap the accumulator lets go of the hour it holds; that
    /// hour is recomputed in full from then on, so the rollups are the same
    /// as if nothing had been capped, and the memory stays under the cap.
    #[test]
    fn at_the_memory_cap_the_hour_is_recomputed_in_full_with_the_same_rollups() {
        let dir = std::env::temp_dir().join(format!("rw-acc-cap-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let control = Arc::new(Control::open(&dir.join("control.sqlite3")).unwrap());
        let env = control.create("capped", &format!("{:0>64}", 1), 30).unwrap();
        let path = dir.join("capped.sqlite3");
        let t = crate::tenants::open_tenant(&path).unwrap();
        let now = crate::time::now_micros() / 1_000_000;
        let hour = now - now.rem_euclid(3600);
        let mut w = Worker::new(control, dir.clone());
        w.acc_max_bytes = 64 << 10;
        let mut peak = 0;
        for batch in 0..40 {
            let mut ids = Vec::new();
            for i in 0..25 {
                t.execute(
                    "INSERT INTO queries (occurred_at, duration, group_hash, sql) VALUES (?, ?, ?, 'SELECT 1')",
                    rusqlite::params![crate::time::quoted_date_micros((hour + batch * 60 + i) * 1_000_000), (batch * 7 + i) % 500 + 1, format!("q{}", i % 6)],
                )
                .unwrap();
                ids.push(t.last_insert_rowid());
            }
            let f = Followup {
                env_id: env.id,
                slug: "capped".into(),
                accepted: ids.len() as i64,
                committed_at_micros: now * 1_000_000,
                rollup_buckets: vec![hour],
                session_buckets: vec![],
                exception_ids: vec![],
                counts: vec![],
                receipted: false,
                rolled_ids: vec![("queries", ids)],
                servers: vec![],
            };
            w.committed(f).unwrap();
            let acc = &w.tenants[&env.id].1.acc;
            peak = peak.max(acc.bytes());
            if batch % 7 == 0 {
                w.flush_all();
            }
        }
        w.flush_all();
        let tenant = &w.tenants[&env.id].1;
        assert!(tenant.full_hours.contains(&hour), "the cap was reached");
        assert_eq!(tenant.acc.row_count(), 0, "the capped hour is not accumulated again");
        assert!(peak <= (64 << 10) + 25 * 1024, "held at most the cap plus one batch: {peak}");
        let dump = |c: &Connection| -> Vec<Vec<rusqlite::types::Value>> {
            let mut s = c
                .prepare("SELECT record_type, group_hash, bucket, name, count, duration_sum, duration_max, p50, p95, p99, digest FROM rollups ORDER BY 1, 2, 3")
                .unwrap();
            s.query_map([], |r| (0..11).map(|i| r.get(i)).collect()).unwrap().map(Result::unwrap).collect()
        };
        let capped = dump(&t);
        assert_eq!(capped.iter().map(|r| r[4].clone()).map(|v| if let rusqlite::types::Value::Integer(n) = v { n } else { 0 }).sum::<i64>(), 1000);
        rollup_job::perform(&t, &t, hour, &mut rollup_job::Rng::PerGroup).unwrap();
        assert_eq!(capped, dump(&t), "same rollups as a full recompute");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
