//! Exactly-once across files: a batch whose rows straddle UTC midnight
//! touches yesterday's and today's day files (core and detail) plus meta.
//! The real `railwatch serve` binary is killed (abort) at each injected
//! point of the commit sequence; then a fresh server starts, recovers, and
//! the client retries the same delivery the way the gem does (same body,
//! same delivery id) until it gets a 200. Every table must then hold the
//! batch exactly once, across every file, with no gaps or duplicate ids.
//!
//! Points (storage::writer::crash_point): mid_transaction (rows inserted,
//! nothing committed), after_day_commit:1 (one day file committed, meta
//! not), before_meta_commit (all day files committed, meta not),
//! after_meta_commit (all committed, ack not sent).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

fn railwatch(data: &Path, port: u16) -> Command {
    let mut c = Command::new(BIN);
    c.env("RAILWATCH_DATA_DIR", data)
        .env("RAILWATCH_BIND", format!("127.0.0.1:{port}"))
        .env("RAILWATCH_SECRET_KEY_BASE", "00".repeat(64))
        .env("RAILWATCH_RATE_LIMIT_ENV", "0")
        .env("RAILWATCH_RATE_LIMIT_IP", "0")
        .env("RAILWATCH_LOG", "warn")
        .env_remove("RAILWATCH_CONFIG");
    c
}

