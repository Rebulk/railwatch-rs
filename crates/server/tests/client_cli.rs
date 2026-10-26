//! The `railwatch` client commands, run as the real binary against a real
//! server on an ephemeral port (the web router mounted at /monitoring, as
//! the CRM mounts it), over seeded telemetry; and against a stub that
//! answers like an older server, for the commands whose tool it lacks.
//!
//! One test, sequential: Config::load reads process-global env.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");
const SECRET_KEY_BASE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn micros_ago(secs: i64) -> String {
    railwatch_server::time::quoted_date_micros(railwatch_server::time::now_micros() - secs * 1_000_000)
}

/// A port nothing listens on: the default endpoint's, so a dev server on
/// this machine's :3000 is not what the test talks to.
fn closed_port() -> String {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port().to_string()
}

/// The client, with nothing from the environment that would point it
/// elsewhere: its config under `home`, no RAILWATCH_URL/TOKEN.
fn client(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("PORT", closed_port())
        .env("RAILWATCH_SERVER_MOUNT_PATH", "/monitoring")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[track_caller]
fn ok(o: &Output) -> String {
    assert_eq!(o.status.code(), Some(0), "stdout:\n{}\nstderr:\n{}", out(o), err(o));
    out(o)
}

/// An execution with an N+1 (12 runs of one shape from one line), the
/// request an exception issue happened in, and its rollup.
fn seed(t: &rusqlite::Connection) {
    t.execute(
        "INSERT INTO executions (execution_id, kind, name, route, group_hash, status, occurred_at, duration, detail, counters)
         VALUES ('11111111-2222-3333-4444-555555555555', 'request', 'GET /orders/:id', '/orders/:id', 'g_orders', 500, ?1, 812000, ?2, '{\"queries\":13}')",
        rusqlite::params![micros_ago(120), json!({"path": "/orders/12"}).to_string()],
    )
    .unwrap();
    for k in 0..12 {
        t.execute(
            "INSERT INTO queries (execution_id, occurred_at, duration, sql, group_hash, source)
             VALUES ('11111111-2222-3333-4444-555555555555', ?1, 2000, 'SELECT \"line_items\".* FROM \"line_items\" WHERE \"line_items\".\"order_id\" = ?', 'g_items', 'app/views/orders/show.html.erb:7')",
            [micros_ago(120 - k)],
        )
        .unwrap();
    }
    t.execute(
        "INSERT INTO n_plus_ones (execution_id, occurred_at, count, sql, group_hash, source)
         VALUES ('11111111-2222-3333-4444-555555555555', ?1, 12, 'SELECT \"line_items\".* FROM \"line_items\" WHERE \"line_items\".\"order_id\" = ?', 'g_items', 'app/views/orders/show.html.erb:7')",
        [micros_ago(119)],
    )
    .unwrap();
    let frames = json!([
        {"file": "app/models/order.rb", "line": 31, "function": "total", "in_app": true, "code": {"31": "line_items.sum(&:price) / count"}},
        {"file": "/gems/actionpack-8.1.4/lib/a.rb", "line": 1, "function": "a", "in_app": false},
        {"file": "/gems/actionpack-8.1.4/lib/b.rb", "line": 2, "function": "b", "in_app": false},
    ]);
    t.execute(
        "INSERT INTO exceptions (id, class_name, message, occurred_at, group_hash, frames, execution_id, execution_source)
         VALUES (7, 'ZeroDivisionError', 'divided by 0', ?1, 'gx', ?2, '11111111-2222-3333-4444-555555555555', 'request')",
        rusqlite::params![micros_ago(110), frames.to_string()],
    )
    .unwrap();
    let hour = railwatch_server::time::quoted_date_micros(railwatch_server::time::now_micros() / 3_600_000_000 * 3_600_000_000);
    t.execute(
        "INSERT INTO rollups (record_type, group_hash, name, bucket, count, error_count, duration_sum, p95) VALUES ('request', 'g_orders', 'GET /orders/:id', ?1, 1, 1, 812000, 812000)",
        [hour],
    )
    .unwrap();
}

static STUB_SAW_TOKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A stub MCP server that has only the tools named (an older release):
/// tools/list lists them, tools/call answers -32601 for the rest.
async fn old_server(tools: &'static [&'static str]) -> String {
    use axum::routing::post;
    let app = axum::Router::new().route(
        "/mcp",
        post(move |headers: axum::http::HeaderMap, body: String| async move {
            let body: Value = serde_json::from_str(&body).unwrap_or_default();
            if headers.contains_key("authorization") {
                STUB_SAW_TOKEN.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let id = body["id"].clone();
            let r = match body["method"].as_str() {
                Some("tools/list") => json!({"result": {"tools": tools.iter().map(|t| json!({"name": t, "description": "x"})).collect::<Vec<_>>()}}),
                Some("tools/call") if tools.contains(&body["params"]["name"].as_str().unwrap_or("")) => {
                    json!({"result": {"content": [{"type": "text", "text": json!({"error": "no executions"}).to_string()}]}})
                }
                Some("tools/call") => json!({"error": {"code": -32601, "message": format!("Unknown tool: {}", body["params"]["name"].as_str().unwrap_or(""))}}),
                _ => json!({"error": {"code": -32601, "message": "Method not found"}}),
            };
            let mut r = r;
            r["jsonrpc"] = json!("2.0");
            r["id"] = id;
            ([("content-type", "application/json")], r.to_string())
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_commands_against_a_server_mounted_under_a_path() {
    let dir = Dir(std::env::temp_dir().join(format!("rw-client-cli-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let home = dir.0.join("home");
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", dir.0.join("data").to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET_KEY_BASE);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::set_var("RAILWATCH_BASE_PATH", "/monitoring");
        std::env::remove_var("RAILWATCH_TRUSTED_PROXY_SECRET_FILE");
        std::env::remove_var("RAILWATCH_CONFIG");
    }
    let cfg = Config::load().expect("config");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let (env_id, token, read_only) = {
        let owner = control.tx(|tx| control::insert_user(tx, "Owner", "owner@example.com", &control::base58(16), true)).unwrap();
        let account = control.tx(|tx| control::create_account(tx, "Acme", owner)).unwrap();
        let app = control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _) = control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
        let scopes = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (_, token) = control.tx(|tx| control::issue_api_token(tx, owner, "cli", &scopes(&["mcp", "read"]), None, None, None)).unwrap();
        let (_, api_only) = control.tx(|tx| control::issue_api_token(tx, owner, "api", &scopes(&["api", "read"]), None, None, None)).unwrap();
        control
            .with(|c| {
                c.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                       first_seen_at, last_seen_at, created_at, updated_at, sample, culprit)
                     VALUES (?1, ?2, 12, 'ZeroDivisionError: divided by 0', 'exception', 'open', 'gx', 4, 2, ?3, ?3, ?3, ?3, ?4, 'app/models/order.rb:31')",
                    rusqlite::params![
                        app,
                        env,
                        micros_ago(110),
                        json!({"exception_id": 7, "execution_id": "11111111-2222-3333-4444-555555555555", "deploy": "abc1234"}).to_string()
                    ],
                )
            })
            .unwrap();
        (env, token, api_only)
    };
    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    let slug: String = control.with(|c| c.query_row("SELECT slug FROM environments WHERE id = ?", [env_id], |r| r.get(0))).unwrap();
    std::fs::create_dir_all(cfg.tenants_dir()).unwrap();
    seed(&railwatch_server::tenants::open_tenant(&cfg.tenants_dir().join(format!("{slug}.sqlite3"))).unwrap());
    let state = Arc::new(AppState {
        control: control.clone(),
        tenants: tenants.clone(),
        ip_limit: FixedWindow::new(0),
        env_limit: FixedWindow::new(0),
        trust_proxy: false,
        sourcemaps_dir: dir.0.join("sourcemaps"),
        cpu: Arc::new(tokio::sync::Semaphore::new(2)),
        disk: Default::default(),
    });
    let web_state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    let app = web::mounted(&cfg.base_path, web::router(web_state), http::router(state)).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}/monitoring");
    let old = old_server(&["list_issues", "get_issue"]).await;
    let h = home.clone();
    // The client is blocking; keep the runtime's workers free for the server.
    let run = move |args: Vec<String>| {
        let h = h.clone();
        async move { tokio::task::spawn_blocking(move || client(&h, &args.iter().map(String::as_str).collect::<Vec<_>>())).await.unwrap() }
    };
    let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    // Nothing saved: the default is the embedded server's address
    // (localhost:$PORT under RAILWATCH_SERVER_MOUNT_PATH), and the message
    // says so when nothing answers there.
    let none = run(a(&["issues"])).await;
    assert_eq!(none.status.code(), Some(1));
    assert!(err(&none).contains("cannot reach http://localhost:") && err(&none).contains("/monitoring/mcp (the default"), "{}", err(&none));
    assert!(err(&none).contains("railwatch login"), "{}", err(&none));

    // No token: 401, with what to do.
    let anon = run(a(&["issues", "--url", &base])).await;
    assert_eq!(anon.status.code(), Some(1));
    assert!(err(&anon).contains("401") && err(&anon).contains("--token"), "{}", err(&anon));

    // A token without the mcp scope: 403, said plainly.
    let forbidden = run(a(&["issues", "--url", &base, "--token", &read_only])).await;
    assert_eq!(forbidden.status.code(), Some(1));
    assert!(err(&forbidden).contains("403") && err(&forbidden).contains("mcp and read"), "{}", err(&forbidden));

    // login checks the server first and saves nothing when it refuses.
    let bad_login = run(a(&["login", "--url", &base, "--token", "rwp_wrong"])).await;
    assert_eq!(bad_login.status.code(), Some(1));
    assert!(err(&bad_login).contains("Nothing saved"));
    assert!(!home.join("config/railwatch/client.toml").exists());

    // login: the dashboard's address is enough; /mcp is appended. 0600.
    let login = ok(&run(a(&["login", "--url", &base, "--token", &token])).await);
    assert!(login.contains(&format!("{base}/mcp answers with a token")), "{login}");
    let saved = home.join("config/railwatch/client.toml");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(saved.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);

    // issues: one line per issue.
    let issues = ok(&run(a(&["issues"])).await);
    assert!(issues.starts_with("SHOP-12       4×  exception"), "{issues}");
    assert!(issues.contains("ZeroDivisionError: divided by 0"));
    // --json is the tool's own result.
    let raw: Value = serde_json::from_str(&ok(&run(a(&["issues", "--json"])).await)).unwrap();
    assert_eq!(raw[0]["key"], "SHOP-12");
    assert_eq!(raw[0]["url"], format!("http://{addr}/monitoring/issues/1"));

    // issue: this server has no investigate_issue yet, so get_issue,
    // rendered: the in-app frame with its code, the rest collapsed.
    let issue = ok(&run(a(&["issue", "SHOP-12"])).await);
    assert!(issue.starts_with("SHOP-12  ZeroDivisionError: divided by 0\n"), "{issue}");
    assert!(issue.contains("→ app/models/order.rb:31 in total\n        line_items.sum(&:price) / count"), "{issue}");
    assert!(issue.contains("deploy: abc1234"), "{issue}");
    // Not found is exit 2, with the tool's hint.
    let missing = run(a(&["issue", "SHOP-999"])).await;
    assert_eq!(missing.status.code(), Some(2));
    assert!(err(&missing).contains("issue not found") && err(&missing).contains("hint:"), "{}", err(&missing));

    // last: the newest execution, summary first, the N+1 with its line.
    let last = ok(&run(a(&["last"])).await);
    assert!(last.starts_with("GET /orders/:id  /orders/12  500  812 ms\n"), "{last}");
    assert!(last.contains("app/views/orders/show.html.erb:7 — 12×"), "{last}");

    // why: by route name, or by a path (resolved through the newest
    // request), on a server without explain_route: get_route.
    let why = ok(&run(a(&["why", "GET /orders/:id"])).await);
    assert!(why.starts_with("GET /orders/:id\n1 request · p50"), "{why}");
    assert!(why.contains("app/views/orders/show.html.erb:7"), "{why}");
    let by_path = ok(&run(a(&["why", "/orders/12"])).await);
    assert!(by_path.starts_with("(GET /orders/:id: the route of the newest request matching /orders/12)\n"), "{by_path}");
    let no_route = run(a(&["why", "GET /nowhere"])).await;
    assert_eq!(no_route.status.code(), Some(2), "{}", err(&no_route));

    // open prints the dashboard link (no terminal here, so nothing opens).
    assert_eq!(ok(&run(a(&["open", "SHOP-12"])).await).trim(), format!("http://{addr}/monitoring/issues/1"));
    let exe = ok(&run(a(&["open", "11111111-2222-3333-4444-555555555555"])).await);
    assert!(exe.trim().ends_with("/requests/11111111-2222-3333-4444-555555555555"), "{exe}");
    assert_eq!(ok(&run(a(&["open"])).await).trim(), format!("{base}/"));

    // tools, and call: the escape hatch, with JSON-or-string values.
    let tools = ok(&run(a(&["tools"])).await);
    assert!(tools.lines().any(|l| l.starts_with("list_issues ")), "{tools}");
    let called: Value = serde_json::from_str(&ok(&run(a(&["call", "list_issues", "--arg", "limit=1", "--arg", "status=open", "--json"])).await)).unwrap();
    assert_eq!(called.as_array().map(Vec::len), Some(1));
    // An unknown tool: exit 2, naming it.
    let unknown = run(a(&["call", "no_such_tool"])).await;
    assert_eq!(unknown.status.code(), Some(2));
    assert!(err(&unknown).contains("Unknown tool: no_such_tool") && err(&unknown).contains("railwatch tools"), "{}", err(&unknown));

    // tail: what is there now (the 500), then it follows until stopped.
    let tail_line = {
        let h = home.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::BufRead;
            let mut child = Command::new(BIN)
                .args(["tail", "--errors", "--interval", "0.2"])
                .env_clear()
                .env("HOME", &h)
                .env("XDG_CONFIG_HOME", h.join("config"))
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
            // SAFETY: signalling our own child, by its pid.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            child.wait().unwrap();
            line
        })
        .await
        .unwrap()
    };
    assert!(tail_line.contains("500") && tail_line.contains("GET /orders/:id  11111111-2222-3333-4444-555555555555"), "{tail_line}");

    // The saved token belongs to the saved server: the stub never sees it.
    // Against an older server, the commands whose tool it lacks say which
    // release has it; the ones it can serve still work.
    let cmp = run(a(&["compare", "--url", &old])).await;
    assert_eq!(cmp.status.code(), Some(2));
    assert!(err(&cmp).contains("needs the compare tool") && err(&cmp).contains("upgrade the server to railwatch-rs v0.2.9"), "{}", err(&cmp));
    let old_last = run(a(&["last", "--url", &old])).await;
    assert_eq!(old_last.status.code(), Some(2));
    assert!(err(&old_last).contains("needs the last_execution tool") && err(&old_last).contains("v0.2.7"), "{}", err(&old_last));
    let old_why = run(a(&["why", "GET /x", "--url", &old])).await;
    assert!(err(&old_why).contains("needs the explain_route tool"), "{}", err(&old_why));
    let old_issue = run(a(&["issue", "SHOP-1", "--url", &old])).await;
    assert_eq!(old_issue.status.code(), Some(2), "get_issue answers no executions -> not found");
    assert!(err(&old_issue).contains("no executions"), "{}", err(&old_issue));
    assert!(!STUB_SAW_TOKEN.load(std::sync::atomic::Ordering::SeqCst), "the saved token went to another server");

    // Usage errors are exit 2.
    assert_eq!(run(a(&["issue"])).await.status.code(), Some(2));
    assert_eq!(run(a(&["issues", "--bogus"])).await.status.code(), Some(2));
    assert_eq!(run(a(&["compare", "--fail-if-worse", "lots"])).await.status.code(), Some(2));
}

#[test]
fn tail_on_a_server_without_search_requests_says_so() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let old = rt.block_on(old_server(&["list_issues"]));
    let home = std::env::temp_dir().join(format!("rw-client-tail-{}", std::process::id()));
    let o = client(&home, &["tail", "--url", &old]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("needs the search_requests tool"), "{}", err(&o));
}
