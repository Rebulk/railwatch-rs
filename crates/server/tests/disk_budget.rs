//! The real binary at start: retention catches up on day files that
//! expired while it was down (the 03:00 timer alone missed them), and the
//! disk budget (storage::budget) trims to max_data_bytes before the first
//! request, oldest detail days first, recording the cutoff for core days.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use railwatch_server::storage::layout::{self, Tier, day_path, meta_path, open_or_create};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn serve(data: &Path, port: u16, env: &[(&str, &str)]) -> Child {
    let mut c = Command::new(BIN);
    c.env("RAILWATCH_DATA_DIR", data)
        .env("RAILWATCH_BIND", format!("127.0.0.1:{port}"))
        .env("RAILWATCH_SECRET_KEY_BASE", "00".repeat(64))
        .env("RAILWATCH_LOG", "warn")
        .env_remove("RAILWATCH_CONFIG")
        .env_remove("RAILWATCH_MAX_DATA_BYTES")
        .env_remove("RAILWATCH_MIN_FREE_BYTES");
    for (k, v) in env {
        c.env(k, v);
    }
    // The log goes to stdout.
    c.arg("serve").stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap()
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let started = Instant::now();
    while !f() {
        assert!(started.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGTERM, and the server must exit on its own (a thread holding a
/// sender to the worker kept it from ever stopping once).
fn stop(mut child: Child) -> String {
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("the server did not exit within 20 s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut log = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut log).unwrap();
    log
}

/// The server's open handles to deleted files under `root`: each keeps
/// that file's space allocated.
fn open_deleted(pid: u32, root: &Path) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter(|t| t.starts_with(root) && t.to_string_lossy().ends_with(" (deleted)"))
        .count()
}

fn day(days_ago: i64) -> String {
    let now = railwatch_server::time::now_micros() / 1_000_000;
    railwatch_server::time::utc_seconds_string(now - days_ago * 86_400)[..10].to_string()
}

/// A data dir with one environment ("production", 30-day retention) and a
/// core + detail file for each of `days_ago`, each padded to ~200 KB.
fn data_dir(name: &str, days_ago: &[i64]) -> (Dir, PathBuf) {
    let root = std::env::temp_dir().join(format!("rw-disk-budget-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let out = Command::new(BIN)
        .args(["setup", "--email", "a@example.com", "--password-stdin", "--account", "a", "--app", "a", "--env", "production"])
        .env("RAILWATCH_DATA_DIR", &root)
        .env_remove("RAILWATCH_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(b"a-long-enough-password\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(out.status.success());
    let dir = root.join("telemetry").join("a-a-production");
    open_or_create(&meta_path(&dir)).unwrap();
    for d in days_ago {
        for tier in [Tier::Core, Tier::Detail] {
            let c = open_or_create(&day_path(&dir, tier, &day(*d))).unwrap();
            c.execute_batch("CREATE TABLE pad (b BLOB); INSERT INTO pad VALUES (zeroblob(200000)); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
    }
    (Dir(root), dir)
}

#[test]
fn retention_catches_up_at_start() {
    // 40 and 35 days old: past core retention (30). 10 days: past detail (7).
    let (_root, dir) = data_dir("catchup", &[40, 35, 10, 2]);
    let port = free_port();
    let child = serve(&_root.0, port, &[]);
    wait_until("the start-up prune", || layout::days(&dir, Tier::Detail) == vec![day(2)]);
    assert_eq!(layout::days(&dir, Tier::Core), vec![day(10), day(2)]);
    wait_until("the prune's release of deleted files", || open_deleted(child.id(), &_root.0) == 0);
    stop(child);
}

#[test]
fn over_max_data_bytes_at_start_trims_oldest_first_and_records_the_cutoff() {
    let (_root, dir) = data_dir("budget", &[6, 5, 4, 3, 2, 1]);
    let total = railwatch_server::storage::budget::usage(&_root.0.join("telemetry")).0;
    let file = std::fs::metadata(day_path(&dir, Tier::Core, &day(6))).unwrap().len();
    // Room for all but ~7.5 day files: the six detail days before today,
    // then the two oldest core days.
    let max = (total - 7 * file - file / 2).to_string();
    let port = free_port();
    let child = serve(&_root.0, port, &[("RAILWATCH_MAX_DATA_BYTES", &max)]);
    wait_until("the server", || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    // The pass ran before the listener opened.
    assert!(layout::days(&dir, Tier::Detail).is_empty());
    assert_eq!(layout::days(&dir, Tier::Core), vec![day(4), day(3), day(2), day(1)]);
    assert!(meta_path(&dir).is_file());
    assert_eq!(open_deleted(child.id(), &_root.0), 0, "no handle keeps a trimmed file's space");
    let control = rusqlite::Connection::open(_root.0.join("control.sqlite3")).unwrap();
    let through: String = control.query_row("SELECT raw_pruned_through FROM environments", [], |r| r.get(0)).unwrap();
    assert_eq!(&through[..10], day(4), "raw rows before the end of the newest deleted day are refused");
    let log = stop(child);
    assert!(log.contains("telemetry over max_data_bytes: deleted the oldest day files"), "{log}");
    assert!(log.contains("raw telemetry through this day is gone"), "{log}");
}

/// POST /ingest with a gzipped NDJSON body; the status.
fn post(port: u16, token: &str, body: &[u8]) -> u16 {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let head = format!(
        "POST /ingest HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/x-ndjson\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.split(' ').nth(1).unwrap().parse().unwrap()
}

/// `n` log records stamped now, ~1 KB each, gzipped.
fn logs(n: usize) -> Vec<u8> {
    let all: serde_json::Value = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let mut body = Vec::new();
    for i in 0..n {
        let mut r = all["log"].clone();
        r["timestamp"] = serde_json::json!(now);
        r["message"] = serde_json::json!(format!("{i} {}", railwatch_server::control::new_token().repeat(20)));
        body.extend_from_slice(serde_json::to_string(&r).unwrap().as_bytes());
        body.push(b'\n');
    }
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut e, &body).unwrap();
    e.finish().unwrap()
}

/// A burst into today's files: the writers' running estimate wakes the
/// budget pass as soon as it crosses max_data_bytes, so telemetry never
/// gets more than about one batch per request in flight over it (it used
/// to grow for up to 10 s, to 2.3x a 60M budget). Then, with only today's
/// files left over the budget, ingest answers 503; it would otherwise grow
/// them until the free-space floor stopped it.
#[test]
fn a_burst_stays_within_the_budget_plus_one_batch_per_sender() {
    let (_root, _dir) = data_dir("burst", &[6, 5, 4, 3, 2, 1]);
    let token = String::from_utf8(
        Command::new(BIN).args(["env", "create", "burst"]).env("RAILWATCH_DATA_DIR", &_root.0).env_remove("RAILWATCH_CONFIG").output().unwrap().stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let telemetry = _root.0.join("telemetry");
    let usage = || railwatch_server::storage::budget::usage(&telemetry).0;
    let old = usage();
    // Room for today's new files and a few batches before the burst.
    let max = old + 4_000_000;
    let port = free_port();
    let child = serve(&_root.0, port, &[("RAILWATCH_MAX_DATA_BYTES", &max.to_string())]);
    wait_until("the server", || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
    // Today's files exist; then one batch's bytes.
    let body = logs(100);
    assert_eq!(post(port, &token, &body), 200);
    let before = usage();
    assert_eq!(post(port, &token, &body), 200);
    let batch = usage() - before;
    assert!(batch > 50_000, "one batch wrote {batch} bytes");

    const SENDERS: u64 = 4;
    let peak = std::sync::atomic::AtomicU64::new(0);
    let done = std::sync::atomic::AtomicBool::new(false);
    let statuses = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                peak.fetch_max(usage(), std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let senders: Vec<_> = (0..SENDERS).map(|_| s.spawn(|| (0..25).map(|_| post(port, &token, &body)).collect::<Vec<_>>())).collect();
        for h in senders {
            statuses.lock().unwrap().extend(h.join().unwrap());
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let statuses = statuses.into_inner().unwrap();
    let (ok, refused) = (statuses.iter().filter(|s| **s == 200).count(), statuses.iter().filter(|s| **s == 503).count());
    assert_eq!(ok + refused, statuses.len(), "only 200 or 503: {statuses:?}");
    // 100 batches is ~3x the room the old days free: some went in and
    // the rest were refused once only today's files were left.
    assert!(ok > 0 && refused > 0, "{ok} accepted, {refused} refused");
    let peak = peak.into_inner();
    assert!(peak <= max + SENDERS * batch, "peak {peak} over max {max} + {SENDERS} x {batch}");
    assert!(layout::days(&_dir, Tier::Detail).len() <= 1, "old detail days were trimmed for the burst");
    let log = stop(child);
    assert!(log.contains("telemetry over max_data_bytes: deleted the oldest day files"), "{log}");
    assert_eq!(log.matches("still over max_data_bytes with no older day file left to delete").count(), 1, "logged once: {log}");
}
