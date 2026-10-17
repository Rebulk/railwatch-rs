//! rw-rollup-bench: cost of keeping one busy hour's rollups current.
//! Replays the hour's raw rows in 500-record batches (ids in order) and
//! measures, per flush every N batches: full RollupJob recompute of the
//! hour vs the incremental accumulator's flush. Prints totals.
//!
//!   rw-rollup-bench TENANT.sqlite3 HOUR_EPOCH [batches_per_flush=10]

use std::time::Instant;

use railwatch_server::rollup::{
    incremental::{self, Accumulator},
    rollup_job,
};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let path = std::path::Path::new(&a[0]);
    let hour: i64 = a[1].parse().unwrap();
    let per_flush: usize = a.get(2).map(|s| s.parse().unwrap()).unwrap_or(10);
    let read = railwatch_server::jobs::worker::open_read(path).unwrap();
    let write = railwatch_server::tenants::open_tenant(path).unwrap();
    let tables = ["executions", "queries", "outgoing_requests", "cache_events", "view_renders", "visits", "spans"];
    let mut ids: Vec<(&str, i64)> = Vec::new();
    for t in tables {
        let mut s = read.prepare(&format!("SELECT id FROM {t} ORDER BY id")).unwrap();
        ids.extend(s.query_map([], |r| r.get::<_, i64>(0)).unwrap().map(|r| (t, r.unwrap())));
    }
    // Interleave tables proportionally, 500 records per batch.
    ids.sort_by_key(|(t, id)| (*id, *t));
    let batches: Vec<&[(&str, i64)]> = ids.chunks(500).collect();
    let mut acc = Accumulator::default();
    let (mut inc_ingest, mut inc_flush, mut full) = (0.0, 0.0, 0.0);
    let mut flushes = 0;
    for (i, b) in batches.iter().enumerate() {
        let t = Instant::now();
        for table in tables {
            let v: Vec<i64> = b.iter().filter(|(t, _)| *t == table).map(|(_, id)| *id).collect();
            if !v.is_empty() {
                acc.ingest(&read, table, &v).unwrap();
            }
        }
        inc_ingest += t.elapsed().as_secs_f64();
        if (i + 1) % per_flush == 0 || i + 1 == batches.len() {
            let t = Instant::now();
            incremental::flush(&mut acc, &write).unwrap();
            inc_flush += t.elapsed().as_secs_f64();
            let t = Instant::now();
            rollup_job::perform(&read, &write, hour, &mut rollup_job::Rng::PerGroup).unwrap();
            full += t.elapsed().as_secs_f64();
            flushes += 1;
        }
    }
    println!(
        "{} records in {} batches, {flushes} flushes: incremental ingest {:.0} ms + flush {:.0} ms = {:.0} ms total; full recompute per flush {:.0} ms total ({:.1} ms/flush avg)",
        ids.len(),
        batches.len(),
        inc_ingest * 1e3,
        inc_flush * 1e3,
        (inc_ingest + inc_flush) * 1e3,
        full * 1e3,
        full * 1e3 / flushes as f64
    );
}
