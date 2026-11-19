//! Regression tests for the 2026-11-19 GA reviews (tenancy, security and
//! hunt), the web side, over the real `web::router` and real HTTP.
//!
//! One server per test, each in its own data dir; Config::load reads
//! process-global env, so the tests take a lock around it.

#![allow(dead_code)] // TEMP: helpers for the next tests

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState, proxy_auth};
use serde_json::{Value, json};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PROXY_SECRET: &[u8] = b"proxy-secret-proxy-secret-proxy-secret-0123";
static ENV: Mutex<()> = Mutex::new(());

struct Server {
    addr: SocketAddr,
    control: Arc<Control>,
    dir: std::path::PathBuf,
    _rt: tokio::runtime::Runtime,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `proxy`: with trusted proxy auth on (PROXY_SECRET).
fn start(name: &str, signup: &str, proxy: bool) -> Server {
    let dir = std::env::temp_dir().join(format!("rw-ga1119-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let secret_file = dir.join("proxy_secret");
    std::fs::write(&secret_file, PROXY_SECRET).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let cfg = {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
            std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
            std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
            std::env::set_var("RAILWATCH_SIGNUP", signup);
            std::env::set_var("RAILWATCH_TRUST_PROXY", "0");
            std::env::remove_var("RAILWATCH_TRUSTED_PROXIES");
            std::env::remove_var("RAILWATCH_BASE_PATH");
            if proxy {
                std::env::set_var("RAILWATCH_TRUSTED_PROXY_SECRET_FILE", secret_file.to_str().unwrap());
            } else {
                std::env::remove_var("RAILWATCH_TRUSTED_PROXY_SECRET_FILE");
            }
            std::env::remove_var("RAILWATCH_CONFIG");
        }
        Config::load().expect("config")
    };
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let addr = rt.block_on(async {
        let (tx, _rx) = followups::channel();
        let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
        let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
        let app = web::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });
    Server { addr, control, dir, _rt: rt }
}

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

    fn location(&self) -> &str {
        self.header("location").unwrap_or("")
    }

    /// The Inertia page object of an HTML response.
    fn page(&self) -> Value {
        let start = self.body.find("type=\"application/json\">").map(|i| i + 24).expect("an Inertia page");
        let end = self.body[start..].find("</script>").unwrap() + start;
        serde_json::from_str(&self.body[start..end].replace("\\/", "/")).unwrap()
    }
}

/// A cookie jar over raw sockets. `proxy`: the claims a trusted proxy
/// signs on every request this browser sends (email, role).
#[derive(Default)]
struct Browser {
    jar: HashMap<String, String>,
    proxy: Option<(String, String)>,
}

impl Browser {
    fn proxied(email: &str, role: &str) -> Browser {
        Browser { jar: HashMap::new(), proxy: Some((email.into(), role.into())) }
    }

