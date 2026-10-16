//! Ack latency of a durable spool vs a SQLite commit, on the same disk.
//!
//!   rw-spool-bench DIR N BODY_BYTES
//!
//! spool:  append [u32 len][body] to one segment file, fdatasync, "ack". One writer,
//!         N sequential appends (the worst case: no group commit).
//! group:  same, but 16 bodies per fdatasync (what a writer thread that drains a
//!         channel would do under concurrency).
//! sqlite: one BEGIN IMMEDIATE / INSERT blob / COMMIT per body, WAL,
//!         synchronous=FULL (durable at ack) and =NORMAL (the server's setting).
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::Instant;

fn pct(mut v: Vec<f64>) -> String {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    format!("p50 {:.2} ms  p99 {:.2} ms  max {:.2} ms", p(0.5), p(0.99), v[v.len() - 1])
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dir, n, size) = (&a[1], a[2].parse::<usize>().unwrap(), a[3].parse::<usize>().unwrap());
    let body: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::create_dir_all(dir).unwrap();

    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(format!("{dir}/seg.ndjson")).unwrap();
    let mut lat = Vec::new();
    for _ in 0..n {
        let t = Instant::now();
        f.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&body).unwrap();
        unsafe { libc::fdatasync(f.as_raw_fd()) };
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("spool, fdatasync per batch   {}", pct(lat));

    let mut lat = Vec::new();
    for _ in 0..n / 16 {
        let t = Instant::now();
        for _ in 0..16 {
            f.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&body).unwrap();
        }
        unsafe { libc::fdatasync(f.as_raw_fd()) };
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("spool, 16 batches / fdatasync {}  (per group)", pct(lat));

    for sync in ["FULL", "NORMAL"] {
        let p = format!("{dir}/s-{sync}.sqlite3");
        let _ = std::fs::remove_file(&p);
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch(&format!("PRAGMA journal_mode=wal; PRAGMA synchronous={sync}; CREATE TABLE b(id INTEGER PRIMARY KEY, body BLOB);")).unwrap();
        let mut lat = Vec::new();
        for _ in 0..n {
            let t = Instant::now();
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            c.execute("INSERT INTO b(body) VALUES (?)", [&body]).unwrap();
            c.execute_batch("COMMIT").unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        println!("sqlite blob, synchronous={sync:6} {}", pct(lat));
    }
}
