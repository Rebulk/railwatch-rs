//! GA review 2026-11-19 (security #2): `POST /ingest/sourcemaps` wrote to
//! the data dir with no per-environment cap, no pause check, no pruning and
//! no cleanup, and `max_data_bytes` did not see it. One ingest token could
//! fill the server's disk at ~6 GiB a minute, paused or not. This is the
//! reviewer's reproduction, inverted.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;

fn request(addr: SocketAddr, path: &str, headers: &[(&str, String)], body: &[u8]) -> u16 {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn dir_bytes(p: &std::path::Path) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        let m = e.metadata().unwrap();
        n += if m.is_dir() { dir_bytes(&e.path()) } else { m.len() };
    }
    n
}

#[test]
fn sourcemap_uploads_are_refused_when_paused_capped_per_environment_and_counted_by_the_disk_budget() {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let dir = std::env::temp_dir().join(format!("rw-ga-sm-{}-{}", std::process::id(), u64::from_le_bytes(b)));
    std::fs::create_dir_all(&dir).unwrap();
    let control = Arc::new(Control::open_with(&dir.join("control.sqlite3"), true).unwrap());
    let token = control::new_token();
    let env = control.create("victim-env", &control::digest(&token), 30).unwrap();
    control.with(|c| c.execute("UPDATE environments SET paused_at = '2026-01-01 00:00:00' WHERE id = ?", [env.id])).unwrap();

    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    l.set_nonblocking(true).unwrap();
    let d2 = dir.clone();
    let c2 = control.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (tx, rx) = followups::channel();
            let tenants = Tenants::new(d2.join("telemetry"), Duration::from_secs(60), tx);
            let state = Arc::new(AppState {
                control: c2.clone(),
                tenants,
                ip_limit: FixedWindow::new(2_400),
                env_limit: FixedWindow::new(600),
                trust_proxy: false,
                trusted_proxies: Vec::new(),
                sourcemaps_dir: d2.join("sourcemaps"),
                cpu: Arc::new(tokio::sync::Semaphore::new(4)),
                disk: Default::default(),
            });
            tokio::spawn(followups::run(c2, rx));
            let listener = tokio::net::TcpListener::from_std(l).unwrap();
            axum::serve(listener, http::router(state).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
    });
    std::thread::sleep(Duration::from_millis(200));

    let auth = ("Authorization", format!("Bearer {token}"));
    let blob = vec![b'A'; 10 * 1024 * 1024 - 64];
    let upload = |i: usize| {
        request(
            addr,
            "/ingest/sourcemaps",
            &[auth.clone(), ("X-Railwatch-Deploy", format!("d{i}")), ("X-Railwatch-Filename", format!("assets/app-{i}.js"))],
            &blob,
        )
    };
    // Paused: refused, nothing written.
    assert_eq!(upload(0), 409, "refused while paused");
    assert_eq!(dir_bytes(&dir.join("sourcemaps")), 0);

    control.with(|c| c.execute("UPDATE environments SET paused_at = NULL WHERE id = ?", [env.id])).unwrap();
    let mut statuses = Vec::new();
    for i in 0..30 {
        statuses.push(upload(i));
    }
    assert!(statuses.iter().all(|s| *s == 201), "{statuses:?}");
    let written = dir_bytes(&dir.join("sourcemaps"));
    let cap = railwatch_server::storage::sourcemaps::MAX_ENV_BYTES;
    assert!(written <= cap, "{} MiB kept, the cap is {} MiB", written >> 20, cap >> 20);
    let deploys = std::fs::read_dir(dir.join("sourcemaps/victim-env")).unwrap().count();
    assert!(deploys <= railwatch_server::storage::sourcemaps::MAX_DEPLOYS, "{deploys} deploys kept");
    let (counted, _) = railwatch_server::storage::budget::usage(&dir.join("telemetry"));
    assert!(counted >= written, "max_data_bytes sees the source maps ({counted} of {written})");

    // A much lower upload rate than /ingest's 600 a minute.
    let mut limited = false;
    for i in 30..30 + railwatch_server::storage::sourcemaps::UPLOADS_PER_MINUTE as usize {
        let s =
            request(addr, "/ingest/sourcemaps", &[auth.clone(), ("X-Railwatch-Deploy", "small".into()), ("X-Railwatch-Filename", format!("a{i}.js"))], b"{}");
        if s == 429 {
            limited = true;
            break;
        }
    }
    assert!(limited, "uploads are rate limited per environment");
    // Over quota (enforce_quota on): refused like /ingest's records.
    control.with(|c| c.execute("UPDATE accounts SET monthly_event_quota = 0, additional_events_cap = 0", [])).unwrap();
    let s = request(addr, "/ingest/sourcemaps", &[auth.clone(), ("X-Railwatch-Deploy", "q".into()), ("X-Railwatch-Filename", "q.js".into())], b"{}");
    assert_eq!(s, 402, "over quota");
    let _ = std::fs::remove_dir_all(&dir);
}
