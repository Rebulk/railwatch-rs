//! Uptime over the real web router (docs/uptime.md): the public status
//! page and its JSON show only what is marked public and 404 when off; only
//! admins change monitors; the MCP tool and /api/v1 answer.
//!
//! One test, sequential, because Config::load reads process-global env.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::tenants::Tenants;
use railwatch_server::uptime::{self, MonitorInput, probe};
use railwatch_server::web::{self, WebState, cookies};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn session_cookie(session_id: i64) -> String {
    let keys = cookies::Keys::new(SECRET.as_bytes());
    format!("{}={}", cookies::SESSION_TOKEN, keys.sign(cookies::SESSION_TOKEN, &session_id.to_string()))
}

fn set_cookie(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers().get_all(reqwest::header::SET_COOKIE).iter().find_map(|v| {
        let pair = v.to_str().ok()?.split(';').next()?;
        let (k, val) = pair.split_once('=')?;
        (k.trim() == name).then(|| val.trim().to_string())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uptime_over_http() {
    let dir = std::env::temp_dir().join(format!("rw-uptime-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::remove_var("RAILWATCH_CONFIG");
        std::env::remove_var("RAILWATCH_ALLOW_PRIVATE_MONITOR_TARGETS");
    }
    let cfg = Config::load().expect("config");
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let password = control::base58(16);
    let owner = control.tx(|tx| control::insert_user(tx, "Ada", "ada@example.com", &password, true)).unwrap();
    let account = control.tx(|tx| control::create_account(tx, "Acme", owner)).unwrap();
    let member = control.tx(|tx| control::insert_user(tx, "Mo", "mo@example.com", &password, true)).unwrap();
    control.tx(|tx| control::insert_membership(tx, account, member, "member")).unwrap();
    let app = control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let (env, _) = control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let (_, token) = control.tx(|tx| control::issue_api_token(tx, owner, "t", &["api".into(), "read".into(), "mcp".into()], None, None, None)).unwrap();

    // Monitors and checks, as the probe loop would record them.
    let add = |m: MonitorInput| control.with(|x| uptime::insert(x, env, &m)).unwrap().unwrap();
    let public = add(MonitorInput {
        name: "web-1 internal".into(),
        url: "https://shop.example.com/up".into(),
        public: true,
        public_name: Some("Storefront".into()),
        ..Default::default()
    });
    let private = add(MonitorInput { name: "Admin".into(), url: "https://admin.example.com/secret-health".into(), ..Default::default() });
    let now = railwatch_server::time::now_micros();
    let down = probe::Outcome {
        status_code: Some(500),
        latency_ms: 30,
        failure: Some(probe::Failure::Status),
        detail: Some("status 500, expected 2xx or 3xx".into()),
    };
    let up = probe::Outcome { status_code: Some(200), latency_ms: 25, failure: None, detail: None };
    for (i, id) in [public, private].into_iter().enumerate() {
        let at = now - 600_000_000 + i as i64;
        control.with(|x| uptime::record(x, id, at, &down)).unwrap();
        control.with(|x| uptime::record(x, id, at + 60_000_000, &down)).unwrap();
        control.with(|x| uptime::record(x, id, at + 120_000_000, &up)).unwrap();
    }
    control.with(|x| uptime::save_status_page(x, env, "acme-status", "Acme status", false)).unwrap().unwrap();

    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    let router = web::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

    // ----- the status page is off: 404, the same as an unknown slug -------
    for path in ["/status/acme-status", "/status/acme-status.json", "/status/nope", "/status/..%2Fx"] {
        let r = http.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(r.status(), 404, "{path} while the page is off");
    }

    // ----- on: only the public monitor and public fields -------------------
    control.with(|x| uptime::save_status_page(x, env, "acme-status", "Acme status", true)).unwrap().unwrap();
    let r = http.get(format!("{base}/status/acme-status")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "public, max-age=30");
    assert!(r.headers().get(reqwest::header::SET_COOKIE).is_none(), "the public page sets no cookie");
    assert!(r.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    let html = r.text().await.unwrap();
    assert!(html.contains("Acme status") && html.contains("Storefront") && html.contains("All systems operational"), "{html}");
    assert!(html.contains("Resolved after"), "the public monitor's incident is listed");
    assert!(!html.contains("<script"), "no dashboard bundle");
    let json = http.get(format!("{base}/status/acme-status.json")).send().await.unwrap();
    assert_eq!(json.headers()["content-type"], "application/json");
    let body = json.text().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["components"].as_array().unwrap().len(), 1);
    assert_eq!(v["components"][0]["name"], "Storefront");
    assert_eq!(v["incidents"].as_array().unwrap().len(), 1);
    for text in [&html, &body] {
        for leak in ["Admin", "admin.example.com", "secret-health", "web-1 internal", "shop.example.com", "status 500", "\"id\"", "environment"] {
            assert!(!text.contains(leak), "the public page leaks {leak:?}");
        }
    }

    // ----- monitors: members read, only admins write -----------------------
    let mut csrf = Vec::new();
    for user in [owner, member] {
        let sid = control.with(|x| control::create_session(x, user, Some("ua"), Some("127.0.0.1"))).unwrap();
        let sess = session_cookie(sid);
        let r = http.get(format!("{base}/apps/{app}/envs/{env}/uptime")).header("Cookie", &sess).send().await.unwrap();
        assert_eq!(r.status(), 200, "the Uptime page renders for every member");
        let xsrf = set_cookie(&r, "XSRF-TOKEN").unwrap();
        let rw = set_cookie(&r, cookies::SESSION).unwrap();
        let cookie = format!("{sess}; {}={rw}", cookies::SESSION);
        // The Inertia JSON of the same page.
        let page: serde_json::Value = http
            .get(format!("{base}/apps/{app}/envs/{env}/uptime"))
            .header("Cookie", &cookie)
            .header("X-Inertia", "true")
            .header("X-Inertia-Version", web::assets::manifest().version.as_str())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page["component"], "uptime/index");
        assert_eq!(page["props"]["monitors"].as_array().map(Vec::len), Some(2));
        assert_eq!(page["props"]["can_manage"], user == owner, "only the owner manages");
        csrf.push((cookie, xsrf));
    }
    let post = |cookie: &str, xsrf: &str, body: serde_json::Value| {
        http.post(format!("{base}/apps/{app}/envs/{env}/uptime/monitors"))
            .header("Cookie", cookie)
            .header("Origin", &base)
            .header("X-XSRF-TOKEN", xsrf)
            .json(&body)
            .send()
    };
    let (member_cookie, member_xsrf) = &csrf[1];
    let r = post(member_cookie, member_xsrf, serde_json::json!({"name": "Mine", "url": "https://example.com/"})).await.unwrap();
    assert_eq!(r.status(), 403, "a member cannot add monitors");
    let (owner_cookie, owner_xsrf) = &csrf[0];
    let r = post(owner_cookie, owner_xsrf, serde_json::json!({"name": "Fast", "url": "https://example.com/", "interval_seconds": "10"})).await.unwrap();
    assert_eq!(r.status(), 302);
    let r = post(owner_cookie, owner_xsrf, serde_json::json!({"name": "Inside", "url": "http://169.254.169.254/latest/meta-data"})).await.unwrap();
    assert_eq!(r.status(), 302);
    let names: Vec<String> = control.with(|x| Ok(uptime::monitors(x, env)?.into_iter().map(|m| m.name).collect())).unwrap();
    assert_eq!(names, ["Admin", "web-1 internal"], "a 10 s interval and a metadata address are both refused");
    let r = post(
        owner_cookie,
        owner_xsrf,
        serde_json::json!({"name": "Shop API", "url": "https://api.example.com/up", "interval_seconds": "30", "timeout_seconds": "5"}),
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 302);
    let added = control.with(|x| uptime::monitors(x, env)).unwrap().into_iter().find(|m| m.name == "Shop API").expect("added");
    assert_eq!((added.interval_seconds, added.timeout_seconds, added.status.as_str()), (30, 5, "pending"));

    // ----- MCP and /api/v1 ----------------------------------------------------
    let call = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "uptime", "arguments": {}}});
    let r = http.post(format!("{base}/mcp")).bearer_auth(&token).json(&call).send().await.unwrap();
    let v: serde_json::Value = r.json().await.unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{v}"));
    let out: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(out["monitors"].as_array().unwrap().len(), 3);
    assert_eq!(out["incidents"].as_array().unwrap().len(), 2);
    assert_eq!(out["status_page"], format!("{base}/status/acme-status"));
    assert!(!text.contains("null"), "compact: no null fields");
    let api = http.get(format!("{base}/api/v1/applications/{app}/environments/{env}/uptime")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(api.status(), 200);
    let v: serde_json::Value = api.json().await.unwrap();
    assert_eq!(v["data"].as_array().unwrap().len(), 3);
    assert_eq!(v["data"][1]["days"].as_array().unwrap().len(), 90);

    let _ = std::fs::remove_dir_all(&dir);
}
