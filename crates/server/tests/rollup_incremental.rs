//! The incremental accumulator writes exactly what a full recompute writes.
//! Raw rows arrive in id-ordered slices (as batches would), each slice is
//! folded in and flushed; the final rollups table must equal a full
//! recompute of every hour (same per-group RNG), digest bytes included.
//! Uses the diff fixtures' records through the real ingest mapper/writer.

use std::path::PathBuf;

use railwatch_server::rollup::{incremental, incremental::Accumulator, rollup_job};
use rusqlite::Connection;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rw-incr-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&d);
    d
}

/// A tenant with raw rows: synthetic but realistic (many groups, ties on
/// occurred_at, several hours, every rolled source with some rows).
fn seed(conn: &Connection) {
    let mut rng: u64 = 0x9e3779b97f4a7c15;
    let mut next = |m: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % m
    };
    let tx = conn.unchecked_transaction().unwrap();
    for i in 0..30_000u64 {
        let hour = 1_791_900_000 + 3600 * (next(3) as i64);
        let at = railwatch_server::time::quoted_date_micros((hour + next(3600) as i64) * 1_000_000 + (next(4) as i64) * 250_000);
        let dur = next(400_000) as i64 + 1;
        match i % 6 {
            0 | 1 => {
                let kind = ["request", "job_attempt", "command"][next(3) as usize];
                tx.execute(
                    "INSERT INTO executions (occurred_at, kind, name, duration, group_hash, status, outcome, counters, detail, stages) VALUES (?, ?, ?, ?, ?, ?, ?, '{}', '{}', '[]')",
                    rusqlite::params![at, kind, format!("N{}", next(40)), dur, format!("g{}", next(40)), [200, 404, 500, 0][next(4) as usize], ["ok", "failed"][next(2) as usize]],
                ).unwrap();
            }
            2 | 3 => {
                tx.execute(
                    "INSERT INTO queries (occurred_at, duration, group_hash, sql) VALUES (?, ?, ?, ?)",
                    rusqlite::params![at, dur % 5000, format!("q{}", next(120)), format!("SELECT {}", next(120))],
                )
                .unwrap();
            }
            4 => {
                tx.execute(
                    "INSERT INTO cache_events (occurred_at, duration, group_hash, store, key, type) VALUES (?, ?, ?, 'redis', 'k', ?)",
                    rusqlite::params![at, dur % 900, format!("c{}", next(10)), ["hit", "miss", "generate", "write"][next(4) as usize]],
                )
                .unwrap();
            }
            _ => {
                tx.execute(
                    "INSERT INTO view_renders (occurred_at, duration, group_hash, identifier, kind) VALUES (?, ?, ?, 'x.erb', ?)",
                    rusqlite::params![at, dur % 9000, format!("v{}", next(10)), ["template", "partial", "collection"][next(3) as usize]],
                )
                .unwrap();
            }
        }
    }
    tx.commit().unwrap();
}

fn dump(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut s = conn
        .prepare("SELECT record_type, group_hash, bucket, name, count, error_count, client_error_count, duration_sum, duration_max, p50, p95, p99, digest, extra FROM rollups ORDER BY record_type, group_hash, bucket")
        .unwrap();
    s.query_map([], |r| (0..14).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect()).unwrap().map(Result::unwrap).collect()
}

#[test]
fn incremental_equals_full_recompute() {
    let full_path = tmp("full.sqlite3");
    let full = railwatch_server::tenants::open_tenant(&full_path).unwrap();
    seed(&full);
    let incr_path = tmp("incr.sqlite3");
    std::fs::copy(&full_path, &incr_path).unwrap();
    let incr = railwatch_server::tenants::open_tenant(&incr_path).unwrap();

    for h in rollup_job::hours_with_rows(&full).unwrap() {
        rollup_job::perform(&full, &full, h, &mut rollup_job::Rng::PerGroup).unwrap();
    }

    // Batches of ids, interleaving tables; the accumulator starts cold
    // part-way through (the first slices are "already committed").
    let mut acc = Accumulator::default();
    for table in ["executions", "queries", "cache_events", "view_renders"] {
        let ids: Vec<i64> =
            incr.prepare(&format!("SELECT id FROM {table} ORDER BY id")).unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        for chunk in ids.chunks(733) {
            acc.ingest(&incr, table, chunk).unwrap();
            incremental::flush(&mut acc, &incr).unwrap();
        }
    }
    let (a, b) = (dump(&full), dump(&incr));
    assert!(!a.is_empty());
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x, y);
    }
    let _ = std::fs::remove_file(&full_path);
    let _ = std::fs::remove_file(&incr_path);
}

fn command(conn: &Connection, at_secs: i64) -> i64 {
    conn.execute(
        "INSERT INTO executions (occurred_at, kind, name, duration, group_hash, status, outcome, counters, detail, stages) \
         VALUES (?, 'command', 'db:migrate', 1000, 'gcmd', 0, 'ok', '{}', '{}', '[]')",
        [railwatch_server::time::quoted_date_micros(at_secs * 1_000_000)],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn command_count(conn: &Connection, hour: i64) -> Option<i64> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT count FROM rollups WHERE record_type = 'command' AND group_hash = 'gcmd' AND bucket = ?",
        [railwatch_server::time::utc_seconds_string(hour)],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// Rails' follow-up drain could skip recomputing an hour for a receipt that
/// committed after that hour's recompute had read it (a per-pass
/// `recomputed` set and committed_at against the pass's start). The live
/// path here folds each committed batch into the accumulator: a late row for
/// an hour more than two hours old, already rolled up and evicted from
/// memory, is counted on the next flush, without a full recompute.
#[test]
fn a_late_row_for_an_old_rolled_up_hour_is_counted() {
    let path = tmp("late.sqlite3");
    let t = railwatch_server::tenants::open_tenant(&path).unwrap();
    let now = railwatch_server::time::now_micros() / 1_000_000;
    let old_hour = now - now.rem_euclid(3600) - 3 * 3600;
    let mut acc = Accumulator::default();
    let first = command(&t, old_hour + 600);
    acc.ingest(&t, "executions", &[first]).unwrap();
    incremental::flush(&mut acc, &t).unwrap();
    assert_eq!(command_count(&t, old_hour), Some(1));
    // As the worker does after each flush: the old hour leaves memory.
    acc.evict_before(now - now.rem_euclid(3600) - 3600);
    assert_eq!(acc.row_count(), 0);
    // A second command for that hour, committed later (a late batch).
    let late = command(&t, old_hour + 1200);
    acc.ingest(&t, "executions", &[late]).unwrap();
    incremental::flush(&mut acc, &t).unwrap();
    assert_eq!(command_count(&t, old_hour), Some(2), "the late row is counted");
    // And it is what a full recompute writes.
    let incremental: Vec<_> = dump(&t);
    rollup_job::perform(&t, &t, old_hour, &mut rollup_job::Rng::PerGroup).unwrap();
    assert_eq!(dump(&t), incremental);
    let _ = std::fs::remove_file(&path);
}
