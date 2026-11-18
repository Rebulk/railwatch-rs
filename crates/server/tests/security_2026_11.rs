//! Regression tests for the 2026-11 security review, over the real
//! `web::router` and real HTTP: membership by consent (9), unverified
//! accounts that squat an address (13), the verification link's rate limit
//! (15) and the dashboard's Content-Security-Policy (15).
//!
//! One server per test, each in its own data dir; Config::load reads
//! process-global env, so the tests take a lock around it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState};
use serde_json::Value;

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
static ENV: Mutex<()> = Mutex::new(());

struct Server {
    addr: SocketAddr,
    control: Arc<Control>,
    state: Arc<WebState>,
    dir: std::path::PathBuf,
    _rt: tokio::runtime::Runtime,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(name: &str, signup: &str) -> Server {
    start_with(name, signup, false)
}

fn start_with(name: &str, signup: &str, trust_proxy: bool) -> Server {
    let dir = std::env::temp_dir().join(format!("rw-sec2611-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
            std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
            std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
            std::env::set_var("RAILWATCH_SIGNUP", signup);
            std::env::set_var("RAILWATCH_TRUST_PROXY", if trust_proxy { "1" } else { "0" });
            std::env::remove_var("RAILWATCH_CONFIG");
        }
        Config::load().expect("config")
    };
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let (state, addr) = rt.block_on(async {
        let (tx, _rx) = followups::channel();
        let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
        let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
        let app = web::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, addr)
    });
    Server { addr, control, state, dir, _rt: rt }
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

/// A cookie jar over raw sockets: the request reaches the server exactly as
/// written, and the test picks every header (X-Forwarded-For included).
#[derive(Default)]
struct Browser {
    jar: HashMap<String, String>,
}

impl Browser {
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

