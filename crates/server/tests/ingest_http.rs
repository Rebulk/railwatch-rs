//! End to end over HTTP against a server on an ephemeral port: the direct
//! path, the receipted state machine (committed / already_committed / 409 /
//! 503 clock skew / 410 too old / paused / 402 quota), rejection rules,
//! limits, rate limits, and that one failing batch in a coalesced
//! transaction rolls back alone.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use flate2::write::GzEncoder;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

struct Server {
    addr: SocketAddr,
    dir: tempdir::Dir,
    control: Arc<Control>,
    token: String,
}

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn start(ip_limit: u64, env_limit: u64) -> Server {
    let dir = std::env::temp_dir().join(format!("rw-http-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let control = Arc::new(Control::open_with(&dir.join("control.sqlite3"), true).unwrap());
    let token = control::new_token();
    control.create("test-env", &control::digest(&token), 30).unwrap();
    let (std_listener, addr) = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        l.set_nonblocking(true).unwrap();
        (l, a)
    };
    let c2 = control.clone();
    let d2 = dir.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (tx, rx) = followups::channel();
            let tenants = Tenants::new(d2.join("telemetry"), Duration::from_secs(60), tx);
            let state = Arc::new(AppState {
                control: c2.clone(),
                tenants,
                ip_limit: FixedWindow::new(ip_limit),
                env_limit: FixedWindow::new(env_limit),
                trust_proxy: true,
                sourcemaps_dir: d2.join("sourcemaps"),
                cpu: Arc::new(tokio::sync::Semaphore::new(4)),
            });
            tokio::spawn(followups::run(c2, rx));
            let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
            axum::serve(listener, http::router(state).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
    });
    Server { addr, dir: tempdir::Dir(dir), control, token }
}

fn rand_suffix() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    u64::from_le_bytes(b)
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

fn gzip(body: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(body).unwrap();
    e.finish().unwrap()
}

fn ndjson(records: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        out.extend_from_slice(serde_json::to_string(r).unwrap().as_bytes());
        out.push(b'\n');
    }
    out
}

fn fixture(t: &str) -> Value {
    let all: Value = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let mut r = all[t].clone();
    r["timestamp"] = json!(now());
    r
}

/// A minimal HTTP/1.1 client: (status, headers, body).
fn request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    // The server may answer (413) and close before reading the whole body.
    let _ = s.write_all(body);
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let hs = lines.filter_map(|l| l.split_once(": ").map(|(a, b)| (a.to_ascii_lowercase(), b.to_string()))).collect();
    (status, hs, buf[split + 4..].to_vec())
}

impl Server {
    fn post(&self, body: &[u8], extra: &[(&str, String)]) -> (u16, Vec<(String, String)>, Value) {
        let mut headers = vec![
            ("Authorization", format!("Bearer {}", self.token)),
            ("Content-Type", "application/x-ndjson".into()),
            ("Content-Encoding", "gzip".into()),
            ("X-Railwatch-Version", "0.9.0".into()),
        ];
        headers.extend(extra.iter().cloned());
        let (status, hs, body) = request(self.addr, "POST", "/ingest", &headers, body);
        let json = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() };
        (status, hs, json)
    }

    /// New environments are per-day directories; the union reader shows
    /// them as one tenant.
    fn tenant(&self) -> rusqlite::Connection {
        railwatch_server::dashboard::pool::open_tenant_reader(&self.dir.0.join("telemetry"), "test-env").unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        self.tenant().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }
}

fn receipt_headers(body: &[u8], batch_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("X-Railwatch-Producer-Id", "0f8fad5b-d9cb-469f-a165-70867728950e".into()),
        ("X-Railwatch-Batch-Id", batch_id.into()),
        ("X-Railwatch-Body-SHA256", Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()),
        ("X-Railwatch-Policy", "everything-v1".into()),
    ]
}

