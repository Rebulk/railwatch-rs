//! Google sign-in end to end: the real `railwatch serve` binary against a
//! mock Google (an OpenID provider in this test: /authorize, /token,
//! /certs) that checks PKCE, the client secret and the redirect URI, issues
//! single-use codes and signs ID tokens with test keys. A small cookie-jar
//! browser walks sign-in page -> POST /auth/google_oauth2 -> the provider
//! -> the callback, as a person clicking "Continue with Google" would.
//!
//! The mock signs whatever claims the test asks for, so every rejection
//! (wrong aud/iss, expired, bad signature, wrong nonce, unverified email)
//! is a genuine token the server must refuse.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use railwatch_server::control::{self, Control};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");
const CLIENT_ID: &str = "railwatch-test.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "test-secret";

fn key(name: &str) -> ring::signature::RsaKeyPair {
    let der = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oidc").join(format!("{name}.pk8"))).unwrap();
    ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap()
}

fn jwk(kid: &str, k: &ring::signature::RsaKeyPair) -> Value {
    let c: ring::rsa::PublicKeyComponents<Vec<u8>> = k.public().into();
    json!({"kty": "RSA", "alg": "RS256", "use": "sig", "kid": kid, "n": B64.encode(&c.n), "e": B64.encode(&c.e)})
}

fn sign(kid: &str, k: &ring::signature::RsaKeyPair, claims: &Value) -> String {
    let head = B64.encode(serde_json::to_vec(&json!({"alg": "RS256", "kid": kid, "typ": "JWT"})).unwrap());
    let input = format!("{head}.{}", B64.encode(serde_json::to_vec(claims).unwrap()));
    let mut sig = vec![0; k.public().modulus_len()];
    k.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
    format!("{input}.{}", B64.encode(sig))
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

// ---------------------------------------------------------------------------
// The mock provider.

/// What the next authorization should produce. `claims` overrides the
/// defaults; `sign_with` picks the key (kid, key name).
#[derive(Clone)]
struct Next {
    email: String,
    sub: String,
    email_verified: Value,
    claims: Value,
    sign_with: (String, String),
}

impl Next {
    fn user(email: &str, sub: &str) -> Next {
        Next { email: email.into(), sub: sub.into(), email_verified: json!(true), claims: json!({}), sign_with: ("k1".into(), "google-k1".into()) }
    }
}

struct Grant {
    challenge: String,
    redirect_uri: String,
    nonce: String,
    next: Next,
}

#[derive(Default)]
struct Provider {
    next: Option<Next>,
    grants: HashMap<String, Grant>,
    /// Published kids -> key file.
    published: Vec<(String, String)>,
    jwks_fetches: usize,
    log: Vec<String>,
}

struct Mock {
    base: String,
    state: Arc<Mutex<Provider>>,
}

impl Mock {
    fn start() -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let state = Arc::new(Mutex::new(Provider { published: vec![("k1".into(), "google-k1".into())], ..Default::default() }));
        let (s2, b2) = (state.clone(), base.clone());
        std::thread::spawn(move || {
            for conn in l.incoming().flatten() {
                let (s3, b3) = (s2.clone(), b2.clone());
                std::thread::spawn(move || serve_one(conn, &s3, &b3));
            }
        });
        Mock { base, state }
    }

    fn next(&self, n: Next) {
        self.state.lock().unwrap().next = Some(n);
    }
}

