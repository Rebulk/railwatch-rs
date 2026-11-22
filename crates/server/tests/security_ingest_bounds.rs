//! Audit 2026-11-22 (rw-h-ingest): three ways one ingest token grew shared
//! state that nothing bounded, over the real ingest router and real HTTP.
//!
//! 1. Every distinct `server` named on a batch's rows became a watchdog row
//!    in the control database every account shares, never deleted (not even
//!    with its environment) and scanned each tick with the one control
//!    connection held.
//! 2. `/ingest/deploys` and `/ingest/kamal` wrote while the environment was
//!    paused or over quota, and deploy rows had no per-environment limit.
//! 3. `/ingest/sourcemaps` counted file bytes only, so empty maps under deep
//!    filenames made directories the per-environment cap never saw.
//!
//! Dummy data only, loopback only.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::jobs::watchdog;
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use serde_json::{Value, json};

struct Srv {
    addr: SocketAddr,
    dir: std::path::PathBuf,
    control: Arc<Control>,
    heartbeats: Arc<watchdog::Heartbeats>,
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The ingest router with the background jobs' heartbeats wired to the
/// follow-ups, as main.rs does.
fn start(name: &str) -> Srv {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let dir = std::env::temp_dir().join(format!("rw-bounds-{}-{name}-{}", std::process::id(), u64::from_le_bytes(b)));
    std::fs::create_dir_all(&dir).unwrap();
    let control = Arc::new(Control::open_with(&dir.join("control.sqlite3"), true).unwrap());
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    l.set_nonblocking(true).unwrap();
    let (hb_tx, hb_rx) = std::sync::mpsc::channel();
    let (c2, d2) = (control.clone(), dir.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (tx, rx) = followups::channel();
            let tenants = Tenants::new(d2.join("telemetry"), Duration::from_secs(60), tx);
            let jobs = railwatch_server::jobs::start(c2.clone(), d2.join("telemetry"));
            hb_tx.send(jobs.background.heartbeats.clone()).unwrap();
            let state = Arc::new(AppState {
                control: c2.clone(),
                tenants,
                ip_limit: FixedWindow::new(2_400),
                env_limit: FixedWindow::new(0),
                trust_proxy: false,
                trusted_proxies: Vec::new(),
                sourcemaps_dir: d2.join("sourcemaps"),
                cpu: Arc::new(tokio::sync::Semaphore::new(4)),
                disk: Default::default(),
            });
            tokio::spawn(followups::run_with(c2, rx, None, Some(jobs.background.clone())));
            let listener = tokio::net::TcpListener::from_std(l).unwrap();
            axum::serve(listener, http::router(state).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
    });
    let heartbeats = hb_rx.recv().unwrap();
    Srv { addr, dir, control, heartbeats }
}

/// A separate account with one application and one environment: (env id, token).
fn tenant(c: &Control, account: &str) -> (i64, String) {
    c.tx(|tx| {
        let acct = control::insert_account(tx, account, account, 30)?;
        let app = control::insert_application(tx, acct, "web", "web", "WEB", None, "main")?;
        control::create_environment(tx, app, "production")
    })
    .unwrap()
}

fn request(addr: SocketAddr, path: &str, headers: &[(&str, String)], body: &[u8]) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

fn count(c: &Control, sql: &str, id: i64) -> i64 {
    c.with(|x| x.query_row(sql, [id], |r| r.get(0))).unwrap()
}

fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

fn health_batch(servers: impl Iterator<Item = String>) -> Vec<u8> {
    let all: Value = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let t = now();
    let mut body = Vec::new();
    for s in servers {
        let mut r = all["health"].clone();
        r["timestamp"] = json!(t);
        r["server"] = json!(s);
        body.extend_from_slice(serde_json::to_string(&r).unwrap().as_bytes());
        body.push(b'\n');
    }
    body
}

/// Waits for the follow-up of a committed batch to reach the watchdog.
fn wait_for(hb: &watchdog::Heartbeats, env: i64, what: impl Fn(usize) -> bool) -> usize {
    let started = Instant::now();
    loop {
        let n = hb.snapshot().iter().filter(|((e, _), _)| *e == env).count();
        if what(n) || started.elapsed() > Duration::from_secs(60) {
            return n;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const HEARTBEATS: &str = "SELECT COUNT(*) FROM environment_heartbeats WHERE environment_id = ?";

/// One batch naming 2,000 servers leaves at most the cap watched and
/// persisted; the rest count for the environment as a whole.
#[test]
fn a_batch_naming_more_servers_than_the_cap_leaves_at_most_the_cap() {
    let s = start("hb-cap");
    let (env, token) = tenant(&s.control, "many-hosts");
    let cap = watchdog::MAX_SERVERS_PER_ENVIRONMENT;
    let body = health_batch((0..2_000).map(|i| format!("h{i}")));
    let (status, answer) = request(s.addr, "/ingest", &[("Authorization", format!("Bearer {token}"))], &body);
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains("\"accepted\":2000"), "every record is still stored: {answer}");
    // One beat per batch: once any pair is there, the whole batch is.
    let seen = wait_for(&s.heartbeats, env, |n| n > 0);
    assert!(seen <= cap + 1, "{seen} pairs watched; the cap is {cap} named servers plus the environment");
    let named: Vec<String> = s.heartbeats.snapshot().into_iter().filter(|((e, srv), _)| *e == env && !srv.is_empty()).map(|((_, srv), _)| srv).collect();
    assert_eq!(named.len(), cap, "the first {cap} names are watched");
    assert!(s.heartbeats.snapshot().iter().any(|((e, srv), _)| *e == env && srv.is_empty()), "the rest count for the environment as a whole");
    // A second batch of new names adds none.
    let body = health_batch((2_000..2_500).map(|i| format!("h{i}")));
    assert_eq!(request(s.addr, "/ingest", &[("Authorization", format!("Bearer {token}"))], &body).0, 200);
    std::thread::sleep(Duration::from_millis(500));
    watchdog::check(&s.control.conn(), &s.heartbeats, railwatch_server::time::now_micros(), 60).unwrap();
    let rows = count(&s.control, HEARTBEATS, env) as usize;
    assert!(rows <= cap + 1, "{rows} control rows; the cap is {cap} + 1");
}

/// Rows a server from before this fix left for a deleted environment (the
/// delete did not touch the table) go at the next start, and the pairs
/// still in memory do not put them back. That the delete itself now removes
/// them is web::controllers' deletion test.
#[test]
fn a_deleted_environment_leaves_no_heartbeat_rows() {
    let s = start("hb-delete");
    let (env, token) = tenant(&s.control, "gone-co");
    let (other, _) = tenant(&s.control, "stays-co");
    let body = health_batch((0..5).map(|i| format!("web-{i}")));
    assert_eq!(request(s.addr, "/ingest", &[("Authorization", format!("Bearer {token}"))], &body).0, 200);
    assert_eq!(wait_for(&s.heartbeats, env, |n| n == 5), 5);
    s.heartbeats.beat(other, &["web-1".into()], railwatch_server::time::now_micros());
    watchdog::check(&s.control.conn(), &s.heartbeats, railwatch_server::time::now_micros(), 60).unwrap();
    assert_eq!(count(&s.control, HEARTBEATS, env), 5);
    // The environment goes the way an older binary deleted it: its own
    // rows, not the watchdog's.
    s.control.with(|c| c.execute("DELETE FROM environments WHERE id = ?", [env])).unwrap();
    // The next start trims them.
    let restarted = watchdog::Heartbeats::default();
    restarted.restore(&s.control.conn()).unwrap();
    assert_eq!(count(&s.control, HEARTBEATS, env), 0, "a deleted environment's rows go at the next start");
    assert!(restarted.snapshot().iter().all(|((e, _), _)| *e != env));
    // A tick with its pairs still in memory (the running process) does not recreate them.
    watchdog::check(&s.control.conn(), &s.heartbeats, railwatch_server::time::now_micros(), 60).unwrap();
    assert_eq!(count(&s.control, HEARTBEATS, env), 0);
    assert_eq!(count(&s.control, HEARTBEATS, other), 1, "another environment's row stays");
}

/// Control: the watchdog still alerts once for a real server that goes
/// silent, and once when it reports again, with the cap and window cleanup
/// in place.
#[test]
fn the_watchdog_still_alerts_for_a_real_silent_server() {
    let s = start("hb-alert");
    let (env, _) = tenant(&s.control, "alerting-co");
    let c = s.control.conn();
    c.execute(
        "INSERT INTO integrations (account_id, created_at, enabled, kind, name, settings, updated_at)
         SELECT a.account_id, '2026-01-01', 1, 'slack', 'x', '{\"webhook_url\":\"https://hooks.slack.com/services/T/B/x\"}', '2026-01-01'
         FROM environments e JOIN applications a ON a.id = e.application_id WHERE e.id = ?",
        [env],
    )
    .unwrap();
    let integ = c.last_insert_rowid();
    for event in ["silent_environment", "environment_recovered"] {
        c.execute(
            "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at)
             SELECT application_id, '2026-01-01', ?, '{}', ?, '2026-01-01' FROM environments WHERE id = ?",
            rusqlite::params![event, integ, env],
        )
        .unwrap();
    }
    let hb = watchdog::Heartbeats::default();
    let t0 = 1_791_900_000_000_000i64;
    hb.beat(env, &["web-1".into(), "web-2".into()], t0);
    assert!(watchdog::check(&c, &hb, t0 + 30_000_000, 60).unwrap().is_empty());
    hb.beat(env, &["web-1".into()], t0 + 50_000_000);
    let t = watchdog::check(&c, &hb, t0 + 70_000_000, 60).unwrap();
    assert_eq!(t, vec![watchdog::Transition::Silent { env, server: "web-2".into(), last_seen: t0 }]);
    assert!(watchdog::check(&c, &hb, t0 + 80_000_000, 60).unwrap().is_empty(), "no repeat");
    hb.beat(env, &["web-2".into()], t0 + 90_000_000);
    let t = watchdog::check(&c, &hb, t0 + 95_000_000, 60).unwrap();
    assert!(matches!(&t[..], [watchdog::Transition::Recovered { server, .. }] if server == "web-2"), "{t:?}");
    assert_eq!(count_on(&c, "SELECT COUNT(*) FROM alerts WHERE event = 'silent_environment'"), 1);
    assert_eq!(count_on(&c, "SELECT COUNT(*) FROM alerts WHERE event = 'environment_recovered'"), 1);
}

/// An environment that once named more servers than the cap gets an
/// environment-wide entry (server ""). It must keep advancing while the
/// watched servers report, or the watchdog calls the whole environment
/// silent although every watched server is sending.
#[test]
fn a_capped_environment_is_not_reported_silent_while_its_servers_report() {
    let s = start("hb-capped");
    let (env, _) = tenant(&s.control, "capped-co");
    let c = s.control.conn();
    let hb = watchdog::Heartbeats::default();
    let t0 = 1_791_900_000_000_000i64;
    let names = |n: usize| (0..n).map(|i| format!("web-{i}")).collect::<Vec<String>>();
    // One batch past the cap: the extra name counts for the environment.
    hb.beat(env, &names(watchdog::MAX_SERVERS_PER_ENVIRONMENT + 1), t0);
    assert!(watchdog::check(&c, &hb, t0 + 1_000_000, 60).unwrap().is_empty());
    // Then only the watched servers report, every 20 s, well past the
    // 60 s silence threshold.
    for step in 1..=6 {
        let at = t0 + step * 20_000_000;
        hb.beat(env, &names(watchdog::MAX_SERVERS_PER_ENVIRONMENT), at);
        let t = watchdog::check(&c, &hb, at + 1_000_000, 60).unwrap();
        assert!(t.is_empty(), "no server, and not the environment, is silent at step {step}: {t:?}");
    }
}

/// Rows past the watch window are deleted; one still marked silent stays,
/// so a server back after a long outage still gets its recovery alert.
#[test]
fn rows_past_the_watch_window_are_deleted_unless_still_silent() {
    let s = start("hb-window");
    let (env, _) = tenant(&s.control, "window-co");
    let c = s.control.conn();
    let t0 = 1_791_900_000_000_000i64;
    let day = watchdog::WATCH_WINDOW_SECS * 1_000_000;
    c.execute("INSERT INTO environment_heartbeats (environment_id, server, last_seen_micros) VALUES (?, 'retired', ?)", rusqlite::params![env, t0 - 2 * day])
        .unwrap();
    c.execute(
        "INSERT INTO environment_heartbeats (environment_id, server, last_seen_micros, silent_since_micros) VALUES (?, 'down', ?, ?)",
        rusqlite::params![env, t0 - 2 * day, t0 - 2 * day + 60_000_000],
    )
    .unwrap();
    let hb = watchdog::Heartbeats::default();
    hb.beat(env, &["web-1".into()], t0);
    assert!(watchdog::check(&c, &hb, t0 + 1, 60).unwrap().is_empty());
    let left: Vec<String> =
        c.prepare("SELECT server FROM environment_heartbeats ORDER BY server").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(left, ["down", "web-1"], "the retired server's row went; the silent one stays");
    hb.beat(env, &["down".into()], t0 + 2);
    let t = watchdog::check(&c, &hb, t0 + 3, 60).unwrap();
    assert!(matches!(&t[..], [watchdog::Transition::Recovered { server, .. }] if server == "down"), "{t:?}");
}

fn count_on(c: &rusqlite::Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// A paused environment's deploy posts store nothing, refused with the
/// source-map action's 409; over quota, its 402.
#[test]
fn a_paused_or_over_quota_environment_stores_no_deploy_marker() {
    let s = start("paused-deploy");
    let (env, token) = tenant(&s.control, "paused-co");
    let auth = ("Authorization", format!("Bearer {token}"));
    let ct = ("Content-Type", "application/json".to_string());
    s.control.with(|c| c.execute("UPDATE environments SET paused_at = '2026-01-01 00:00:00' WHERE id = ?", [env])).unwrap();
    let deploy = request(s.addr, "/ingest/deploys", &[auth.clone(), ct.clone()], br#"{"deploy":"release-1"}"#);
    let map = request(s.addr, "/ingest/sourcemaps", &[auth.clone(), ("X-Railwatch-Deploy", "d".into()), ("X-Railwatch-Filename", "a.js".into())], b"{}");
    assert_eq!(map, (409, r#"{"error":"environment is paused"}"#.to_string()));
    assert_eq!(deploy, map, "the same refusal as the source-map action");
    assert_eq!(count(&s.control, "SELECT COUNT(*) FROM deploys WHERE environment_id = ?", env), 0);

    s.control.with(|c| c.execute("UPDATE environments SET paused_at = NULL WHERE id = ?", [env])).unwrap();
    s.control.with(|c| c.execute("UPDATE accounts SET monthly_event_quota = 0, additional_events_cap = 0", [])).unwrap();
    let deploy = request(s.addr, "/ingest/deploys", &[auth.clone(), ct.clone()], br#"{"deploy":"release-1"}"#);
    assert_eq!(deploy, (402, r#"{"error":"quota"}"#.to_string()));
    assert_eq!(count(&s.control, "SELECT COUNT(*) FROM deploys WHERE environment_id = ?", env), 0);
}

/// The same for Kamal markers: nothing about the hosts is stored.
#[test]
fn a_paused_or_over_quota_environment_stores_no_kamal_marker() {
    let s = start("paused-kamal");
    let (env, token) = tenant(&s.control, "paused-co");
    let auth = ("Authorization", format!("Bearer {token}"));
    let ct = ("Content-Type", "application/json".to_string());
    let stored = || -> (String, String) {
        s.control.with(|c| c.query_row("SELECT expected_servers, kamal_config FROM environments WHERE id = ?", [env], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap()
    };
    s.control.with(|c| c.execute("UPDATE environments SET paused_at = '2026-01-01 00:00:00' WHERE id = ?", [env])).unwrap();
    let kamal = request(s.addr, "/ingest/kamal", &[auth.clone(), ct.clone()], br#"{"hosts":["web-9"],"version":"v1"}"#);
    assert_eq!(kamal, (409, r#"{"error":"environment is paused"}"#.to_string()));
    assert_eq!(stored(), ("[]".to_string(), "{}".to_string()), "Kamal hosts are not stored");

    s.control.with(|c| c.execute("UPDATE environments SET paused_at = NULL WHERE id = ?", [env])).unwrap();
    s.control.with(|c| c.execute("UPDATE accounts SET monthly_event_quota = 0, additional_events_cap = 0", [])).unwrap();
    let kamal = request(s.addr, "/ingest/kamal", &[auth.clone(), ct.clone()], br#"{"hosts":["web-9"]}"#);
    assert_eq!(kamal, (402, r#"{"error":"quota"}"#.to_string()));
    assert_eq!(stored(), ("[]".to_string(), "{}".to_string()));
}

/// After N+1 distinct deploys the environment holds the newest N, and an
/// ordinary deploy is still recorded and answered as before.
#[test]
fn an_environment_keeps_only_its_newest_deploys() {
    let s = start("deploy-cap");
    let (env, token) = tenant(&s.control, "busy-co");
    let (other, other_token) = tenant(&s.control, "quiet-co");
    let n = http::MAX_DEPLOYS_PER_ENVIRONMENT;
    let ct = ("Content-Type", "application/json".to_string());
    let post = |token: &str, body: String| request(s.addr, "/ingest/deploys", &[("Authorization", format!("Bearer {token}")), ct.clone()], body.as_bytes());
    let first = post(&other_token, json!({"deploy": "v1", "ref": "abc", "timestamp": "2026-10-13T10:00:00Z"}).to_string());
    assert_eq!(first, (201, r#"{"ok":true,"deploy":"v1"}"#.to_string()), "an ordinary deploy is recorded as before");
    // Distinct, increasing timestamps: release-0 is the oldest.
    for i in 0..=n {
        let at = format!("2026-10-{:02}T{:02}:{:02}:00Z", 1 + i / 1440, (i / 60) % 24, i % 60);
        let (st, body) = post(&token, json!({"deploy": format!("release-{i}"), "timestamp": at}).to_string());
        assert_eq!(st, 201, "release-{i}: {body}");
    }
    assert_eq!(count(&s.control, "SELECT COUNT(*) FROM deploys WHERE environment_id = ?", env), n);
    let oldest: i64 = count(&s.control, "SELECT COUNT(*) FROM deploys WHERE environment_id = ? AND deploy = 'release-0'", env);
    assert_eq!(oldest, 0, "the oldest went");
    assert_eq!(count(&s.control, &format!("SELECT COUNT(*) FROM deploys WHERE environment_id = ? AND deploy = 'release-{n}'"), env), 1, "the newest stays");
    assert_eq!(count(&s.control, "SELECT COUNT(*) FROM deploys WHERE environment_id = ?", other), 1, "another environment's deploys are untouched");
    let r: Option<String> =
        s.control.with(|c| c.query_row("SELECT ref FROM deploys WHERE environment_id = ? AND deploy = 'v1'", [other], |r| r.get(0))).unwrap();
    assert_eq!(r.as_deref(), Some("abc"));
}

/// Control: an ordinary nested map stores and reads back byte for byte.
const MAP: &[u8] = br#"{"version":3,"sources":["app.ts"],"names":[],"mappings":"AAAA"}"#;

fn maps_env(name: &str) -> (Srv, String, String) {
    let s = start(name);
    let (env, token) = tenant(&s.control, "maps-co");
    let slug: String = s.control.with(|c| c.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0))).unwrap();
    (s, slug, token)
}

fn upload(s: &Srv, token: &str, deploy: &str, file: &str, body: &[u8]) -> (u16, String) {
    request(
        s.addr,
        "/ingest/sourcemaps",
        &[("Authorization", format!("Bearer {token}")), ("X-Railwatch-Deploy", deploy.into()), ("X-Railwatch-Filename", file.into())],
        body,
    )
}

/// An empty map is refused and creates nothing; an ordinary nested map
/// still stores intact.
#[test]
fn an_empty_source_map_is_refused() {
    let (s, slug, token) = maps_env("maps-empty");
    assert_eq!(upload(&s, &token, "v1", "vite/assets/app-123.js", MAP).0, 201);
    assert_eq!(std::fs::read(s.dir.join(format!("sourcemaps/{slug}/v1/vite/assets/app-123.js.map"))).unwrap(), MAP);
    let (st, body) = upload(&s, &token, "v1", "000/a/a/a.js", b"");
    assert_eq!((st, body.as_str()), (422, r#"{"error":"the source map is empty"}"#));
    assert!(!s.dir.join(format!("sourcemaps/{slug}/v1/000")).exists(), "nothing is created for an empty map");
}

/// Uploads past the environment's file and directory limit are refused and
/// create nothing; what was stored before is still served.
#[test]
fn source_map_uploads_past_the_entry_limit_are_refused() {
    let (s, slug, token) = maps_env("maps-entries");
    assert_eq!(upload(&s, &token, "v1", "vite/assets/app-123.js", MAP).0, 201);
    // The environment 300 entries short of its limit (filled on disk: the
    // walk over tens of thousands of entries per upload is otherwise what
    // the test would spend its time on).
    let limit = railwatch_server::storage::sourcemaps::MAX_ENV_ENTRIES;
    let env_dir = s.dir.join(format!("sourcemaps/{slug}"));
    let fill = env_dir.join("v1/fill");
    std::fs::create_dir_all(&fill).unwrap();
    let have = walk(&env_dir);
    for i in 0..(limit - 300 - have) {
        std::fs::write(fill.join(i.to_string()), b"x").unwrap();
    }
    assert_eq!(walk(&env_dir), limit - 300);
    // Deep filenames: 126 directories and a map each (127 entries). Two
    // fit, then every one is refused (507) and nothing more is created.
    let statuses: Vec<u16> = (0..5).map(|i| upload(&s, &token, "v1", &format!("{i:03}{}", "/a".repeat(126)), b"{}").0).collect();
    assert_eq!(statuses, [201, 201, 507, 507, 507]);
    assert_eq!(walk(&env_dir), limit - 300 + 2 * 127, "refused uploads create nothing");
    assert_eq!(std::fs::read(env_dir.join("v1/vite/assets/app-123.js.map")).unwrap(), MAP, "the ordinary map is still there");
}

/// A filename the upload accepts must also be writable: the map is written
/// to a temporary name first, and an upload that cannot be written must not
/// have pruned any older deploy's maps on the way.
#[test]
fn an_upload_that_cannot_be_written_prunes_nothing() {
    let (s, slug, token) = maps_env("maps-tmpname");
    let env_dir = s.dir.join(format!("sourcemaps/{slug}"));
    // The deploy-count cap: every older deploy past it is pruned before the
    // new map is written.
    let max = railwatch_server::storage::sourcemaps::MAX_DEPLOYS;
    for d in 0..max {
        assert_eq!(upload(&s, &token, &format!("v{d:03}"), "app.js", MAP).0, 201);
    }
    let before = std::fs::read_dir(&env_dir).unwrap().count();
    // 248 bytes is a valid filename; with ".map" it is 252, and its
    // temporary name (".map.tmp") would be 256, past the filesystem's 255.
    let long = "a".repeat(248);
    let (st, body) = upload(&s, &token, "vnew", &long, MAP);
    assert_eq!((st, body.as_str()), (422, r#"{"error":"invalid deploy or filename"}"#), "refused cleanly, not a 500");
    assert_eq!(std::fs::read_dir(&env_dir).unwrap().count(), before, "a refused upload pruned no older deploy");
    assert!(env_dir.join("v000/app.js.map").exists(), "the oldest deploy's map is still there");
    // One byte shorter is written as before (and prunes the oldest deploy).
    assert_eq!(upload(&s, &token, "vnew", &"a".repeat(247), MAP).0, 201);
}

fn walk(p: &std::path::Path) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        n += 1;
        if e.file_type().unwrap().is_dir() {
            n += walk(&e.path());
        }
    }
    n
}
