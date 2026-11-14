//! page-replay: GET dashboard pages through the real web router over a data
//! dir (a copy of a server's), at a frozen "now", signed in as a user, and
//! write each page's Inertia response. For comparing a server's pages with
//! Rails' over snapshots taken at the same moment.
//!
//!   page-replay DATA_DIR CASES.json NOW_EPOCH USER_ID OUT.json [WARM_RUNS]
//!
//! CASES.json is a list of {name, path (with query string), account_id?}
//! (account_id: the account cookie, for account-level pages such as
//! /issues/:id). Writes {name => {component, props} | {status, body}}. A session row is added to the copy's
//! control DB, so never point it at a live server's data dir.
//!
//! A case with `body` is a POST of that JSON (an MCP call: path "/mcp" and
//! `authorization`, "Bearer <token>"), its output {status, body}.
//!
//! Timings (bench/perf/dashboard.sh): each case runs 1 + WARM_RUNS times
//! and OUT.json.timings.json gets {name => {first, first_read_bytes, warm:
//! [sorted ms]}}. `first` is the case's first call in this process; on a
//! data dir out of the page cache and as the process's first case, that is
//! a cold read. `first_read_bytes` is what that call read from storage
//! (/proc/self/io read_bytes, mmap faults included): unlike its time, it
//! does not move with other load on the machine.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use railwatch_server::config::Config;
use railwatch_server::control::Control;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState, cookies};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 5 {
        eprintln!("usage: page-replay DATA_DIR CASES NOW_EPOCH USER_ID OUT [WARM]");
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&a[0]);
    // SAFETY: before any other thread reads the environment.
    unsafe { std::env::set_var("RAILWATCH_DATA_DIR", &dir) };
    let cfg = Config::load().expect("config");
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control db"));
    let now = (a[2].parse::<f64>().unwrap() * 1e6).round() as i64;
    railwatch_server::time::travel_to(now);
    let user: i64 = a[3].parse().unwrap();
    let session = control.with(|c| railwatch_server::control::create_session(c, user, None, None)).expect("session");
    let (followups, _rx) = railwatch_server::followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), std::time::Duration::from_secs(600), followups);
    let state = Arc::new(WebState::new(control, cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    let cookie = format!("{}={}", cookies::SESSION_TOKEN, state.keys.sign(cookies::SESSION_TOKEN, &session.to_string()));
    let state_keys = cookies::Keys::new(&cfg.secret().expect("secret"));
    let app = web::router(state);

    let keys = state_keys;
    let warm: usize = a.get(5).map_or(0, |w| w.parse().unwrap());
    let get = |path: String, account: Option<i64>, headers: Vec<(&'static str, String)>, body: Option<String>| {
        let app = app.clone();
        let mut cookie = cookie.clone();
        if let Some(id) = account {
            cookie = format!("{cookie}; {}={}", cookies::ACCOUNT_ID, keys.sign(cookies::ACCOUNT_ID, &id.to_string()));
        }
        async move {
            let r = if body.is_some() { Request::post(&path).header("content-type", "application/json") } else { Request::get(&path) };
            let mut r = r.header("host", "localhost").header("cookie", cookie);
            for (k, v) in headers {
                r = r.header(k, v);
            }
            let res = app.oneshot(r.body(body.map_or_else(Body::empty, Body::from)).unwrap()).await.unwrap();
            let status = res.status().as_u16();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&body).to_string())
        }
    };
    let version = web::assets::manifest().version.clone();

    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let mut out = serde_json::Map::new();
    let mut timings = serde_json::Map::new();
    for c in &cases {
        let post = c.get("body").map(|b| b.as_str().map_or_else(|| b.to_string(), str::to_string));
        let mut headers = vec![("x-inertia", "true".into()), ("x-inertia-version", version.clone()), ("accept", "text/html, application/xhtml+xml".into())];
        if let Some(auth) = c["authorization"].as_str() {
            headers.push(("authorization", auth.to_string()));
        }
        let mut samples = Vec::with_capacity(warm + 1);
        let mut last = None;
        let mut first_read = 0;
        for i in 0..=warm {
            let (read0, t0) = (read_bytes(), std::time::Instant::now());
            last = Some(get(c["path"].as_str().unwrap().to_string(), c["account_id"].as_i64(), headers.clone(), post.clone()).await);
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
            if i == 0 {
                first_read = read_bytes() - read0;
            }
        }
        let (status, body) = last.unwrap();
        let v = match serde_json::from_str::<Value>(&body) {
            Ok(v) if post.is_some() => serde_json::json!({"status": status, "body": v}),
            Ok(page) if status == 200 => serde_json::json!({"component": page["component"], "props": page["props"]}),
            _ => serde_json::json!({"status": status, "body": body.chars().take(300).collect::<String>()}),
        };
        let name = c["name"].as_str().unwrap().to_string();
        let first = samples.remove(0);
        samples.sort_by(|x, y| x.partial_cmp(y).unwrap());
        timings.insert(name.clone(), serde_json::json!({"first": first, "first_read_bytes": first_read, "warm": samples}));
        out.insert(name, v);
    }
    std::fs::write(&a[4], serde_json::to_string(&Value::Object(out)).unwrap()).unwrap();
    if a.len() > 5 {
        std::fs::write(format!("{}.timings.json", a[4]), serde_json::to_string(&Value::Object(timings)).unwrap()).unwrap();
    }
    eprintln!("replayed at {now}");
}

/// Bytes this process has read from storage so far (Linux /proc/self/io;
/// 0 where it is missing).
fn read_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("read_bytes: ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0)
}
