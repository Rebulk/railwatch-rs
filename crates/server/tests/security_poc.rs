//! Adversarial security PoCs for the pre-open-source review (auth / authz /
//! web). These boot the REAL `web::router` over HTTP and send crafted
//! requests, so they exercise the live session middleware, CSRF guard,
//! redirect builder and token scoping — not a reimplementation.
//!
//! Run: `cargo test --test security_poc -- --nocapture`
//!
//! One test, sequential, because Config::load reads process-global env.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState, cookies};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn session_cookie(session_id: i64) -> String {
    let keys = cookies::Keys::new(SECRET.as_bytes());
    format!("{}={}", cookies::SESSION_TOKEN, keys.sign(cookies::SESSION_TOKEN, &session_id.to_string()))
}

/// Pulls one Set-Cookie `name=value` back out of a response's headers.
fn set_cookie(res: &reqwest::Response, name: &str) -> Option<String> {
    for v in res.headers().get_all(reqwest::header::SET_COOKIE) {
        let s = v.to_str().ok()?;
        if let Some(pair) = s.split(';').next()
            && let Some((k, val)) = pair.split_once('=')
            && k.trim() == name
        {
            return Some(val.trim().to_string());
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_pocs() {
    // ----- a server backed by a temp control DB -----------------------------
    let dir = std::env::temp_dir().join(format!("rw-secpoc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", dir.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1"); // http: secure_cookies off, simpler
        std::env::remove_var("RAILWATCH_CONFIG");
    }
    let cfg = Config::load().expect("config");
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));

    // Account A (user A, owner) and a separate Account B (user B, owner),
    // both with one random password (none is written into the source).
    let password = control::base58(16);
    let (user_a, account_a, issue_a, session_a, token_a) = {
        let ua = control.tx(|tx| control::insert_user(tx, "Ada", "ada@example.com", &password, true)).unwrap();
        let aa = control.tx(|tx| control::create_account(tx, "Acme", ua)).unwrap();
        let app = control.tx(|tx| control::insert_application(tx, aa, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _) = control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
        let ia = control
            .with(|c| {
                c.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users, first_seen_at, last_seen_at, created_at, updated_at, sample)
                     VALUES (?1, ?2, 1, 'Boom', 'exception', 'open', 'g1', 1, 0, '2026-10-14 03:00:00', '2026-10-14 03:00:00', '2026-10-14 03:00:00', '2026-10-14 03:00:00', '{}')",
                    rusqlite::params![app, env],
                )?;
                Ok(c.last_insert_rowid())
            })
            .unwrap();
        let sa = control.with(|c| control::create_session(c, ua, Some("ua"), Some("127.0.0.1"))).unwrap();
        // An API+read+write token for user A, scoped to all of A's accounts.
        let (_id, raw) =
            control.tx(|tx| control::issue_api_token(tx, ua, "t", &["api".into(), "read".into(), "write".into(), "mcp".into()], None, None, None)).unwrap();
        (ua, aa, ia, sa, raw)
    };

    // Account B, a DIFFERENT owner, with its own issue.
    let issue_b = {
        let ub = control.tx(|tx| control::insert_user(tx, "Bo", "bo@example.com", &password, true)).unwrap();
        let ab = control.tx(|tx| control::create_account(tx, "Beta", ub)).unwrap();
        let app = control.tx(|tx| control::insert_application(tx, ab, "Else", "else", "ELSE", None, "main")).unwrap();
        let (env, _) = control.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
        control
            .with(|c| {
                c.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users, first_seen_at, last_seen_at, created_at, updated_at, sample)
                     VALUES (?1, ?2, 1, 'Secret', 'exception', 'open', 'g2', 1, 0, '2026-10-14 03:00:00', '2026-10-14 03:00:00', '2026-10-14 03:00:00', '2026-10-14 03:00:00', '{}')",
                    rusqlite::params![app, env],
                )?;
                Ok(c.last_insert_rowid())
            })
            .unwrap()
    };
    let _ = (user_a, account_a);

    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    let app = web::router(state).into_make_service_with_connect_info::<SocketAddr>();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let base = format!("http://{addr}");
    let origin = base.clone();

    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let sess = session_cookie(session_a);

    // ----- obtain a valid CSRF token for A's session -------------------------
    let r = http.get(format!("{base}/settings/profile")).header("Cookie", &sess).send().await.unwrap();
    let xsrf = set_cookie(&r, "XSRF-TOKEN").expect("XSRF-TOKEN issued");
    let rw_session = set_cookie(&r, cookies::SESSION).expect("_railwatch_session issued");
    let cookies_hdr = format!("{sess}; {}={}", cookies::SESSION, rw_session);

    // ===== PoC 1: open redirect via Referer-driven back_or (fixed) ===========
    // /issues/bulk redirects to `back_or("/issues")`, which reflects the
    // Referer's path. A Referer whose path was `//evil.example` yielded a
    // protocol-relative Location -> off-site redirect. Now the fallback.
    let evil = http
        .post(format!("{base}/issues/bulk"))
        .header("Cookie", &cookies_hdr)
        .header("Origin", &origin)
        .header("X-XSRF-TOKEN", &xsrf)
        .header("Referer", format!("{origin}//evil.example/x"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let loc = evil.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    println!("[PoC1] status={} Location={loc:?}", evil.status());
    assert_eq!(loc, "/issues", "a protocol-relative Referer path falls back instead of redirecting off-site");
    // The backslash spelling, which browsers also read as protocol-relative.
    let evil2 = http
        .post(format!("{base}/issues/bulk"))
        .header("Cookie", &cookies_hdr)
        .header("Origin", &origin)
        .header("X-XSRF-TOKEN", &xsrf)
        .header("Referer", format!("{origin}/\\evil.example/x"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let loc = evil2.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert_eq!(loc, "/issues", "a /\\ Referer path falls back too");

    // Control: a normal same-origin Referer path stays a safe local path.
    let safe = http
        .post(format!("{base}/issues/bulk"))
        .header("Cookie", &cookies_hdr)
        .header("Origin", &origin)
        .header("X-XSRF-TOKEN", &xsrf)
        .header("Referer", format!("{origin}/issues?status=open"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let loc2 = safe.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    println!("[control] normal Referer -> Location={loc2:?}");
    assert!(loc2.starts_with("/issues"), "a normal referer stays on-site");

    // ===== Soundness control: CSRF is enforced on mutations ==================
    let no_token = http
        .post(format!("{base}/issues/bulk"))
        .header("Cookie", &cookies_hdr)
        .header("Origin", &origin)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    println!("[csrf] no X-XSRF-TOKEN -> {}", no_token.status());
    assert_eq!(no_token.status(), 422, "CSRF: a mutation without the session token is refused");

    // Cross-site Origin is refused even with a (leaked) token.
    let cross = http
        .post(format!("{base}/issues/bulk"))
        .header("Cookie", &cookies_hdr)
        .header("Origin", "https://evil.example")
        .header("X-XSRF-TOKEN", &xsrf)
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    println!("[csrf] cross-site Origin -> {}", cross.status());
    assert_eq!(cross.status(), 422, "CSRF: a cross-site Origin is refused");

    // ===== Soundness control: tenant isolation on /api/v1 ====================
    // User A's token must not read Account B's issue.
    let b = http.get(format!("{base}/api/v1/issues/{issue_b}")).header("Authorization", format!("Bearer {token_a}")).send().await.unwrap();
    println!("[isolation] A's token reads B's issue {issue_b} -> {}", b.status());
    assert_eq!(b.status(), 404, "tenant isolation: cross-account issue is a 404");

    // A's own issue is readable (positive control).
    let a = http.get(format!("{base}/api/v1/issues/{issue_a}")).header("Authorization", format!("Bearer {token_a}")).send().await.unwrap();
    println!("[isolation] A's token reads A's own issue {issue_a} -> {}", a.status());
    assert_eq!(a.status(), 200, "A can read its own issue");

    // Unauthenticated API is 401.
    let anon = http.get(format!("{base}/api/v1/issues/{issue_a}")).send().await.unwrap();
    assert_eq!(anon.status(), 401, "no token -> 401");

    // ===== Sign-in throttles: per email without lockout, per IP refusing =====
    // Failures are counted per email (10 / 5 min), but a full email bucket
    // never refuses: whoever knows ada@'s address must not be able to keep
    // Ada out (issue #4). Past the bucket a wrong password gets exactly the
    // usual answer, after the same bcrypt work, so nothing says the bucket
    // is full; the right password signs in. Only the per-IP limit (30 /
    // 5 min) refuses outright.
    let anon = http.get(format!("{base}/sign_in")).send().await.unwrap();
    let anon_xsrf = set_cookie(&anon, "XSRF-TOKEN").expect("XSRF-TOKEN for an anonymous session");
    let anon_cookies = format!("{}={}", cookies::SESSION, set_cookie(&anon, cookies::SESSION).expect("anonymous session cookie"));
    // (Location, flash alert, seconds): the redirect is followed with the
    // session cookie it set, which carries the flash.
    let sign_in = |email: &str, password: &str| {
        let req = http
            .post(format!("{base}/sign_in"))
            .header("Cookie", &anon_cookies)
            .header("Origin", &origin)
            .header("X-XSRF-TOKEN", &anon_xsrf)
            .header("Content-Type", "application/json")
            .body(format!(r#"{{"email":"{email}","password":"{password}"}}"#));
        let (http, base) = (http.clone(), base.clone());
        async move {
            let t = std::time::Instant::now();
            let r = req.send().await.unwrap();
            let secs = t.elapsed().as_secs_f64();
            let to = r.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let mut alert = String::new();
            if to == "/sign_in" {
                let cookie = format!("{}={}", cookies::SESSION, set_cookie(&r, cookies::SESSION).expect("the flash rides in the session cookie"));
                let body = http.get(format!("{base}/sign_in")).header("Cookie", cookie).send().await.unwrap().text().await.unwrap();
                let start = body.find("type=\"application/json\">").map(|i| i + 24).expect("an Inertia page");
                let end = body[start..].find("</script>").unwrap() + start;
                let page: serde_json::Value = serde_json::from_str(&body[start..end].replace("\\/", "/")).unwrap();
                alert = page["props"]["flash"]["alert"].as_str().unwrap_or("").to_string();
            }
            (to, alert, secs)
        }
    };
    let incorrect = "That email or password is incorrect";
    let mut before = Vec::new();
    for i in 0..10 {
        let (to, alert, secs) = sign_in(if i % 2 == 0 { "ADA@example.com" } else { "ada@example.com" }, "wrong").await;
        assert_eq!((to.as_str(), alert.as_str()), ("/sign_in", incorrect));
        before.push(secs);
    }
    // ada@'s bucket is full. A wrong password: the same answer, the same work.
    let mut after = Vec::new();
    for _ in 0..3 {
        let (to, alert, secs) = sign_in("ada@example.com", "still wrong").await;
        assert_eq!((to.as_str(), alert.as_str()), ("/sign_in", incorrect), "a full bucket looks like a wrong password");
        after.push(secs);
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (b, a) = (median(&mut before), median(&mut after));
    println!("[sign-in] wrong password: median {:.0} ms before the email bucket filled, {:.0} ms after", b * 1e3, a * 1e3);
    assert!(a > b * 0.5, "bcrypt still runs once the bucket is full, so timing does not tell ({a:.3} s vs {b:.3} s)");
    let (to, _, _) = sign_in("ada@example.com", &password).await;
    assert_eq!(to, "/dashboard", "the owner's right password signs in although ada@'s failure bucket is full");
    let (to, _, _) = sign_in("bo@example.com", &password).await;
    assert_eq!(to, "/dashboard", "another email from the same IP signs in");
    // Brute force from one IP: 15 attempts so far; the per-IP limit (30)
    // refuses from the 31st on, even with the right password.
    for _ in 15..30 {
        let (to, alert, _) = sign_in("ada@example.com", "guess").await;
        assert_eq!((to.as_str(), alert.as_str()), ("/sign_in", incorrect));
    }
    let too_many = "Too many sign-in attempts. Try again in a few minutes.";
    let (to, alert, _) = sign_in("ada@example.com", "guess").await;
    assert_eq!((to.as_str(), alert.as_str()), ("/sign_in", too_many), "the 31st attempt from one IP is refused");
    let (to, alert, _) = sign_in("ada@example.com", &password).await;
    assert_eq!((to.as_str(), alert.as_str()), ("/sign_in", too_many), "the per-IP limit refuses even the right password");

    std::fs::remove_dir_all(&dir).ok();
    println!("\nAll PoCs ran: open redirect FIXED; CSRF + tenant isolation SOUND.");
}
