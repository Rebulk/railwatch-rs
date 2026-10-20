//! security-b PoCs (adversarial pre-open-source review, ingest focus).
//!
//! Regression tests:
//!  1. A valid ingest-token holder could panic the `/ingest/deploys` handler
//!     task with a crafted `timestamp` whose UTF-8 bytes made `parse_time`
//!     slice a `&str` at a non-char boundary (http.rs `split_offset`): no
//!     HTTP response, a per-connection DoS (a crash if built panic=abort).
//!     Now every such timestamp is a clean 422 like any unparseable one.
//!
//! These spin the real ingest server on an ephemeral port (same shape as
//! tests/ingest_http.rs) and talk to it with a tolerant raw HTTP client, so
//! a reset connection is observed rather than unwrapped.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;

struct Dir(std::path::PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    addr: SocketAddr,
    _dir: Dir,
    token: String,
}

fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("rw-secb-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let control = Arc::new(Control::open_with(&dir.join("control.sqlite3"), true).unwrap());
    let token = control::new_token();
    control.create("test-env", &control::digest(&token), 30).unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    l.set_nonblocking(true).unwrap();
    let d2 = dir.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (tx, rx) = followups::channel();
            let tenants = Tenants::new(d2.join("telemetry"), Duration::from_secs(60), tx);
            let state = Arc::new(AppState {
                control: control.clone(),
                tenants,
                ip_limit: FixedWindow::new(0),
                env_limit: FixedWindow::new(0),
                trust_proxy: false,
                sourcemaps_dir: d2.join("sourcemaps"),
                cpu: Arc::new(tokio::sync::Semaphore::new(4)),
                disk: Default::default(),
            });
            tokio::spawn(followups::run(control, rx));
            let listener = tokio::net::TcpListener::from_std(l).unwrap();
            axum::serve(listener, http::router(state).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
    });
    std::thread::sleep(Duration::from_millis(200));
    Server { addr, _dir: Dir(dir), token }
}

fn rand_suffix() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    u64::from_le_bytes(b)
}

/// Tolerant raw HTTP/1.1 client: Some(status) if the server returned a valid
/// response line, None if the connection was reset / returned no HTTP (e.g.
/// the handler task panicked before writing a response).
fn request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> Option<(u16, Vec<u8>)> {
    let mut s = std::net::TcpStream::connect(addr).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).ok()?;
    let _ = s.write_all(body);
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let line = String::from_utf8_lossy(&buf[..split]);
    let status: u16 = line.lines().next()?.split(' ').nth(1)?.parse().ok()?;
    Some((status, buf[split + 4..].to_vec()))
}

fn bearer(token: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {token}"))
}

fn ping_ok(s: &Server) -> bool {
    matches!(request(s.addr, "GET", "/ingest/ping", &[bearer(&s.token)], b""), Some((200, _)))
}

/// A graceful, unparseable timestamp (no multibyte) is a clean 422, proving
/// the deploys path itself works and validates the field.
#[test]
fn deploys_rejects_bad_timestamp_gracefully() {
    let s = start();
    let headers = [bearer(&s.token), ("Content-Type", "application/json".into())];
    let body = br#"{"deploy":"x","timestamp":"not-a-date"}"#;
    let got = request(s.addr, "POST", "/ingest/deploys", &headers, body);
    assert_eq!(got.map(|(st, _)| st), Some(422), "a plain bad timestamp is a clean 422");
    assert!(ping_ok(&s));
}

/// Was a panic: the SAME request but with a `timestamp` whose bytes made
/// `split_offset` slice `&s[s.len()-6..]` inside a multibyte char. Now the
/// handler answers the same 422 as for any unparseable timestamp.
#[test]
fn deploys_multibyte_timestamp_is_a_clean_422() {
    let s = start();
    let headers = [bearer(&s.token), ("Content-Type", "application/json".into())];
    // "€a€" = E2 82 AC 61 E2 82 AC (7 bytes); len-6 == 1 is inside the first '€'.
    let body = "{\"deploy\":\"x\",\"timestamp\":\"\u{20ac}a\u{20ac}\"}".as_bytes();
    let got = request(s.addr, "POST", "/ingest/deploys", &headers, body);
    assert_eq!(got.map(|(st, _)| st), Some(422), "a multibyte timestamp is a clean 422, not a dropped connection");
    assert!(ping_ok(&s));
}

/// A handful of other multibyte timestamps that land off a char boundary at
/// len-6 (and in the fraction), to cover the class of inputs.
#[test]
fn deploys_various_multibyte_timestamps_are_422() {
    let s = start();
    for ts in ["\u{20ac}a\u{20ac}", "aa\u{20ac}\u{20ac}a", "\u{1F600}\u{1F600}", "a\u{1F600}aa\u{1F600}", "2026-10-13 10:00:00.\u{20ac}\u{20ac}"] {
        let headers = [bearer(&s.token), ("Content-Type", "application/json".into())];
        let body = format!("{{\"deploy\":\"x\",\"timestamp\":\"{ts}\"}}");
        let got = request(s.addr, "POST", "/ingest/deploys", &headers, body.as_bytes());
        assert_eq!(got.map(|(st, _)| st), Some(422), "{ts:?}");
    }
    assert!(ping_ok(&s), "server must still serve after the batch of crafted timestamps");
}
