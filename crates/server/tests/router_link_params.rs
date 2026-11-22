//! The environment pages' links that come "from the router"
//! (`_environment_path`, `_logs_path`, `_people_path`) are the server's own
//! same-origin paths: the query string cannot set them, so a link sent to a
//! member cannot point "Logs", "Users" or "View distributed trace" at
//! another site. Over the real `web::router` and real HTTP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState, cookies};
use serde_json::Value;

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Config::load reads process-global env.
static ENV: Mutex<()> = Mutex::new(());

struct Fixture {
    base: String,
    app: i64,
    env: i64,
    cookie: String,
    version: String,
    http: reqwest::Client,
    dir: std::path::PathBuf,
}

impl Fixture {
    async fn get(&self, path: &str, query: &[(&str, &str)], inertia: bool) -> (u16, String) {
        let mut r = self.http.get(format!("{}/apps/{}/envs/{}{path}", self.base, self.app, self.env)).query(query).header("Cookie", &self.cookie);
        if inertia {
            r = r.header("X-Inertia", "true").header("X-Inertia-Version", self.version.as_str());
        }
        let r = r.send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    async fn props(&self, path: &str, query: &[(&str, &str)]) -> Value {
        let (status, body) = self.get(path, query, true).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<Value>(&body).unwrap()["props"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One member, one environment, two executions sharing trace `t-1`.
async fn start(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("rw-link-params-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("telemetry")).unwrap();
    let cfg = {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
            std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
            std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
            std::env::remove_var("RAILWATCH_BASE_PATH");
            std::env::remove_var("RAILWATCH_CONFIG");
        }
        Config::load().expect("config")
    };
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let password = control::base58(16);
    let member = control.tx(|tx| control::insert_user(tx, "Mo", "mo@example.com", &password, true)).unwrap();
    let account = control.tx(|tx| control::create_account(tx, "Acme", member)).unwrap();
    let app = control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let (env, _) = control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let slug: String = control.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0))).unwrap();
    {
        let t = railwatch_server::tenants::open_tenant(&cfg.tenants_dir().join(format!("{slug}.sqlite3"))).unwrap();
        for id in ["e-1", "e-2"] {
            t.execute(
                "INSERT INTO executions (execution_id, kind, name, status, occurred_at, duration, trace_id) VALUES (?1, 'request', 'GET /orders/:id', 200, '2026-10-25 11:59:59', 812000, 't-1')",
                [id],
            )
            .unwrap();
        }
    }

    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    let router = web::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let sid = control.with(|x| control::create_session(x, member, Some("ua"), Some("127.0.0.1"))).unwrap();
    let keys = cookies::Keys::new(SECRET.as_bytes());
    let value = control.with(|x| control::session_cookie(x, sid)).unwrap().expect("the session exists");
    let cookie = format!("{}={}", cookies::SESSION_TOKEN, keys.sign(cookies::SESSION_TOKEN, &value));
    Fixture {
        base: format!("http://{addr}"),
        app,
        env,
        cookie,
        version: web::assets::manifest().version.clone(),
        http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
        dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_link_params_in_the_query_string_are_ignored() {
    let f = start("forged").await;
    let env_path = format!("/apps/{}/envs/{}", f.app, f.env);
    let forged = [("_logs_path", "https://evil.example/login"), ("_people_path", "javascript:alert(1)"), ("_environment_path", "https://evil.example/rw")];

    let props = f.props("/tenants/acme", &forged).await;
    assert!(!props.to_string().contains("evil.example") && !props.to_string().contains("javascript:"), "{}", props["links"]);
    assert_eq!(props["links"]["logs"], format!("{env_path}/logs?window=24h&q=tenant%3Aacme"), "{}", props["links"]);
    assert_eq!(props["links"]["people"], format!("{env_path}/users?window=24h"), "{}", props["links"]);

    // The first-visit HTML document carries the same page JSON.
    let (status, html) = f.get("/tenants/acme", &forged, false).await;
    assert_eq!(status, 200);
    // (Its `url` echoes the query string, so look at the links themselves.)
    let at = html.find("\"links\":").expect("the page JSON has links");
    let links = &html[at..html[at..].find('}').map_or(html.len(), |e| at + e)];
    assert!(!links.contains("evil.example") && !links.contains("javascript:"), "the forged links reached the HTML document: {links}");
    assert!(links.contains("/logs?window=24h"), "{links}");

    let props = f.props("/requests/e-1", &forged).await;
    assert_eq!(props["trace_url"], format!("{env_path}/traces/t-1"), "{}", props["trace_url"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_and_trace_links_are_this_environments_pages() {
    let f = start("links").await;
    let env_path = format!("/apps/{}/envs/{}", f.app, f.env);

    // The tenant's logs (filtered to it, percent-encoded) and users pages,
    // in the current window.
    let links = f.props("/tenants/acme%20co", &[("window", "7d")]).await["links"].clone();
    assert_eq!(links["logs"], format!("{env_path}/logs?window=7d&q=tenant%3A%22acme+co%22"), "{links}");
    assert_eq!(links["people"], format!("{env_path}/users?window=7d"), "{links}");

    // A custom range carries from/to instead of a preset.
    let range = [("from", "2026-10-25T00:00:00Z"), ("to", "2026-10-26T00:00:00Z")];
    let links = f.props("/tenants/acme", &range).await["links"].clone();
    let logs = links["logs"].as_str().unwrap();
    assert!(
        logs.starts_with(&format!("{env_path}/logs?from=2026-10-25T00%3A00%3A00"))
            && logs.contains("&to=2026-10-26T00%3A00%3A00")
            && logs.ends_with("&q=tenant%3Aacme"),
        "{links}"
    );

    let trace_url = f.props("/requests/e-1", &[]).await["trace_url"].clone();
    assert!(trace_url.as_str().is_some_and(|u| u.starts_with(&format!("{env_path}/traces/"))), "{trace_url}");
    assert_eq!(trace_url, format!("{env_path}/traces/t-1"));
}
