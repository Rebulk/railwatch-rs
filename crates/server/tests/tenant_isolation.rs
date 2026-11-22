//! Cross-tenant isolation, as a permanent suite: on one server, nothing
//! account A's principals send reaches account B's data, and nothing they
//! write changes it. The GA tenancy review of 2026-11-19 checked this by hand
//! (railwatch-reviews/ga-2026-11-19/tenancy-FINDINGS.md); this file is its
//! throwaway PoCs made permanent, over the real routers and real HTTP.
//!
//! The fixture, built once per test binary: account A (owner Alice, admin
//! Ann, member Mallory) and account B (owner Bob), with Dan a member of
//! both. Each account has two applications with two environments each, all
//! with real telemetry ingested over /ingest (every wire type, the
//! exceptions grouped into issues by the real worker), a deploy, a source
//! map, a webhook integration with an alert rule and an alert, a comment, an
//! uptime monitor, a (disabled) status page, a private saved view and a
//! pending invitation. B's telemetry and rows carry marker strings
//! (`Tag::markers`, plus B's slugs, environment token prefixes and Bob's API
//! token); no response to A may contain one. Every probe is aimed at one of
//! B's real objects (`Selectors`), and
//! `account_bs_own_principals_see_bs_data_through_the_same_probes` checks
//! that the same probes, sent by B, do show B's markers.
//!
//! Principals of A: three browser sessions (owner, admin, member), eight API
//! tokens (all-access, read-only, account-restricted, app-restricted,
//! security, a member's, and Dan's account- and app-restricted ones) and an
//! environment's ingest key.
//!
//! What keeps this from rotting: the routes are enumerated from the source,
//! not typed in. Dashboard pages come from `env_pages::PAGES`, MCP tools and
//! prompts from `tools/list` and `prompts/list` at runtime (a new argument
//! fails loudly in `aim`), and where a list is hand-kept (the dashboard's
//! other routes, the API's actions, MCP's methods and resources, ingest's
//! paths) a guard test parses the source and fails on anything the list does
//! not name.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use railwatch_server::alerts::integration::{self, SettingsCipher};
use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::dashboard::pool::open_tenant_reader;
use railwatch_server::followups;
use railwatch_server::http::{self as ingest, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use railwatch_server::uptime::{self, MonitorInput};
use railwatch_server::web::{self, WebState, env_pages::PAGES};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

// ---------------------------------------------------------------------------
// The server: pages, MCP, API and cable (web::router) with ingest as the
// fallback, as `railwatch serve` mounts them, plus the follow-ups and the
// worker so exceptions become issues.

struct Server {
    addr: SocketAddr,
    control: Arc<Control>,
    tenants_dir: PathBuf,
    sourcemaps_dir: PathBuf,
    _rt: tokio::runtime::Runtime,
}

/// The fixture's data dir, removed when the test process exits (the
/// fixture is a static, so it is never dropped).
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn remove_data_dir() {
    if let Some(d) = DATA_DIR.get() {
        let _ = std::fs::remove_dir_all(d);
    }
}

fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("rw-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    DATA_DIR.set(dir.clone()).unwrap();
    // SAFETY: registers a plain extern "C" fn that only reads a OnceLock.
    unsafe { libc::atexit(remove_data_dir) };
    // SAFETY: called once, from the OnceLock initialiser (every other test
    // thread is blocked on it), before this file starts any thread.
    let cfg = unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::set_var("RAILWATCH_SIGNUP", "open");
        std::env::set_var("RAILWATCH_TRUST_PROXY", "0");
        std::env::set_var("RAILWATCH_ALERT_DELIVERY", "off");
        for k in ["RAILWATCH_TRUSTED_PROXIES", "RAILWATCH_BASE_PATH", "RAILWATCH_TRUSTED_PROXY_SECRET_FILE", "RAILWATCH_CONFIG", "RAILWATCH_SMTP_ADDRESS"] {
            std::env::remove_var(k);
        }
        Config::load().expect("config")
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let sourcemaps_dir = cfg.data_dir.join("sourcemaps");
    let addr = rt.block_on(async {
        let (tx, rx) = followups::channel();
        let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
        let jobs = railwatch_server::jobs::start(control.clone(), cfg.tenants_dir());
        let hub = web::cable::Hub::new();
        tokio::spawn(followups::run_with(control.clone(), rx, Some(hub.clone()), Some(jobs.background.clone())));
        let ingest_state = Arc::new(AppState {
            control: control.clone(),
            tenants: tenants.clone(),
            ip_limit: FixedWindow::new(0),
            env_limit: FixedWindow::new(0),
            trust_proxy: false,
            trusted_proxies: Vec::new(),
            sourcemaps_dir: sourcemaps_dir.clone(),
            cpu: Arc::new(tokio::sync::Semaphore::new(4)),
            disk: tenants.disk(),
        });
        let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, hub).expect("web state"));
        let app = web::mounted("", web::router(state), ingest::router(ingest_state)).into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });
    Server { addr, control, tenants_dir: cfg.tenants_dir(), sourcemaps_dir, _rt: rt }
}

// ---------------------------------------------------------------------------
// HTTP over raw sockets (copied from ga_web_2026_11_19.rs, plus chunked
// bodies, so a marker split across chunks is still seen).

#[derive(Debug)]
struct Res {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Res {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

fn raw(addr: SocketAddr, method: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Res {
    let mut c = TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    c.write_all(head.as_bytes()).unwrap();
    let _ = c.write_all(body);
    let mut buf = Vec::new();
    let _ = c.read_to_end(&mut buf);
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or_else(|| panic!("no response to {method} {path}"));
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> =
        head.split("\r\n").skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.to_string(), v.trim().to_string())).collect();
    let mut body = buf[split + 4..].to_vec();
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked")) {
        body = dechunk(&body);
    }
    Res { status, headers, body: String::from_utf8_lossy(&body).to_string() }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
        let size = String::from_utf8_lossy(&b[..i]);
        let n = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        if n == 0 || b.len() < i + 2 + n {
            break;
        }
        out.extend_from_slice(&b[i + 2..i + 2 + n]);
        b = &b[(i + 4 + n).min(b.len())..];
    }
    out
}

/// A cookie jar: one browser.
#[derive(Clone, Default)]
struct Browser {
    jar: HashMap<String, String>,
}

impl Browser {
    fn request(&mut self, s: &Server, method: &str, path: &str, form: Option<&str>, extra: &[(&str, &str)]) -> Res {
        let mut h: Vec<(String, String)> = Vec::new();
        if !self.jar.is_empty() {
            let cookies: Vec<String> = self.jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
            h.push(("Cookie".into(), cookies.join("; ")));
        }
        if form.is_some() {
            h.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
            h.push(("Origin".into(), format!("http://{}", s.addr)));
        }
        h.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let r = raw(s.addr, method, path, &h, form.unwrap_or("").as_bytes());
        for (k, v) in &r.headers {
            if k.eq_ignore_ascii_case("set-cookie") {
                let pair = v.split(';').next().unwrap();
                let (name, value) = pair.split_once('=').unwrap();
                if value.is_empty() || v.contains("Max-Age=0") {
                    self.jar.remove(name);
                } else {
                    self.jar.insert(name.to_string(), value.to_string());
                }
            }
        }
        r
    }

    fn get(&mut self, s: &Server, path: &str) -> Res {
        self.request(s, "GET", path, None, &[])
    }

    /// A form request with the page's CSRF token, as Inertia sends it.
    fn send(&mut self, s: &Server, method: &str, path: &str, form: &str) -> Res {
        let token = self.csrf(s);
        self.request(s, method, path, Some(form), &[("X-CSRF-Token", &token)])
    }

    fn csrf(&mut self, s: &Server) -> String {
        let mut r = self.get(s, "/sign_in");
        if r.status == 302 {
            r = self.get(s, "/settings/profile");
        }
        let i = r.body.find("name=\"csrf-token\" content=\"").map(|i| i + 27).expect("csrf meta");
        r.body[i..i + r.body[i..].find('"').unwrap()].to_string()
    }

