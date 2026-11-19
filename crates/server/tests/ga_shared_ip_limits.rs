//! GA tenancy review 2026-11-19, T4: the per-address limits in front of
//! ingest and MCP were counted before authentication, so customers behind
//! one egress address (a PaaS, CI, a NAT) used up each other's. Now only
//! requests that fail to authenticate count toward the address; an
//! authenticated one counts toward its environment (ingest) or credential
//! (MCP).

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState};

fn request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> u16 {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    let _ = s.write_all(body);
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rw-ga-ip-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn one_tenants_ingest_never_uses_up_another_tenants_shared_address_limit() {
    let d = dir("ingest");
    let control = Arc::new(Control::open(&d.join("control.sqlite3")).unwrap());
    let (a, b) = (control::new_token(), control::new_token());
    control.create("acme-web-production", &control::digest(&a), 30).unwrap();
    control.create("bravo-web-production", &control::digest(&b), 30).unwrap();
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    l.set_nonblocking(true).unwrap();
    let (c2, d2) = (control.clone(), d.clone());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let (tx, _rx) = followups::channel();
            let tenants = Tenants::new(d2.join("telemetry"), Duration::from_secs(60), tx);
            let state = Arc::new(AppState {
                control: c2,
                disk: tenants.disk(),
                tenants,
                // A small per-address limit, a large per-environment one.
                ip_limit: FixedWindow::new(5),
                env_limit: FixedWindow::new(1_000),
                trust_proxy: false,
                trusted_proxies: Vec::new(),
                sourcemaps_dir: d2.join("sourcemaps"),
                cpu: Arc::new(tokio::sync::Semaphore::new(2)),
            });
            let listener = tokio::net::TcpListener::from_std(l).unwrap();
            axum::serve(listener, http::router(state).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
    });
    std::thread::sleep(Duration::from_millis(200));
    let ping = |t: &str| request(addr, "GET", "/ingest/ping", &[("Authorization", format!("Bearer {t}"))], b"");
    // Tenant A, same address, far past the address limit: all answered.
    for _ in 0..20 {
        assert_eq!(ping(&a), 200);
    }
    assert_eq!(ping(&b), 200, "B, from the same address, is not limited by A's traffic");
    // Unauthenticated garbage still hits the address limit.
    let statuses: Vec<u16> = (0..8).map(|_| ping("rw_wrong")).collect();
    assert_eq!(statuses.iter().filter(|s| **s == 401).count(), 5, "{statuses:?}");
    assert!(statuses.ends_with(&[429, 429, 429]), "{statuses:?}");
    let _ = std::fs::remove_dir_all(&d);
}

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_tokens_mcp_calls_never_use_up_another_tokens_shared_address_limit() {
    let d = dir("mcp");
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", d.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::remove_var("RAILWATCH_CONFIG");
    }
    let cfg = Config::load().expect("config");
    let control = Arc::new(Control::open(&cfg.control_path()).unwrap());
    let token = |email: &str| {
        let u = control.tx(|tx| control::insert_user(tx, "U", email, "correct horse battery", true)).unwrap();
        control.tx(|tx| control::create_account(tx, email, u)).unwrap();
        control.tx(|tx| control::issue_api_token(tx, u, "t", &["mcp".into(), "read".into()], None, None, None)).unwrap().1
    };
    let (alice, bob) = (token("alice@a.test"), token("bob@b.test"));
    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(60), tx);
    let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, web::router(state).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() });
    let call = move |t: String| {
        tokio::task::spawn_blocking(move || {
            let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
            request(addr, "POST", "/mcp", &[("Authorization", format!("Bearer {t}")), ("Content-Type", "application/json".into())], body)
        })
    };
    let limit = railwatch_server::web::mcp::PREAUTH_IP_LIMIT;
    for _ in 0..limit {
        assert_eq!(call(alice.clone()).await.unwrap(), 200);
    }
    assert_eq!(call(alice.clone()).await.unwrap(), 429, "Alice's own credential is limited");
    // PR #107 review M3: the same token written differently (whitespace
    // inside the header value before CRLF, which hyper may or may not strip)
    // is the same credential, not a fresh bucket.
    for variant in [format!("{alice} "), format!("{alice}\t"), format!("{alice} \t ")] {
        assert_eq!(call(variant.clone()).await.unwrap(), 429, "{variant:?} shares Alice's bucket");
    }
    assert_eq!(call(bob.clone()).await.unwrap(), 200, "Bob, same address, is not");
    // Oversized bodies (413) count toward the address: past the limit,
    // even a valid token from that address is refused.
    let oversized = move || {
        tokio::task::spawn_blocking(move || {
            // Chunked, so the server reads up to its 4 MiB cap and stops.
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            s.write_all(b"POST /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            let chunk = vec![b' '; 1 << 20];
            for _ in 0..5 {
                if s.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).and_then(|_| s.write_all(&chunk)).and_then(|_| s.write_all(b"\r\n")).is_err() {
                    break;
                }
            }
            let _ = s.write_all(b"0\r\n\r\n");
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).split(' ').nth(1).and_then(|c| c.parse::<u16>().ok()).unwrap_or(0)
        })
    };
    for _ in 0..limit {
        assert_eq!(oversized().await.unwrap(), 413);
    }
    assert_eq!(call(bob.clone()).await.unwrap(), 429, "the address is full of 413s");
    let _ = std::fs::remove_dir_all(&d);
}
