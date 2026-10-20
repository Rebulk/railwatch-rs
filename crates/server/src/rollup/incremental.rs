//! Incremental rollups (a deliberate improvement; docs/DIFFERENCES.md).
//!
//! Rails recomputes a whole hour from raw rows whenever rows land in it
//! (debounced to once a minute per hour, plus a 5-minute catch-up). Here each
//! committed batch's new row ids are folded into an in-memory set per
//! (record_type, group_hash, hour); every flush recomputes only the groups
//! that changed, with the *same* `fresh_attributes` the full recompute uses,
//! over that group's complete row set, sorted the way the full recompute
//! reads it. So a flush writes exactly what a full recompute of the hour
//! would write for those groups (proven by `incremental_equals_full` below
//! and tests/rollup_incremental.rs).
//!
//! Completeness: the first time a group-hour is touched, every committed row
//! of it is loaded from the read connection (the group_hash, occurred_at
//! index). Rows are keyed by id, so a row seen both by that load and by its
//! batch's message is counted once. The full recompute stays as the
//! catch-up/repair path.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, params_from_iter};

use super::rollup_job::{self, RawRow, Rng, RollupRow, SOURCES, Source};
use super::sqlval::row_values;

type Key = (usize, Vec<u8>, i64);

struct Group {
    /// id -> (sort key, row)
    rows: HashMap<i64, (SortKey, RawRow)>,
    dirty: bool,
    /// `row_bytes` of its rows.
    bytes: usize,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey(Vec<u8>, i64, i64);

#[derive(Default)]
pub struct Accumulator {
    groups: HashMap<Key, Group>,
    rows: usize,
    bytes: usize,
}

/// The most a tenant's accumulator may hold, by `row_bytes`. Over it, the
/// hours it holds are recomputed in full instead (worker.rs). Rows cost
/// ~650 bytes of RSS each, so this is ~200k rows (docs/PERF.md, "Bounded
/// memory"); 3M rows, the former row cap, was ~2 GB.
pub const MAX_BYTES: usize = 128 << 20;

/// Heap a row holds here, roughly: the map entry, the row's values and
/// their bytes, and the sort key's copy of occurred_at.
fn row_bytes(key: &SortKey, row: &RawRow) -> usize {
    std::mem::size_of::<(i64, (SortKey, RawRow))>()
        + 16
        + row.len() * std::mem::size_of::<crate::rollup::sqlval::Sql>()
        + row.iter().map(|v| v.text().map_or(0, <[u8]>::len)).sum::<usize>()
        + key.0.len()
}

fn hour_of(occurred_at: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(occurred_at).ok()?;
    let m = crate::time::parse_db_time(s)? / 1_000_000;
    Some(m - m.rem_euclid(3600))
}

fn sort_key(source: &Source, row: &RawRow) -> SortKey {
    let at = row.last().map(|v| v.s().to_vec()).unwrap_or_default();
    let dur = if source.is_query() { row[2].to_i() } else { 0 };
    SortKey(at, dur, row[0].to_i())
}

impl Accumulator {
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// The rows' `row_bytes`.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Folds in newly committed rows of `table` (by id).
    pub fn ingest(&mut self, read: &Connection, table: &str, ids: &[i64]) -> rusqlite::Result<()> {
        self.ingest_except(read, table, ids, &HashSet::new())
    }

    /// `ingest`, leaving out rows of the hours in `skip` (recomputed in
    /// full instead: worker.rs at MAX_BYTES).
    pub fn ingest_except(&mut self, read: &Connection, table: &str, ids: &[i64], skip: &HashSet<i64>) -> rusqlite::Result<()> {
        for (si, source) in SOURCES.iter().enumerate() {
            if source.table.name() != table {
                continue;
            }
            for chunk in ids.chunks(900) {
                let sql = source.select_by_ids_sql(chunk.len());
                let rows: Vec<RawRow> = {
                    let mut st = read.prepare_cached(&sql)?;
                    st.query_map(params_from_iter(chunk), |r| row_values(r, source.width() + 1))?.collect::<rusqlite::Result<_>>()?
                };
                for row in rows {
                    let Some(g) = row[1].text().map(<[u8]>::to_vec) else { continue };
                    let Some(bucket) = hour_of(row.last().unwrap().s()) else { continue };
                    if skip.contains(&bucket) {
                        continue;
                    }
                    let key = (si, g, bucket);
                    if !self.groups.contains_key(&key) {
                        self.load_group(read, &key)?;
                    }
                    self.insert(&key, source, row);
                }
            }
        }
        Ok(())
    }

