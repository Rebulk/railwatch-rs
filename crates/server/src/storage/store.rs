//! What the tenant writer thread (tenants.rs) writes through: the Rails
//! single file, or a per-day directory. Same transaction shape for both:
//! begin, a savepoint per batch, commit.

use std::path::Path;

use rusqlite::Connection;

use super::layout::{self, Layout};
use super::writer::DayWriter;
use crate::ingest::batch::Prepared;
use crate::ingest::writer::{self, WriteError, Written};

pub enum Store {
    Single(Connection),
    Days(Box<DayWriter>),
}

impl Store {
    /// Opens the environment's storage, creating a per-day directory when
    /// it has none yet (new environments are per-day; an existing single
    /// file stays one until `railwatch split`).
    pub fn open(tenants_dir: &Path, slug: &str) -> rusqlite::Result<Store> {
        match Layout::detect(tenants_dir, slug) {
            Some(Layout::Single(path)) => crate::tenants::open_tenant(&path).map(Store::Single),
            Some(Layout::Days(dir)) => DayWriter::open(&dir).map(|w| Store::Days(Box::new(w))),
            None => {
                let dir = tenants_dir.join(slug);
                layout::ensure(&dir)?;
                DayWriter::open(&dir).map(|w| Store::Days(Box::new(w)))
            }
        }
    }

    /// The connection receipts are read from.
    pub fn meta(&self) -> &Connection {
        match self {
            Store::Single(c) => c,
            Store::Days(w) => w.meta_conn(),
        }
    }

    pub fn begin(&mut self) -> rusqlite::Result<()> {
        match self {
            Store::Single(c) => c.execute_batch("BEGIN IMMEDIATE"),
            Store::Days(w) => w.begin(),
        }
    }

    /// One batch in its own savepoint: a batch that fails rolls back alone.
    pub fn write(&mut self, batch: &mut Prepared, gc_floor_micros: i64) -> Result<Written, WriteError> {
        match self {
            Store::Single(c) => {
                c.execute_batch("SAVEPOINT batch")?;
                match writer::write(c, batch, gc_floor_micros) {
                    Ok(v) => {
                        c.execute_batch("RELEASE batch")?;
                        Ok(v)
                    }
                    Err(e) => {
                        c.execute_batch("ROLLBACK TO batch; RELEASE batch")?;
                        Err(e)
                    }
                }
            }
            Store::Days(w) => {
                w.savepoint()?;
                match writer::write_to(w.as_mut(), batch, gc_floor_micros) {
                    Ok(v) => {
                        w.release()?;
                        Ok(v)
                    }
                    Err(e) => {
                        w.rollback_to()?;
                        Err(e)
                    }
                }
            }
        }
    }

    pub fn commit(&mut self) -> rusqlite::Result<()> {
        match self {
            Store::Single(c) => c.execute_batch("COMMIT"),
            Store::Days(w) => w.commit(),
        }
    }

    /// Bytes the commits since the last call wrote (budget::take_written).
    pub fn take_written(&mut self) -> u64 {
        match self {
            Store::Single(c) => super::budget::take_written(c),
            Store::Days(w) => w.take_written(),
        }
    }

    /// Closes cached files that no longer exist (DayWriter::drop_deleted).
    pub fn drop_deleted(&mut self) {
        if let Store::Days(w) = self {
            w.drop_deleted();
        }
    }

    pub fn rollback(&mut self) {
        match self {
            Store::Single(c) => {
                let _ = c.execute_batch("ROLLBACK");
            }
            Store::Days(w) => w.rollback(),
        }
    }
}
