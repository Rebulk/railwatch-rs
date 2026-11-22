//! Audit 2026-11-22 (rw-h-ssrf, alert outbox): every account shares one
//! alert outbox. A pass claimed the oldest 200 due alerts across all
//! accounts, alert rules could be duplicated without limit (each copy makes
//! its own alert for one event), and an alert with no attempt an hour after
//! it was raised was never claimed again and stayed `pending` with nothing
//! said. So one account's volume could delay another account's alerts and
//! then drop them silently.
//!
//! Control database and the real web router on loopback; no destination is
//! contacted. Dummy data only.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::alerts::{self, outbox, rules};
use railwatch_server::config::Config;
use railwatch_server::control::{self, Control};
use railwatch_server::followups;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::{self, WebState, cookies};
use rusqlite::params;

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rw-outbox-shares-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An account with one application, environment and webhook integration:
/// (account, application, environment, integration, owner).
fn account(c: &Control, name: &str, slug: &str) -> (i64, i64, i64, i64, i64) {
    let owner = c.tx(|tx| control::insert_user(tx, name, &format!("owner@{slug}.test"), &control::base58(16), true)).unwrap();
    let account = c.tx(|tx| control::create_account(tx, name, owner)).unwrap();
    let app = c.tx(|tx| control::insert_application(tx, account, "Shop", slug, &slug.to_uppercase(), None, "main")).unwrap();
    let (env, _) = c.tx(|tx| control::create_environment(tx, app, "production")).unwrap();
    let integration = c
        .with(|x| {
            x.execute(
                "INSERT INTO integrations (account_id, created_at, enabled, kind, name, settings, updated_at)
                 VALUES (?, '2026-01-01', 1, 'webhook', 'Hook', '{\"url\":\"https://hooks.example.com/h\"}', '2026-01-01')",
                [account],
            )?;
            Ok(x.last_insert_rowid())
        })
        .unwrap();
    (account, app, env, integration, owner)
}

fn quota_payload(env: i64) -> railwatch_server::rjson::Value {
    railwatch_server::rhash! { "message" => "over quota", "environment_id" => env }
}

/// Account A has far more due alerts than one pass claims, all due before
/// account B's one alert. B's alert must still be in the first batch.
#[test]
fn one_accounts_backlog_does_not_push_another_accounts_alert_out_of_the_batch() {
    let d = dir("claim");
    let control = Control::open(&d.join("control.sqlite3")).unwrap();
    let (_, _, env_a, integ_a, _) = account(&control, "Acme", "acme");
    let (_, _, env_b, integ_b, _) = account(&control, "Bobco", "bobco");
    let c = control.conn();
    for _ in 0..(outbox::BATCH_SIZE + 100) {
        outbox::create(&c, None, integ_a, "quota", None, &quota_payload(env_a), "pending", None).unwrap();
    }
    c.execute("UPDATE alerts SET next_delivery_at = datetime('now', '-1 minute') WHERE integration_id = ?", [integ_a]).unwrap();
    let b = outbox::create(&c, None, integ_b, "quota", None, &quota_payload(env_b), "pending", None).unwrap();

    let batch = outbox::claim_due(&c, outbox::BATCH_SIZE).unwrap();
    assert_eq!(batch.len() as i64, outbox::BATCH_SIZE, "control: a full batch was claimed");
    assert!(batch.iter().any(|a| a.integration_id == integ_a), "control: account A's alerts are claimed too");
    assert!(batch.iter().any(|a| a.id == b), "account B's alert is not in the first batch: it waits behind account A's backlog");
    drop(c);
    let _ = std::fs::remove_dir_all(&d);
}