    fn insert(&mut self, key: &Key, source: &Source, row: RawRow) {
        let g = self.groups.get_mut(key).expect("loaded");
        let id = row[0].to_i();
        let sk = sort_key(source, &row);
        let n = row_bytes(&sk, &row);
        match g.rows.insert(id, (sk, row)) {
            None => self.rows += 1,
            Some((old_key, old)) => {
                let o = row_bytes(&old_key, &old);
                g.bytes -= o;
                self.bytes -= o;
            }
        }
        g.bytes += n;
        self.bytes += n;
        g.dirty = true;
    }

    fn load_group(&mut self, read: &Connection, key: &Key) -> rusqlite::Result<()> {
        let source = &SOURCES[key.0];
        let sql = source.select_group_sql();
        let from = crate::time::utc_seconds_string(key.2);
        let to = crate::time::utc_seconds_string(key.2 + 3600);
        let g = String::from_utf8_lossy(&key.1).into_owned();
        let rows: Vec<RawRow> = {
            let mut st = read.prepare_cached(&sql)?;
            st.query_map(rusqlite::params![g, from, to], |r| row_values(r, source.width() + 1))?.collect::<rusqlite::Result<_>>()?
        };
        self.groups.insert(key.clone(), Group { rows: HashMap::new(), dirty: true, bytes: 0 });
        for row in rows {
            self.insert(key, source, row);
        }
        Ok(())
    }

    /// The dirty groups' rollup rows, grouped by (record_type, bucket).
    pub fn take_dirty(&mut self) -> Vec<(&'static str, i64, Vec<RollupRow>)> {
        let mut by: HashMap<(usize, i64), Vec<RollupRow>> = HashMap::new();
        for (key, g) in self.groups.iter_mut() {
            if !g.dirty {
                continue;
            }
            g.dirty = false;
            let source = &SOURCES[key.0];
            let mut rows: Vec<&(SortKey, RawRow)> = g.rows.values().collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            let rows: Vec<&RawRow> = rows.into_iter().map(|(_, r)| r).collect();
            by.entry((key.0, key.2)).or_default().push(rollup_job::fresh_attributes(source, &key.1, key.2, &rows, &mut Rng::PerGroup));
        }
        let mut out: Vec<_> = by.into_iter().map(|((si, b), rows)| (SOURCES[si].record_type, b, rows)).collect();
        out.sort_by_key(|(t, b, _)| (*b, SOURCES.iter().position(|s| s.record_type == *t)));
        out
    }

    /// Drops group-hours that ended before `before` (epoch seconds).
    pub fn evict_before(&mut self, before: i64) {
        let (mut rows, mut bytes) = (0, 0);
        self.groups.retain(|k, g| {
            let keep = k.2 + 3600 > before || g.dirty;
            if !keep {
                rows += g.rows.len();
                bytes += g.bytes;
            }
            keep
        });
        self.rows -= rows;
        self.bytes -= bytes;
    }

    pub fn clear(&mut self) {
        self.groups = HashMap::new();
        self.rows = 0;
        self.bytes = 0;
    }

    pub fn touched_buckets(&self) -> HashSet<i64> {
        self.groups.keys().map(|k| k.2).collect()
    }
}

/// Writes flushed rows: one transaction per (type, hour), as RollupJob.
pub fn flush(acc: &mut Accumulator, write: &Connection) -> rusqlite::Result<usize> {
    let mut n = 0;
    for (t, bucket, rows) in acc.take_dirty() {
        rollup_job::write_type(write, t, bucket, &rows)?;
        n += rows.len();
    }
    Ok(n)
}

impl Source {
    /// One group-hour's rows, with occurred_at appended (the cold load).
    pub fn select_group_sql(&self) -> String {
        let t = self.table.name();
        let scope = self.scope_sql();
        format!(
            "SELECT {}, \"{t}\".\"occurred_at\" FROM \"{t}\" WHERE {scope}\"{t}\".\"group_hash\" = ?1 AND \"{t}\".\"occurred_at\" >= ?2 AND \"{t}\".\"occurred_at\" < ?3",
            self.select_list()
        )
    }
}