fn v7_at(ms: u64) -> String {
    let rand = rand_suffix();
    let hex = format!("{:012x}7{:03x}{:x}{:015x}", ms, rand & 0xfff, 8 | (rand >> 60 & 3), rand >> 4 & 0xfff_ffff_ffff_ffff);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[test]
fn direct_path_writes_every_fixture_type_and_counts_usage() {
    let s = start(0, 0);
    let all: Value = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let records: Vec<Value> = all.as_object().unwrap().keys().map(|k| fixture(k)).collect();
    let (status, _, ack) = s.post(&gzip(&ndjson(&records)), &[]);
    assert_eq!(status, 200);
    assert_eq!(ack, json!({"accepted": 28, "rejected": 0, "rejections": []}));
    // Five execution fixtures share one execution_id: INSERT OR IGNORE keeps one.
    assert_eq!(s.count("executions"), 1);
    assert_eq!(s.count("people"), 1);
    assert_eq!(s.count("logs_fts"), 1);
    assert_eq!(s.count("query_shapes"), 1);
    assert_eq!(s.count("ingest_batches"), 1);
    std::thread::sleep(Duration::from_millis(1500));
    let env = s.control.list().unwrap().remove(0);
    assert_eq!(env.events_this_month, 28, "usage flushed to the control DB");
}

#[test]
fn rejections_are_counted_and_reported_like_rails() {
    let s = start(0, 0);
    let mut old = fixture("log");
    old["timestamp"] = json!(now() - 31.0 * 86_400.0);
    let mut future = fixture("log");
    future["timestamp"] = json!(now() + 7200.0);
    let mut floor = fixture("log");
    // Inside 30d, below the floor (start of the 30-day-ago hour + 1h): halfway
    // between the two, so it holds at any minute of the hour.
    let window_start = now() - 30.0 * 86_400.0;
    let retention_floor = (window_start / 3600.0).floor() * 3600.0 + 3600.0;
    floor["timestamp"] = json!((window_start + retention_floor) / 2.0);
    let mut bad_v = fixture("query");
    bad_v["v"] = json!(2);
    let records = vec![fixture("query"), old, future, floor, bad_v, json!({"t": "martian", "timestamp": now()}), json!("x")];
    let (status, _, ack) = s.post(&gzip(&ndjson(&records)), &[]);
    assert_eq!(status, 200);
    assert_eq!(ack["accepted"], 1);
    assert_eq!(ack["rejected"], 6);
    let reasons: Vec<&str> = ack["rejections"].as_array().unwrap().iter().map(|r| r["reason"].as_str().unwrap()).collect();
    assert_eq!(
        reasons,
        vec![
            "timestamp out of range",
            "timestamp out of range",
            "outside retained window",
            "TypeError: query v2 is not v1",
            "unknown type",
            "TypeError: record must be an object",
        ]
    );
    assert_eq!(ack["rejections"][5]["type"], Value::Null);
    let stored: String = s.tenant().query_row("SELECT rejections FROM ingest_batches", [], |r| r.get(0)).unwrap();
    assert!(stored.starts_with(r#"[{"type":"log","reason":"timestamp out of range"}"#), "{stored}");
}

#[test]
fn malformed_bodies_and_auth() {
    let s = start(0, 0);
    assert_eq!(s.post(b"not gzip", &[]).2, json!({"error": "bad gzip: not in gzip format"}));
    assert_eq!(s.post(&gzip(b"{bad\n"), &[]).2, json!({"error": "bad json: expected object key, got 'bad' at line 1 column 2"}));
    assert_eq!(s.post(&gzip(b"   \n"), &[]).0, 200, "a gzip of whitespace decodes to no records");
    assert_eq!(s.post(b"", &[]).2, json!({"error": "empty body"}));
    let (status, _, _) = request(s.addr, "POST", "/ingest", &[("Authorization", "Bearer rw_nope".into())], b"x");
    assert_eq!(status, 401);
    let (status, _, _) = request(s.addr, "POST", "/ingest", &[], b"x");
    assert_eq!(status, 401);
    let (status, _, body) = request(s.addr, "GET", "/ingest/ping", &[("Authorization", format!("Bearer {}", s.token))], b"");
    assert_eq!((status, serde_json::from_slice::<Value>(&body).unwrap()), (200, json!({"ok": true, "environment": "test-env", "retention_days": 30})));
    let (status, _, body) = request(s.addr, "POST", "/ingest", &[], &vec![b'x'; 32 * 1024 * 1024 + 1]);
    assert_eq!((status, body), (413, br#"{"error":"body too large"}"#.to_vec()));
    let (status, _, _) = request(s.addr, "GET", "/nope", &[], b"");
    assert_eq!(status, 404);
}

#[test]
fn receipted_state_machine() {
    let s = start(0, 0);
    let body = gzip(&ndjson(&[fixture("query"), fixture("log")]));
    let now_ms = (now() * 1000.0) as u64;
    let id = v7_at(now_ms);
    let h = receipt_headers(&body, &id);

    let (status, _, first) = s.post(&body, &h);
    assert_eq!(status, 200);
    assert_eq!(first["disposition"], "committed");
    assert_eq!(first["accepted"], 2);
    assert!(first["committed_at"].as_str().unwrap().ends_with('Z'));

    // Same bytes again: the original answer, nothing written twice.
    let (status, _, again) = s.post(&body, &h);
    assert_eq!(status, 200);
    assert_eq!(again["disposition"], "already_committed");
    assert_eq!(again["committed_at"], first["committed_at"]);
    assert_eq!(s.count("queries"), 1);
    assert_eq!(s.count("ingest_receipts"), 1);

    // Same id, different bytes: 409.
    let other = gzip(&ndjson(&[fixture("log")]));
    let (status, _, conflict) = s.post(&other, &receipt_headers(&other, &id));
    assert_eq!((status, conflict), (409, json!({"error": "delivery id already used for different content"})));

    // Same id and bytes, different metadata (dropped count): 409 too.
    let mut h2 = h.clone();
    h2.push(("X-Railwatch-Dropped", "3".into()));
    assert_eq!(s.post(&body, &h2).0, 409);

    // A body that does not match its digest.
    let mut wrong = h.clone();
    wrong[2].1 = "0".repeat(64);
    assert_eq!(s.post(&body, &wrong).2, json!({"error": "body digest mismatch"}));

    // From the future: 503, Retry-After 300.
    let future = v7_at(now_ms + 10 * 60 * 1000);
    let (status, headers, deferred) = s.post(&body, &receipt_headers(&body, &future));
    assert_eq!(status, 503);
    assert!(headers.contains(&("retry-after".into(), "300".into())));
    assert_eq!(deferred, json!({"disposition": "deferred", "reason": "clock skew", "accepted": 0, "rejected": 0}));

    // Too old: 410.
    let old = v7_at(now_ms - 8 * 86_400 * 1000);
    assert_eq!(s.post(&body, &receipt_headers(&body, &old)).2, json!({"error": "delivery id is too old to accept"}));

    // Invalid headers: 400.
    let mut bad = h.clone();
    bad[3].1 = "everything-v2".into();
    assert_eq!(s.post(&body, &bad).2, json!({"error": "unsupported policy"}));

    // Paused and over quota are deferred, but an already committed delivery
    // is still acknowledged.
    s.control.conn().execute("UPDATE environments SET paused_at = '2026-01-01 00:00:00'", []).unwrap();
    let fresh = v7_at(now_ms);
    assert_eq!(s.post(&body, &receipt_headers(&body, &fresh)).2, json!({"disposition": "deferred", "reason": "paused", "accepted": 0, "rejected": 0}));
    assert_eq!(s.post(&body, &h).2["disposition"], "already_committed");
    assert_eq!(s.post(&body, &[]).2, json!({"accepted": 0, "rejected": 0, "reason": "paused"}));
    s.control.conn().execute("UPDATE environments SET paused_at = NULL", []).unwrap();
    s.control.conn().execute("UPDATE accounts SET monthly_event_quota = 0", []).unwrap();
    let (status, _, quota) = s.post(&body, &receipt_headers(&body, &fresh));
    assert_eq!((status, quota), (402, json!({"disposition": "deferred", "reason": "quota", "accepted": 0, "rejected": 0})));
    assert_eq!(s.post(&body, &[]).0, 402);
}

#[test]
fn a_failing_batch_rolls_back_alone_when_coalesced() {
    let s = start(0, 0);
    // A bignum Dropped header passes mapping and fails the ingest_batches
    // insert (Rails: ActiveModel::RangeError -> 500).
    let threads: Vec<_> = (0..24)
        .map(|i| {
            let addr = s.addr;
            let token = s.token.clone();
            std::thread::spawn(move || {
                let mut q = fixture("query");
                q["execution_id"] = json!(format!("exec-{i}"));
                let body = gzip(&ndjson(&[q]));
                let mut headers = vec![("Authorization", format!("Bearer {token}")), ("Content-Encoding", "gzip".into())];
                if i % 4 == 0 {
                    headers.push(("X-Railwatch-Dropped", "99999999999999999999".into()));
                }
                request(addr, "POST", "/ingest", &headers, &body).0
            })
        })
        .collect();
    let statuses: Vec<u16> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(statuses.iter().filter(|s| **s == 500).count(), 6);
    assert_eq!(statuses.iter().filter(|s| **s == 200).count(), 18);
    assert_eq!(s.count("queries"), 18, "the failed batches' rows rolled back");
    assert_eq!(s.count("ingest_batches"), 18);
}

#[test]
fn rate_limits_answer_429_with_retry_after() {
    let s = start(0, 3);
    let body = gzip(&ndjson(&[fixture("log")]));
    let statuses: Vec<u16> = (0..5).map(|_| s.post(&body, &[]).0).collect();
    assert_eq!(statuses, vec![200, 200, 200, 429, 429]);
    let (_, headers, json_body) = s.post(&body, &[]);
    assert!(headers.contains(&("retry-after".into(), "60".into())));
    assert_eq!(json_body, json!({"error": "rate limit exceeded", "retry_after": 60}));

    let s = start(2, 0);
    let statuses: Vec<u16> = (0..3).map(|_| s.post(&body, &[("X-Forwarded-For", "192.0.2.1".into())]).0).collect();
    assert_eq!(statuses, vec![200, 200, 429]);
    assert_eq!(s.post(&body, &[("X-Forwarded-For", "192.0.2.2".into())]).0, 200, "XFF is its own bucket when trusted");
}

#[test]
fn deploys_kamal_and_sourcemaps() {
    let s = start(0, 0);
    let auth = ("Authorization", format!("Bearer {}", s.token));
    let ct = ("Content-Type", "application/json".to_string());
    let (status, _, body) = request(
        s.addr,
        "POST",
        "/ingest/deploys",
        &[auth.clone(), ct.clone()],
        br#"{"deploy":"v1","ref":"abc","timestamp":"2026-10-13T10:00:00Z","commits":[{"sha":"abc1","message":"m"}]}"#,
    );
    assert_eq!((status, serde_json::from_slice::<Value>(&body).unwrap()), (201, json!({"ok": true, "deploy": "v1"})));
    let (status, _, body) = request(s.addr, "POST", "/ingest/deploys", &[auth.clone(), ct.clone()], br#"{"deploy":"v2","url":"javascript:x"}"#);
    assert_eq!((status, serde_json::from_slice::<Value>(&body).unwrap()), (422, json!({"error": "Url must be a single http:// or https:// URL"})));
    let (status, _, body) = request(s.addr, "POST", "/ingest/deploys", &[auth.clone(), ct.clone()], b"{}");
    assert_eq!(
        (status, serde_json::from_slice::<Value>(&body).unwrap()),
        (400, json!({"error": "param is missing or the value is empty or invalid: deploy"})),
        "params.require(:deploy)"
    );
    // Without a timestamp a re-post keeps deployed_at; a new deploy gets now.
    let deployed_at = |d: &str| -> String { s.control.with(|c| c.query_row("SELECT deployed_at FROM deploys WHERE deploy = ?", [d], |r| r.get(0))).unwrap() };
    let (status, _, _) = request(s.addr, "POST", "/ingest/deploys", &[auth.clone(), ct.clone()], br#"{"deploy":"v1","ref":"abd"}"#);
    assert_eq!(status, 201);
    assert_eq!(deployed_at("v1"), "2026-10-13 10:00:00", "a re-post without timestamp keeps deployed_at");
    let r: Option<String> = s.control.with(|c| c.query_row("SELECT ref FROM deploys WHERE deploy = 'v1'", [], |r| r.get(0))).unwrap();
    assert_eq!(r.as_deref(), Some("abd"), "the other fields are still updated");
    request(s.addr, "POST", "/ingest/deploys", &[auth.clone(), ct.clone()], br#"{"deploy":"v1","timestamp":"2026-10-13T11:00:00Z"}"#);
    assert_eq!(deployed_at("v1"), "2026-10-13 11:00:00", "an explicit timestamp still sets it");
    request(s.addr, "POST", "/ingest/deploys", &[auth.clone(), ct.clone()], br#"{"deploy":"v3"}"#);
    assert!(deployed_at("v3").as_str() > "2026-10-14", "a new deploy without timestamp is now");
    let (status, _, body) = request(s.addr, "POST", "/ingest/kamal", &[auth.clone(), ct.clone()], br#"{"hosts":[" b ","a","a",""],"roles":"web"}"#);
    assert_eq!((status, serde_json::from_slice::<Value>(&body).unwrap()), (201, json!({"ok": true, "hosts": ["a", "b"]})));
    let (status, _, body) = request(
        s.addr,
        "POST",
        "/ingest/sourcemaps",
        &[auth.clone(), ("X-Railwatch-Deploy", "v1".into()), ("X-Railwatch-Filename", "assets/app-123.js".into())],
        b"{\"version\":3}",
    );
    assert_eq!((status, serde_json::from_slice::<Value>(&body).unwrap()), (201, json!({"ok": true, "filename": "assets/app-123.js", "bytes": 13})));
    assert_eq!(std::fs::read(s.dir.0.join("sourcemaps/test-env/v1/assets/app-123.js.map")).unwrap(), b"{\"version\":3}");
    let (status, _, _) =
        request(s.addr, "POST", "/ingest/sourcemaps", &[auth, ("X-Railwatch-Deploy", "v1".into()), ("X-Railwatch-Filename", "../../etc/passwd".into())], b"x");
    assert_eq!(status, 422);
}

#[test]
fn an_invalid_utf8_type_is_rejected_alone_with_a_scrubbed_type() {
    // Rails used to fail the whole batch (500) encoding the rejection; now
    // the type is `scrub`bed (one U+FFFD per invalid sequence) and the rest
    // of the batch is accepted.
    let s = start(0, 0);
    let mut body = ndjson(&[fixture("query")]);
    body.extend_from_slice(b"{\"t\":\"log\xff\xfe-x\",\"timestamp\":");
    body.extend_from_slice(format!("{}}}\n", now()).as_bytes());
    let mut raw = ndjson(&[fixture("log")]);
    body.append(&mut raw);
    let (status, _, ack) = s.post(&gzip(&body), &[]);
    assert_eq!(status, 200, "{ack}");
    assert_eq!((ack["accepted"].as_i64(), ack["rejected"].as_i64()), (Some(2), Some(1)));
    assert_eq!(ack["rejections"][0]["type"], "log\u{fffd}\u{fffd}-x");
    let stored: String = s.tenant().query_row("SELECT rejections FROM ingest_batches", [], |r| r.get(0)).unwrap();
    assert!(stored.contains("log\u{fffd}\u{fffd}-x"), "{stored}");
}
