//! rw-ingest-floor: the ingest path without HTTP, for the floor the server
//! is measured against (docs/PERF.md, "Ingest floor").
//!
//! Reads gzip NDJSON bodies (`rw-load --dump DIR`), then, on one thread:
//! 1. parse + map every body (payload::parse_bytes, batch::map_all), timed;
//! 2. writes the mapped batches into a fresh environment through the same
//!    Store the tenant writer uses: BEGIN, a savepoint per batch, COMMIT,
//!    PER_TXN batches per transaction (the server coalesced ~8 at 16
//!    senders to one app), timed.
//!
//! Prints rows/s and events/s for each stage, and the CPU time of each.
//!
//!   rw-ingest-floor BODIES_DIR DATA_DIR [PER_TXN=8] [BATCHES=all]
//!
//! DATA_DIR must not exist (a new per-day environment `e1` is created in it).

use std::time::Instant;

use railwatch_server::ingest::{batch, payload};
use railwatch_server::rjson::Value;
use railwatch_server::storage::store::Store;

// The server's allocator, which memory::configure also hands to SQLite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn cpu_secs() -> f64 {
    // SAFETY: getrusage fills the struct it is given.
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut r) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(r.ru_utime) + tv(r.ru_stime)
}

fn main() {
    // As the server's main: no transparent huge pages, so the floor pays
    // the same page-fault cost it is compared against.
    // SAFETY: a process attribute; no pointers are passed.
    unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0) };
    railwatch_server::memory::configure();
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: rw-ingest-floor BODIES_DIR DATA_DIR [PER_TXN=8] [BATCHES=all]");
        std::process::exit(2);
    }
    let per_txn: usize = a.get(2).map(|s| s.parse().expect("PER_TXN")).unwrap_or(8);
    let mut paths: Vec<_> = std::fs::read_dir(&a[0]).expect("bodies dir").map(|e| e.unwrap().path()).collect();
    paths.sort();
    if let Some(n) = a.get(3) {
        paths.truncate(n.parse().expect("BATCHES"));
    }
    let bodies: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();

    // 1. parse + map
    let (t, c) = (Instant::now(), cpu_secs());
    let mut prepared = Vec::with_capacity(bodies.len());
    for raw in &bodies {
        let records = payload::parse_bytes(raw, true).expect("body parses");
        let meta = batch::Meta {
            dropped_by_client: Value::Int(0),
            backpressure_factor: 1.0,
            bytes: Value::Int(raw.len() as i64),
            gem_version: Some(b"0.9.0".to_vec()),
            received_at_micros: railwatch_server::time::now_micros(),
        };
        prepared.push(batch::map_all(records, 0.0, meta, None));
    }
    let (map_wall, map_cpu) = (t.elapsed().as_secs_f64(), cpu_secs() - c);
    let records: usize = prepared.iter().map(|p| p.record_count).sum();
    let rows: usize = prepared.iter().map(|p| p.row_count()).sum();
    let rejected: usize = prepared.iter().map(|p| p.rejections.len()).sum();

    // 2. write
    let dir = std::path::Path::new(&a[1]);
    assert!(!dir.exists(), "{} exists", dir.display());
    std::fs::create_dir_all(dir).unwrap();
    let mut store = Store::open(dir, "e1").expect("open store");
    let (t, c) = (Instant::now(), cpu_secs());
    let mut txns = 0;
    for chunk in prepared.chunks_mut(per_txn) {
        store.begin().expect("begin");
        for p in chunk.iter_mut() {
            store.write(p, 0).expect("write");
        }
        store.commit().expect("commit");
        txns += 1;
    }
    let (write_wall, write_cpu) = (t.elapsed().as_secs_f64(), cpu_secs() - c);

    let per = |n: usize, s: f64| n as f64 / s;
    println!("batches {} records {records} rows {rows} rejected {rejected} txns {txns} per_txn {per_txn}", bodies.len());
    println!(
        "parse+map: wall {map_wall:.2}s cpu {map_cpu:.2}s  {:.0} events/s  {:.1} cpu-ms/1k events",
        per(records, map_wall),
        map_cpu * 1e6 / records as f64
    );
    println!(
        "write:     wall {write_wall:.2}s cpu {write_cpu:.2}s  {:.0} rows/s  {:.0} events/s  {:.1} cpu-ms/1k events",
        per(rows, write_wall),
        per(records, write_wall),
        write_cpu * 1e6 / records as f64
    );
    println!("both, one thread: {:.0} events/s  {:.1} cpu-ms/1k events", per(records, map_wall + write_wall), (map_cpu + write_cpu) * 1e6 / records as f64);
}