fn serve_one(mut conn: TcpStream, state: &Mutex<Provider>, base: &str) {
    let Some((method, target, headers, body)) = read_request(&mut conn) else { return };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let (status, ctype, out, location): (u16, &str, String, Option<String>) = match (method.as_str(), path) {
        ("GET", "/authorize") => {
            let q: HashMap<String, String> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
            let mut p = state.lock().unwrap();
            p.log.push(format!("authorize {}", q.get("scope").cloned().unwrap_or_default()));
            let ok = q.get("client_id").map(String::as_str) == Some(CLIENT_ID)
                && q.get("response_type").map(String::as_str) == Some("code")
                && q.get("code_challenge_method").map(String::as_str) == Some("S256")
                && q.contains_key("code_challenge")
                && q.contains_key("nonce")
                && q.contains_key("state");
            match (ok, p.next.take()) {
                (true, Some(next)) => {
                    let code = format!("code-{}", p.grants.len() + 1);
                    let redirect = q["redirect_uri"].clone();
                    p.grants.insert(
                        code.clone(),
                        Grant { challenge: q["code_challenge"].clone(), redirect_uri: redirect.clone(), nonce: q["nonce"].clone(), next },
                    );
                    let to = format!("{redirect}?code={code}&state={}&scope=email%20profile%20openid", enc(&q["state"]));
                    (302, "text/plain", String::new(), Some(to))
                }
                _ => (400, "text/plain", "bad authorization request".into(), None),
            }
        }
        ("POST", "/token") => {
            let f: HashMap<String, String> = form_urlencoded::parse(&body).into_owned().collect();
            let mut p = state.lock().unwrap();
            // Basic auth (oauth2's default) or client_secret_post.
            let basic =
                headers.get("authorization").and_then(|v| v.strip_prefix("Basic ")).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
            let creds = basic.and_then(|b| String::from_utf8(b).ok());
            let client_ok = creds.as_deref() == Some(&format!("{}:{}", enc(CLIENT_ID), enc(CLIENT_SECRET)))
                || (f.get("client_id").map(String::as_str) == Some(CLIENT_ID) && f.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET));
            // Single use: the code is gone whatever happens next.
            let grant = f.get("code").and_then(|c| p.grants.remove(c));
            let verifier_ok = |g: &Grant| f.get("code_verifier").is_some_and(|v| B64.encode(Sha256::digest(v.as_bytes())) == g.challenge);
            match grant {
                Some(g)
                    if client_ok
                        && f.get("grant_type").map(String::as_str) == Some("authorization_code")
                        && f.get("redirect_uri") == Some(&g.redirect_uri)
                        && verifier_ok(&g) =>
                {
                    p.log.push("token ok".into());
                    let mut claims = json!({"iss": base, "aud": CLIENT_ID, "azp": CLIENT_ID, "sub": g.next.sub, "email": g.next.email,
                        "email_verified": g.next.email_verified, "name": "Ada Lovelace", "nonce": g.nonce, "iat": now(), "exp": now() + 3600});
                    for (k, v) in g.next.claims.as_object().unwrap() {
                        if v.is_null() {
                            claims.as_object_mut().unwrap().remove(k);
                        } else {
                            claims[k] = v.clone();
                        }
                    }
                    let id_token = sign(&g.next.sign_with.0, &key(&g.next.sign_with.1), &claims);
                    (
                        200,
                        "application/json",
                        json!({"access_token": "ya29.test", "token_type": "Bearer", "expires_in": 3599, "scope": "openid email profile", "id_token": id_token})
                            .to_string(),
                        None,
                    )
                }
                g => {
                    p.log.push(format!("token refused (grant {})", if g.is_some() { "bad" } else { "unknown/used" }));
                    (400, "application/json", json!({"error": "invalid_grant", "error_description": "Bad Request"}).to_string(), None)
                }
            }
        }
        ("GET", "/certs") => {
            let mut p = state.lock().unwrap();
            p.jwks_fetches += 1;
            let keys: Vec<Value> = p.published.iter().map(|(kid, file)| jwk(kid, &key(file))).collect();
            (200, "application/json", json!({"keys": keys}).to_string(), None)
        }
        _ => (404, "text/plain", "not found".into(), None),
    };
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n", out.len());
    if let Some(l) = location {
        head.push_str(&format!("Location: {l}\r\n"));
    }
    let _ = conn.write_all(format!("{head}\r\n{out}").as_bytes());
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

type Request = (String, String, HashMap<String, String>, Vec<u8>);