    fn sign_in(&mut self, s: &Server, email: &str, password: &str) {
        let r = self.send(s, "POST", "/sign_in", &format!("email={}&password={}", enc(email), enc(password)));
        assert_eq!((r.status, r.header("location")), (302, Some("/dashboard")), "{email} signs in");
    }
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn api(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> Res {
    let mut h = vec![("Authorization".to_string(), format!("Bearer {token}")), ("Accept".to_string(), "application/json".to_string())];
    if body.is_some() {
        h.push(("Content-Type".into(), "application/json".into()));
    }
    raw(s.addr, method, path, &h, body.map(Value::to_string).unwrap_or_default().as_bytes())
}

fn mcp(s: &Server, token: &str, method: &str, params: Value) -> Res {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let h = [("Authorization".to_string(), format!("Bearer {token}")), ("Content-Type".to_string(), "application/json".to_string())];
    raw(s.addr, "POST", "/mcp", &h, body.as_bytes())
}

fn gzip(body: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(body).unwrap();
    e.finish().unwrap()
}

/// POST to the ingest API with an environment's key.
fn ingest_post(s: &Server, key: &str, path: &str, content_type: &str, body: &[u8], extra: &[(&str, &str)]) -> Res {
    let mut h = vec![
        ("Authorization".to_string(), format!("Bearer {key}")),
        ("Content-Type".to_string(), content_type.to_string()),
        ("X-Railwatch-Version".to_string(), "0.9.0".to_string()),
    ];
    h.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    raw(s.addr, "POST", path, &h, body)
}

fn ingest_records(s: &Server, key: &str, path: &str, ndjson: &[u8], extra: &[(&str, &str)]) -> Res {
    let mut extra = extra.to_vec();
    extra.push(("Content-Encoding", "gzip"));
    ingest_post(s, key, path, "application/x-ndjson", &gzip(ndjson), &extra)
}

// ---------------------------------------------------------------------------
// The fixture

/// One account's names. Everything B ingests or stores is made from its tag,
/// so B's markers appear nowhere else.
struct Tag {
    upper: &'static str,
    camel: &'static str,
    lower: &'static str,
    ip: &'static str,
    account: &'static str,
    owner: (&'static str, &'static str),
    /// (name, slug, issue prefix)
    apps: [(&'static str, &'static str, &'static str); 2],
}

const A_TAG: Tag = Tag {
    upper: "ACQ",
    camel: "Acq",
    lower: "acq",
    ip: "198.51.100.10",
    account: "Acq Industries QA",
    owner: ("Alice", "alice@acq.test"),
    apps: [("Acq Storefront", "acq-storefront", "ACQS"), ("Acq Backoffice", "acq-backoffice", "ACQK")],
};

const B_TAG: Tag = Tag {
    upper: "ZQB",
    camel: "Zqb",
    lower: "zqb",
    ip: "203.0.113.77",
    account: "Bravo Holdings ZQ",
    owner: ("Bob", "bob@zqb.test"),
    apps: [("ZQ Storefront", "zq-storefront", "ZQS"), ("ZQ Backoffice", "zq-backoffice", "ZQK")],
};

impl Tag {
    /// Strings only this account's data contains: its telemetry (log and
    /// exception messages, controller, job and route names, the customer
    /// host, the client IP, an LLM workflow, a person) and its control rows
    /// (account and app names, owner, webhook and monitor hosts, saved view,
    /// comment, deploy, invitation, source map).
    fn markers(&self) -> Vec<String> {
        let (u, c, l) = (self.upper, self.camel, self.lower);
        vec![
            format!("{c}Widget"),
            format!("{l}widgets"),
            format!("{l}-tenant"),
            format!("{l}-model"),
            format!("{l}-session-1"),
            format!("{l}-span"),
            format!("{l}-user-1"),
            format!("{u}-LOG-MARKER"),
            format!("{u}-EXC-MARKER"),
            format!("{l}-customer.example"),
            self.ip.to_string(),
            format!("{u}-WORKFLOW-MARKER"),
            format!("{c}ada"),
            self.account.to_string(),
            self.apps[0].0.to_string(),
            self.apps[1].0.to_string(),
            self.owner.1.to_string(),
            format!("hooks.{l}.example"),
            format!("status.{l}.example"),
            format!("{u} private view"),
            format!("{u}-COMMENT-MARKER"),
            format!("{l}deploy"),
            format!("carol@{l}.test"),
            format!("{l}-sourcemap"),
        ]
    }
}

struct Env {
    id: i64,
    name: String,
    slug: String,
    /// The ingest key.
    token: String,
    /// The request fixture's group hash, per account (B's exists only in B).
    group: String,
    execution: String,
    trace: String,
    /// What each detail page and tool selects by, read back from the
    /// tenant (`selectors`): {id} on /queries, /spans, /profiles, /users,
    /// /tenants, /sessions, /releases, /deploys, and get_job/explain_query's
    /// group hashes.
    sel: Selectors,
}

#[derive(Default)]
struct Selectors {
    query_group: String,
    span_group: String,
    job_group: String,
    profile_id: i64,
    person: String,
    tenant: String,
    session: String,
    release: String,
    deploy_id: i64,
}

/// The ids B's own rows have, from its telemetry and control rows.
fn selectors(s: &Server, slug: &str, env: i64) -> Selectors {
    let t = open_tenant_reader(&s.tenants_dir, slug).unwrap();
    let text = |sql: &str| t.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap_or_else(|e| panic!("{slug}: {sql}: {e}"));
    Selectors {
        query_group: text("SELECT group_hash FROM queries LIMIT 1"),
        span_group: text("SELECT group_hash FROM spans LIMIT 1"),
        job_group: text("SELECT group_hash FROM executions WHERE kind = 'job_attempt' LIMIT 1"),
        profile_id: t.query_row("SELECT id FROM profiles LIMIT 1", [], |r| r.get(0)).unwrap(),
        person: text("SELECT ref FROM people LIMIT 1"),
        tenant: text("SELECT app_tenant FROM executions WHERE app_tenant IS NOT NULL LIMIT 1"),
        session: text("SELECT session_id FROM sessions LIMIT 1"),
        release: text("SELECT deploy FROM executions LIMIT 1"),
        deploy_id: one(s, "SELECT id FROM deploys WHERE environment_id = ? ORDER BY id LIMIT 1", [env]),
    }
}

struct App {
    id: i64,
    slug: String,
    name: String,
    envs: Vec<Env>,
}

struct Account {
    id: i64,
    slug: String,
    name: String,
    owner: i64,
    apps: Vec<App>,
    issue: i64,
    issue_key: String,
    integration: i64,
    rule: i64,
    alert: i64,
    monitor: i64,
    monitor_name: String,
    /// A security incident on the account's first environment, its actor
    /// the account's marker address (B_TAG.ip, a leak marker).
    incident: i64,
    view: i64,
    invitation: i64,
    status_slug: String,
    owner_membership: i64,
}

impl Account {
    fn env(&self) -> &Env {
        &self.apps[0].envs[0]
    }

    fn envs(&self) -> impl Iterator<Item = &Env> {
        self.apps.iter().flat_map(|a| a.envs.iter())
    }
}

struct Fx {
    s: Server,
    a: Account,
    b: Account,
    password: String,
    bob_token: String,
    bob_token_id: i64,
    bob_session: i64,
    dan_b_membership: i64,
    /// A's browser sessions: owner, admin, member.
    sessions: Vec<(&'static str, Browser)>,
    bob_browser: Browser,
    /// A's API tokens (name, raw token).
    tokens: Vec<(&'static str, String)>,
    b_markers: Vec<String>,
}

/// The fixture, built once; if building it fails, every test fails with
/// that one message instead of building it again.
fn fx() -> &'static Fx {
    static FX: OnceLock<Result<Fx, String>> = OnceLock::new();
    let built = FX.get_or_init(|| {
        std::panic::catch_unwind(build)
            .map_err(|e| e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())
    });
    match built {
        Ok(fx) => fx,
        Err(e) => panic!("the fixture failed to build: {e}"),
    }
}

const FIXTURE_EXECUTION: &str = "5c3b1e2a-9d4f-4a2b-8e1c-2f6a7b9c0d3e";
const FIXTURE_TRACE: &str = "8f14e45f-ceea-4b8f-8ac2-9a6e0c9f1a11";

/// Every wire fixture type, renamed for the account: its own group hashes,
/// execution and trace ids, and marker strings in messages, names, hosts
/// and the client IP. Returns (ndjson, the request's group hash).
fn telemetry(t: &Tag, execution: &str, trace: &str) -> (Vec<u8>, String) {
    let fixtures: Map<String, Value> = serde_json::from_str(include_str!("fixtures/wire_fixtures.json")).unwrap();
    let mut text = serde_json::to_string(&fixtures).unwrap();
    for g in fixtures.values().filter_map(|r| r["_group"].as_str()) {
        let h: String = Sha256::digest(format!("{}{g}", t.lower)).iter().take(16).map(|b| format!("{b:02x}")).collect();
        text = text.replace(g, &h);
    }
    let renames = [
        ("Widget", format!("{}Widget", t.camel)),
        ("widgets", format!("{}widgets", t.lower)),
        ("hello world", format!("{}-LOG-MARKER", t.upper)),
        ("kaboom", format!("{}-EXC-MARKER", t.upper)),
        ("example.com", format!("{}-customer.example", t.lower)),
        ("127.0.0.1", t.ip.to_string()),
        ("Write article", format!("{}-WORKFLOW-MARKER", t.upper)),
        ("Ada", format!("{}ada", t.camel)),
        ("claude-opus-5", format!("{}-model", t.lower)),
        ("3f9c2a1", format!("{}deploy-r1", t.lower)),
        ("pdf.render", format!("{}-span.render", t.lower)),
        (r#""user":"1""#, format!(r#""user":"{}-user-1""#, t.lower)),
        // serde_json writes "tenant":null; give every record the account's tenant.
        (r#""tenant":null"#, format!(r#""tenant":"{}-tenant""#, t.lower)),
        (FIXTURE_EXECUTION, execution.to_string()),
        (FIXTURE_TRACE, trace.to_string()),
    ];
    for (from, to) in renames {
        text = text.replace(from, &to);
    }
    let mut fixtures: Map<String, Value> = serde_json::from_str(&text).unwrap();
    // The execution fixtures share one execution_id (one row is kept); the
    // job attempt gets its own so the account has a job to find.
    fixtures["job_attempt"]["execution_id"] = json!(uuid::Uuid::new_v4().to_string());
    // One browser session, linked from the request and the visit, so the
    // session pages and tools have a journey to show.
    let session = format!("{}-session-1", t.lower);
    fixtures["session"]["id"] = json!(session);
    fixtures["session"]["started_at"] = json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - 120);
    fixtures["request"]["session_id"] = json!(session);
    fixtures["visit"]["session_id"] = json!(session);
    fixtures["user"]["id"] = json!(format!("{}-user-1", t.lower));
    let group = fixtures["request"]["_group"].as_str().unwrap().to_string();
    // A second release of the same route, so compare has two sides.
    let mut later = fixtures["request"].clone();
    later["execution_id"] = json!(uuid::Uuid::new_v4().to_string());
    later["deploy"] = json!(format!("{}deploy-r2", t.lower));
    fixtures.insert("request_r2".into(), later);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() - 60.0;
    let mut out = Vec::new();
    for (_, mut r) in fixtures {
        r["timestamp"] = json!(now);
        out.extend_from_slice(&serde_json::to_vec(&r).unwrap());
        out.push(b'\n');
    }
    (out, group)
}

fn one<T: rusqlite::types::FromSql>(s: &Server, sql: &str, p: impl rusqlite::Params) -> T {
    s.control.with(|x| x.query_row(sql, p, |r| r.get(0))).unwrap()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < until, "fixture: timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn user(s: &Server, name: &str, email: &str, password: &str) -> i64 {
    s.control.tx(|tx| control::insert_user(tx, name, email, password, true)).unwrap()
}

/// An account with two applications of two environments each, real
/// telemetry in all four, and one of every control row a tenant owns.
fn account(s: &Server, t: &Tag, owner: i64) -> Account {
    let id = s.control.tx(|tx| control::create_account(tx, t.account, owner)).unwrap();
    let slug: String = one(s, "SELECT slug FROM accounts WHERE id = ?", [id]);
    let cipher = SettingsCipher::new(SECRET.as_bytes());
    let mut settings = Map::new();
    settings.insert("url".into(), json!(format!("https://hooks.{}.example/secret-{}", t.lower, t.lower)));
    let name = format!("{} hook", t.upper);
    let integration = s.control.with(|x| integration::insert(x, &cipher, id, "webhook", &name, &settings)).unwrap();
    let mut apps = Vec::new();
    for (name, app_slug, prefix) in t.apps {
        let app = s.control.tx(|tx| control::insert_application(tx, id, name, app_slug, prefix, None, "main")).unwrap();
        let mut envs = Vec::new();
        for env_name in ["production", "staging"] {
            let (env, token) = s.control.tx(|tx| control::create_environment(tx, app, env_name)).unwrap();
            let env_slug: String = one(s, "SELECT slug FROM environments WHERE id = ?", [env]);
            let (execution, trace) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
            let (records, group) = telemetry(t, &execution, &trace);
            let r = ingest_records(s, &token, "/ingest", &records, &[]);
            assert_eq!((r.status, r.json()["rejected"].clone()), (200, json!(0)), "fixture ingest into {env_slug}: {}", r.body);
            envs.push(Env { id: env, name: env_name.into(), slug: env_slug, token, group, execution, trace, sel: Selectors::default() });
        }
        apps.push(App { id: app, slug: app_slug.into(), name: name.into(), envs });
    }
    let now = control::now_db();
    let rule = s
        .control
        .with(|x| {
            x.execute(
                "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?1, ?2, 'new_issue', '{}', ?3, ?2)",
                rusqlite::params![apps[0].id, now, integration],
            )?;
            Ok(x.last_insert_rowid())
        })
        .unwrap();
    let env = apps[0].envs[0].id;
    // The worker groups each environment's exception into an issue.
    for e in apps.iter().flat_map(|a| a.envs.iter()) {
        wait_for(&format!("an issue in {}", e.slug), || one::<i64>(s, "SELECT count(*) FROM issues WHERE environment_id = ?", [e.id]) > 0);
    }
    let (issue, number): (i64, i64) = s
        .control
        .with(|x| x.query_row("SELECT id, number FROM issues WHERE environment_id = ? ORDER BY id LIMIT 1", [env], |r| Ok((r.get(0)?, r.get(1)?))))
        .unwrap();
    // An alert of the rule (as rules::fire records it; delivery is off).
    let alert = s
        .control
        .with(|x| {
            x.execute(
                "INSERT INTO alerts (alert_rule_id, created_at, event, integration_id, issue_id, payload, status, updated_at)
                 VALUES (?1, ?2, 'new_issue', ?3, ?4, ?5, 'skipped', ?2)",
                rusqlite::params![rule, now, integration, issue, json!({"title": format!("{}-EXC-MARKER", t.upper)}).to_string()],
            )?;
            Ok(x.last_insert_rowid())
        })
        .unwrap();

    let deploy = format!("{}deploy1", t.lower);
    let body = json!({"deploy": deploy, "url": format!("https://git.{}.example/c/1", t.lower)}).to_string();
    for e in apps.iter().flat_map(|a| a.envs.iter()) {
        let r = ingest_post(s, &e.token, "/ingest/deploys", "application/json", body.as_bytes(), &[]);
        assert!(r.status < 300, "deploy: {} {}", r.status, r.body);
    }
    for e in apps.iter_mut().flat_map(|a| a.envs.iter_mut()) {
        e.sel = selectors(s, &e.slug, e.id);
    }
    let map = format!(r#"{{"version":3,"sources":["{}-sourcemap.js"],"names":[],"mappings":"AAAA"}}"#, t.lower);
    let r = ingest_post(
        s,
        &apps[0].envs[0].token,
        "/ingest/sourcemaps",
        "application/json",
        map.as_bytes(),
        &[("X-Railwatch-Deploy", &deploy), ("X-Railwatch-Filename", "app.js.map")],
    );
    assert!(r.status < 300, "source map: {} {}", r.status, r.body);

    let monitor_name = format!("{} monitor", t.upper);
    let m = MonitorInput { name: monitor_name.clone(), url: format!("https://status.{}.example/health", t.lower), ..Default::default() };
    let monitor = s.control.with(|x| uptime::insert(x, env, &m)).unwrap().unwrap();
    let status_slug = format!("{}-status", t.lower);
    s.control.with(|x| uptime::save_status_page(x, env, &status_slug, &format!("{} status", t.upper), false)).unwrap().unwrap();
    let view = s
        .control
        .with(|x| {
            x.execute(
                "INSERT INTO saved_views (created_at, environment_id, name, page, params, pinned, query, shared, updated_at, user_id, \"window\")
                 VALUES (?1, ?2, ?3, 'requests', '{}', 1, ?4, 0, ?1, ?5, '24h')",
                rusqlite::params![now, env, format!("{} private view", t.upper), format!("{} secret query", t.lower), owner],
            )?;
            Ok(x.last_insert_rowid())
        })
        .unwrap();
    s.control
        .with(|x| {
            x.execute(
                "INSERT INTO comments (body, created_at, issue_id, updated_at, user_id) VALUES (?1, ?2, ?3, ?2, ?4)",
                rusqlite::params![format!("{}-COMMENT-MARKER", t.upper), now, issue, owner],
            )
        })
        .unwrap();
    let incident = s
        .control
        .with(|x| {
            x.execute(
                "INSERT INTO security_incidents (environment_id, actor, status, severity, identity, opened_at, last_signal_at, created_at, updated_at)
                 VALUES (?1, ?2, 'open', 'high', 0, ?3, ?3, ?3, ?3)",
                rusqlite::params![env, t.ip, railwatch_server::time::now_micros()],
            )?;
            let incident = x.last_insert_rowid();
            x.execute(
                "INSERT INTO security_incident_signals (incident_id, detector, subject, severity, first_at, last_at, count, summary, identity)
                 VALUES (?1, 'secrets_probe', '', 'high', ?2, ?2, 3, ?3, 0)",
                rusqlite::params![incident, railwatch_server::time::now_micros(), format!("3 probes of /{}widgets/.env", t.lower)],
            )?;
            Ok(incident)
        })
        .unwrap();
    let invitation = s.control.tx(|tx| control::create_invitation(tx, id, owner, &format!("carol@{}.test", t.lower), "member")).unwrap().id;
    let owner_membership: i64 = one(s, "SELECT id FROM memberships WHERE account_id = ? AND user_id = ?", [id, owner]);
    Account {
        id,
        slug,
        name: t.account.into(),
        owner,
        issue_key: format!("{}-{number}", t.apps[0].2),
        apps,
        issue,
        integration,
        rule,
        alert,
        monitor,
        monitor_name,
        incident,
        view,
        invitation,
        status_slug,
        owner_membership,
    }
}

fn build() -> Fx {
    let s = start();
    let password = control::base58(16);
    let alice = user(&s, A_TAG.owner.0, A_TAG.owner.1, &password);
    let ann = user(&s, "Ann", "ann@acq.test", &password);
    let mallory = user(&s, "Mallory", "mallory@acq.test", &password);
    let bob = user(&s, B_TAG.owner.0, B_TAG.owner.1, &password);
    let dan = user(&s, "Dan", "dan@both.test", &password);
    let a = account(&s, &A_TAG, alice);
    let b = account(&s, &B_TAG, bob);
    for (acct, uid, role) in [(a.id, ann, "admin"), (a.id, mallory, "member"), (a.id, dan, "member"), (b.id, dan, "member")] {
        s.control.tx(|tx| control::insert_membership(tx, acct, uid, role)).unwrap();
    }
    let dan_b_membership: i64 = one(&s, "SELECT id FROM memberships WHERE account_id = ? AND user_id = ?", [b.id, dan]);
    // Usage is flushed once a second; wait so B's rows are settled before
    // anything snapshots them.
    for e in a.envs().chain(b.envs()) {
        wait_for("usage", || one::<i64>(&s, "SELECT count(*) FROM ingest_usage_months WHERE environment_id = ?", [e.id]) > 0);
    }

    let scopes = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let rw = scopes(&["api", "mcp", "read", "write"]);
    let issue = |uid: i64, name: &str, sc: &[String], acct: Option<i64>, app: Option<i64>| {
        s.control.tx(|tx| control::issue_api_token(tx, uid, name, sc, acct, app, None)).unwrap()
    };
    let (a_app, a_id) = (a.apps[0].id, a.id);
    let tokens = vec![
        ("alice.all", issue(alice, "all", &rw, None, None).1),
        ("alice.read-only", issue(alice, "ro", &scopes(&["api", "mcp", "read"]), None, None).1),
        ("alice.account-A", issue(alice, "acct", &rw, Some(a_id), None).1),
        ("alice.app-A", issue(alice, "app", &rw, None, Some(a_app)).1),
        ("alice.security", issue(alice, "sec", &scopes(&["api", "mcp", "read", "security"]), None, None).1),
        ("mallory.all", issue(mallory, "all", &rw, None, None).1),
        ("dan.account-A", issue(dan, "acct", &rw, Some(a_id), None).1),
        ("dan.app-A", issue(dan, "app", &rw, None, Some(a_app)).1),
    ];
    let (bob_token_id, bob_token) = issue(bob, "all", &scopes(&["api", "mcp", "read", "write", "security"]), None, None);

    let mut sessions = Vec::new();
    for (name, email) in [("alice (owner)", "alice@acq.test"), ("ann (admin)", "ann@acq.test"), ("mallory (member)", "mallory@acq.test")] {
        let mut br = Browser::default();
        br.sign_in(&s, email, &password);
        sessions.push((name, br));
    }
    let mut bob_browser = Browser::default();
    bob_browser.sign_in(&s, "bob@zqb.test", &password);
    let bob_session: i64 = one(&s, "SELECT max(id) FROM sessions WHERE user_id = ?", [bob]);

    let mut b_markers = B_TAG.markers();
    b_markers.push(b.slug.clone());
    b_markers.extend(b.apps.iter().map(|a| a.slug.clone()));
    b_markers.extend(b.envs().map(|e| e.token[..12].to_string()));
    b_markers.push(bob_token.clone());
    Fx { s, a, b, password, bob_token, bob_token_id, bob_session, dan_b_membership, sessions, bob_browser, tokens, b_markers }
}

// ---------------------------------------------------------------------------
// Checks

/// What a request sent, as the strings a response may quote back: the
/// path, each of its segments and query values, and the body.
fn sent_parts(path: &str, body: &str) -> Vec<String> {
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    let mut out = vec![path.to_string(), body.to_string()];
    out.extend(p.split('/').map(str::to_string));
    out.extend(form_urlencoded::parse(q.as_bytes()).map(|(_, v)| v.into_owned()));
    out.extend(form_urlencoded::parse(body.as_bytes()).map(|(_, v)| v.into_owned()));
    out
}

/// The B markers in `body`, after removing what the request itself sent
/// (a page or error that quotes the caller's input back is not a leak:
/// tenancy review I1).
fn leaks(fx: &Fx, sent: &[String], body: &str) -> Vec<String> {
    let mut body = body.replace("\\/", "/");
    let mut sent: Vec<&String> = sent.iter().filter(|s| s.len() >= 3).collect();
    // Longest first: an environment slug contains the account and app slugs.
    sent.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in sent {
        body = body.replace(s.as_str(), "");
    }
    fx.b_markers.iter().filter(|m| body.contains(m.as_str())).cloned().collect()
}

/// Columns that change with use, not with a write by A.
const VOLATILE: [&str; 4] = ["last_seen_at", "events_this_month", "usage_month", "last_used_at"];

fn rows(x: &rusqlite::Connection, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut st = x.prepare(sql)?;
    let names: Vec<String> = st.column_names().map(|n| n.to_string()).collect();
    let keep: Vec<usize> = (0..names.len()).filter(|i| !VOLATILE.contains(&names[*i].as_str())).collect();
    st.query_map([], |r| {
        let mut cols = Vec::new();
        for &i in &keep {
            cols.push(format!("{}={:?}", names[i], r.get::<_, rusqlite::types::Value>(i)?));
        }
        Ok(cols.join(" "))
    })?
    .collect()
}

/// Everything of B's that A could change: every control row B owns, B's
/// telemetry (row counts, and that none of A's markers landed in it), and
/// B's source maps on disk.
fn snapshot(fx: &Fx) -> Vec<String> {
    let b = &fx.b;
    let ids = |v: Vec<i64>| v.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let apps = ids(b.apps.iter().map(|a| a.id).collect());
    let envs = ids(b.envs().map(|e| e.id).collect());
    let issues = format!("SELECT id FROM issues WHERE application_id IN ({apps})");
    let integrations = format!("SELECT id FROM integrations WHERE account_id = {}", b.id);
    let queries = [
        format!("SELECT * FROM accounts WHERE id = {}", b.id),
        format!("SELECT * FROM users WHERE id = {}", b.owner),
        format!("SELECT * FROM memberships WHERE account_id = {} ORDER BY id", b.id),
        format!("SELECT * FROM invitations WHERE account_id = {} ORDER BY id", b.id),
        format!("SELECT * FROM applications WHERE account_id = {} ORDER BY id", b.id),
        format!("SELECT * FROM environments WHERE application_id IN ({apps}) ORDER BY id"),
        // updated_at moves with last_used_at (record_use), which B's own use changes.
        format!(
            "SELECT id, user_id, name, scopes, account_id, application_id, expires_at, token_digest, revoked_at, role_ceiling FROM api_tokens
             WHERE user_id = {} OR account_id = {} OR application_id IN ({apps}) ORDER BY id",
            b.owner, b.id
        ),
        format!("SELECT id, user_id FROM sessions WHERE user_id = {} ORDER BY id", b.owner),
        format!("SELECT * FROM issues WHERE application_id IN ({apps}) ORDER BY id"),
        format!("SELECT * FROM issue_activities WHERE issue_id IN ({issues}) ORDER BY id"),
        format!("SELECT * FROM comments WHERE issue_id IN ({issues}) ORDER BY id"),
        format!("SELECT * FROM integrations WHERE account_id = {} ORDER BY id", b.id),
        format!("SELECT * FROM alert_rules WHERE application_id IN ({apps}) OR integration_id IN ({integrations}) ORDER BY id"),
        format!("SELECT * FROM alerts WHERE issue_id IN ({issues}) OR integration_id IN ({integrations}) ORDER BY id"),
        format!("SELECT * FROM deploys WHERE environment_id IN ({envs}) ORDER BY id"),
        format!("SELECT * FROM saved_views WHERE environment_id IN ({envs}) ORDER BY id"),
        format!("SELECT * FROM uptime_monitors WHERE environment_id IN ({envs}) ORDER BY id"),
        format!("SELECT * FROM security_incidents WHERE environment_id IN ({envs}) ORDER BY id"),
        format!("SELECT * FROM status_pages WHERE environment_id IN ({envs}) OR slug = '{}'", b.status_slug),
        format!("SELECT * FROM ingest_usage_months WHERE environment_id IN ({envs}) ORDER BY id"),
    ];
    let mut out: Vec<String> =
        fx.s.control
            .with(|x| {
                let mut out = Vec::new();
                for q in &queries {
                    out.push(format!("-- {q}"));
                    out.extend(rows(x, q)?);
                }
                Ok(out)
            })
            .unwrap();
    for e in b.envs() {
        let t = open_tenant_reader(&fx.s.tenants_dir, &e.slug).unwrap();
        for sql in [
            "SELECT count(*) FROM logs",
            "SELECT count(*) FROM exceptions",
            "SELECT count(*) FROM executions",
            "SELECT count(*) FROM people",
            "SELECT count(*) FROM logs WHERE message LIKE '%ACQ%'",
        ] {
            let n: i64 = t.query_row(sql, [], |r| r.get(0)).unwrap();
            out.push(format!("{} {sql} = {n}", e.slug));
        }
        let mut files = Vec::new();
        walk(&fx.s.sourcemaps_dir.join(&e.slug), &mut files);
        files.sort();
        let mut h = Sha256::new();
        for f in &files {
            h.update(f.to_string_lossy().as_bytes());
            h.update(std::fs::read(f).unwrap_or_default());
        }
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        out.push(format!("{} sourcemaps {} files {digest}", e.slug, files.len()));
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn changed(before: &[String], after: &[String]) -> Option<String> {
    if before == after {
        return None;
    }
    let (b, a): (BTreeSet<&String>, BTreeSet<&String>) = (before.iter().collect(), after.iter().collect());
    let gone: Vec<&&String> = b.difference(&a).take(5).collect();
    let new: Vec<&&String> = a.difference(&b).take(5).collect();
    Some(format!("B's state changed: gone {gone:?}, new {new:?}"))
}

/// Fails with every collected problem, not just the first.
fn report(what: &str, failures: Vec<String>) {
    assert!(failures.is_empty(), "{what}: {} problem(s):\n{}", failures.len(), failures.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}

// ---------------------------------------------------------------------------
// Dashboard

/// One request a browser sends: the router's route it exercises, the path
/// and form actually sent, and whether it must be refused (403/404) rather
/// than merely return nothing of B's.
struct Probe {
    method: &'static str,
    route: String,
    path: String,
    form: String,
    refused: bool,
}

fn probe(method: &'static str, route: &str, path: String, form: &str, refused: bool) -> Probe {
    Probe { method, route: route.to_string(), path, form: form.to_string(), refused }
}

/// The `{id}` an environment page under `sub` selects one of the
/// environment's own rows by: the selector its page function reads. A new
/// detail page fails here until it is given one.
fn detail_id(e: &Env, sub: &str) -> String {
    let s = &e.sel;
    match sub.strip_suffix("/{id}") {
        None => String::new(),
        Some("/requests" | "/jobs" | "/scheduled_tasks" | "/commands" | "/executions") => e.execution.clone(),
        Some("/queries") => s.query_group.clone(),
        Some("/spans") => s.span_group.clone(),
        Some("/profiles") => s.profile_id.to_string(),
        Some("/users") => s.person.clone(),
        Some("/deploys") => s.deploy_id.to_string(),
        Some("/releases") => s.release.clone(),
        Some("/sessions") => s.session.clone(),
        Some("/tenants") => s.tenant.clone(),
        Some(other) => panic!("env page {other}/{{id}}: tell detail_id() which of B's ids it selects by"),
    }
}

/// Every GET a browser of A can send that names B, or that could list B.
fn dashboard_reads(fx: &Fx) -> Vec<Probe> {
    let (a, b) = (&fx.a, &fx.b);
    let (aa, ae, ba, be) = (a.apps[0].id, a.env().id, b.apps[0].id, b.env().id);
    let mut v = Vec::new();
    for (sub, _) in PAGES {
        let route = if *sub == "/" { "/apps/{app}/envs/{env}".to_string() } else { format!("/apps/{{app}}/envs/{{env}}{sub}") };
        let id = detail_id(b.env(), sub);
        let fill = |app: i64, env: i64| {
            route
                .replace("{app}", &app.to_string())
                .replace("{env}", &env.to_string())
                .replace("{group_hash}", &b.env().group)
                .replace("{id}", &id)
                .replace("{trace_id}", &b.env().trace)
        };
        for (app, env) in [(ba, be), (aa, be), (ba, ae)] {
            v.push(probe("GET", &route, fill(app, env), "", true));
        }
        // A's own environment, asked for B's ids: B's rows live in B's
        // files, never in A's.
        v.push(probe("GET", &route, fill(aa, ae), "", false));
    }
    let refused = [
        ("/apps/{app}", format!("/apps/{ba}")),
        ("/apps/{app}/edit", format!("/apps/{ba}/edit")),
        ("/apps/{app}/envs/new", format!("/apps/{ba}/envs/new")),
        ("/apps/{app}/envs/{env}/edit", format!("/apps/{ba}/envs/{be}/edit")),
        ("/apps/{app}/envs/{env}/edit", format!("/apps/{aa}/envs/{be}/edit")),
        ("/apps/{app}/envs/{env}/uptime", format!("/apps/{ba}/envs/{be}/uptime")),
        ("/apps/{app}/envs/{env}/uptime", format!("/apps/{aa}/envs/{be}/uptime")),
        ("/issues/{id}", format!("/issues/{}", b.issue)),
        ("/issues/{id}/merge_candidates", format!("/issues/{}/merge_candidates", b.issue)),
        ("/status/{slug}", format!("/status/{}", b.status_slug)),
    ];
    for (route, path) in refused {
        v.push(probe("GET", route, path, "", true));
    }
    let listed = [
        ("/", "/".to_string()),
        ("/dashboard", "/dashboard".to_string()),
        ("/issues", "/issues".to_string()),
        ("/issues", format!("/issues?application_id={ba}&environment_id={be}&q=zq")),
        ("/issues/{id}/merge_candidates", format!("/issues/{}/merge_candidates?q=zq", a.issue)),
        ("/alerts", "/alerts".to_string()),
        ("/alerts", format!("/alerts?application_id={ba}&environment_id={be}")),
        ("/settings/members", "/settings/members".to_string()),
        ("/settings/integrations", "/settings/integrations".to_string()),
        ("/settings/profile", "/settings/profile".to_string()),
        ("/settings/sessions", "/settings/sessions".to_string()),
        ("/settings/password", "/settings/password".to_string()),
        ("/settings/email", "/settings/email".to_string()),
        ("/settings/appearance", "/settings/appearance".to_string()),
        ("/apps/new", "/apps/new".to_string()),
        ("/accounts/new", "/accounts/new".to_string()),
        ("/apps/{app}/envs/{env}/uptime", format!("/apps/{aa}/envs/{ae}/uptime")),
        ("/mcp", "/mcp".to_string()),
        ("/.well-known/mcp.json", "/.well-known/mcp.json".to_string()),
        ("/source", "/source".to_string()),
        ("/icon.png", "/icon.png".to_string()),
        ("/icon.svg", "/icon.svg".to_string()),
        ("/vite/{*path}", "/vite/app.js".to_string()),
    ];
    for (route, path) in listed {
        v.push(probe("GET", route, path, "", false));
    }
    v
}

/// Every write a browser of A can aim at B's ids (or at A's own, carrying
/// B's ids in the form). The tenancy review's attack_writes.py.
fn dashboard_writes(fx: &Fx) -> Vec<Probe> {
    let (a, b) = (&fx.a, &fx.b);
    let (aa, ae, ba, be) = (a.apps[0].id, a.env().id, b.apps[0].id, b.env().id);
    let (bi, ai) = (b.issue, a.issue);
    let w = |m: &'static str, route: &str, path: String, form: String| probe(m, route, path, &form, false);
    let mut v = vec![
        w("PATCH", "/apps/{app}", format!("/apps/{ba}"), "name=pwned".into()),
        w("DELETE", "/apps/{app}", format!("/apps/{ba}"), format!("confirmation={}&password_challenge={}", enc(&b.apps[0].name), fx.password)),
        w("POST", "/apps", "/apps".into(), format!("name=From+A&slug=from-a&issue_prefix=FRA&account_id={}", b.id)),
        w("POST", "/apps/{app}/envs", format!("/apps/{ba}/envs"), "name=evil".into()),
        w("POST", "/issues/bulk", "/issues/bulk".into(), format!("ids[]={bi}&action_name=resolve")),
        w("POST", "/issues/bulk", "/issues/bulk".into(), format!("ids[]={ai}&ids[]={bi}&action_name=merge")),
        w("PATCH", "/issues/{id}", format!("/issues/{bi}"), "action_name=resolve".into()),
        w("PATCH", "/issues/{id}", format!("/issues/{bi}"), "title=evil".into()),
        w("PATCH", "/issues/{id}", format!("/issues/{bi}"), "action_name=assign_me".into()),
        w("PATCH", "/issues/{id}", format!("/issues/{ai}"), format!("action_name=merge&target_id={bi}")),
        w("PATCH", "/issues/{id}", format!("/issues/{ai}"), format!("action_name=assign&assignee_id={}", b.owner)),
        w("POST", "/issues/{id}/comments", format!("/issues/{bi}/comments"), "body=evil".into()),
        w(
            "POST",
            "/settings/integrations",
            "/settings/integrations".into(),
            format!("kind=webhook&name=From+A&settings[url]=https%3A%2F%2Fa.example%2Fh&account_id={}", b.id),
        ),
        w("PATCH", "/settings/integrations/{id}", format!("/settings/integrations/{}", b.integration), "name=evil&enabled=0".into()),
        w("DELETE", "/settings/integrations/{id}", format!("/settings/integrations/{}", b.integration), String::new()),
        w("POST", "/settings/integrations/{id}/send_test", format!("/settings/integrations/{}/send_test", b.integration), String::new()),
        w("POST", "/settings/alert_rules", "/settings/alert_rules".into(), format!("application_id={ba}&integration_id={}&event=new_issue", a.integration)),
        w("POST", "/settings/alert_rules", "/settings/alert_rules".into(), format!("application_id={aa}&integration_id={}&event=new_issue", b.integration)),
        w("DELETE", "/settings/alert_rules/{id}", format!("/settings/alert_rules/{}", b.rule), String::new()),
        w("POST", "/alerts/{id}/retry", format!("/alerts/{}/retry", b.alert), String::new()),
        w("POST", "/settings/members", "/settings/members".into(), format!("email=probe%40acq.test&role=member&account_id={}", b.id)),
        w("PATCH", "/settings/members/{id}", format!("/settings/members/{}", b.owner_membership), "role=member".into()),
        w("PATCH", "/settings/members/{id}", format!("/settings/members/{}", fx.dan_b_membership), "role=admin".into()),
        w("DELETE", "/settings/members/{id}", format!("/settings/members/{}", fx.dan_b_membership), String::new()),
        w("DELETE", "/settings/members/{id}", format!("/settings/members/{}", b.owner_membership), String::new()),
        w("POST", "/settings/members/{id}/transfer_ownership", format!("/settings/members/{}/transfer_ownership", b.owner_membership), String::new()),
        w("DELETE", "/settings/invitations/{id}", format!("/settings/invitations/{}", b.invitation), String::new()),
        w("POST", "/settings/invitations/{id}/resend", format!("/settings/invitations/{}/resend", b.invitation), String::new()),
        w("POST", "/settings/profile/generate_mcp_token", "/settings/profile/generate_mcp_token".into(), format!("restriction=account%3A{}", b.id)),
        w("POST", "/settings/profile/generate_mcp_token", "/settings/profile/generate_mcp_token".into(), format!("restriction=application%3A{ba}")),
        w("DELETE", "/settings/profile/revoke_mcp_token", "/settings/profile/revoke_mcp_token".into(), format!("token_id={}", fx.bob_token_id)),
        w("DELETE", "/sessions/{id}", format!("/sessions/{}", fx.bob_session), String::new()),
        w("POST", "/accounts/{id}/switch", format!("/accounts/{}/switch", b.id), String::new()),
        w("DELETE", "/accounts/{id}", format!("/accounts/{}", b.id), format!("confirmation={}&password_challenge={}", enc(&b.name), fx.password)),
        w("PATCH", "/apps/{app}/envs/{env}/uptime/status_page", format!("/apps/{ba}/envs/{be}/uptime/status_page"), "title=evil&enabled=1".into()),
        // Taking B's status page slug for A's own page.
        w(
            "PATCH",
            "/apps/{app}/envs/{env}/uptime/status_page",
            format!("/apps/{aa}/envs/{ae}/uptime/status_page"),
            format!("slug={}&title=A&enabled=1", b.status_slug),
        ),
        w("POST", "/apps/{app}/envs/{env}/uptime/monitors", format!("/apps/{ba}/envs/{be}/uptime/monitors"), "name=x&url=https%3A%2F%2Fx.example".into()),
    ];
    // Environment-scoped writes: B's app and environment, then A's app
    // with B's environment.
    for (app, env) in [(ba, be), (aa, be)] {
        let e = format!("/apps/{app}/envs/{env}");
        v.push(w("PATCH", "/apps/{app}/envs/{env}", e.clone(), "name=evil".into()));
        v.push(w("DELETE", "/apps/{app}/envs/{env}", e.clone(), format!("confirmation={}&password_challenge={}", b.env().name, fx.password)));
        v.push(w("POST", "/apps/{app}/envs/{env}/rotate_token", format!("{e}/rotate_token"), String::new()));
        v.push(w("POST", "/apps/{app}/envs/{env}/pause", format!("{e}/pause"), String::new()));
        v.push(w("POST", "/apps/{app}/envs/{env}/saved_views", format!("{e}/saved_views"), "name=x&page=requests".into()));
    }
    // B's row ids under B's environment, then under A's own.
    for (app, env) in [(ba, be), (aa, ae)] {
        let e = format!("/apps/{app}/envs/{env}");
        let (view, mon) = (format!("{e}/saved_views/{}", b.view), format!("{e}/uptime/monitors/{}", b.monitor));
        v.push(w("PATCH", "/apps/{app}/envs/{env}/saved_views/{id}", view.clone(), "name=evil&shared=1".into()));
        v.push(w("DELETE", "/apps/{app}/envs/{env}/saved_views/{id}", view, String::new()));
        v.push(w("PATCH", "/apps/{app}/envs/{env}/uptime/monitors/{id}", mon.clone(), "name=evil&enabled=0".into()));
        v.push(w("DELETE", "/apps/{app}/envs/{env}/uptime/monitors/{id}", mon, String::new()));
    }
    v
}

/// Owner, admin and member of A request every page, settings screen and
/// Inertia prop set that names B's applications, environments, issues and
/// telemetry ids: each is refused, and nothing anywhere shows B's data.
#[test]
fn account_a_browsers_see_nothing_of_account_b_on_any_dashboard_page() {
    let fx = fx();
    let probes = dashboard_reads(fx);
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for (who, browser) in &fx.sessions {
            let (probes, failures) = (&probes, &failures);
            let mut br = browser.clone();
            scope.spawn(move || {
                let own = format!("/apps/{}/envs/{}/logs", fx.a.apps[0].id, fx.a.env().id);
                let r = br.get(&fx.s, &own);
                assert!(r.status == 200 && r.body.contains("ACQ-LOG-MARKER"), "{who} reads A's own logs (the session works): {}", r.status);
                for p in probes {
                    let r = br.get(&fx.s, &p.path);
                    let sent = sent_parts(&p.path, "");
                    let leaked = leaks(fx, &sent, &r.body);
                    let mut f = failures.lock().unwrap();
                    if p.refused && !matches!(r.status, 403 | 404) {
                        f.push(format!("{who}: GET {} -> {}, expected 403/404", p.path, r.status));
                    }
                    if !leaked.is_empty() {
                        f.push(format!("{who}: GET {} -> {} shows B's {leaked:?}", p.path, r.status));
                    }
                }
            });
        }
    });
    report("dashboard reads", failures.into_inner().unwrap());
}

/// Owner, admin and member of A send every dashboard write at B's ids
/// (and at A's own pages carrying B's ids): none succeeds against B, and
/// B's rows, telemetry and source maps are byte-for-byte unchanged after
/// each request.
#[test]
fn account_a_browsers_cannot_change_anything_of_account_b() {
    let fx = fx();
    let probes = dashboard_writes(fx);
    let mut failures = Vec::new();
    let mut before = snapshot(fx);
    // One principal at a time, so a change is pinned to its request.
    for (who, browser) in &fx.sessions {
        let mut br = browser.clone();
        for p in &probes {
            let r = br.send(&fx.s, p.method, &p.path, &p.form);
            if !matches!(r.status, 302 | 403 | 404 | 422) {
                failures.push(format!("{who}: {} {} [{}] -> {}", p.method, p.path, p.form, r.status));
            }
            let sent = sent_parts(&p.path, &p.form);
            let leaked = leaks(fx, &sent, &r.body);
            if !leaked.is_empty() {
                failures.push(format!("{who}: {} {} answered with B's {leaked:?}", p.method, p.path));
            }
            let after = snapshot(fx);
            if let Some(d) = changed(&before, &after) {
                failures.push(format!("{who}: {} {} [{}] -> {}: {d}", p.method, p.path, p.form, r.status));
                before = after;
            }
        }
    }
    report("dashboard writes", failures);
}

// ---------------------------------------------------------------------------
// /api/v1

/// Every action of api.rs `route()`, as (Action variant, method, path).
/// `every_api_action_mcp_method_and_ingest_path_is_covered` fails when the
/// `Action` enum names one this list does not.
const API_ROUTES: &[(&str, &str, &str)] = &[
    ("Me", "GET", "/api/v1/me"),
    ("Accounts", "GET", "/api/v1/accounts"),
    ("Applications", "GET", "/api/v1/applications"),
    ("Application", "GET", "/api/v1/applications/{app}"),
    ("Environment", "GET", "/api/v1/applications/{app}/environments/{env}"),
    ("Summary", "GET", "/api/v1/applications/{app}/environments/{env}/summary"),
    ("Requests", "GET", "/api/v1/applications/{app}/environments/{env}/requests"),
    ("RequestGroup", "GET", "/api/v1/applications/{app}/environments/{env}/requests/{group}"),
    ("Execution", "GET", "/api/v1/applications/{app}/environments/{env}/executions/{execution}"),
    ("Jobs", "GET", "/api/v1/applications/{app}/environments/{env}/jobs"),
    ("Queries", "GET", "/api/v1/applications/{app}/environments/{env}/queries"),
    ("Exceptions", "GET", "/api/v1/applications/{app}/environments/{env}/exceptions"),
    ("Logs", "GET", "/api/v1/applications/{app}/environments/{env}/logs"),
    ("Deploys", "GET", "/api/v1/applications/{app}/environments/{env}/deploys"),
    ("CreateDeploy", "POST", "/api/v1/applications/{app}/environments/{env}/deploys"),
    ("Releases", "GET", "/api/v1/applications/{app}/environments/{env}/releases"),
    ("Users", "GET", "/api/v1/applications/{app}/environments/{env}/users"),
    ("Tenants", "GET", "/api/v1/applications/{app}/environments/{env}/tenants"),
    ("Spans", "GET", "/api/v1/applications/{app}/environments/{env}/spans"),
    ("Series", "GET", "/api/v1/applications/{app}/environments/{env}/series?type=request"),
    ("Visits", "GET", "/api/v1/applications/{app}/environments/{env}/visits"),
    ("Uptime", "GET", "/api/v1/applications/{app}/environments/{env}/uptime"),
    ("SecurityIncidents", "GET", "/api/v1/applications/{app}/environments/{env}/security_incidents"),
    ("SecurityIncident", "GET", "/api/v1/applications/{app}/environments/{env}/security_incidents/{incident}"),
    ("Issues", "GET", "/api/v1/issues"),
    ("Issue", "GET", "/api/v1/issues/{issue}"),
    ("UpdateIssue", "PATCH", "/api/v1/issues/{issue}"),
    ("UpdateIssue", "PUT", "/api/v1/issues/{issue}"),
    ("CreateComment", "POST", "/api/v1/issues/{issue}/comments"),
];

struct ApiProbe {
    method: &'static str,
    path: String,
    body: Option<Value>,
    refused: bool,
}

fn api_probes(fx: &Fx) -> Vec<ApiProbe> {
    let (a, b) = (&fx.a, &fx.b);
    let (aa, ae, ba, be) = (a.apps[0].id, a.env().id, b.apps[0].id, b.env().id);
    let mut v = Vec::new();
    for (action, method, template) in API_ROUTES {
        let body = match *action {
            "CreateDeploy" => Some(json!({"deploy": "acq-api-probe"})),
            "UpdateIssue" => Some(json!({"status": "resolved", "priority": "urgent", "assignee_id": b.owner, "title": "evil"})),
            "CreateComment" => Some(json!({"body": "ACQ attacker comment"})),
            _ => None,
        };
        let fill = |app: i64, env: i64| {
            template
                .replace("{app}", &app.to_string())
                .replace("{env}", &env.to_string())
                .replace("{group}", &b.env().group)
                .replace("{execution}", &b.env().execution)
                .replace("{issue}", &b.issue.to_string())
                .replace("{incident}", &b.incident.to_string())
        };
        let p = |path: String, body: Option<Value>, refused: bool| ApiProbe { method, path, body, refused };
        if template.contains("{issue}") {
            v.push(p(fill(0, 0), body, true));
        } else if template.contains("{env}") {
            for (app, env) in [(ba, be), (aa, be), (ba, ae)] {
                v.push(p(fill(app, env), body.clone(), true));
            }
            // A's own environment, with B's group hash / execution id in the
            // path and B's ids in the query and body: never B's rows, and a
            // write lands in A's environment (the path wins).
            let own = fill(aa, ae);
            let sep = if own.contains('?') { '&' } else { '?' };
            let own = format!("{own}{sep}application_id={ba}&environment_id={be}");
            let body = body.map(|mut b2| {
                b2["application_id"] = json!(ba);
                b2["environment_id"] = json!(be);
                b2
            });
            v.push(p(own, body, false));
        } else if template.contains("{app}") {
            v.push(p(fill(ba, 0), body, true));
        } else {
            v.push(p(template.to_string(), body.clone(), false));
            v.push(p(format!("{template}?application_id={ba}&environment_id={be}&account_id={}&q=zq", b.id), body.clone(), false));
            v.push(p(format!("{template}?application_id[]={ba}&application_id[]={aa}"), body, false));
        }
    }
    v
}

/// Every API token of A (all-access, read-only, account- and app-restricted,
/// security, a member's, a two-account user's restricted ones) gets 403/404
/// for every /api/v1 action on B's ids, GET, PATCH, PUT and POST alike, and
/// never a byte of B's data; B is unchanged afterwards.
#[test]
fn account_a_api_tokens_get_nothing_of_account_b_over_the_rest_api() {
    let fx = fx();
    let probes = api_probes(fx);
    let before = snapshot(fx);
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for (who, token) in &fx.tokens {
            let (probes, failures) = (&probes, &failures);
            scope.spawn(move || {
                let r = api(&fx.s, token, "GET", "/api/v1/applications", None);
                assert!(r.status == 200 && r.body.contains(&fx.a.apps[0].name), "{who} lists A's applications (the token works): {} {}", r.status, r.body);
                for p in probes {
                    let r = api(&fx.s, token, p.method, &p.path, p.body.as_ref());
                    let mut sent = sent_parts(&p.path, "");
                    if let Some(b) = &p.body {
                        strings(b, &mut sent);
                    }
                    let leaked = leaks(fx, &sent, &r.body);
                    let mut f = failures.lock().unwrap();
                    if p.refused && !matches!(r.status, 403 | 404) {
                        f.push(format!("{who}: {} {} -> {}, expected 403/404: {}", p.method, p.path, r.status, &r.body[..r.body.len().min(200)]));
                    }
                    if !leaked.is_empty() {
                        f.push(format!("{who}: {} {} -> {} shows B's {leaked:?}", p.method, p.path, r.status));
                    }
                }
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.extend(changed(&before, &snapshot(fx)));
    report("API", failures);
}

// ---------------------------------------------------------------------------
// MCP

/// Arguments that name the environment; each shape fills them its own way.
const IDENTITY_ARGS: [&str; 4] = ["application_slug", "account_slug", "application_id", "environment"];
/// Selectors a tool takes exactly one of (session_journey).
const EXCLUSIVE: [&str; 3] = ["session_id", "execution_id", "user"];
/// Arguments that narrow or page a result: left out so the widest result
/// comes back (a filter could hide a leak). Required ones get a value from
/// the schema's enum.
const FILTER_ARGS: &[&str] = &[
    "window",
    "sort",
    "dir",
    "limit",
    "shape",
    "group",
    "max_tokens",
    "level",
    "has_errors",
    "timeline_offset",
    "timeline_limit",
    "timeline_type",
    "kind",
    "before",
    "after",
    "since",
    "metrics",
    "min_count",
    "tail",
    "frames",
    "incidents",
    "event",
    "status",
    "cursor",
    "group_by",
    "resource",
    "min_severity",
];

/// The argument sets a tool is called with: B's identity in every shape an
/// agent could pass it (B's slugs, B's ids, A's application with B's
/// environment, and A's own environment) with B's object ids (route, group
/// hash, execution, IP, ...) in every one. An argument this does not know
/// fails the test: a new tool argument must be aimed at B before it ships.
fn aim(fx: &Fx, tool: &Value) -> Vec<Map<String, Value>> {
    let (a, b) = (&fx.a, &fx.b);
    let name = tool["name"].as_str().unwrap();
    let schema = &tool["inputSchema"];
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    let required: Vec<&str> = schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let issue_shaped = (props.contains_key("key") || props.contains_key("issue_id")) && !props.contains_key("environment");
    let obj = |v: Value| v.as_object().unwrap().clone();
    let shapes: Vec<Map<String, Value>> = if issue_shaped {
        vec![
            obj(json!({"key": b.issue_key})),
            obj(json!({"key": format!("{}/{}/{}", b.slug, b.apps[0].id, b.issue_key)})),
            obj(json!({"key": format!("{}/{}/{}", a.slug, b.apps[0].id, b.issue_key)})),
            obj(json!({"issue_id": b.issue})),
            obj(json!({"issue_id": b.issue, "key": a.issue_key})),
        ]
    } else {
        vec![
            obj(json!({"application_slug": b.apps[0].slug, "account_slug": b.slug, "environment": b.env().name})),
            obj(json!({"application_id": b.apps[0].id, "environment": b.env().slug})),
            obj(json!({"application_slug": a.apps[0].slug, "environment": b.env().slug})),
            obj(json!({"application_slug": a.apps[0].slug, "account_slug": a.slug, "environment": a.env().name})),
        ]
    };
    let target = |p: &str| -> Option<Value> {
        Some(match p {
            "route" => json!(format!("GET /{}widgets(.:format)", B_TAG.lower)),
            "group_hash" => json!(match name {
                "explain_query" => &b.env().sel.query_group,
                "get_job" => &b.env().sel.job_group,
                _ => &b.env().group,
            }),
            "execution_id" => json!(b.env().execution),
            "session_id" => json!(b.env().sel.session),
            "user" => json!(b.env().sel.person),
            "ip" => json!(B_TAG.ip),
            "path" => json!(format!("/{}widgets", B_TAG.lower)),
            "job" => json!(format!("{}WidgetJob", B_TAG.camel)),
            "profile_id" => json!(b.env().sel.profile_id),
            "monitor" => json!(b.monitor_name),
            "incident_id" => json!(b.incident),
            "component" => json!(format!("{}Widgets/Index", B_TAG.camel)),
            "q" => json!("zqb"),
            _ => return None,
        })
    };
    let write = |p: &str| -> Option<Value> {
        Some(match p {
            "status" => json!("resolved"),
            "priority" => json!("urgent"),
            "assignee_email" => json!(B_TAG.owner.1),
            "agent" => json!("probe"),
            "body" => json!("ACQ attacker comment"),
            _ => return None,
        })
    };
    let mut out = Vec::new();
    for shape in shapes {
        let mut args = Map::new();
        let mut expand: Vec<(String, Vec<Value>)> = Vec::new();
        for (p, spec) in &props {
            if let Some(v) = shape.get(p) {
                args.insert(p.clone(), v.clone());
            } else if IDENTITY_ARGS.contains(&p.as_str()) || p == "key" || p == "issue_id" {
                // This shape leaves it out.
            } else if let Some(v) = issue_shaped.then(|| write(p)).flatten() {
                args.insert(p.clone(), v);
            } else if let Some(v) = target(p) {
                args.insert(p.clone(), v);
            } else if FILTER_ARGS.contains(&p.as_str()) {
                if required.contains(&p.as_str()) {
                    if let Some(e) = spec["enum"].as_array() {
                        expand.push((p.clone(), e.clone()));
                    } else if let Some(first) = spec["items"]["enum"].as_array().and_then(|e| e.first()) {
                        args.insert(p.clone(), json!([first]));
                    } else {
                        panic!("{name}: required argument {p:?} has no enum to pick from; teach aim() a value");
                    }
                }
            } else {
                panic!("MCP tool {name} takes {p:?}, which this suite does not know how to aim at account B: add it to aim()");
            }
        }
        let mut sets = vec![args];
        // Selectors a tool takes exactly one of: one call per selector.
        let given: Vec<&str> = EXCLUSIVE.iter().copied().filter(|k| sets[0].contains_key(*k)).collect();
        if given.len() > 1 {
            let all = sets.remove(0);
            for keep in &given {
                let mut one = all.clone();
                one.retain(|k, _| k == keep || !given.contains(&k.as_str()));
                sets.push(one);
            }
        }
        for (p, values) in expand {
            sets = sets
                .into_iter()
                .flat_map(|s| values.iter().map(move |v| (s.clone(), v.clone())))
                .map(|(mut s, v)| {
                    s.insert(p.clone(), v);
                    s
                })
                .collect();
        }
        out.extend(sets);
    }
    out
}

/// B's environment summary resource in every form, from the templates the
/// discovery document lists (`every_api_action_mcp_method_and_ingest_path_is_covered`
/// fails if a template is added).
fn b_resources(fx: &Fx) -> Vec<String> {
    let (a, b) = (&fx.a, &fx.b);
    let (bs, ba, be) = (&b.slug, &b.apps[0].slug, &b.env().name);
    vec![
        "railwatch://applications".to_string(),
        format!("railwatch://applications/{bs}/{ba}/environments/{be}/summary"),
        format!("railwatch://applications/{ba}/environments/{be}/summary"),
        format!("railwatch://applications/{}/{ba}/environments/{be}/summary", a.slug),
        format!("railwatch://applications/{}/{}/environments/{}/summary", a.slug, a.apps[0].slug, b.env().slug),
    ]
}

/// The arguments a prompt is fetched with: B's identity, then A's
/// application with B's environment. An argument this does not know fails.
fn prompt_args(fx: &Fx, prompt: &Value) -> Vec<Map<String, Value>> {
    let (a, b) = (&fx.a, &fx.b);
    let name = prompt["name"].as_str().unwrap();
    let mut out = Vec::new();
    for (app, env) in [(&b.apps[0].slug, &b.env().name), (&a.apps[0].slug, &b.env().slug)] {
        let mut args = Map::new();
        for arg in prompt["arguments"].as_array().into_iter().flatten() {
            let n = arg["name"].as_str().unwrap();
            let v = match n {
                "key" => json!(b.issue_key),
                "application_slug" => json!(app),
                "environment" => json!(env),
                "route" => json!(format!("GET /{}widgets(.:format)", B_TAG.lower)),
                "window" => continue,
                _ => panic!("MCP prompt {name} takes {n:?}, which this suite does not know how to aim at account B: add it to prompt_args()"),
            };
            args.insert(n.to_string(), v);
        }
        out.push(args);
    }
    out
}

fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Number(n) => out.push(n.to_string()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

/// Every MCP tool in `tools/list` (enumerated at runtime, so a new tool is
/// checked the day it ships), called by every A token with B's identity in
/// every shape and B's object ids; every resource B has, read in every URI
/// form; every prompt with B's arguments. Nothing of B comes back, and B's
/// issues and comments are unchanged.
#[test]
fn every_mcp_tool_resource_and_prompt_keeps_account_bs_data_from_account_a() {
    let fx = fx();
    let before = snapshot(fx);
    let failures = Mutex::new(Vec::new());
    let calls = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for (who, token) in &fx.tokens {
            let (failures, calls) = (&failures, &calls);
            scope.spawn(move || {
                let mut n = 0;
                let mut check = |what: String, sent: Vec<String>, r: Res| {
                    n += 1;
                    let leaked = leaks(fx, &sent, &r.body);
                    if r.status != 200 || !leaked.is_empty() {
                        failures.lock().unwrap().push(format!("{who}: {what} -> {} {leaked:?} {}", r.status, &r.body[..r.body.len().min(300)]));
                    }
                };
                let r = mcp(&fx.s, token, "tools/call", json!({"name": "list_applications", "arguments": {}}));
                assert!(r.body.contains(&fx.a.apps[0].name), "{who} lists A's applications over MCP (the token works): {}", r.body);
                check("initialize".into(), vec![], mcp(&fx.s, token, "initialize", json!({})));
                let r = mcp(&fx.s, token, "tools/list", json!({}));
                let tools = r.json()["result"]["tools"].as_array().cloned().unwrap_or_else(|| panic!("{who}: tools/list: {}", r.body));
                check("tools/list".into(), vec![], r);
                for tool in &tools {
                    let name = tool["name"].as_str().unwrap();
                    for args in aim(fx, tool) {
                        let mut sent = Vec::new();
                        strings(&Value::Object(args.clone()), &mut sent);
                        let r = mcp(&fx.s, token, "tools/call", json!({"name": name, "arguments": args}));
                        check(format!("tools/call {name} {}", Value::Object(args)), sent, r);
                    }
                }
                let r = mcp(&fx.s, token, "resources/list", json!({}));
                check("resources/list".into(), vec![], r);
                for uri in b_resources(fx) {
                    let r = mcp(&fx.s, token, "resources/read", json!({"uri": uri}));
                    check(format!("resources/read {uri}"), vec![uri.clone()], r);
                }
                let r = mcp(&fx.s, token, "prompts/list", json!({}));
                let prompts = r.json()["result"]["prompts"].as_array().cloned().unwrap_or_default();
                check("prompts/list".into(), vec![], r);
                for prompt in &prompts {
                    let name = prompt["name"].as_str().unwrap();
                    for args in prompt_args(fx, prompt) {
                        let mut sent = Vec::new();
                        strings(&Value::Object(args.clone()), &mut sent);
                        let r = mcp(&fx.s, token, "prompts/get", json!({"name": name, "arguments": args}));
                        check(format!("prompts/get {name} {}", Value::Object(args)), sent, r);
                    }
                }
                calls.lock().unwrap().insert(*who, (n, tools.len(), prompts.len()));
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.extend(changed(&before, &snapshot(fx)));
    let calls = calls.into_inner().unwrap();
    assert!(calls.values().all(|(_, tools, _)| *tools >= 25), "every token saw the tool list: {calls:?}");
    report("MCP", failures);
}

// ---------------------------------------------------------------------------
// /cable

/// A WebSocket client over a raw socket (copied from ga_web_2026_11_19.rs).
struct Ws(TcpStream);

impl Ws {
    fn open(s: &Server, cookies: &HashMap<String, String>) -> Ws {
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let cookie: Vec<String> = cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let req = format!(
            "GET /cable HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: actioncable-v1-json\r\nCookie: {}\r\n\r\n",
            s.addr,
            cookie.join("; ")
        );
        c.write_all(req.as_bytes()).unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            c.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101"), "{}", String::from_utf8_lossy(&head));
        Ws(c)
    }

    fn send(&mut self, text: &str) {
        let p = text.as_bytes();
        assert!(p.len() < 126, "short frames only");
        let mask = [1u8, 2, 3, 4];
        let mut f = vec![0x81, 0x80 | p.len() as u8];
        f.extend_from_slice(&mask);
        f.extend(p.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.0.write_all(&f).unwrap();
    }

    /// The next text message that is not a ping (a timeout panics).
    fn recv(&mut self) -> Value {
        loop {
            let mut h = [0u8; 2];
            self.0.read_exact(&mut h).unwrap();
            let mut len = (h[1] & 0x7f) as usize;
            if len == 126 {
                let mut l = [0u8; 2];
                self.0.read_exact(&mut l).unwrap();
                len = u16::from_be_bytes(l) as usize;
            } else if len == 127 {
                let mut l = [0u8; 8];
                self.0.read_exact(&mut l).unwrap();
                len = u64::from_be_bytes(l) as usize;
            }
            let mut p = vec![0u8; len];
            self.0.read_exact(&mut p).unwrap();
            if h[0] & 0x0f == 0x1 {
                let v: Value = serde_json::from_slice(&p).expect("a JSON message");
                if v["type"] != "ping" {
                    return v;
                }
            }
        }
    }
}

/// Owner, admin and member of A cannot subscribe to the live channel of any
/// of B's environments, by number or by string, while their own
/// environment's subscription is confirmed on the same socket.
#[test]
fn account_a_browsers_cannot_subscribe_to_account_bs_live_channel() {
    let fx = fx();
    let mut failures = Vec::new();
    for (who, browser) in &fx.sessions {
        let mut ws = Ws::open(&fx.s, &browser.jar);
        assert_eq!(ws.recv()["type"], "welcome", "{who}");
        // Broadcasts for A's own (confirmed) environment may arrive in
        // between; one for any other identifier would be B's.
        let own = json!({"channel": "EnvironmentChannel", "id": fx.a.env().id}).to_string();
        let mut subscribe = |id: Value| {
            let identifier = json!({"channel": "EnvironmentChannel", "id": id}).to_string();
            ws.send(&json!({"command": "subscribe", "identifier": identifier}).to_string());
            loop {
                let m = ws.recv();
                match m["type"].as_str() {
                    Some(t) => return t.to_string(),
                    None => assert_eq!(m["identifier"], json!(own), "{who}: a broadcast for another environment: {m}"),
                }
            }
        };
        assert_eq!(subscribe(json!(fx.a.env().id)), "confirm_subscription", "{who}: own environment");
        for e in fx.b.envs() {
            for id in [json!(e.id), json!(e.id.to_string())] {
                let got = subscribe(id.clone());
                if got != "reject_subscription" {
                    failures.push(format!("{who}: subscribe to B's environment {id} -> {got}"));
                }
            }
        }
    }
    report("cable", failures);
}

// ---------------------------------------------------------------------------
// Ingest keys

fn b_tenant_count(fx: &Fx, sql: &str) -> i64 {
    fx.b.envs().map(|e| open_tenant_reader(&fx.s.tenants_dir, &e.slug).unwrap().query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap()).sum()
}

/// An ingest key of A writes only to its own environment, whatever B ids,
/// slugs or deploy names its requests carry (headers, query, body, path),
/// over every ingest route; it is no credential on the API, MCP or B's
/// pages.
#[test]
fn account_a_ingest_key_only_ever_writes_to_its_own_environment() {
    let fx = fx();
    let (key, own, b) = (&fx.a.env().token, fx.a.env(), &fx.b);
    let before = snapshot(fx);
    let mut failures = Vec::new();
    let be = b.env();
    let id = be.id.to_string();

    let log = json!({"v": 1, "t": "log", "timestamp": 0, "level": "info", "message": "ACQ-INGEST-PROBE", "tags": [], "context": "{}",
                     "environment": be.slug, "environment_id": be.id, "application_id": b.apps[0].id});
    let mut log = log;
    log["timestamp"] = json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() - 5.0);
    let headers =
        [("X-Railwatch-Environment", be.slug.as_str()), ("X-Railwatch-Environment-Id", id.as_str()), ("X-Railwatch-Application", b.apps[0].slug.as_str())];
    let r = ingest_records(&fx.s, key, &format!("/ingest?environment_id={}&environment={}", be.id, be.slug), format!("{log}\n").as_bytes(), &headers);
    assert_eq!(r.status, 200, "{}", r.body);
    let in_a: i64 = open_tenant_reader(&fx.s.tenants_dir, &own.slug)
        .unwrap()
        .query_row("SELECT count(*) FROM logs WHERE message = 'ACQ-INGEST-PROBE'", [], |r| r.get(0))
        .unwrap();
    if in_a == 0 {
        failures.push("the probe log did not land in A's own environment".into());
    }
    for path in [format!("/ingest/{}", be.slug), format!("/ingest/{}/records", be.id)] {
        let r = ingest_records(&fx.s, key, &path, format!("{log}\n").as_bytes(), &[]);
        if r.status != 404 {
            failures.push(format!("POST {path} -> {}, expected 404", r.status));
        }
    }

    let r = raw(fx.s.addr, "GET", &format!("/ingest/ping?environment_id={}", be.id), &[("Authorization".into(), format!("Bearer {key}"))], b"");
    if r.json()["environment"] != json!(own.slug) {
        failures.push(format!("/ingest/ping answers {} for A's key", r.body));
    }
    let deploy = json!({"deploy": "acq-probe-deploy", "environment": be.slug, "environment_id": be.id, "application_id": b.apps[0].id}).to_string();
    let r = ingest_post(&fx.s, key, "/ingest/deploys", "application/json", deploy.as_bytes(), &headers);
    if r.status >= 300 {
        failures.push(format!("deploys -> {} {}", r.status, r.body));
    }
    let kamal = json!({"version": "acq-kamal-probe", "destination": be.name, "service": b.apps[0].slug, "environment_id": be.id, "command": "deploy",
                       "hosts": ["h1"], "performer": "probe"})
    .to_string();
    let r = ingest_post(&fx.s, key, "/ingest/kamal", "application/json", kamal.as_bytes(), &headers);
    if r.status >= 300 {
        failures.push(format!("kamal -> {} {}", r.status, r.body));
    }
    let deploys: i64 = one(&fx.s, "SELECT count(*) FROM deploys WHERE environment_id = ? AND deploy = 'acq-probe-deploy'", [own.id]);
    let kamal: String = one(&fx.s, "SELECT coalesce(kamal_config, '') FROM environments WHERE id = ?", [own.id]);
    if deploys != 1 || !kamal.contains("acq-kamal-probe") {
        failures.push(format!("A's deploy ({deploys}) and Kamal config ({kamal:?}) did not land in A's own environment"));
    }
    // B's own deploy name and file name, from A's key: stored under A.
    let map = br#"{"version":3,"sources":["acq-probe.js"],"names":[],"mappings":"AAAA"}"#;
    let r =
        ingest_post(&fx.s, key, "/ingest/sourcemaps", "application/json", map, &[("X-Railwatch-Deploy", "zqbdeploy1"), ("X-Railwatch-Filename", "app.js.map")]);
    if r.status >= 300 {
        failures.push(format!("sourcemaps -> {} {}", r.status, r.body));
    }
    for (method, path) in [("GET", "/api/v1/applications".to_string()), ("GET", format!("/api/v1/applications/{}/environments/{}/logs", b.apps[0].id, be.id))] {
        let r = api(&fx.s, key, method, &path, None);
        if r.status != 401 || !leaks(fx, &[], &r.body).is_empty() {
            failures.push(format!("ingest key on {path} -> {}", r.status));
        }
    }
    let r = mcp(&fx.s, key, "tools/list", json!({}));
    if r.status != 401 {
        failures.push(format!("ingest key on /mcp -> {}", r.status));
    }
    let r = raw(fx.s.addr, "GET", &format!("/apps/{}/envs/{}/logs", b.apps[0].id, be.id), &[("Authorization".into(), format!("Bearer {key}"))], b"");
    if !leaks(fx, &[], &r.body).is_empty() || r.status == 200 {
        failures.push(format!("ingest key on B's logs page -> {}", r.status));
    }
    if b_tenant_count(fx, "SELECT count(*) FROM logs WHERE message LIKE '%ACQ-INGEST-PROBE%'") != 0 {
        failures.push("the probe log landed in one of B's environments".into());
    }
    failures.extend(changed(&before, &snapshot(fx)));
    report("ingest key", failures);
}

// ---------------------------------------------------------------------------
// The scans are not vacuous

/// The probes reach B's data when B itself sends them: Bob's session gets
/// every environment page with the same ids A's probes use, and Bob's token
/// calls every read-only MCP tool with the same arguments (`aim`, B's own
/// shape), and each shows B's markers. So "no marker in A's responses"
/// means the data was withheld, not that a probe aimed at nothing.
#[test]
fn account_bs_own_principals_see_bs_data_through_the_same_probes() {
    let fx = fx();
    let (b, t) = (&fx.b, &fx.bob_token);
    let (ba, be) = (b.apps[0].id, b.env().id);
    let mut failures = Vec::new();
    let mut br = fx.bob_browser.clone();
    for p in dashboard_reads(fx).iter().filter(|p| p.path.starts_with(&format!("/apps/{ba}/envs/{be}"))) {
        let r = br.get(&fx.s, &p.path);
        if r.status != 200 || leaks(fx, &[], &r.body).is_empty() {
            failures.push(format!("Bob: GET {} -> {} with no B marker", p.path, r.status));
        }
    }
    for path in [format!("/issues/{}", b.issue), format!("/apps/{ba}/envs/{be}/uptime"), "/settings/integrations".into(), "/alerts".into()] {
        let r = br.get(&fx.s, &path);
        if r.status != 200 || leaks(fx, &[], &r.body).is_empty() {
            failures.push(format!("Bob: GET {path} -> {} with no B marker", r.status));
        }
    }
    let r = mcp(&fx.s, t, "tools/list", json!({}));
    for tool in r.json()["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        // Writes would change B (the snapshot tests compare against it);
        // their lookups are the read tools' (get_issue covers the key forms).
        if matches!(name, "update_issue" | "add_comment") {
            continue;
        }
        // aim()'s first shape is B's own identity, with B's object ids.
        let args = aim(fx, tool).remove(0);
        let r = mcp(&fx.s, t, "tools/call", json!({"name": name, "arguments": args}));
        if leaks(fx, &[], &r.body).is_empty() {
            failures.push(format!("Bob: MCP {name} {} shows no B marker: {}", Value::Object(args), &r.body[..r.body.len().min(200)]));
        }
    }
    let base = format!("/api/v1/applications/{ba}/environments/{be}");
    // Counts and time series only: no string of B's to find. A's probes of
    // these are asserted refused (403/404), which needs no marker.
    const NUMBERS_ONLY: [&str; 2] = ["Summary", "Series"];
    for (action, _, template) in API_ROUTES.iter().filter(|(a, m, _)| *m == "GET" && !NUMBERS_ONLY.contains(a)) {
        let path = template
            .replace("/api/v1/applications/{app}/environments/{env}", &base)
            .replace("{app}", &ba.to_string())
            .replace("{group}", &b.env().group)
            .replace("{execution}", &b.env().execution)
            .replace("{issue}", &b.issue.to_string())
            .replace("{incident}", &b.incident.to_string());
        let r = api(&fx.s, t, "GET", &path, None);
        if r.status != 200 || leaks(fx, &[], &r.body).is_empty() {
            failures.push(format!("Bob: API {action} GET {path} -> {} with no B marker", r.status));
        }
    }
    report("positive control", failures);
}

// ---------------------------------------------------------------------------
// Guards: a route, action or method the lists above do not name fails here.

/// Routes deliberately not probed with B's ids, and why.
const EXEMPT: &[(&str, &str, &str)] = &[
    ("GET", "/sign_in", "pre-auth credential form: no account context"),
    ("POST", "/sign_in", "pre-auth credential endpoint: no account context"),
    ("GET", "/sign_up", "pre-auth: no account context"),
    ("POST", "/sign_up", "pre-auth: creates a new user, no account id taken"),
    ("DELETE", "/users", "deletes the signed-in user itself; takes no id"),
    ("GET", "/identity/email_verification", "the caller's own identity, token-bound"),
    ("POST", "/identity/email_verification", "the caller's own identity, token-bound"),
    ("GET", "/identity/password_reset/new", "pre-auth form"),
    ("GET", "/identity/password_reset/edit", "token-bound to one user, no account id"),
    ("POST", "/identity/password_reset", "token-bound to one user, no account id"),
    ("PATCH", "/identity/password_reset", "token-bound to one user, no account id"),
    ("GET", "/invitations/{token}", "a capability URL: the invitation token is the authorization"),
    ("POST", "/accounts", "creates a new account owned by the caller; no id to aim"),
    ("PATCH", "/settings/profile", "the caller's own user row"),
    ("PATCH", "/settings/password", "the caller's own user row"),
    ("PATCH", "/settings/email", "the caller's own user row"),
    ("GET", "/auth/google_oauth2", "Google sign-in: pre-auth, answers 404"),
    ("POST", "/auth/google_oauth2", "Google sign-in: pre-auth"),
    ("GET", "/auth/{provider}/callback", "Google sign-in: pre-auth, state-bound"),
    ("GET", "/auth/failure", "Google sign-in: pre-auth"),
    ("POST", "/railwatch/beacon", "answers 204 and stores nothing"),
    ("POST", "/beacon", "only under a base path; answers 204 and stores nothing"),
    ("GET", "/cable", "account_a_browsers_cannot_subscribe_to_account_bs_live_channel"),
    ("POST", "/mcp", "every_mcp_tool_resource_and_prompt_keeps_account_bs_data_from_account_a"),
    ("ANY", "/api/{*rest}", "account_a_api_tokens_get_nothing_of_account_b_over_the_rest_api"),
];

/// The span of `src` from the `(` at `open` to its matching `)`.
fn call_span(src: &str, open: usize) -> &str {
    let (mut depth, mut quoted) = (0usize, false);
    for (i, c) in src[open..].char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return &src[open..open + i + 1];
                }
            }
            _ => {}
        }
    }
    &src[open..]
}

/// Every (method, path) any `.route(` in src/web registers, read from the
/// source; `&full` is env_pages' loop over PAGES, a constant is resolved in
/// its file. Test modules are skipped.
fn source_routes() -> BTreeSet<(String, String)> {
    let route_re = regex::Regex::new(r#"\.route\(\s*(?:"([^"]+)"|([A-Z_]+)\s*,|&full\s*,)"#).unwrap();
    let method_re = regex::Regex::new(r"\b(get|post|patch|put|delete|any)\(").unwrap();
    let mut files = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web"), &mut files);
    let mut out = BTreeSet::new();
    for f in files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")) {
        let text = std::fs::read_to_string(f).unwrap();
        let src = text.split("#[cfg(test)]\nmod tests").next().unwrap();
        // Every `.route(` call must be one the scanner reads: a path it
        // cannot resolve fails here instead of being skipped. (`.route(` in
        // a comment or doc line is not a call.)
        let calls = src.lines().filter(|l| !l.trim_start().starts_with("//")).map(|l| l.matches(".route(").count()).sum::<usize>();
        let parsed = route_re.find_iter(src).count();
        assert_eq!(calls, parsed, "{}: a .route( whose path the isolation guard cannot read; teach source_routes() its form", f.display());
        for m in route_re.captures_iter(src) {
            let open = m.get(0).unwrap().start() + ".route".len();
            let methods: Vec<String> = method_re.captures_iter(call_span(src, open)).map(|c| c[1].to_uppercase()).collect();
            assert!(!methods.is_empty(), "{}: a .route( with no method: {}", f.display(), &m[0]);
            let paths: Vec<String> = if let Some(p) = m.get(1) {
                vec![p.as_str().to_string()]
            } else if let Some(c) = m.get(2) {
                let re = regex::Regex::new(&format!(r#"const {}: &str = "([^"]+)";"#, c.as_str())).unwrap();
                vec![re.captures(src).unwrap_or_else(|| panic!("{}: const {} not found", f.display(), c.as_str()))[1].to_string()]
            } else {
                PAGES
                    .iter()
                    .map(|(sub, _)| if *sub == "/" { "/apps/{app}/envs/{env}".to_string() } else { format!("/apps/{{app}}/envs/{{env}}{sub}") })
                    .collect()
            };
            for p in &paths {
                for method in &methods {
                    out.insert((method.clone(), p.clone()));
                }
            }
        }
    }
    out
}

/// Every route the web router registers (read from the source, so a new
/// one is seen) is probed by the dashboard tests, covered by the API, MCP
/// or cable test, or exempted above with a reason; and every route those
/// lists name still exists. PUT routes share their PATCH handler
/// (`patch(x).put(x)`) and count as covered with it.
#[test]
fn every_web_route_in_the_source_is_probed_or_exempted() {
    let fx = fx();
    let source = source_routes();
    assert!(source.len() > 100, "the source scan found the routes: {source:?}");
    let mut covered: BTreeSet<(String, String)> = BTreeSet::new();
    for p in dashboard_reads(fx).into_iter().chain(dashboard_writes(fx)) {
        covered.insert((p.method.to_string(), p.route));
    }
    let exempt: BTreeSet<(String, String)> = EXEMPT.iter().map(|(m, p, _)| (m.to_string(), p.to_string())).collect();
    let mut missing = Vec::new();
    for (m, p) in &source {
        let key = (m.clone(), p.clone());
        let patch = ("PATCH".to_string(), p.clone());
        let via_patch = m == "PUT" && (covered.contains(&patch) || exempt.contains(&patch));
        if !covered.contains(&key) && !exempt.contains(&key) && !via_patch {
            missing.push(format!("{m} {p}"));
        }
    }
    assert!(
        missing.is_empty(),
        "routes no isolation probe covers (add a probe to dashboard_reads/dashboard_writes, or an EXEMPT entry with a reason):\n{}",
        missing.join("\n")
    );
    let stale: Vec<String> = covered.union(&exempt).filter(|r| !source.contains(*r)).map(|(m, p)| format!("{m} {p}")).collect();
    assert!(stale.is_empty(), "probes or exemptions for routes the source no longer has:\n{}", stale.join("\n"));
}

/// The hand-kept lists match the source: every variant of api.rs's
/// `Action` enum is in API_ROUTES, every JSON-RPC method of mcp.rs's
/// `dispatch_rpc` is called by the MCP test, the discovery document's
/// resource templates are the ones `b_resources` expands, and every ingest
/// path http.rs routes is sent by the ingest-key test.
#[test]
fn every_api_action_mcp_method_and_ingest_path_is_covered() {
    let src = |f: &str| std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(f)).unwrap();
    let between = |s: &str, from: &str, to: &str| -> String {
        let i = s.find(from).unwrap_or_else(|| panic!("{from:?} not found"));
        s[i..][..s[i..].find(to).unwrap()].to_string()
    };

    let api = src("web/api.rs");
    let body = between(&api, "enum Action {", "}");
    let variants: BTreeSet<String> =
        body.lines().skip(1).map(|l| l.trim().trim_end_matches(',').to_string()).filter(|l| !l.is_empty() && !l.starts_with("//")).collect();
    let listed: BTreeSet<String> = API_ROUTES.iter().map(|(a, _, _)| a.to_string()).collect();
    assert_eq!(variants, listed, "api.rs Action variants vs API_ROUTES");

    let mcp_src = src("web/mcp.rs");
    let rpc = between(&mcp_src, "fn dispatch_rpc", "\n    fn ");
    let methods: BTreeSet<String> = regex::Regex::new(r#"Some\("([a-z/]+)"\) =>"#).unwrap().captures_iter(&rpc).map(|c| c[1].to_string()).collect();
    let called: BTreeSet<String> =
        ["initialize", "tools/list", "tools/call", "resources/list", "resources/read", "prompts/list", "prompts/get"].iter().map(|s| s.to_string()).collect();
    assert_eq!(methods, called, "mcp.rs dispatch_rpc methods vs what the MCP test calls");

    let fx = fx();
    let r = raw(fx.s.addr, "GET", "/.well-known/mcp.json", &[], b"");
    let templates: BTreeSet<String> = r.json()["resources"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    let known: BTreeSet<String> = ["railwatch://applications", "railwatch://applications/<slug>/environments/<name>/summary", "railwatch://docs/<name>"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(templates, known, "MCP resource templates vs b_resources (docs are static text, no tenant data)");

    let http = src("http.rs");
    let route_fn = between(&http, "fn route(method: &Method, path: &str) -> Option<Action>", "\n}");
    let paths: BTreeSet<String> = regex::Regex::new(r#""(/ingest[^"]*)""#).unwrap().captures_iter(&route_fn).map(|c| c[1].to_string()).collect();
    let sent: BTreeSet<String> = ["/ingest", "/ingest/ping", "/ingest/deploys", "/ingest/kamal", "/ingest/sourcemaps"].iter().map(|s| s.to_string()).collect();
    assert_eq!(paths, sent, "http.rs ingest routes vs what the ingest-key test sends");
}
