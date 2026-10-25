//! `base_path` + `trusted_proxy_secret_file` over real HTTP: the web router
//! mounted at /railwatch with the ingest API at the root, as `serve` builds
//! it, and requests a proxy in front would send.
//!
//! One test, sequential, because Config::load reads process-global env.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState, proxy_auth};
use serde_json::json;

const SECRET_KEY_BASE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PROXY_SECRET: &[u8] = b"proxy-secret-proxy-secret-proxy-secret-0123";

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// One request over a raw socket, so the path reaches the server exactly
/// as written (reqwest would normalize `..` and percent-encodings away).
async fn raw(addr: SocketAddr, method: &str, path: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn set_cookie(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok()?.split(';').next()?.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn signed(email: &str, role: &str) -> String {
    proxy_auth::sign(PROXY_SECRET, &json!({"email": email, "name": "Ada", "role": role, "exp": now() + 60}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mounted_under_a_base_path_behind_a_trusted_proxy() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let dir = std::env::temp_dir().join(format!("rw-trusted-proxy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let secret_file = dir.join("proxy_secret");
    std::fs::write(&secret_file, PROXY_SECRET).unwrap();
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET_KEY_BASE);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::set_var("RAILWATCH_BASE_PATH", "/railwatch/");
        std::env::set_var("RAILWATCH_TRUSTED_PROXY_SECRET_FILE", secret_file.to_str().unwrap());
        std::env::remove_var("RAILWATCH_CONFIG");
    }
    let cfg = Config::load().expect("config");
    assert_eq!(cfg.base_path, "/railwatch");
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let (app_id, env_id, token) = {
        let owner = control.tx(|tx| control::insert_user(tx, "Owner", "owner@example.com", &control::base58(16), true)).unwrap();
        let account = control.tx(|tx| control::create_account(tx, "Acme", owner)).unwrap();
        let app = control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, token) = control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
        (app, env, token)
    };

    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    // What `setup` does after creating the environment.
    let slug: String = control.with(|c| c.query_row("SELECT slug FROM environments WHERE id = ?", [env_id], |r| r.get(0))).unwrap();
    tenants.ensure(&slug).unwrap();
    let state = Arc::new(AppState {
        control: control.clone(),
        tenants: tenants.clone(),
        ip_limit: FixedWindow::new(0),
        env_limit: FixedWindow::new(0),
        trust_proxy: false,
        sourcemaps_dir: dir.join("sourcemaps"),
        cpu: Arc::new(tokio::sync::Semaphore::new(2)),
        disk: Default::default(),
    });
    let web_state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    // What every route sees of the header: proxy_auth::layer removes it.
    let echo = axum::Router::new()
        .route("/echo", axum::routing::get(|h: axum::http::HeaderMap| async move { if h.contains_key(proxy_auth::HEADER) { "leaked" } else { "stripped" } }))
        .layer(axum::middleware::from_fn_with_state(web_state.clone(), proxy_auth::layer))
        .with_state(web_state.clone());
    let off_state = {
        // The same server with proxy auth off.
        let mut off = cfg.clone();
        off.trusted_proxy_secret = None;
        let (tx, _rx) = followups::channel();
        Arc::new(WebState::new(control.clone(), off, Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx), web::cable::Hub::new()).unwrap())
    };
    let off_echo = axum::Router::new()
        .route("/echo", axum::routing::get(|h: axum::http::HeaderMap| async move { if h.contains_key(proxy_auth::HEADER) { "leaked" } else { "stripped" } }))
        .layer(axum::middleware::from_fn_with_state(off_state.clone(), proxy_auth::layer))
        .with_state(off_state.clone());
    let off_app = web::mounted(&cfg.base_path, web::router(off_state), http::router(state.clone())).into_make_service_with_connect_info::<SocketAddr>();
    let off_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let off_addr = off_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(off_listener, off_app).await.unwrap() });
    let app = web::mounted(&cfg.base_path, web::router(web_state).merge(echo.clone()), http::router(state)).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let get = |path: &str, auth: Option<String>| {
        let mut r = http.get(format!("{base}{path}"));
        if let Some(a) = auth {
            r = r.header(proxy_auth::HEADER, a);
        }
        r.send()
    };

    // Ingest stays at the root: the gem's RAILWATCH_INGEST_URL is unchanged.
    let up = get("/up", None).await.unwrap();
    assert_eq!(up.status(), 200, "/up at the root");
    let ping = http.get(format!("{base}/ingest/ping")).bearer_auth(&token).send().await.unwrap();
    assert!(ping.status().is_success(), "/ingest/ping at the root with the env token: {}", ping.status());

    // Unsigned: the normal sign-in flow, under the base path.
    let r = get("/railwatch/dashboard", None).await.unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(r.headers()["location"], "/railwatch/sign_in", "redirects carry the base path");
    let sign_in = get("/railwatch/sign_in", None).await.unwrap();
    let cookie_paths: Vec<String> = sign_in.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect();
    assert!(!cookie_paths.is_empty() && cookie_paths.iter().all(|c| c.contains("Path=/railwatch;")), "cookies scoped to the mount: {cookie_paths:?}");
    let html = sign_in.text().await.unwrap();
    assert!(html.contains(r#"<meta name="railwatch-mount" content="/railwatch">"#), "route helpers get the mount");
    assert!(html.contains(r#"<meta name="action-cable-url" content="/railwatch/cable">"#), "cable under the mount");
    assert!(html.contains(r#"src="/railwatch/vite/assets/"#), "the entry script under the mount");
    assert!(html.contains(r#"href="/railwatch/icon.png""#));
    let script = html.split(r#"src="/railwatch/vite/"#).nth(1).unwrap().split('"').next().unwrap();
    let js = get(&format!("/railwatch/vite/{script}"), None).await.unwrap();
    assert_eq!(js.status(), 200, "assets served under the mount");
    assert_eq!(get("/dashboard", None).await.unwrap().status(), 404, "pages are not also at the root");
    // MCP and the API move with the pages; their discovery URLs say so.
    let mcp: serde_json::Value = get("/railwatch/mcp", None).await.unwrap().json().await.unwrap();
    assert_eq!(mcp["endpoint"], format!("http://{addr}/railwatch/mcp"));
    assert_eq!(get("/railwatch/api/v1/accounts", None).await.unwrap().status(), 401, "the API under the mount, token required");
    assert_eq!(get("/mcp", None).await.unwrap().status(), 404, "not also at the root");
    // Paths that try to climb out of the base path, or smuggle a slash, go
    // nowhere: no route matches them, and nothing normalizes them first.
    for path in ["/railwatch/../dashboard", "/railwatch/..%2fdashboard", "/railwatch%2fdashboard", "//railwatch/dashboard", "/railwatch/%2e%2e/up"] {
        assert_eq!(raw(addr, "GET", path).await, 404, "{path}");
    }
    assert_eq!(raw(addr, "GET", "/railwatch/vite/../../control.sqlite3").await, 404, "assets stay inside dist/");
    assert_eq!(raw(addr, "GET", "/railwatchx/dashboard").await, 404, "a sibling of the base path is not it");

    // Signed by the proxy: that user, created on first sight, no sign-in.
    let page = format!("/railwatch/apps/{app_id}/envs/{env_id}");
    let r = get(&page, Some(signed("ada@example.com", "admin"))).await.unwrap();
    assert_eq!(r.status(), 200, "a proxied admin sees the environment");
    let html = r.text().await.unwrap();
    assert!(html.contains("ada@example.com"), "the page is rendered for the proxied user");
    let (role, sessions): (String, i64) = control
        .with(|c| {
            c.query_row(
                "SELECT m.role, (SELECT count(*) FROM sessions s WHERE s.user_id = u.id) FROM users u JOIN memberships m ON m.user_id = u.id WHERE u.email = 'ada@example.com'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!((role.as_str(), sessions), ("admin", 1));
    get(&page, Some(signed("ada@example.com", "admin"))).await.unwrap();
    let sessions: i64 = control.with(|c| c.query_row("SELECT count(*) FROM sessions WHERE user_agent = 'railwatch trusted proxy'", [], |r| r.get(0))).unwrap();
    assert_eq!(sessions, 1, "one session row reused, not one per request");

    // A mutating request still needs the session's CSRF token: the proxy
    // forwards the browser's XSRF cookie and header, it does not bypass CSRF.
    let r = http
        .post(format!("{base}/railwatch/apps/{app_id}/envs/{env_id}/saved_views"))
        .header(proxy_auth::HEADER, signed("ada@example.com", "admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422, "no CSRF token: refused even with a valid proxy header");
    // With the token the browser was given, it goes through.
    let page_res = get(&page, Some(signed("ada@example.com", "admin"))).await.unwrap();
    let (xsrf, rw_session) = (set_cookie(&page_res, "XSRF-TOKEN").expect("XSRF-TOKEN"), set_cookie(&page_res, "_railwatch_session").expect("session"));
    let post = |path: String, referer: &str| {
        http.post(format!("{base}{path}"))
            .header(proxy_auth::HEADER, signed("ada@example.com", "admin"))
            .header("cookie", format!("_railwatch_session={rw_session}; XSRF-TOKEN={xsrf}"))
            .header("x-xsrf-token", &xsrf)
            .header("origin", &base)
            .header("referer", referer)
            .header("content-type", "application/json")
    };
    let r = post(format!("/railwatch/apps/{app_id}/envs/{env_id}/saved_views"), &format!("{base}/railwatch/apps/{app_id}/envs/{env_id}/requests"))
        .body(json!({"name": "Slow", "page": "requests"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302, "with the session's CSRF token the write is accepted");
    assert_eq!(r.headers()["location"], format!("/railwatch/apps/{app_id}/envs/{env_id}/requests"), "back to the Referer, under the base path");
    // Referers that would send the redirect off-site, or out of the base
    // path into the host app, fall back to the default.
    for (referer, to) in [
        (format!("{base}/railwatch//evil.example/x"), "/railwatch/issues"),
        (format!("{base}/railwatch/\\evil.example"), "/railwatch/issues"),
        (format!("{base}/admin/users"), "/railwatch/issues"),
        // Only the Referer's path is used, so another host is just a path here.
        ("https://evil.example/railwatch/x".to_string(), "/railwatch/x"),
    ] {
        let r = post("/railwatch/issues/bulk".into(), &referer).body("{}").send().await.unwrap();
        assert_eq!(r.headers().get("location").map(|v| v.to_str().unwrap()), Some(to), "{referer}");
    }

    // Forged, expired, or too long-lived headers: 401, never a fallback.
    let forged =
        proxy_auth::sign(b"not-the-secret-not-the-secret-not-the-se", &json!({"email": "eve@example.com", "name": "Eve", "role": "admin", "exp": now() + 60}));
    assert_eq!(get(&page, Some(forged)).await.unwrap().status(), 401);
    let expired = proxy_auth::sign(PROXY_SECRET, &json!({"email": "ada@example.com", "name": "Ada", "role": "admin", "exp": now() - 1}));
    assert_eq!(get(&page, Some(expired)).await.unwrap().status(), 401);
    let forever = proxy_auth::sign(PROXY_SECRET, &json!({"email": "ada@example.com", "name": "Ada", "role": "admin", "exp": now() + 86_400}));
    assert_eq!(get(&page, Some(forever)).await.unwrap().status(), 401);
    let eve: i64 = control.with(|c| c.query_row("SELECT count(*) FROM users WHERE email = 'eve@example.com'", [], |r| r.get(0))).unwrap();
    assert_eq!(eve, 0, "a forged header provisions nobody");

    // The header never reaches a handler, valid or not.
    assert_eq!(get("/railwatch/echo", Some(signed("ada@example.com", "admin"))).await.unwrap().text().await.unwrap(), "stripped");
    // With proxy auth off, the header is ignored entirely: a forged one does
    // nothing (no 401, no user), and it is still stripped.
    let off = |path: &str, auth: String| http.get(format!("http://{off_addr}{path}")).header(proxy_auth::HEADER, auth).send();
    let forged_off =
        proxy_auth::sign(b"anything-anything-anything-anything", &json!({"email": "mallory@example.com", "name": "M", "role": "admin", "exp": now() + 60}));
    let r = off(&page, forged_off.clone()).await.unwrap();
    assert_eq!((r.status().as_u16(), r.headers()["location"].to_str().unwrap()), (302, "/railwatch/sign_in"), "off: a normal signed-out request");
    let r = off(&page, signed("mallory@example.com", "admin")).await.unwrap();
    assert_eq!(r.status(), 302, "off: even a correctly signed header is not honoured");
    let mallory: i64 = control.with(|c| c.query_row("SELECT count(*) FROM users WHERE email = 'mallory@example.com'", [], |r| r.get(0))).unwrap();
    assert_eq!(mallory, 0, "off: nobody is provisioned");
    let off_echo_app = off_echo.into_make_service();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, off_echo_app).await.unwrap() });
    let r = http.get(format!("http://{echo_addr}/echo")).header(proxy_auth::HEADER, forged_off).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "stripped", "off: removed all the same");

    // The live channel: the same header authorizes the websocket handshake.
    let ws = http
        .get(format!("{base}/railwatch/cable"))
        .header(proxy_auth::HEADER, signed("ada@example.com", "member"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-protocol", "actioncable-v1-json")
        .send()
        .await
        .unwrap();
    assert_eq!(ws.status(), 101, "cable upgrades under the mount");
    let bad_ws = http
        .get(format!("{base}/railwatch/cable"))
        .header(proxy_auth::HEADER, "v1.e30.AAAA")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ws.status(), 401, "a bad header refuses the handshake too");
    let role: String = control
        .with(|c| c.query_row("SELECT m.role FROM memberships m JOIN users u ON u.id = m.user_id WHERE u.email = 'ada@example.com'", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(role, "member", "the proxy's role wins on every request");

    // MCP and the API: the signed header is a credential there too, for the
    // same user, so an agent behind the proxy needs no pasted token.
    let rpc = |auth: Option<String>, origin: Option<&str>, body: serde_json::Value| {
        let mut r = http.post(format!("{base}/railwatch/mcp")).header("content-type", "application/json").body(body.to_string());
        if let Some(a) = auth {
            r = r.header(proxy_auth::HEADER, a);
        }
        if let Some(o) = origin {
            r = r.header("origin", o);
        }
        r.send()
    };
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let r = rpc(Some(signed("agent@example.com", "member")), None, list.clone()).await.unwrap();
    assert_eq!(r.status(), 200, "tools/list with the proxy's header, no token");
    let tools: serde_json::Value = r.json().await.unwrap();
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"get_execution") && names.contains(&"add_comment"), "read and write tools, as in the dashboard: {names:?}");
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_applications", "arguments": {}}});
    let r: serde_json::Value = rpc(Some(signed("agent@example.com", "member")), None, call).await.unwrap().json().await.unwrap();
    let apps = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(apps.contains("\"shop\""), "the proxied user sees the proxy's account: {apps}");
    let agent: i64 = control.with(|c| c.query_row("SELECT count(*) FROM users WHERE email = 'agent@example.com'", [], |r| r.get(0))).unwrap();
    assert_eq!(agent, 1, "the agent is a user like any proxied visitor");
    assert_eq!(rpc(None, None, list.clone()).await.unwrap().status(), 401, "no header, no token: still 401");
    let forged =
        proxy_auth::sign(b"not-the-secret-not-the-secret-not-the-se", &json!({"email": "eve@example.com", "name": "Eve", "role": "admin", "exp": now() + 60}));
    assert_eq!(rpc(Some(forged), None, list.clone()).await.unwrap().status(), 401, "a forged header is refused on MCP too");
    // Another site's page, through a browser that passes the app's gate:
    // refused, since these routes take no CSRF token.
    let r = rpc(Some(signed("agent@example.com", "member")), Some("https://evil.example"), list.clone()).await.unwrap();
    assert_eq!(r.status(), 403, "cross-origin MCP through the proxy");
    // A token, when one is sent, is the only credential looked at.
    let r = http
        .post(format!("{base}/railwatch/mcp"))
        .header(proxy_auth::HEADER, signed("agent@example.com", "member"))
        .bearer_auth("rwp_not_a_token")
        .body(list.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "a bad token is not rescued by the proxy's header");
    // Nor is an Authorization header that is not even ASCII.
    let r = http
        .post(format!("{base}/railwatch/mcp"))
        .header(proxy_auth::HEADER, signed("agent@example.com", "member"))
        .header("authorization", reqwest::header::HeaderValue::from_bytes(b"Bearer \xff").unwrap())
        .body(list.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "an unreadable Authorization is a bad token, not a fall back to the proxy");
    let api = |path: &str, auth: Option<String>, origin: Option<&str>| {
        let mut r = http.get(format!("{base}{path}"));
        if let Some(a) = auth {
            r = r.header(proxy_auth::HEADER, a);
        }
        if let Some(o) = origin {
            r = r.header("origin", o);
        }
        r.send()
    };
    let me = api("/railwatch/api/v1/me", Some(signed("agent@example.com", "member")), None).await.unwrap();
    assert_eq!(me.status(), 200, "/api/v1 with the proxy's header");
    let me: serde_json::Value = me.json().await.unwrap();
    assert_eq!(me["user"]["email"], "agent@example.com");
    assert_eq!(me["token"]["name"], proxy_auth::SESSION_AGENT);
    assert_eq!(api("/railwatch/api/v1/me", None, None).await.unwrap().status(), 401);
    // Writes are the dashboard's (issues, comments); a deploy is not one.
    let deploy = http
        .post(format!("{base}/railwatch/api/v1/applications/{app_id}/environments/{env_id}/deploys"))
        .header(proxy_auth::HEADER, signed("agent@example.com", "admin"))
        .header("content-type", "application/json")
        .body(json!({"sha": "abc1234"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(deploy.status(), 403, "the proxy's user cannot record deploys");
    assert_eq!(api("/railwatch/api/v1/me", Some(signed("agent@example.com", "member")), Some("https://evil.example")).await.unwrap().status(), 403);
    // Proxy auth off: the header grants nothing on MCP or the API either.
    let off_rpc =
        http.post(format!("http://{off_addr}/railwatch/mcp")).header(proxy_auth::HEADER, signed("agent@example.com", "member")).body(list.to_string());
    assert_eq!(off_rpc.send().await.unwrap().status(), 401, "off: MCP ignores the header");
    let off_me = http.get(format!("http://{off_addr}/railwatch/api/v1/me")).header(proxy_auth::HEADER, signed("agent@example.com", "member"));
    assert_eq!(off_me.send().await.unwrap().status(), 401, "off: the API ignores the header");

    unsafe {
        std::env::remove_var("RAILWATCH_BASE_PATH");
        std::env::remove_var("RAILWATCH_TRUSTED_PROXY_SECRET_FILE");
    }
    std::fs::remove_dir_all(&dir).ok();
}