/// An alert that never had an attempt within the hour is no longer claimed
/// (Rails' reconcile window). It must not then sit `pending` forever with
/// nothing said: the delivery pass marks it failed, with the reason.
#[tokio::test(flavor = "multi_thread")]
async fn an_alert_the_outbox_never_reached_within_the_hour_is_marked_failed_not_left_pending() {
    let d = dir("aged");
    let control = Arc::new(Control::open(&d.join("control.sqlite3")).unwrap());
    let (_, _, env, integ, _) = account(&control, "Acme", "acme");
    let id = {
        let c = control.conn();
        let id = outbox::create(&c, None, integ, "quota", None, &quota_payload(env), "pending", None).unwrap();
        c.execute("UPDATE alerts SET created_at = datetime('now', '-2 hours') WHERE id = ?", [id]).unwrap();
        id
    };
    assert_eq!(alerts::deliver_pass(&control, &d.join("telemetry")).await.unwrap(), 0, "control: past the window it is not claimed");
    let (status, error): (String, Option<String>) =
        control.with(|x| x.query_row("SELECT status, error FROM alerts WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
    assert_eq!(status, "failed", "an alert the outbox never reached stays pending forever, with no error");
    assert!(error.as_deref().is_some_and(|e| e.contains("hour")), "the alert says why it was not sent: {error:?}");
    let _ = std::fs::remove_dir_all(&d);
}

fn session_cookie(c: &Control, session_id: i64) -> String {
    let keys = cookies::Keys::new(SECRET.as_bytes());
    let value = c.with(|x| control::session_cookie(x, session_id)).unwrap().expect("the session exists");
    format!("{}={}", cookies::SESSION_TOKEN, keys.sign(cookies::SESSION_TOKEN, &value))
}

fn set_cookie(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers().get_all(reqwest::header::SET_COOKIE).iter().find_map(|v| {
        let pair = v.to_str().ok()?.split(';').next()?;
        let (k, val) = pair.split_once('=')?;
        (k.trim() == name).then(|| val.trim().to_string())
    })
}

/// The same rule added five times through the settings form: one event
/// must still make one alert, not one per copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_alert_rule_is_refused_so_one_event_makes_one_alert() {
    let d = dir("rules");
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", d.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::remove_var("RAILWATCH_CONFIG");
    }
    let cfg = Config::load().expect("config");
    let control = Arc::new(Control::open(&cfg.control_path()).expect("control"));
    let (_, app, env, integ, owner) = account(&control, "Acme", "acme");
    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    let state = Arc::new(WebState::new(control.clone(), cfg.clone(), tenants, web::cable::Hub::new()).expect("web state"));
    let router = web::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

    // An imported rule keeps the Rails text as it was stored, formatted
    // differently from what the form writes ("{}"): still the same rule.
    control
        .with(|x| {
            let now = control::now_db();
            x.execute(
                "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?, ?, 'quota', '{ }', ?, ?)",
                params![app, now, integ, now],
            )
        })
        .unwrap();

    let sid = control.with(|x| control::create_session(x, owner, Some("ua"), Some("127.0.0.1"))).unwrap();
    let sess = session_cookie(&control, sid);
    let r = http.get(format!("{base}/settings/integrations")).header("Cookie", &sess).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let xsrf = set_cookie(&r, "XSRF-TOKEN").unwrap();
    let cookie = format!("{sess}; {}={}", cookies::SESSION, set_cookie(&r, cookies::SESSION).unwrap());
    for _ in 0..5 {
        let r = http
            .post(format!("{base}/settings/alert_rules"))
            .header("Cookie", &cookie)
            .header("Origin", &base)
            .header("X-XSRF-TOKEN", &xsrf)
            .json(&serde_json::json!({"application_id": app, "integration_id": integ, "event": "quota"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302);
    }
    let rules_n: i64 = control.with(|x| x.query_row("SELECT COUNT(*) FROM alert_rules WHERE application_id = ?", [app], |r| r.get(0))).unwrap();
    assert!(rules_n >= 1, "control: the imported rule is there");

    let fired = {
        let c = control.conn();
        rules::fire_issueless(&c, app, "quota", &alerts::as_hash(quota_payload(env))).unwrap()
    };
    let alerts_n: i64 = control.with(|x| x.query_row("SELECT COUNT(*) FROM alerts WHERE integration_id = ?", params![integ], |r| r.get(0))).unwrap();
    assert!(!fired.is_empty(), "control: the event fired the rule");
    assert_eq!(
        (rules_n, alerts_n),
        (1, 1),
        "an imported rule plus five identical submissions: {rules_n} rules, one event made {alerts_n} alerts to the same destination"
    );
    let _ = std::fs::remove_dir_all(&d);
}
