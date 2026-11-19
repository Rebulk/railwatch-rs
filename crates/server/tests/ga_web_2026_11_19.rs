//! Regression tests for the 2026-11-19 GA reviews (tenancy, security and
//! hunt), the web side, over the real `web::router` and real HTTP.
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
use railwatch_server::web::{self, WebState, proxy_auth};
use serde_json::{Value, json};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PROXY_SECRET: &[u8] = b"proxy-secret-proxy-secret-proxy-secret-0123";
static ENV: Mutex<()> = Mutex::new(());

struct Server {
    addr: SocketAddr,
    control: Arc<Control>,
    dir: std::path::PathBuf,
    /// Every recipient and message the SMTP server received.
    mail: Inbox,
    _rt: tokio::runtime::Runtime,
}

impl Server {
    /// Waits (briefly: mail is sent in the background) for `n` mails.
    fn mails(&self, n: usize) -> Vec<(String, String)> {
        for _ in 0..200 {
            if self.mail.lock().unwrap().len() >= n {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.mail.lock().unwrap().clone()
    }
}

/// (recipient, message) of every mail sent.
type Inbox = Arc<Mutex<Vec<(String, String)>>>;

/// A minimal SMTP server: records (RCPT TO, DATA) of every mail.
fn smtp_sink(rt: &tokio::runtime::Runtime) -> (u16, Inbox) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mail = Arc::new(Mutex::new(Vec::new()));
    let out = mail.clone();
    rt.spawn(async move {
        loop {
            let Ok((c, _)) = listener.accept().await else { return };
            let out = out.clone();
            tokio::spawn(async move {
                let (r, mut w) = c.into_split();
                let mut lines = BufReader::new(r).lines();
                let _ = w.write_all(b"220 sink\r\n").await;
                let (mut to, mut data, mut body) = (String::new(), false, String::new());
                while let Ok(Some(line)) = lines.next_line().await {
                    if data {
                        if line == "." {
                            data = false;
                            out.lock().unwrap().push((std::mem::take(&mut to), std::mem::take(&mut body)));
                            let _ = w.write_all(b"250 ok\r\n").await;
                        } else {
                            body.push_str(&line);
                            body.push('\n');
                        }
                        continue;
                    }
                    let u = line.to_ascii_uppercase();
                    let reply: &[u8] = if u.starts_with("EHLO") {
                        b"250 sink\r\n"
                    } else if u.starts_with("RCPT TO:") {
                        to = line[8..].trim().trim_matches(['<', '>']).to_string();
                        b"250 ok\r\n"
                    } else if u.starts_with("DATA") {
                        data = true;
                        b"354 go\r\n"
                    } else if u.starts_with("QUIT") {
                        let _ = w.write_all(b"221 bye\r\n").await;
                        return;
                    } else {
                        b"250 ok\r\n"
                    };
                    let _ = w.write_all(reply).await;
                }
            });
        }
    });
    (port, mail)
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
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let (smtp_port, mail) = smtp_sink(&rt);
    let cfg = {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("RAILWATCH_SMTP_ADDRESS", "127.0.0.1");
            std::env::set_var("RAILWATCH_SMTP_PORT", smtp_port.to_string());
            std::env::set_var("RAILWATCH_SMTP_SECURITY", "none");
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
    Server { addr, control, dir, mail, _rt: rt }
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

// ---------------------------------------------------------------------------
// Hunt H2, tenancy I2: what a removed member keeps

/// Acme with Alice (owner) and Adam (admin, signed in as `adam`).
fn acme(s: &Server) -> (i64, i64, i64, String) {
    let password = control::base58(16);
    let alice = user(s, "Alice", "alice@acme.test", &password, true);
    let account = s.control.tx(|tx| control::create_account(tx, "Acme", alice)).unwrap();
    let adam = user(s, "Adam", "adam@acme.test", &password, true);
    s.control.tx(|tx| control::insert_membership(tx, account, adam, "admin")).unwrap();
    (account, alice, adam, password)
}

/// A removed admin's pending invitations go with them, so they cannot come
/// back (as admin) through an invitation they sent to an address they own.
#[test]
fn removing_an_admin_withdraws_the_invitations_they_sent() {
    let s = start("h2-remove", "open", false);
    let (account, _, adam, password) = acme(&s);
    let mut a = Browser::default();
    a.sign_in(&s, "adam@acme.test", &password);
    assert_eq!(a.post(&s, "/settings/members", "email=alt%40evil.test&role=admin").status, 302);
    let mails = s.mails(1);
    let link = mails.iter().find(|(to, _)| to == "alt@evil.test").map(|(_, b)| b.clone()).expect("the invitation mail");
    let token = link.split("/invitations/").nth(1).unwrap().split_whitespace().next().unwrap().trim_end_matches('=').to_string();
    let found = s.control.with(|x| control::find_invitation_by_token(x, &token)).unwrap();
    assert!(found.is_some(), "the token from the mail is the invitation's: {link}");

    let mut alice = Browser::default();
    alice.sign_in(&s, "alice@acme.test", &password);
    let membership: i64 = one(&s, "SELECT id FROM memberships WHERE account_id = ? AND user_id = ?", [account, adam]);
    assert_eq!(alice.send(&s, "DELETE", &format!("/settings/members/{membership}"), "").status, 302);
    assert_eq!(one::<i64>(&s, "SELECT count(*) FROM invitations WHERE inviter_id = ?", [adam]), 0, "withdrawn with him");

    // The address he controls signs up with the invitation: no way in.
    let mut alt = Browser::default();
    let r = alt.get(&s, &format!("/invitations/{token}"));
    assert_eq!(r.status, 302, "an unknown invitation: {}", r.body);
    let p = control::base58(16);
    alt.post(&s, "/sign_up", &format!("name=Alt&email=alt%40evil.test&password={p}&password_confirmation={p}&terms=1&invitation_token={token}"));
    let joined: i64 =
        one(&s, "SELECT count(*) FROM memberships m JOIN users u ON u.id = m.user_id WHERE u.email = 'alt@evil.test' AND m.account_id = ?", [account]);
    assert_eq!(joined, 0, "not back in Acme");
}

/// Making an admin a member also withdraws the invitations they sent.
#[test]
fn demoting_an_admin_withdraws_the_invitations_they_sent() {
    let s = start("h2-demote", "open", false);
    let (account, alice, adam, password) = acme(&s);
    s.control.tx(|tx| control::create_invitation(tx, account, adam, "alt@evil.test", "admin")).unwrap();
    s.control.tx(|tx| control::create_invitation(tx, account, alice, "keep@acme.test", "member")).unwrap();
    let mut b = Browser::default();
    b.sign_in(&s, "alice@acme.test", &password);
    let membership: i64 = one(&s, "SELECT id FROM memberships WHERE account_id = ? AND user_id = ?", [account, adam]);
    assert_eq!(b.send(&s, "PATCH", &format!("/settings/members/{membership}"), "role=member").status, 302);
    let left: Vec<String> = s.control.with(|x| x.prepare("SELECT email FROM invitations")?.query_map([], |r| r.get(0))?.collect()).unwrap();
    assert_eq!(left, ["keep@acme.test"], "only the demoted admin's are withdrawn");
}

/// A removed member's tokens restricted to that account (or one of its
/// applications) are revoked; their other tokens are untouched.
#[test]
fn removing_a_member_revokes_their_tokens_for_that_account() {
    let s = start("i2-tokens", "open", false);
    let (account, _, adam, password) = acme(&s);
    let app = s.control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let own = s.control.tx(|tx| control::create_account(tx, "AdamCo", adam)).unwrap();
    let scopes: Vec<String> = ["api", "read"].map(String::from).to_vec();
    let issue = |acc: Option<i64>, ap: Option<i64>| s.control.tx(|tx| control::issue_api_token(tx, adam, "t", &scopes, acc, ap, None)).unwrap().0;
    let (for_account, for_app, for_own, everywhere) = (issue(Some(account), None), issue(None, Some(app)), issue(Some(own), None), issue(None, None));

    let mut b = Browser::default();
    b.sign_in(&s, "alice@acme.test", &password);
    let membership: i64 = one(&s, "SELECT id FROM memberships WHERE account_id = ? AND user_id = ?", [account, adam]);
    assert_eq!(b.send(&s, "DELETE", &format!("/settings/members/{membership}"), "").status, 302);
    let revoked = |id: i64| one::<bool>(&s, "SELECT revoked_at IS NOT NULL FROM api_tokens WHERE id = ?", [id]);
    assert_eq!((revoked(for_account), revoked(for_app)), (true, true), "Acme's tokens are revoked");
    assert_eq!((revoked(for_own), revoked(everywhere)), (false, false), "his own account's and his unrestricted token stay");
}

// ---------------------------------------------------------------------------
// Hunt H3: an invitation needs the invited mailbox

/// Setting your email to the invited address (allowed with your own
/// password, unverified) does not let you accept the invitation.
#[test]
fn an_unverified_email_change_cannot_claim_an_invitation() {
    let s = start("h3", "open", false);
    let (account, alice, _, _) = acme(&s);
    let inv = s.control.tx(|tx| control::create_invitation(tx, account, alice, "newhire@corp.test", "admin")).unwrap();
    let eve_password = control::base58(16);
    user(&s, "Eve", "eve@evil.test", &eve_password, true);
    let mut eve = Browser::default();
    eve.sign_in(&s, "eve@evil.test", &eve_password);
    let r = eve.send(&s, "PATCH", "/settings/email", &format!("email=newhire%40corp.test&password_challenge={eve_password}"));
    assert_eq!(r.status, 302);
    let r = eve.get(&s, &format!("/invitations/{}", inv.token));
    assert_eq!(r.status, 302);
    let joined: i64 =
        one(&s, "SELECT count(*) FROM memberships m JOIN users u ON u.id = m.user_id WHERE u.email = 'newhire@corp.test' AND m.account_id = ?", [account]);
    assert_eq!(joined, 0, "Eve is not in Acme");
    assert_eq!(one::<Option<String>>(&s, "SELECT accepted_at FROM invitations WHERE id = ?", [inv.id]), None, "the invitation is still the real hire's");
}

// ---------------------------------------------------------------------------
// Security PLAUSIBLE 3: email change

/// Changing the email tells the old address and signs out the user's other
/// sessions (a stolen one included); the session that made the change stays.
#[test]
fn an_email_change_notifies_the_old_address_and_ends_other_sessions() {
    let s = start("email-change", "open", false);
    let password = control::base58(16);
    user(&s, "Ada", "ada@example.com", &password, true);
    let mut stolen = Browser::default();
    stolen.sign_in(&s, "ada@example.com", &password);
    let mut b = Browser::default();
    b.sign_in(&s, "ada@example.com", &password);
    let r = b.send(&s, "PATCH", "/settings/email", &format!("email=ada2%40example.com&password_challenge={password}"));
    assert_eq!(r.status, 302);
    let mails = s.mails(2);
    let old = mails.iter().find(|(to, _)| to == "ada@example.com").expect("the old address is told");
    assert!(old.1.contains("ada2@example.com"), "{}", old.1);
    assert!(mails.iter().any(|(to, _)| to == "ada2@example.com"), "the new address gets its verification link");
    assert_eq!(stolen.get(&s, "/dashboard").location(), "/sign_in", "the other session is signed out");
    assert_ne!(b.get(&s, "/settings/email").status, 302, "the changing session stays");
}

// ---------------------------------------------------------------------------
// Hunt H4: /cable limits

/// A WebSocket client over a raw socket: masked text frames out, server
/// frames in. Enough of RFC 6455 for /cable.
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

    fn send(&mut self, text: &str) -> std::io::Result<()> {
        let p = text.as_bytes();
        let mut f = vec![0x81];
        match p.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n < 65536 => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = [1u8, 2, 3, 4];
        f.extend_from_slice(&mask);
        f.extend(p.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.0.write_all(&f)
    }

    /// The next text message that is not a ping; None once the server closed.
    fn recv(&mut self) -> Option<Value> {
        loop {
            let mut h = [0u8; 2];
            self.0.read_exact(&mut h).ok()?;
            let mut len = (h[1] & 0x7f) as usize;
            if len == 126 {
                let mut l = [0u8; 2];
                self.0.read_exact(&mut l).ok()?;
                len = u16::from_be_bytes(l) as usize;
            } else if len == 127 {
                let mut l = [0u8; 8];
                self.0.read_exact(&mut l).ok()?;
                len = u64::from_be_bytes(l) as usize;
            }
            let mut p = vec![0u8; len];
            self.0.read_exact(&mut p).ok()?;
            match h[0] & 0x0f {
                0x8 => return None,
                0x1 => {
                    let v: Value = serde_json::from_slice(&p).ok()?;
                    if v["type"] != "ping" {
                        return Some(v);
                    }
                }
                _ => {}
            }
        }
    }
}

/// One socket cannot pile up subscriptions: an identifier is at most 256
/// bytes, a connection holds at most 16, and a message over 4 KiB closes it.
#[test]
fn cable_subscriptions_per_connection_are_bounded() {
    let s = start("h4-cable", "open", false);
    let (account, _, _, password) = acme(&s);
    let app = s.control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let (env, _) = s.control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let mut b = Browser::default();
    b.sign_in(&s, "alice@acme.test", &password);
    let mut ws = Ws::open(&s, &b.jar);
    assert_eq!(ws.recv().unwrap()["type"], "welcome");
    let subscribe = |pad: &str| {
        let identifier = json!({"channel": "EnvironmentChannel", "id": env, "pad": pad}).to_string();
        json!({"command": "subscribe", "identifier": identifier}).to_string()
    };
    let mut confirmed = 0;
    for i in 0..20 {
        ws.send(&subscribe(&i.to_string())).unwrap();
        if ws.recv().unwrap()["type"] == "confirm_subscription" {
            confirmed += 1;
        }
    }
    assert_eq!(confirmed, 16, "at most 16 subscriptions per connection");
    let mut ws = Ws::open(&s, &b.jar);
    assert_eq!(ws.recv().unwrap()["type"], "welcome");
    ws.send(&subscribe(&"x".repeat(300))).unwrap();
    assert_eq!(ws.recv().unwrap()["type"], "reject_subscription", "a long identifier is not kept");
    ws.send(&subscribe("ok")).unwrap();
    assert_eq!(ws.recv().unwrap()["type"], "confirm_subscription", "the connection still works");
    let _ = ws.send(&subscribe(&"x".repeat(100_000)));
    assert_eq!(ws.recv(), None, "a 100 KB message closes the connection");
}

// ---------------------------------------------------------------------------
// Security 4: uptime monitor URLs

/// A health-check URL often carries a credential. Members, and tokens of
/// members, see only its host on the page, through MCP and the API, and in
/// the uptime alerts' payload; admins see it whole.
#[test]
fn members_see_only_the_host_of_an_uptime_monitor_url() {
    use railwatch_server::uptime::{self, MonitorInput};
    let s = start("sec4-uptime", "open", false);
    let (account, _, adam, password) = acme(&s);
    let mallory = user(&s, "Mallory", "mallory@acme.test", &password, true);
    s.control.tx(|tx| control::insert_membership(tx, account, mallory, "member")).unwrap();
    let app = s.control.tx(|tx| control::insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let (env, _) = s.control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let url = "https://example.com/health?token=MONITORSECRET";
    let monitor = s.control.with(|x| uptime::insert(x, env, &MonitorInput { name: "Health".into(), url: url.into(), ..Default::default() })).unwrap().unwrap();
    // A failed check whose error, incident and alert repeat the URL, as they did.
    let error = format!("error sending request for url ({url})");
    s.control.with(|x| x.execute("UPDATE uptime_monitors SET last_error = ? WHERE id = ?", rusqlite::params![error, monitor])).unwrap();
    s.control
        .with(|x| {
            x.execute("INSERT INTO uptime_incidents (monitor_id, started_at, cause, detail) VALUES (?, 1, 'connect', ?)", rusqlite::params![monitor, error])
        })
        .unwrap();
    let integration = s
        .control
        .tx(|tx| {
            Ok(railwatch_server::alerts::integration::insert(
                tx,
                &railwatch_server::alerts::integration::SettingsCipher::new(SECRET.as_bytes()),
                account,
                "email",
                "Ops",
                &serde_json::from_value(json!({"to": "ops@acme.test"})).unwrap(),
            )?)
        })
        .unwrap();
    s.control
        .with(|x| {
            x.execute(
                "INSERT INTO alerts (event, integration_id, payload, status, created_at, updated_at) VALUES ('monitor_down', ?, ?, 'sent', '2026-11-19 00:00:00', 'x')",
                rusqlite::params![integration, json!({"monitor": "Health", "url": url, "environment_id": env, "message": format!("Health (production) is down. {url}")}).to_string()],
            )
        })
        .unwrap();
    let scopes: Vec<String> = ["api", "mcp", "read"].map(String::from).to_vec();
    let (_, member_token) = s.control.tx(|tx| control::issue_api_token(tx, mallory, "t", &scopes, None, None, None)).unwrap();
    let (_, admin_token) = s.control.tx(|tx| control::issue_api_token(tx, adam, "t", &scopes, None, None, None)).unwrap();

    let page = |email: &str, path: &str| {
        let mut b = Browser::default();
        b.sign_in(&s, email, &password);
        let r = b.get(&s, path);
        assert_eq!(r.status, 200, "{path}");
        r.page()["props"].to_string()
    };
    let api = |token: &str| {
        let mut b = Browser::default();
        let r = b.request(&s, "GET", &format!("/api/v1/applications/{app}/environments/{env}/uptime"), None, &[("Authorization", &format!("Bearer {token}"))]);
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    };
    let mcp = |token: &str| {
        let mut b = Browser::default();
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "uptime", "arguments": {"application": "shop", "environment": "production"}}}).to_string();
        let r = b.request(&s, "POST", "/mcp", Some(&body), &[("Content-Type", "application/json"), ("Authorization", &format!("Bearer {token}"))]);
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    };
    let uptime_page = format!("/apps/{app}/envs/{env}/uptime");
    for (what, seen) in [
        ("page", page("mallory@acme.test", &uptime_page)),
        ("alerts page", page("mallory@acme.test", "/alerts")),
        ("env alerts page", page("mallory@acme.test", &format!("/apps/{app}/envs/{env}/alerts"))),
        ("REST", api(&member_token)),
        ("MCP", mcp(&member_token)),
    ] {
        assert!(!seen.contains("MONITORSECRET"), "a member sees the monitor's secret on the {what}: {seen}");
        assert!(seen.contains("example.com"), "and still sees which host it checks on the {what}");
    }
    assert!(page("adam@acme.test", &uptime_page).contains("MONITORSECRET"), "an admin sees the URL whole");
    assert!(api(&admin_token).contains("MONITORSECRET") && mcp(&admin_token).contains("MONITORSECRET"), "so does an admin's token");
}
