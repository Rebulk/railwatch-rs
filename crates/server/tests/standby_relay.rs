//! `standby_relay` (standby.rs): a second server on a held data dir serves
//! its own address at once, relaying to the holder through serve.sock, and
//! keeps serving it after the hand-over. The two servers here differ by
//! port; in production they are two containers whose loopbacks differ.
//! Every batch posted to the standby's address, before, during and after
//! the hand-over, is stored once.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;

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
        .env("RAILWATCH_STANDBY_RELAY", "true")
        .env("RAILWATCH_LOG", "warn")
        .env("RAILWATCH_ENV_FILE", "/dev/null")
        .env_remove("RAILWATCH_CONFIG");
    c
}

fn up(port: u16) -> bool {
    get(port, "/up").is_some_and(|(status, _)| status == 200)
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let started = Instant::now();
    while !f() {
        assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn get(port: u16, path: &str) -> Option<(u16, String)> {
    exchange(port, &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").into_bytes())
}

fn exchange(port: u16, request: &[u8]) -> Option<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
    s.write_all(request).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text.split(' ').nth(1)?.parse().ok()?;
    Some((status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default()))
}

fn ingest(port: u16, token: &str, execution_id: &str) -> Option<(u16, String)> {
    ingest_with(port, token, execution_id, None)
}

/// `batch_id`: send it the way the gem's reporter does, with a receipt
/// (X-Railwatch-Producer-Id/Batch-Id/Body-SHA256/Policy), so a retry of the
/// same batch is acknowledged rather than stored again.
fn ingest_with(port: u16, token: &str, execution_id: &str, batch_id: Option<&str>) -> Option<(u16, String)> {
    // A retry resends the same bytes: the receipted batch's time is fixed.
    let now =
        if batch_id.is_some() { RECEIPTED_AT.with(|t| *t) } else { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() };
    let all: serde_json::Value = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let mut record = all["request"].clone();
    record["timestamp"] = now.into();
    record["execution_id"] = execution_id.into();
    let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(serde_json::to_string(&record).unwrap().as_bytes()).unwrap();
    gz.write_all(b"\n").unwrap();
    let body = gz.finish().unwrap();
    let receipt = batch_id.map_or_else(String::new, |id| {
        use sha2::Digest;
        let sha: String = sha2::Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "X-Railwatch-Producer-Id: 0f8fad5b-d9cb-469f-a165-70867728950e\r\nX-Railwatch-Batch-Id: {id}\r\nX-Railwatch-Body-SHA256: {sha}\r\nX-Railwatch-Policy: everything-v1\r\n"
        )
    });
    let mut req = format!(
        "POST /ingest HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/x-ndjson\r\n\
         Content-Encoding: gzip\r\nX-Railwatch-Version: 0.9.2\r\n{receipt}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(&body);
    exchange(port, &req)
}

thread_local! {
    static RECEIPTED_AT: f64 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
}

/// A spawned server, killed if the test panics before it is stopped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn term(child: &mut Child) {
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap();
}

#[test]
fn a_standby_relays_to_the_holder_then_takes_over_without_losing_a_batch() {
    let dir = Dir(std::env::temp_dir().join(format!("rw-standby-relay-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let out = railwatch(&dir.0, 0).args(["env", "create", "app-production"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let (a, b) = (free_port(), free_port());

    let mut holder = Server(railwatch(&dir.0, a).arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    wait_until("the holder", || up(a));
    assert!(dir.0.join("serve.sock").exists(), "the holder listens on serve.sock");

    let mut standby = Server(railwatch(&dir.0, b).arg("serve").stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap());
    wait_until("the standby's own address", || up(b));
    assert!(standby.0.try_wait().unwrap().is_none(), "the standby keeps running");

    // A receipted batch through the standby, answered by the holder.
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let batch = format!("{:08x}-{:04x}-7abc-8def-0123456789ab", ms >> 16, ms & 0xffff);
    let receipted = "00000000-0000-4000-8000-0000000000ff";
    assert_eq!(ingest_with(b, &token, receipted, Some(&batch)).map(|r| r.0), Some(200), "receipted batch through the standby");

    // Through the standby, while the holder writes.
    let mut ids = vec![receipted.to_string()];
    for i in 0..5 {
        let id = format!("00000000-0000-4000-8000-{i:012}");
        assert_eq!(ingest(b, &token, &id).map(|r| r.0), Some(200), "batch {i} through the standby");
        ids.push(id);
    }

    // Batches keep arriving on the standby's address while the holder
    // drains and exits; none is refused.
    let poster = {
        let token = token.clone();
        std::thread::spawn(move || {
            (5..45)
                .map(|i| {
                    let id = format!("00000000-0000-4000-8000-{i:012}");
                    let status = ingest(b, &token, &id).map(|r| r.0);
                    std::thread::sleep(Duration::from_millis(25));
                    (id, status)
                })
                .collect::<Vec<_>>()
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    term(&mut holder.0);
    for (id, status) in poster.join().unwrap() {
        assert_eq!(status, Some(200), "{id} across the hand-over");
        ids.push(id);
    }
    assert!(up(b));
    // The agent retries a batch whose answer it lost to the old server's
    // exit: the new holder acknowledges it, it is not stored twice.
    let (status, body) = ingest_with(b, &token, receipted, Some(&batch)).unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("already_committed"), "the retry is a duplicate to the new holder: {body}");
    term(&mut standby.0);
    let mut err = String::new();
    standby.0.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(err.contains("relaying") && err.contains("acquired after"), "the standby says what it did: {err}");

    let tenants = dir.0.join("telemetry");
    let x = railwatch_server::dashboard::pool::open_tenant_reader(&tenants, "app-production").unwrap();
    let stored: i64 = x.query_row("SELECT count(*) FROM executions", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, ids.len() as i64, "every batch stored once");
}