fn serve(data: &Path, port: u16, crash_at: Option<&str>) -> Child {
    let mut c = railwatch(data, port);
    if let Some(p) = crash_at {
        c.env("RAILWATCH_CRASH_AT", p);
    }
    let child = c.arg("serve").stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let started = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(started.elapsed() < Duration::from_secs(20), "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

fn post(port: u16, token: &str, body: &[u8], headers: &[(&str, String)]) -> Option<(u16, Value)> {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    let mut head = format!(
        "POST /ingest HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/x-ndjson\r\nContent-Encoding: gzip\r\nX-Railwatch-Version: 0.9.0\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).ok()?;
    s.write_all(body).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let status: u16 = String::from_utf8_lossy(&buf[..split]).split(' ').nth(1)?.parse().ok()?;
    Some((status, serde_json::from_slice(&buf[split + 4..]).unwrap_or(Value::Null)))
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

/// `n` requests, each with 3 queries and 2 logs, alternating between 23:59
/// yesterday and 00:00:30 today (UTC), so one batch writes four day files.
fn batch(n: usize, tag: &str) -> (Vec<u8>, usize) {
    let all: Value = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let midnight = (now / 86_400.0).floor() * 86_400.0;
    let mut out = Vec::new();
    let mut records = 0;
    for i in 0..n {
        let at = if i % 2 == 0 { midnight - 60.0 + i as f64 * 0.001 } else { midnight + 30.0 + i as f64 * 0.001 };
        let exec = format!("{tag}-exec-{i}");
        let mut push = |kind: &str, extra: &dyn Fn(&mut Value)| {
            let mut r = all[kind].clone();
            r["timestamp"] = json!(at);
            r["execution_id"] = json!(exec);
            extra(&mut r);
            out.extend_from_slice(serde_json::to_string(&r).unwrap().as_bytes());
            out.push(b'\n');
            records += 1;
        };
        push("request", &|_| {});
        for q in 0..3 {
            push("query", &|r| r["sql"] = json!(format!("SELECT {q} FROM t WHERE id = ?")));
        }
        for l in 0..2 {
            push("log", &|r| r["message"] = json!(format!("{tag} log {i}.{l}")));
        }
    }
    (out, records)
}

fn receipt(body: &[u8], delivery: &str) -> Vec<(&'static str, String)> {
    vec![
        ("X-Railwatch-Producer-Id", "0f8fad5b-d9cb-469f-a165-70867728950e".into()),
        ("X-Railwatch-Batch-Id", delivery.into()),
        ("X-Railwatch-Body-SHA256", Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()),
        ("X-Railwatch-Policy", "everything-v1".into()),
    ]
}

fn v7() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let r = u64::from_le_bytes(b);
    let hex = format!("{:012x}7{:03x}{:x}{:015x}", ms, r & 0xfff, 8 | (r >> 60 & 3), r >> 4 & 0xfff_ffff_ffff_ffff);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

struct Counts {
    executions: i64,
    queries: i64,
    logs: i64,
    logs_fts: i64,
    receipts: i64,
    batches: i64,
    distinct_ids: bool,
    day_files: usize,
}

fn counts(data: &Path) -> Counts {
    let c = railwatch_server::dashboard::pool::open_tenant_reader(&data.join("telemetry"), "crash-env").unwrap();
    let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    let distinct = ["executions", "queries", "logs", "ingest_batches"]
        .iter()
        .all(|t| n(&format!("SELECT COUNT(*) FROM {t}")) == n(&format!("SELECT COUNT(DISTINCT id) FROM {t}")));
    Counts {
        executions: n("SELECT COUNT(*) FROM executions"),
        queries: n("SELECT COUNT(*) FROM queries"),
        logs: n("SELECT COUNT(*) FROM logs"),
        // The dashboard's shape (filter_query.rs): MATCH inside IN (...).
        logs_fts: n("SELECT COUNT(*) FROM logs WHERE logs.id IN (SELECT rowid FROM logs_fts WHERE logs_fts MATCH 'log')"),
        receipts: n("SELECT COUNT(*) FROM ingest_receipts"),
        batches: n("SELECT COUNT(*) FROM ingest_batches"),
        distinct_ids: distinct,
        day_files: railwatch_server::storage::layout::all_files(&data.join("telemetry/crash-env")).len() - 1,
    }
}

/// One scenario: an earlier committed batch, then the crashing one, then
/// recovery + retry. Returns the retry's ack.
fn scenario(point: &str) -> Value {
    let dir = Dir(std::env::temp_dir().join(format!("rw-crash-{}-{}", std::process::id(), point.replace(':', "-"))));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let out = railwatch(&dir.0, 0).args(["env", "create", "crash-env"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();

    // A committed batch first, so recovery has rows it must keep.
    let port = free_port();
    let mut server = serve(&dir.0, port, None);
    let (first, first_records) = batch(20, "first");
    let first = gzip(&first);
    let (status, ack) = post(port, &token, &first, &receipt(&first, &v7())).unwrap();
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["accepted"], json!(first_records));
    server.kill().unwrap();
    server.wait().unwrap();

    // The crashing delivery.
    let (body, records) = batch(40, "second");
    let body = gzip(&body);
    let delivery = v7();
    let headers = receipt(&body, &delivery);
    let port = free_port();
    let mut server = serve(&dir.0, port, Some(point));
    let reply = post(port, &token, &body, &headers);
    let status = server.wait().unwrap();
    assert!(!status.success(), "{point}: the server should have aborted, got {status:?} / {reply:?}");
    assert!(reply.as_ref().is_none_or(|(s, _)| *s != 200), "{point}: a 200 was sent before the crash: {reply:?}");

    // Restart (recovery runs when the writer opens) and retry like the gem.
    let port = free_port();
    let mut server = serve(&dir.0, port, None);
    let (status, ack) = post(port, &token, &body, &headers).unwrap();
    assert_eq!(status, 200, "{point}: {ack}");
    // And once more: a replay of a committed delivery is answered from its receipt.
    let (status, again) = post(port, &token, &body, &headers).unwrap();
    assert_eq!(status, 200);
    assert_eq!(again["disposition"], json!("already_committed"), "{point}: {again}");
    server.kill().unwrap();
    server.wait().unwrap();

    let c = counts(&dir.0);
    let req = (20 + 40) as i64;
    assert_eq!(c.executions, req, "{point}: executions");
    assert_eq!(c.queries, req * 3, "{point}: queries");
    assert_eq!(c.logs, req * 2, "{point}: logs");
    assert_eq!(c.logs_fts, req * 2, "{point}: logs_fts");
    assert_eq!(c.receipts, 2, "{point}: receipts");
    assert_eq!(c.batches, 2, "{point}: ingest_batches");
    assert!(c.distinct_ids, "{point}: an id appears twice across day files");
    assert_eq!(c.day_files, 4, "{point}: yesterday + today, core + detail");
    assert_eq!(records, 40 * 6);
    eprintln!(
        "{point}: executions {} queries {} logs {} fts {} receipts {} batches {} (expected {req}/{}/{}/{}/2/2)",
        c.executions,
        c.queries,
        c.logs,
        c.logs_fts,
        c.receipts,
        c.batches,
        req * 3,
        req * 2,
        req * 2
    );
    ack
}

#[test]
fn crash_at_every_point_commits_the_batch_exactly_once() {
    for point in ["mid_transaction:3", "after_day_commit:1", "after_day_commit:3", "before_meta_commit:1", "after_meta_commit:1"] {
        let ack = scenario(point);
        // Crashed after meta's commit: the batch was committed, the retry is
        // answered from the receipt. Otherwise the retry writes it.
        if point.starts_with("after_meta_commit") {
            assert_eq!(ack["disposition"], json!("already_committed"), "{point}: {ack}");
        } else {
            assert_eq!(ack["disposition"], json!("committed"), "{point}: {ack}");
        }
    }
}