    fn request(&mut self, s: &Server, method: &str, path: &str, form: Option<&str>, extra: &[(&str, &str)]) -> Res {
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let body = form.unwrap_or("");
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n", s.addr, body.len());
        if !self.jar.is_empty() {
            let cookies: Vec<String> = self.jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
            head.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
        }
        if form.is_some() {
            head.push_str(&format!("Content-Type: application/x-www-form-urlencoded\r\nOrigin: http://{}\r\n", s.addr));
        }
        if let Some((email, role)) = &self.proxy {
            let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 60;
            let h = proxy_auth::sign(PROXY_SECRET, &json!({"email": email, "name": "Proxied", "role": role, "exp": exp}));
            head.push_str(&format!("{}: {h}\r\n", proxy_auth::HEADER));
        }
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        c.write_all(format!("{head}\r\n{body}").as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = c.read_to_end(&mut buf);
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("a response");
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let mut headers = vec![];
        for line in head.split("\r\n").skip(1) {
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("set-cookie") {
                let pair = v.split(';').next().unwrap();
                let (name, value) = pair.split_once('=').unwrap();
                if value.is_empty() || v.contains("Max-Age=0") {
                    self.jar.remove(name);
                } else {
                    self.jar.insert(name.to_string(), value.to_string());
                }
            }
            headers.push((k.to_string(), v));
        }
        Res { status, headers, body: String::from_utf8_lossy(&buf[split + 4..]).to_string() }
    }

    fn get(&mut self, s: &Server, path: &str) -> Res {
        self.request(s, "GET", path, None, &[])
    }

    /// A form request with the page's CSRF token, as Inertia sends it.
    fn send(&mut self, s: &Server, method: &str, path: &str, form: &str) -> Res {
        let token = self.csrf(s);
        self.request(s, method, path, Some(form), &[("X-CSRF-Token", &token)])
    }

    fn post(&mut self, s: &Server, path: &str, form: &str) -> Res {
        self.send(s, "POST", path, form)
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
        let r = self.post(s, "/sign_in", &format!("email={}&password={}", enc(email), enc(password)));
        assert_eq!((r.status, r.location()), (302, "/dashboard"), "{email} signs in");
    }

    /// Follows a redirect to the page that shows its flash.
    fn flash_after(&mut self, s: &Server, r: &Res) -> Value {
        let mut to = r.location().to_string();
        loop {
            let r = self.get(s, &to);
            match r.status {
                302 => to = r.location().to_string(),
                200 => return r.page()["props"]["flash"].clone(),
                other => panic!("{other} at {to}"),
            }
        }
    }
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn user(s: &Server, name: &str, email: &str, password: &str, verified: bool) -> i64 {
    s.control.tx(|tx| control::insert_user(tx, name, email, password, verified)).unwrap()
}

fn one<T: rusqlite::types::FromSql>(s: &Server, sql: &str, p: impl rusqlite::Params) -> T {
    s.control.with(|x| x.query_row(sql, p, |r| r.get(0))).unwrap()
}

// ---------------------------------------------------------------------------
// Tenancy T3: trusted proxy identities

/// The server as `railwatch setup` leaves it for the sidecar: the host
/// account, bound to the proxy, plus an unrelated company that signed up
/// on its own. Returns (host account, other account, other app, boss's password).
fn proxy_fixture(s: &Server) -> (i64, i64, i64, String) {
    let password = control::base58(16);
    let owner = user(s, "Owner", "owner@host.test", &password, true);
    let host = s.control.tx(|tx| control::create_account(tx, "HostApp", owner)).unwrap();
    s.control.tx(|tx| Ok(proxy_auth::bind_account(tx, host)?)).unwrap();
    s.control.tx(|tx| control::insert_application(tx, host, "Host", "host", "HOST", None, "main")).unwrap();
    let boss = user(s, "Boss", "boss@other.test", &password, true);
    let other = s.control.tx(|tx| control::create_account(tx, "OtherCo", boss)).unwrap();
    let other_app = s.control.tx(|tx| control::insert_application(tx, other, "Other", "other", "OTH", None, "main")).unwrap();
    (host, other, other_app, password)
}

/// A proxy signature for an email that also owns another account acts in
/// the proxy's account only: it cannot switch into, open, manage, delete,
/// or mint a token for the user's other account, and it never acts as an
/// owner of anything.
#[test]
fn a_proxied_identity_stays_in_the_proxys_account_with_the_claimed_role() {
    let s = start("t3-pin", "open", true);
    let (host, other, other_app, _) = proxy_fixture(&s);
    let mut b = Browser::proxied("boss@other.test", "member");

    let dash = b.get(&s, "/dashboard");
    assert_eq!(dash.status, 200, "{}", dash.body);
    let props = &dash.page()["props"];
    assert_eq!(props["account"]["id"], host, "the proxy's account, not the user's own");
    assert_eq!(props["accounts"], json!([{"id": host, "name": "HostApp"}]), "the menu lists the proxy's account only");

    let r = b.post(&s, &format!("/accounts/{other}/switch"), "");
    assert_eq!(r.status, 403, "no switching out of the proxy's account");
    assert_eq!(b.get(&s, &format!("/apps/{other_app}")).status, 404, "the other account's application is not reachable");
    let members = b.get(&s, "/settings/members");
    assert_eq!(members.status, 200);
    let emails: Vec<String> = members.page()["props"]["members"].as_array().unwrap().iter().map(|m| m["email"].as_str().unwrap().to_string()).collect();
    assert!(emails.contains(&"owner@host.test".to_string()), "the host account's members, not OtherCo's: {emails:?}");
    assert_eq!(b.post(&s, "/accounts", "name=Mine").status, 403, "no new account through the proxy");
    let r = b.send(&s, "DELETE", &format!("/accounts/{other}"), "confirmation=OtherCo&password_challenge=x");
    assert_eq!(r.status, 403, "no deleting accounts through the proxy");
    assert_eq!(one::<i64>(&s, "SELECT count(*) FROM accounts WHERE id = ?", [other]), 1);

    // Tokens: only for the proxy's account.
    let profile = b.get(&s, "/settings/profile");
    let targets: Vec<String> = profile.page()["props"]["token_targets"].as_array().unwrap().iter().map(|t| t["value"].as_str().unwrap().to_string()).collect();
    assert!(targets.iter().all(|t| t == &format!("account:{host}") || t.starts_with("application:")), "{targets:?}");
    for restriction in ["", &format!("account:{other}"), &format!("application:{other_app}")] {
        b.post(&s, "/settings/profile/generate_mcp_token", &format!("restriction={}", enc(restriction)));
    }
    let tokens: i64 = one(&s, "SELECT count(*) FROM api_tokens WHERE user_id = (SELECT id FROM users WHERE email = 'boss@other.test')", []);
    assert_eq!(tokens, 0, "no unrestricted token and none for the other account");
    b.post(&s, "/settings/profile/generate_mcp_token", &format!("restriction=account%3A{host}"));
    let tokens: Vec<Option<i64>> = s
        .control
        .with(|x| {
            x.prepare("SELECT account_id FROM api_tokens WHERE user_id = (SELECT id FROM users WHERE email = 'boss@other.test')")?
                .query_map([], |r| r.get(0))?
                .collect()
        })
        .unwrap();
    assert_eq!(tokens, vec![Some(host)], "one for the proxy's account is fine");

    // The claimed role, not the user's role elsewhere: a member cannot manage.
    assert_eq!(b.post(&s, "/settings/members", "email=x%40y.test&role=admin").status, 403, "signed as member: no invitations");
    // Signed as admin they manage the host account, never the other one.
    let mut admin = Browser::proxied("boss@other.test", "admin");
    let r = admin.post(&s, "/settings/members", "email=x%40y.test&role=member");
    assert_eq!(r.status, 302);
    let invited_into: i64 = one(&s, "SELECT account_id FROM invitations WHERE email = 'x@y.test'", []);
    assert_eq!(invited_into, host);
}

/// The host account's owner, signed by the proxy as a member, acts as a
/// member: the proxy's claim, not the database's ownership, is what the
/// request carries. Ownership itself does not change.
#[test]
fn a_proxied_owner_signed_as_member_cannot_act_as_owner() {
    let s = start("t3-owner", "open", true);
    let (host, _, _, _) = proxy_fixture(&s);
    let mut b = Browser::proxied("owner@host.test", "member");
    let members = b.get(&s, "/settings/members");
    assert_eq!(members.page()["props"]["ownership"]["can_delete_account"], false, "{}", members.page()["props"]["ownership"]);
    assert_eq!(b.post(&s, "/settings/members", "email=x%40y.test&role=member").status, 403, "acts as a member");
    let r = b.send(&s, "DELETE", &format!("/accounts/{host}"), "confirmation=HostApp&password_challenge=x");
    assert_eq!(r.status, 403);
    let role: String =
        one(&s, "SELECT role FROM memberships WHERE account_id = ? AND user_id = (SELECT id FROM users WHERE email = 'owner@host.test')", [host]);
    assert_eq!(role, "owner", "the proxy never demotes the owner");
}

/// When the proxy's account is deleted, a proxied request is refused; it is
/// never joined to whichever account now has the lowest id.
#[test]
fn a_proxy_whose_account_is_gone_is_refused_instead_of_joining_the_next_account() {
    let s = start("t3-gone", "open", true);
    let (host, other, _, _) = proxy_fixture(&s);
    s.control
        .with(|x| {
            x.execute_batch(&format!(
                "DELETE FROM applications WHERE account_id = {host}; DELETE FROM memberships WHERE account_id = {host}; DELETE FROM accounts WHERE id = {host};"
            ))
        })
        .unwrap();

    let mut b = Browser::proxied("random-host-user@app.test", "admin");
    let r = b.get(&s, "/dashboard");
    assert_eq!(r.status, 403, "refused: {}", r.body);
    let mcp = b.request(&s, "POST", "/mcp", Some(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#), &[("Content-Type", "application/json")]);
    assert_eq!(mcp.status, 401, "MCP has nobody to be: {}", mcp.body);
    let joined: i64 = one(
        &s,
        "SELECT count(*) FROM memberships WHERE account_id = ? AND user_id IN (SELECT id FROM users WHERE email = 'random-host-user@app.test')",
        [other],
    );
    assert_eq!(joined, 0, "never auto-joined to another account");
}

/// MCP through the proxy is limited to the bound account. Here that is
/// not the lowest account id (an account created before setup bound the
/// proxy, as after an import), so "the first account" never decides.
#[test]
fn proxied_mcp_sees_only_the_bound_account_not_the_first_one() {
    let s = start("t3-mcp", "open", true);
    let password = control::base58(16);
    let boss = user(&s, "Boss", "boss@other.test", &password, true);
    let other = s.control.tx(|tx| control::create_account(tx, "OtherCo", boss)).unwrap();
    s.control.tx(|tx| control::insert_application(tx, other, "Other", "other", "OTH", None, "main")).unwrap();
    let owner = user(&s, "Owner", "owner@host.test", &password, true);
    let host = s.control.tx(|tx| control::create_account(tx, "HostApp", owner)).unwrap();
    s.control.tx(|tx| control::insert_application(tx, host, "Host", "host", "HOST", None, "main")).unwrap();
    s.control.with(|x| x.execute("DELETE FROM trusted_proxy", [])).ok();
    s.control.tx(|tx| Ok(proxy_auth::bind_account(tx, host)?)).unwrap();
    assert!(other < host);

    let mut b = Browser::proxied("boss@other.test", "admin");
    let r = b.request(
        &s,
        "POST",
        "/mcp",
        Some(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_applications","arguments":{}}}"#),
        &[("Content-Type", "application/json")],
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("HOST") || r.body.contains("host"), "{}", r.body);
    assert!(!r.body.contains("OTH") && !r.body.contains("OtherCo"), "the user's own account is not the proxy's: {}", r.body);
    let joined: i64 = one(&s, "SELECT count(*) FROM memberships WHERE account_id = ? AND user_id = ?", [host, boss]);
    assert_eq!(joined, 1, "joined the bound account");
}
