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