fn read_request(conn: &mut TcpStream) -> Option<Request> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let split = loop {
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: HashMap<String, String> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = buf[split + 4..].to_vec();
    while body.len() < len {
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((method, target, headers, body))
}

// ---------------------------------------------------------------------------
// The server under test and a browser.

struct Server {
    child: Child,
    port: u16,
    data: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

impl Server {
    fn start(issuer: &str, signup: &str) -> Server {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let data = std::env::temp_dir().join(format!("rw-google-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&data).unwrap();
        let child = Command::new(BIN)
            .arg("serve")
            .env("RAILWATCH_DATA_DIR", &data)
            .env("RAILWATCH_BIND", format!("127.0.0.1:{port}"))
            .env("RAILWATCH_SECRET_KEY_BASE", "00".repeat(64))
            .env("RAILWATCH_GOOGLE_CLIENT_ID", CLIENT_ID)
            .env("RAILWATCH_GOOGLE_CLIENT_SECRET", CLIENT_SECRET)
            .env("RAILWATCH_GOOGLE_ISSUER", issuer)
            .env("RAILWATCH_SIGNUP", signup)
            .env("RAILWATCH_LOG", "warn")
            .env_remove("RAILWATCH_CONFIG")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(started.elapsed() < Duration::from_secs(20), "server did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Server { child, port, data }
    }

    fn control(&self) -> Control {
        Control::open(&self.data.join("control.sqlite3")).unwrap()
    }

    fn users(&self) -> Vec<(String, Option<String>, Option<String>, bool)> {
        self.control()
            .with(|x| {
                x.prepare("SELECT email, provider, uid, verified FROM users ORDER BY id")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect()
            })
            .unwrap()
    }

    fn sessions(&self) -> i64 {
        self.control().with(|x| x.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))).unwrap()
    }
}

/// Reads a `Connection: close` response to EOF. A server that closes with
/// bytes still unread (or the kernel's RST racing the FIN) can surface as
/// ConnectionReset after the whole response arrived; that is not a failure.
fn read_response(s: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => return buf,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && buf.windows(4).any(|w| w == b"\r\n\r\n") => return buf,
            Err(e) => panic!("reading the response: {e}"),
        }
    }
}

#[derive(Debug)]
struct Res {
    status: u16,
    location: Option<String>,
    body: String,
}

impl Res {
    fn page(&self) -> Value {
        let start = self.body.find("type=\"application/json\">").map(|i| i + 24).expect("an Inertia page");
        let end = self.body[start..].find("</script>").unwrap() + start;
        serde_json::from_str(&self.body[start..end].replace("\\/", "/")).unwrap()
    }
}

#[derive(Default)]
struct Browser {
    jar: HashMap<String, String>,
}

impl Browser {
    fn request(&mut self, url: &str, method: &str, form: Option<&str>) -> Res {
        let rest = url.strip_prefix("http://").unwrap();
        let (hostport, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let mut s = TcpStream::connect(hostport).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let cookies: Vec<String> = self.jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let body = form.unwrap_or("");
        let mut head = format!(
            "{method} {} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nContent-Length: {}\r\n",
            if path.is_empty() { "/" } else { path },
            body.len()
        );
        if !cookies.is_empty() {
            head.push_str(&format!("Cookie: {}\r\n", cookies.join("; ")));
        }
        if form.is_some() {
            head.push_str(&format!("Content-Type: application/x-www-form-urlencoded\r\nOrigin: http://{hostport}\r\n"));
        }
        s.write_all(format!("{head}\r\n{body}").as_bytes()).unwrap();
        let buf = read_response(&mut s);
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let mut location = None;
        for line in head.split("\r\n").skip(1) {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k.to_ascii_lowercase().as_str() {
                "location" => location = Some(v.trim().to_string()),
                "set-cookie" => {
                    let pair = v.trim().split(';').next().unwrap();
                    let (name, value) = pair.split_once('=').unwrap();
                    if value.is_empty() || v.contains("Max-Age=0") {
                        self.jar.remove(name);
                    } else {
                        self.jar.insert(name.to_string(), value.to_string());
                    }
                }
                _ => {}
            }
        }
        Res { status, location, body: String::from_utf8_lossy(&buf[split + 4..]).to_string() }
    }

    fn get(&mut self, url: &str) -> Res {
        self.request(url, "GET", None)
    }

    /// The csrf-token meta of a page (sign-in, or the profile once signed in).
    fn csrf(&mut self, base: &str) -> String {
        let mut r = self.get(&format!("{base}/sign_in"));
        if r.status == 302 {
            r = self.get(&format!("{base}/settings/profile"));
        }
        let i = r.body.find("name=\"csrf-token\" content=\"").map(|i| i + 27).expect("csrf meta");
        r.body[i..i + r.body[i..].find('"').unwrap()].to_string()
    }

    /// Click "Continue with Google": the form post, then the redirect to
    /// the provider. Returns the provider's response (a redirect to our
    /// callback when it accepted the authorization request).
    fn click_google(&mut self, base: &str, extra: &str) -> Res {
        self.click_google_at(base, "/auth/google_oauth2", extra)
    }

    /// The button's form: POST to `action` (the reauthentication button puts
    /// `?reauthenticate=` there) with the CSRF token and `extra` fields.
    fn click_google_at(&mut self, base: &str, action: &str, extra: &str) -> Res {
        let token = self.csrf(base);
        let r = self.request(&format!("{base}{action}"), "POST", Some(&format!("authenticity_token={}{extra}", enc(&token))));
        assert_eq!(r.status, 302, "request phase redirects to the provider: {r:?}");
        self.get(r.location.as_deref().unwrap())
    }

    /// The whole round trip; returns the callback's redirect and the flash
    /// the next page shows.
    fn sign_in_with_google(&mut self, base: &str) -> (String, Value) {
        let at_provider = self.click_google(base, "");
        assert_eq!(at_provider.status, 302, "provider accepted: {}", at_provider.body);
        self.follow(base, &at_provider.location.unwrap())
    }

    /// GET the callback, then follow redirects within our site to the page
    /// that renders the flash.
    fn follow(&mut self, base: &str, callback: &str) -> (String, Value) {
        let r = self.get(callback);
        assert_eq!(r.status, 302, "callback redirects: {r:?}");
        let first = r.location.clone().unwrap();
        let mut to = first.clone();
        loop {
            let r = self.get(&format!("{base}{to}"));
            match r.status {
                302 => to = r.location.unwrap(),
                200 => return (first, r.page()["props"]["flash"].clone()),
                s => panic!("unexpected {s} at {to}"),
            }
        }
    }

    /// Signed in: /sign_in turns a signed-in browser away.
    fn signed_in(&mut self, base: &str) -> bool {
        self.get(&format!("{base}/sign_in")).status == 302
    }
}

fn alert(flash: &Value) -> &str {
    flash["alert"].as_str().unwrap_or("")
}

fn notice(flash: &Value) -> &str {
    flash["notice"].as_str().unwrap_or("")
}

// ---------------------------------------------------------------------------

#[test]
fn google_sign_in_end_to_end() {
    let mock = Mock::start();
    let server = Server::start(&mock.base, "open");
    let base = format!("http://127.0.0.1:{}", server.port);

    // The button shows: google_oauth is true.
    let mut b = Browser::default();
    assert_eq!(b.get(&format!("{base}/sign_in")).page()["props"]["google_oauth"], json!(true));

    // 1. A new user: created verified, signed in.
    mock.next(Next::user("New.Person@Example.com", "g-new"));
    // (A new user has no account: /dashboard sends them on to /accounts/new.)
    let (to, _) = b.sign_in_with_google(&base);
    assert_eq!(to, "/dashboard");
    assert!(b.signed_in(&base));
    assert_eq!(server.users(), vec![("new.person@example.com".into(), Some("google_oauth2".into()), Some("g-new".into()), true)]);
    assert!(mock.state.lock().unwrap().log.iter().any(|l| l == "authorize openid email profile"), "scopes");

    // 2. Signed in already: the callback says so (Rails' before_action).
    // (`/` then forwards to /dashboard and /accounts/new, which carry the
    // flash no further here: a pre-existing self-hosted redirect chain.)
    mock.next(Next::user("new.person@example.com", "g-new"));
    let callback = b.click_google(&base, "").location.unwrap();
    let r = b.get(&callback);
    assert_eq!(r.location.as_deref(), Some("/"));
    assert!(b.jar.contains_key("_railwatch_session"));

    // 3. An existing user with a verified email: linked, not duplicated.
    let linked = server.control().tx(|tx| control::insert_user(tx, "Linked", "linked@example.com", &control::base58(16), true)).unwrap();
    // With an account and an application, /dashboard renders (and shows the flash).
    let acct = server.control().tx(|tx| control::create_account(tx, "Linked Co", linked)).unwrap();
    server.control().tx(|tx| control::insert_application(tx, acct, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let mut b = Browser::default();
    mock.next(Next::user("linked@example.com", "g-linked"));
    let (to, flash) = b.sign_in_with_google(&base);
    assert_eq!((to.as_str(), notice(&flash)), ("/dashboard", "Signed in with Google"));
    let users = server.users();
    assert_eq!(users.len(), 2);
    assert_eq!(users[1], ("linked@example.com".into(), Some("google_oauth2".into()), Some("g-linked".into()), true));

    // 4. An existing user whose email is unverified here: refused.
    server.control().tx(|tx| control::insert_user(tx, "U", "unverified@example.com", &control::base58(16), false)).unwrap();
    let invalid = "That Google account's email is unverified or already registered. Sign in with your password instead.";
    let mut b = Browser::default();
    mock.next(Next::user("unverified@example.com", "g-u"));
    let (to, flash) = b.sign_in_with_google(&base);
    assert_eq!((to.as_str(), alert(&flash)), ("/sign_in", invalid));
    assert!(!b.signed_in(&base));

    // 5. Google has not verified the email: neither linked nor created.
    let sessions = server.sessions();
    for unverified in [json!(false), Value::Null, json!("false")] {
        let mut b = Browser::default();
        mock.next(Next { email_verified: unverified, ..Next::user("linked@example.com", "attacker") });
        let (to, flash) = b.sign_in_with_google(&base);
        assert_eq!((to.as_str(), alert(&flash)), ("/sign_in", invalid));
    }
    assert_eq!(server.users().len(), 3);
    assert_eq!(server.users()[1].2.as_deref(), Some("g-linked"), "not relinked to the attacker");
    assert_eq!(server.sessions(), sessions);

    // 6. Tokens the server must refuse, each a real signed token from the
    // provider: Sign in with Google failed, via /auth/failure.
    let failed = "Sign in with Google failed. Please try again.";
    let bad: Vec<(&str, Next)> = vec![
        ("wrong aud", Next { claims: json!({"aud": "someone-else.apps.googleusercontent.com", "azp": null}), ..Next::user("x@example.com", "g-x") }),
        ("wrong azp", Next { claims: json!({"azp": "someone-else"}), ..Next::user("x@example.com", "g-x") }),
        ("wrong iss", Next { claims: json!({"iss": "https://evil.example.com"}), ..Next::user("x@example.com", "g-x") }),
        ("expired", Next { claims: json!({"exp": now() - 120, "iat": now() - 3720}), ..Next::user("x@example.com", "g-x") }),
        ("not yet valid", Next { claims: json!({"nbf": now() + 600}), ..Next::user("x@example.com", "g-x") }),
        ("no exp", Next { claims: json!({"exp": null}), ..Next::user("x@example.com", "g-x") }),
        ("wrong nonce", Next { claims: json!({"nonce": "replayed-from-elsewhere"}), ..Next::user("x@example.com", "g-x") }),
        ("no nonce", Next { claims: json!({"nonce": null}), ..Next::user("x@example.com", "g-x") }),
        ("bad signature (attacker key under a published kid)", Next { sign_with: ("k1".into(), "attacker".into()), ..Next::user("x@example.com", "g-x") }),
        ("unknown kid", Next { sign_with: ("nope".into(), "attacker".into()), ..Next::user("x@example.com", "g-x") }),
    ];
    for (what, next) in bad {
        let mut b = Browser::default();
        mock.next(next);
        let at_provider = b.click_google(&base, "");
        let callback = at_provider.location.unwrap();
        let r = b.get(&callback);
        assert_eq!(r.location.as_deref(), Some("/auth/failure?message=invalid_credentials&strategy=google_oauth2"), "{what}");
        let (to, flash) = b.follow(&base, &format!("{base}{}", r.location.unwrap()));
        assert_eq!((to.as_str(), alert(&flash)), ("/sign_in", failed), "{what}");
    }
    assert_eq!(server.users().len(), 3, "no user from a refused token");

    // 7. State mismatch, missing state, and a callback with no request phase.
    let mut b = Browser::default();
    mock.next(Next::user("x@example.com", "g-x"));
    let callback = b.click_google(&base, "").location.unwrap();
    let forged = callback.replace("state=", "state=0");
    assert_eq!(b.get(&forged).location.as_deref(), Some("/auth/failure?message=csrf_detected&strategy=google_oauth2"));
    // The state was consumed: the genuine callback is now refused too.
    assert_eq!(b.get(&callback).location.as_deref(), Some("/auth/failure?message=csrf_detected&strategy=google_oauth2"));
    let mut fresh = Browser::default();
    assert_eq!(fresh.get(&callback).location.as_deref(), Some("/auth/failure?message=csrf_detected&strategy=google_oauth2"), "another browser's callback");

    // 8. A replayed code: the same callback URL again, with the browser's
    // state restored (an attacker holding the session cookie too).
    let mut b = Browser::default();
    mock.next(Next::user("replay@example.com", "g-replay"));
    let token = b.csrf(&base);
    let r = b.request(&format!("{base}/auth/google_oauth2"), "POST", Some(&format!("authenticity_token={}", enc(&token))));
    let session_before_callback = b.jar.clone();
    let callback = b.get(r.location.as_deref().unwrap()).location.unwrap();
    assert_eq!(b.get(&callback).location.as_deref(), Some("/dashboard"));
    let mut thief = Browser { jar: session_before_callback };
    thief.jar.remove("session_token");
    assert_eq!(thief.get(&callback).location.as_deref(), Some("/auth/failure?message=invalid_credentials&strategy=google_oauth2"));
    assert!(mock.state.lock().unwrap().log.iter().any(|l| l == "token refused (grant unknown/used)"));
    assert!(!thief.signed_in(&base));

    // 9. The user cancels at Google: error=access_denied.
    let mut b = Browser::default();
    mock.next(Next::user("x@example.com", "g-x"));
    let callback = b.click_google(&base, "").location.unwrap();
    let state = callback.split("state=").nth(1).unwrap().split('&').next().unwrap();
    let denied = format!("{base}/auth/google_oauth2/callback?error=access_denied&state={state}");
    assert_eq!(b.get(&denied).location.as_deref(), Some("/auth/failure?message=access_denied&strategy=google_oauth2"));

    // 10. The request phase needs the form's CSRF token.
    let mut b = Browser::default();
    b.csrf(&base);
    let r = b.request(&format!("{base}/auth/google_oauth2"), "POST", Some("authenticity_token=forged"));
    assert_eq!(r.location.as_deref(), Some("/auth/failure?message=ActionController%3A%3AInvalidAuthenticityToken&strategy=google_oauth2"));
    assert_eq!(b.get(&format!("{base}/auth/google_oauth2")).status, 404, "GET is not a request phase (allowed_request_methods = [:post])");
}

#[test]
fn key_rotation_refetches_the_jwks_once_and_unknown_kids_cannot_force_a_storm() {
    let mock = Mock::start();
    let server = Server::start(&mock.base, "open");
    let base = format!("http://127.0.0.1:{}", server.port);
    let mut b = Browser::default();
    mock.next(Next::user("first@example.com", "g-first"));
    assert_eq!(b.sign_in_with_google(&base).0, "/dashboard");
    assert_eq!(mock.state.lock().unwrap().jwks_fetches, 1);

    // Google starts signing with k2: the unknown kid forces one refresh.
    mock.state.lock().unwrap().published.push(("k2".into(), "google-k2".into()));
    let mut b = Browser::default();
    mock.next(Next { sign_with: ("k2".into(), "google-k2".into()), ..Next::user("rotated@example.com", "g-rot") });
    assert_eq!(b.sign_in_with_google(&base).0, "/dashboard");
    assert_eq!(mock.state.lock().unwrap().jwks_fetches, 2, "one refetch for the new kid");
    let mut b = Browser::default();
    mock.next(Next { sign_with: ("k2".into(), "google-k2".into()), ..Next::user("rotated@example.com", "g-rot") });
    assert_eq!(b.sign_in_with_google(&base).0, "/dashboard");
    assert_eq!(mock.state.lock().unwrap().jwks_fetches, 2, "then cached");

    // A token naming a kid nobody publishes may force at most one fetch a
    // minute (the sender picks the kid); here the minute is already spent.
    for _ in 0..3 {
        let mut b = Browser::default();
        mock.next(Next { sign_with: ("bogus".into(), "attacker".into()), ..Next::user("x@example.com", "g-x") });
        let callback = b.click_google(&base, "").location.unwrap();
        assert_eq!(b.get(&callback).location.as_deref(), Some("/auth/failure?message=invalid_credentials&strategy=google_oauth2"));
    }
    assert_eq!(mock.state.lock().unwrap().jwks_fetches, 2, "no refetch storm");
}

#[test]
fn reauthentication_for_google_users() {
    let mock = Mock::start();
    let server = Server::start(&mock.base, "open");
    let base = format!("http://127.0.0.1:{}", server.port);
    let mut b = Browser::default();
    mock.next(Next::user("owner@example.com", "g-owner"));
    assert_eq!(b.sign_in_with_google(&base).0, "/dashboard");
    let owner: i64 = server.control().with(|x| x.query_row("SELECT id FROM users", [], |r| r.get(0))).unwrap();
    let account = server.control().tx(|tx| control::create_account(tx, "Owner Co", owner)).unwrap();
    // A co-owner, so the user is not the account's only owner.
    let co = server.control().tx(|tx| control::insert_user(tx, "Co", "co@example.com", &control::base58(16), true)).unwrap();
    server
        .control()
        .with(|x| {
            x.execute(
                "INSERT INTO memberships (account_id, user_id, role, created_at, updated_at) VALUES (?1, ?2, 'owner', '2026-01-01', '2026-01-01')",
                [account, co],
            )
        })
        .unwrap();
    // Make the sign-in old.
    server.control().with(|x| x.execute("UPDATE sessions SET authenticated_at = '2020-01-01 00:00:00'", [])).unwrap();
    // A different Google account cannot confirm this user.
    mock.next(Next::user("stranger@example.com", "g-stranger"));
    let at = b.click_google_at(&base, "/auth/google_oauth2?reauthenticate=account", "&prompt=select_account");
    let (to, flash) = b.follow(&base, &at.location.unwrap());
    assert_eq!((to.as_str(), alert(&flash)), ("/settings/members", "Sign in with the Google account already linked to this user."));
    // Cancelled at Google: back to the page it came from, with the alert.
    mock.next(Next::user("owner@example.com", "g-owner"));
    let callback = b.click_google_at(&base, "/auth/google_oauth2?reauthenticate=account", "&prompt=select_account").location.unwrap();
    let state = callback.split("state=").nth(1).unwrap().split('&').next().unwrap();
    let failure = b.get(&format!("{base}/auth/google_oauth2/callback?error=access_denied&state={state}")).location.unwrap();
    let (to, flash) = b.follow(&base, &format!("{base}{failure}"));
    assert_eq!((to.as_str(), alert(&flash)), ("/settings/members", "Google could not confirm your identity. Please try again."));
    // Any /auth/failure hit while signed in is a failed reauthentication,
    // even with no reauthentication kept: the profile, not "already signed in".
    let (to, flash) = b.follow(&base, &format!("{base}/auth/failure?message=access_denied&strategy=google_oauth2"));
    assert_eq!((to.as_str(), alert(&flash)), ("/settings/profile", "Google could not confirm your identity. Please try again."));
    let stale: String = server.control().with(|x| x.query_row("SELECT authenticated_at FROM sessions", [], |r| r.get(0))).unwrap();
    assert!(stale.starts_with("2020"));
    // Deleting the user needs the email typed and a recent Google round-trip
    // (an Inertia DELETE: the XSRF-TOKEN cookie echoed in the header).
    let delete_user = |b: &mut Browser| {
        b.get(&format!("{base}/settings/profile"));
        let rest = base.strip_prefix("http://").unwrap();
        let mut s = TcpStream::connect(rest).unwrap();
        let cookies: Vec<String> = b.jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let body = "confirmation=Owner%40Example.com";
        let x = b.jar.get("XSRF-TOKEN").cloned().unwrap();
        s.write_all(format!("DELETE /users HTTP/1.1\r\nHost: {rest}\r\nConnection: close\r\nCookie: {}\r\nX-XSRF-TOKEN: {x}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", cookies.join("; "), body.len()).as_bytes()).unwrap();
        let buf = String::from_utf8_lossy(&read_response(&mut s)).into_owned();
        buf.lines().find_map(|l| l.to_ascii_lowercase().starts_with("location:").then(|| l[9..].trim().to_string())).unwrap_or_default()
    };
    assert_eq!(delete_user(&mut b), "/settings/profile", "stale sign-in: refused");
    assert_eq!(server.users().len(), 2);
    // The right Google account confirms; then deletion goes through.
    mock.next(Next::user("owner@example.com", "g-owner"));
    let at = b.click_google_at(&base, "/auth/google_oauth2?reauthenticate=user", "&prompt=select_account");
    let (to, flash) = b.follow(&base, &at.location.unwrap());
    assert_eq!((to.as_str(), notice(&flash)), ("/settings/profile", "Identity confirmed. You can now continue."));
    assert_eq!(delete_user(&mut b), "/");
    assert_eq!(server.users().len(), 1, "deleted; the co-owner remains");
}

#[test]
fn invite_only_signs_in_known_users_and_invited_emails_only() {
    let mock = Mock::start();
    let server = Server::start(&mock.base, "invite");
    let base = format!("http://127.0.0.1:{}", server.port);
    let admin = server.control().tx(|tx| control::insert_user(tx, "Admin", "admin@example.com", &control::base58(16), true)).unwrap();
    let account = server.control().tx(|tx| control::create_account(tx, "Acme", admin)).unwrap();
    server.control().tx(|tx| control::create_invitation(tx, account, admin, "invited@example.com", "member").map(|_| ())).unwrap();

    let mut b = Browser::default();
    mock.next(Next::user("stranger@example.com", "g-s"));
    let (to, flash) = b.sign_in_with_google(&base);
    assert_eq!((to.as_str(), alert(&flash)), ("/sign_in", "Sign-up is by invitation only. Ask an account owner to invite you."));
    assert_eq!(server.users().len(), 1);

    let mut b = Browser::default();
    mock.next(Next::user("admin@example.com", "g-admin"));
    assert_eq!(b.sign_in_with_google(&base).0, "/dashboard", "an existing user signs in");

    let mut b = Browser::default();
    mock.next(Next::user("invited@example.com", "g-inv"));
    assert_eq!(b.sign_in_with_google(&base).0, "/dashboard", "an invited email may create its user");
    assert_eq!(server.users().len(), 2);
}

#[test]
fn without_credentials_there_is_no_google_sign_in() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let data = std::env::temp_dir().join(format!("rw-google-off-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let child = Command::new(BIN)
        .arg("serve")
        .env("RAILWATCH_DATA_DIR", &data)
        .env("RAILWATCH_BIND", format!("127.0.0.1:{port}"))
        .env("RAILWATCH_SECRET_KEY_BASE", "00".repeat(64))
        .env("RAILWATCH_LOG", "warn")
        .env_remove("RAILWATCH_GOOGLE_CLIENT_ID")
        .env_remove("RAILWATCH_GOOGLE_CLIENT_SECRET")
        .env_remove("RAILWATCH_CONFIG")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server { child, port, data };
    let started = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(started.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(20));
    }
    let base = format!("http://127.0.0.1:{}", server.port);
    let mut b = Browser::default();
    assert_eq!(b.get(&format!("{base}/sign_in")).page()["props"]["google_oauth"], json!(false));
    let token = b.csrf(&base);
    assert_eq!(b.request(&format!("{base}/auth/google_oauth2"), "POST", Some(&format!("authenticity_token={}", enc(&token)))).status, 404);
    // Rails' "when Google OAuth is not configured (or the callback is hit directly)".
    let (to, flash) = b.follow(&base, &format!("{base}/auth/google_oauth2/callback?code=x&state=y"));
    assert_eq!((to.as_str(), alert(&flash)), ("/sign_in", "Google sign-in is not available right now."));
    let (to, flash) = b.follow(&base, &format!("{base}/auth/failure"));
    assert_eq!((to.as_str(), alert(&flash)), ("/sign_in", "Sign in with Google failed. Please try again."));
}