    /// A form post with the page's CSRF token, as Inertia sends it.
    fn post(&mut self, s: &Server, path: &str, form: &str) -> Res {
        let token = self.csrf(s);
        self.request(s, "POST", path, Some(form), &[("X-CSRF-Token", &token)])
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

// ---------------------------------------------------------------------------
// Finding 9

/// Adding someone to an account is an invitation they accept, whether or
/// not they already have a user here; the admin's reply is the same either
/// way, so it does not say who has an account.
#[test]
fn adding_a_member_invites_them_instead_of_joining_them_without_consent() {
    let s = start("members", "invite");
    let password = control::base58(16);
    let bob = user(&s, "Bob", "bob@example.com", &password, true);
    let alice = user(&s, "Alice", "alice@example.com", &password, true);
    s.control.tx(|tx| control::create_account(tx, "Bobco", bob)).unwrap();
    let mut b = Browser::default();
    b.sign_in(&s, "bob@example.com", &password);

    let existing = b.post(&s, "/settings/members", "email=alice%40example.com&role=member");
    let existing_flash = b.flash_after(&s, &existing);
    let stranger = b.post(&s, "/settings/members", "email=nobody%40example.com&role=member");
    let stranger_flash = b.flash_after(&s, &stranger);

    let memberships: i64 = s.control.with(|x| x.query_row("SELECT COUNT(*) FROM memberships WHERE user_id = ?", [alice], |r| r.get(0))).unwrap();
    assert_eq!(memberships, 0, "Alice is not in Bob's account until she accepts");
    let invited: Vec<String> = s.control.with(|x| x.prepare("SELECT email FROM invitations ORDER BY id")?.query_map([], |r| r.get(0))?.collect()).unwrap();
    assert_eq!(invited, ["alice@example.com", "nobody@example.com"]);
    assert_eq!(existing_flash["notice"], "Invitation sent to alice@example.com", "{existing_flash}");
    assert_eq!(stranger_flash["notice"], "Invitation sent to nobody@example.com");
}

// ---------------------------------------------------------------------------
// Finding 13

/// The link a password reset mails (controllers::signed_token), built here
/// because without SMTP the server no longer logs it.
fn reset_sid(s: &Server, user_id: i64) -> String {
    let digest: String = s.control.with(|x| x.query_row("SELECT password_digest FROM users WHERE id = ?", [user_id], |r| r.get(0))).unwrap();
    let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    let value = format!("{user_id}.{exp}");
    let mac = s.state.keys.sign("password_reset", &format!("{value}.{}", &digest[19..29]));
    format!("{value}.{}", mac.rsplit_once("--").unwrap().1)
}

/// With open sign-up anyone can register an address they do not own; the
/// account stays unverified. The address's real owner proves it with a
/// password reset, which takes the account over: verified, the squatter's
/// password, sessions and tokens gone.
#[test]
fn a_password_reset_takes_an_unverified_account_back_from_whoever_signed_up_with_the_address() {
    let s = start("squat", "open");
    let squatter_password = control::base58(16);
    let mut squatter = Browser::default();
    let r = squatter.post(
        &s,
        "/sign_up",
        &format!("name=Mallory&email=victim%40example.com&password={squatter_password}&password_confirmation={squatter_password}&terms=1"),
    );
    assert_eq!((r.status, r.location()), (302, "/dashboard"), "the squatter signs up");
    let id: i64 = s.control.with(|x| x.query_row("SELECT id FROM users WHERE email = 'victim@example.com'", [], |r| r.get(0))).unwrap();
    s.control.tx(|tx| control::issue_api_token(tx, id, "squat", &["api".into(), "read".into()], None, None, None)).unwrap();

    // The owner asks for a reset: it is sent (the request is claimed).
    let mut owner = Browser::default();
    owner.post(&s, "/identity/password_reset", "email=victim%40example.com");
    let sent: Option<String> = s.control.with(|x| x.query_row("SELECT password_reset_sent_at FROM users WHERE id = ?", [id], |r| r.get(0))).unwrap();
    assert!(sent.is_some(), "a reset goes to an unverified address too");

    let new_password = control::base58(16);
    let token = owner.csrf(&s);
    let r = owner.request(
        &s,
        "PATCH",
        "/identity/password_reset",
        Some(&format!("sid={}&password={new_password}&password_confirmation={new_password}", reset_sid(&s, id))),
        &[("X-CSRF-Token", &token)],
    );
    assert_eq!((r.status, r.location()), (302, "/sign_in"), "{}", r.body);
    let (verified, sessions, tokens): (bool, i64, i64) = s
        .control
        .with(|x| {
            x.query_row(
                "SELECT verified, (SELECT COUNT(*) FROM sessions WHERE user_id = ?1), (SELECT COUNT(*) FROM api_tokens WHERE user_id = ?1) FROM users WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap();
    assert_eq!((verified, sessions, tokens), (true, 0, 0), "the owner's now: verified, the squatter's sessions and tokens gone");
    assert_eq!(squatter.get(&s, "/dashboard").location(), "/sign_in", "the squatter is signed out");
    owner.sign_in(&s, "victim@example.com", &new_password);
}

// ---------------------------------------------------------------------------
// Finding 8

/// Behind two proxies (a CDN, then a local proxy) every request reaches the
/// server from the local proxy with `X-Forwarded-For: <client>, <cdn>`. The
/// per-IP limits must key on the client, or one client's attempts lock out
/// everyone behind the same edge.
#[test]
fn per_ip_limits_key_on_the_client_behind_two_proxies_not_on_the_edge_proxy() {
    let s = start_with("xff", "open", true);
    let attempt = |client: &str| {
        let mut b = Browser::default();
        let token = b.csrf(&s);
        let r = b.request(&s, "POST", "/sign_up", Some("name=x"), &[("X-CSRF-Token", &token), ("X-Forwarded-For", &format!("{client}, 10.0.0.2"))]);
        b.flash_after(&s, &r)["alert"].as_str().unwrap_or("").to_string()
    };
    let limited = "Too many sign-up attempts. Try again later.";
    for _ in 0..10 {
        assert_ne!(attempt("198.51.100.1"), limited);
    }
    assert_eq!(attempt("198.51.100.1"), limited, "the 11th sign-up in an hour from one client");
    assert_ne!(attempt("198.51.100.2"), limited, "another client behind the same proxies is not limited");
}

// ---------------------------------------------------------------------------
// Finding 15: the email verification link

/// Opening verification links is unauthenticated: it is rate-limited per
/// client like the other identity endpoints, so guessing or hammering it
/// is bounded.
#[test]
fn opening_email_verification_links_is_rate_limited_per_client() {
    let s = start("verify-limit", "invite");
    let password = control::base58(16);
    user(&s, "Ada", "ada@example.com", &password, false);
    let mut b = Browser::default();
    b.sign_in(&s, "ada@example.com", &password);
    let alerts: Vec<String> = (0..21)
        .map(|_| {
            let r = b.get(&s, "/identity/email_verification?sid=1.9999999999.bad");
            assert_eq!(r.status, 302);
            b.flash_after(&s, &r)["alert"].as_str().unwrap_or("").to_string()
        })
        .collect();
    assert!(alerts[..20].iter().all(|a| a == "That email verification link is invalid"), "{alerts:?}");
    assert_eq!(alerts[20], "Too many verification attempts. Try again later.");
}

// ---------------------------------------------------------------------------
// Finding 15: alert delivery and proxies

/// The alert client checks and pins the destination's address itself (the
/// SSRF guard); an HTTPS_PROXY in the server's environment must not route
/// deliveries around that, so the client ignores proxy variables.
#[test]
fn alert_delivery_ignores_proxy_environment_variables() {
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let vars = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];
    unsafe {
        for v in vars {
            std::env::set_var(v, format!("http://{}", proxy.local_addr().unwrap()));
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
        std::env::set_var("RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS", "1");
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(railwatch_server::alerts::outbound_http::post_json(
        &format!("https://127.0.0.1:{closed}/hook"),
        "{}".into(),
        &[],
        railwatch_server::alerts::outbound_http::Kind::Webhook,
    ));
    unsafe {
        for v in vars {
            std::env::remove_var(v);
        }
        std::env::remove_var("RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS");
    }
    assert!(result.is_err(), "nothing listens at the destination");
    assert!(proxy.accept().is_err(), "the delivery went through the proxy");
}

// ---------------------------------------------------------------------------
// Finding 15: Content-Security-Policy

fn directive<'a>(csp: &'a str, name: &str) -> Option<Vec<&'a str>> {
    csp.split(';').map(str::trim).find_map(|d| d.strip_prefix(name).filter(|r| r.is_empty() || r.starts_with(' ')).map(|r| r.split_whitespace().collect()))
}

/// The sha256 CSP source of each inline script a page runs (not the
/// Inertia page data, which is JSON and never executed).
fn inline_script_hashes(html: &str) -> Vec<String> {
    use base64::Engine;
    use sha2::Digest;
    let mut out = vec![];
    let mut rest = html;
    while let Some(i) = rest.find("<script") {
        let tag_end = rest[i..].find('>').unwrap() + i;
        let tag = &rest[i..tag_end];
        let close = rest[tag_end..].find("</script>").unwrap() + tag_end;
        if !tag.contains(" src=") && !tag.contains("application/json") {
            let body = &rest[tag_end + 1..close];
            out.push(format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(body.as_bytes()))));
        }
        rest = &rest[close..];
    }
    out
}

/// Pages say where scripts, connections and form posts may go: scripts only
/// from this server plus the one inline dark-mode script by hash (no
/// 'unsafe-inline'), so injected markup cannot run script; connections and
/// forms only back to this server.
#[test]
fn pages_allow_only_this_servers_scripts_and_the_dark_mode_script_by_hash() {
    let s = start("csp", "invite");
    let password = control::base58(16);
    let ada = user(&s, "Ada", "ada@example.com", &password, true);
    let account = s.control.tx(|tx| control::create_account(tx, "Acme", ada)).unwrap();
    s.control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let mut b = Browser::default();
    let sign_in = b.get(&s, "/sign_in");
    b.sign_in(&s, "ada@example.com", &password);
    let dashboard = b.get(&s, "/dashboard");
    assert_eq!(dashboard.status, 200);
    for (page, r) in [("sign-in", &sign_in), ("dashboard", &dashboard)] {
        let csp = r.header("content-security-policy").expect("a CSP");
        let scripts = directive(csp, "script-src").unwrap_or_else(|| panic!("{page}: no script-src in {csp}"));
        assert!(scripts.contains(&"'self'") && !scripts.contains(&"'unsafe-inline'"), "{page}: {csp}");
        let inline = inline_script_hashes(&r.body);
        assert!(!inline.is_empty(), "{page} has the dark-mode script");
        for h in &inline {
            assert!(scripts.contains(&h.as_str()), "{page}: inline script {h} is not allowed by {csp}");
        }
        assert_eq!(directive(csp, "connect-src"), Some(vec!["'self'"]), "{page}: {csp}");
        assert_eq!(directive(csp, "form-action").map(|v| v[0]), Some("'self'"), "{page}: {csp}");
        for kept in ["base-uri", "object-src", "frame-ancestors"] {
            assert!(directive(csp, kept).is_some(), "{page}: {kept} kept");
        }
    }
}
