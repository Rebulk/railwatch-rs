//! The watcher end to end against a real control DB and tenant: the
//! replay of a synthetic probe shaped like a real one (documentation
//! addresses, example domains; no real data), tick by tick as the server
//! runs it, plus incident bookkeeping (dedupe across ticks, monotonic
//! severity, close after quiet), alert routing and the digest.

use std::path::PathBuf;

use rusqlite::{Connection, params};
use serde_json::json;

use super::incidents::Target;
use super::policy::{NoHosting, Policy};
use super::watcher::{evaluate, targets};
use crate::control::{Control, create_account, create_environment, insert_application, insert_user, test_password};
use crate::time::quoted_date_micros;

const S: i64 = 1_000_000;
const MIN: i64 = 60 * S;
/// An arbitrary afternoon (UTC).
const T0: i64 = 1_793_458_200 * S;

const ATTACKER: &str = "203.0.113.66";
const OURS: &str = "192.0.2.10";
const BROWSER: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";

struct Fx {
    dir: PathBuf,
    c: Control,
    tenant: Connection,
    target: Target,
    integration: i64,
    /// The key the fixture's integration settings are encrypted with.
    key: String,
    n: i64,
}

impl Fx {
    fn new(name: &str) -> Fx {
        let dir = std::env::temp_dir().join(format!("rw-sec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _token) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let slug: String = c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0))).unwrap();
        let tenant = crate::tenants::open_tenant(&dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        let key = crate::control::base58(32);
        let cipher = crate::alerts::integration::SettingsCipher::new(key.as_bytes());
        let mut settings = serde_json::Map::new();
        settings.insert("url".into(), json!("https://hooks.example.com/railwatch"));
        settings.insert("secret".into(), json!("s3cret"));
        let integration = c.with(|x| crate::alerts::integration::insert(x, &cipher, account, "webhook", "relay", &settings)).unwrap();
        let p = Policy::parse("[apps.shop]\nenabled = true\n").unwrap();
        let (target, _) = c.with(|x| targets(x, &p)).unwrap().pop().unwrap();
        assert_eq!(target.environment_id, env);
        Fx { dir, c, tenant, target, integration, key, n: 0 }
    }

    #[allow(clippy::too_many_arguments)]
    fn req(&mut self, at: i64, ip: &str, ua: &str, method: &str, path: &str, status: i64, user: Option<&str>, halted: Option<&str>) {
        self.n += 1;
        let mut detail = json!({"ip": ip, "user_agent": ua, "path": path});
        if let Some(h) = halted {
            detail["halted_callback"] = json!(h);
        }
        self.tenant
            .execute(
                "INSERT INTO executions (execution_id, kind, name, method, status, occurred_at, duration, user_ref, detail) VALUES (?1, 'request', ?2, ?3, ?4, ?5, 1000, ?6, ?7)",
                params![format!("x{}", self.n), format!("{method} {path}"), method, status, quoted_date_micros(at), user, detail.to_string()],
            )
            .unwrap();
    }

    fn event(&mut self, at: i64, name: &str, payload: serde_json::Value) {
        self.tenant
            .execute(
                "INSERT INTO logs (level, message, occurred_at, context, tags) VALUES ('event', ?1, ?2, ?3, '[]')",
                params![name, quoted_date_micros(at), payload.to_string()],
            )
            .unwrap();
    }

    fn tick(&self, policy: &str, now: i64) -> super::watcher::Outcome {
        let p = Policy::parse(policy).unwrap();
        let app = p.for_app("shop");
        evaluate(&self.c, Some(&self.tenant), &self.target, &app, &p.watcher, &NoHosting, now, "https://rw.example.com").unwrap()
    }

    /// (actor, severity, status) of every incident, oldest first.
    fn incidents(&self) -> Vec<(String, String, String)> {
        self.c
            .with(|x| {
                x.prepare("SELECT actor, severity, status FROM security_incidents ORDER BY id")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect()
            })
            .unwrap()
    }

    /// (event, transition, severity) of every security alert, oldest first.
    fn alerts(&self) -> Vec<(String, String, String)> {
        self.c
            .with(|x| {
                x.prepare("SELECT event, json_extract(payload, '$.transition'), json_extract(payload, '$.severity') FROM alerts ORDER BY id")?
                    .query_map([], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, Option<String>>(2)?.unwrap_or_default())))?
                    .collect()
            })
            .unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Shadow mode to the fixture's webhook, our own address allowlisted, the
/// app's mobile client named.
fn policy(f: &Fx) -> String {
    format!(
        "[watcher]\nshadow_integration_id = {}\n[apps.shop]\nenabled = true\napp_ua_prefixes = [\"ShopMobile/\"]\nallow_cidrs = [\"{OURS}/32\"]\nallow_ua_prefixes = [\"shop-ci/\"]\n",
        f.integration
    )
}

/// The synthetic probe: what each step adds, at its minute after T0.
fn step(f: &mut Fx, n: usize) {
    let a = ATTACKER;
    match n {
        // 1. Recon: scripts on browser pages, dotfile probes, a spray of 404s.
        1 => {
            for i in 0..6 {
                f.req(T0 + i * 10 * S, a, if i % 2 == 0 { "curl/8.4.0" } else { "Mozilla/5.0" }, "GET", &format!("/p{i}"), 404, None, None);
            }
            for (i, p) in ["/.env", "/.env.local", "/.git/config", "/.git/HEAD", "/.aws/credentials"].iter().enumerate() {
                f.req(T0 + MIN + i as i64 * 5 * S, a, "curl/8.4.0", "GET", p, 404, None, None);
            }
            for i in 0..20 {
                f.req(T0 + 2 * MIN + i * 3 * S, a, "python-requests/2.31", "GET", &format!("/wp-{i}"), 404, None, None);
            }
            for i in 0..6 {
                f.req(T0 + 3 * MIN + i * S, a, "python-requests/2.31", "POST", "/api/mcp", 401, None, None);
            }
        }
        // 2. An OAuth client registered with a name that says what it is.
        2 => {
            f.event(
                T0 + 25 * MIN,
                "security.oauth_client.registered",
                json!({"ip": a, "user_agent": "python-requests/2.31", "client_id": "c-1", "client_name": "pentest-tool", "redirect_uris": ["https://example.invalid/cb"]}),
            );
        }
        // 3. A signup with the app's mobile user agent and a disposable domain.
        3 => {
            f.req(T0 + 59 * MIN, a, "curl/8.4.0", "POST", "/api/v1/registration", 422, None, None);
            f.event(
                T0 + 60 * MIN,
                "security.signup",
                json!({"ip": a, "user_agent": "ShopMobile/1.0", "user_id": 501, "email_domain": "mailinator.com", "via": "api"}),
            );
            f.req(T0 + 60 * MIN, a, "ShopMobile/1.0", "POST", "/api/v1/registration", 201, Some("501"), None);
        }
        // 4. The new account probes privileged pages within a minute.
        4 => {
            for (i, (p, h)) in [("/admin/console", "require_superadmin"), ("/admin/users", "set_current_organization"), ("/settings/billing", "require_admin")]
                .iter()
                .enumerate()
            {
                f.req(T0 + 61 * MIN + i as i64 * 10 * S, a, BROWSER, "GET", p, 302, Some("501"), Some(h));
            }
        }
        // 5. A second signup from the same address.
        5 => {
            f.event(T0 + 72 * MIN, "security.signup", json!({"ip": a, "user_agent": BROWSER, "user_id": 502, "email_domain": "example.com"}));
        }
        _ => unreachable!(),
    }
}

/// Our own agents and CI, all through the replay: they probe, script and
/// get 401s too, and must never raise anything.
fn own_traffic(f: &mut Fx, minute: i64) {
    for i in 0..8 {
        f.req(T0 + minute * MIN + i * S, OURS, "curl/8.4.0", "POST", "/api/mcp", 401, None, None);
        f.req(T0 + minute * MIN + i * S, OURS, "curl/8.4.0", "GET", &format!("/p{i}"), 404, None, None);
    }
    // A CI checker known only by its user agent, from another address.
    for i in 0..8 {
        f.req(T0 + minute * MIN + i * S, "198.51.100.200", "shop-ci/2.1", "GET", "/admin", 403, None, None);
    }
    f.event(T0 + minute * MIN, "security.signup", json!({"ip": OURS, "user_agent": "curl/8.4.0", "user_id": 7, "email_domain": "mailinator.com"}));
}

#[test]
fn replay_of_the_probe_opens_escalates_and_keeps_our_traffic_silent() {
    let mut f = Fx::new("replay");
    let p = policy(&f);
    let attacker = |f: &Fx| f.incidents().into_iter().filter(|(a, _, _)| a == ATTACKER).collect::<Vec<_>>();
    let open = |sev: &str| vec![(ATTACKER.to_string(), sev.to_string(), "open".to_string())];
    let mut log: Vec<String> = Vec::new();

    // Step 1 (recon), evaluated every 15 s over its five minutes: the
    // incident opens LOW at the scripts on browser pages (recorded, not
    // alerted) and escalates to HIGH at the dotfile probes (alerted once).
    step(&mut f, 1);
    own_traffic(&mut f, 1);
    for k in 0..20 {
        f.tick(&p, T0 + 15 * S + k * 15 * S);
    }
    assert_eq!(attacker(&f), open("high"), "recon: one incident, high at the secrets probe");
    let detectors: Vec<String> =
        f.c.with(|x| x.prepare("SELECT detector FROM security_incident_signals ORDER BY detector")?.query_map([], |r| r.get(0))?.collect()).unwrap();
    assert_eq!(detectors, ["auth_refusal_burst", "non_browser_on_web", "scanner_shape", "secrets_probe"]);
    assert_eq!(f.alerts(), [("security_incident".into(), "escalated".into(), "high".into())], "20 ticks over the same evidence: one alert");
    log.push("recon: opened LOW (non_browser_on_web, no alert), escalated HIGH (secrets_probe), alert".into());

    // Step 2: the OAuth client is a high signal on a high incident: no
    // escalation, no second alert.
    step(&mut f, 2);
    f.tick(&p, T0 + 25 * MIN + 15 * S);
    assert_eq!(attacker(&f), open("high"));
    assert_eq!(f.alerts().len(), 1);
    let identity: bool = f.c.with(|x| x.query_row("SELECT identity FROM security_incidents WHERE actor = ?", [ATTACKER], |r| r.get(0))).unwrap();
    assert!(identity, "the incident now touches an OAuth client");
    log.push("oauth client: HIGH (identity), no new alert".into());

    // Step 3: the signup is critical (disposable + spoofed client + an
    // address already flagged): escalation, alerted.
    step(&mut f, 3);
    f.tick(&p, T0 + 60 * MIN + 15 * S);
    assert_eq!(attacker(&f), open("critical"));
    assert_eq!(f.alerts().last().unwrap(), &("security_incident".into(), "escalated".into(), "critical".into()));
    let summary: String = f.c.with(|x| x.query_row("SELECT summary FROM security_incident_signals WHERE detector = 'risky_signup'", [], |r| r.get(0))).unwrap();
    assert!(summary.contains("disposable domain mailinator.com") && summary.contains("also sent curl") && summary.contains("already flagged"), "{summary}");
    log.push("signup: escalated CRITICAL (risky_signup), alert".into());

    // Step 4: the new account probes; recorded on the same incident, which
    // is already critical: nothing more is sent.
    step(&mut f, 4);
    f.tick(&p, T0 + 62 * MIN);
    assert_eq!(attacker(&f), open("critical"));
    let probe: String =
        f.c.with(|x| x.query_row("SELECT summary FROM security_incident_signals WHERE detector = 'new_account_probe'", [], |r| r.get(0))).unwrap();
    assert!(probe.contains("account 501, 1 min old") && probe.contains("require_superadmin"), "{probe}");
    assert_eq!(f.alerts().len(), 2);
    log.push("admin probes: new_account_probe on the CRITICAL incident".into());

    // Step 5: the second signup, after the first signup left the window:
    // the control DB remembers the account, so it is a repeat.
    step(&mut f, 5);
    f.tick(&p, T0 + 72 * MIN + 15 * S);
    let repeat: i64 = f.c.with(|x| x.query_row("SELECT count FROM security_incident_signals WHERE detector = 'repeat_signup'", [], |r| r.get(0))).unwrap();
    assert_eq!(repeat, 2, "signup #2 from the address");
    assert_eq!(attacker(&f), open("critical"), "still one incident: never a second for the same address while open");
    assert_eq!(f.alerts().len(), 2, "critical is already alerted");
    log.push("second signup: repeat_signup on the CRITICAL incident".into());

    // Quiet: an hour after the last evidence it closes, alerting the close.
    f.tick(&p, T0 + 72 * MIN + 61 * MIN);
    assert_eq!(attacker(&f), vec![(ATTACKER.to_string(), "critical".to_string(), "closed".to_string())]);
    assert_eq!(f.alerts().last().unwrap().1, "closed");
    log.push("quiet 60 min: closed, alert".into());

    // Our own traffic raised nothing at all.
    assert_eq!(f.incidents().len(), 1, "only the attacker: {:?}", f.incidents());
    let ours: i64 = f.c.with(|x| x.query_row("SELECT count(*) FROM security_accounts WHERE user_ref = '7'", [], |r| r.get(0))).unwrap();
    assert_eq!(ours, 0, "our own signup is not remembered either");
    // The timeline as recorded.
    let timeline: Vec<(String, String, Option<String>)> =
        f.c.with(|x| {
            x.prepare("SELECT kind, severity, detector FROM security_incident_events ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .unwrap();
    assert_eq!(
        timeline,
        [
            ("opened".into(), "low".into(), Some("non_browser_on_web".into())),
            ("escalated".into(), "high".into(), Some("secrets_probe".into())),
            ("escalated".into(), "critical".into(), Some("risky_signup".into())),
            ("closed".into(), "critical".into(), None),
        ]
    );
    eprintln!("replay timeline:\n  {}", log.join("\n  "));
}

#[test]
fn a_ua_allowlisted_sender_is_still_caught_probing_secrets() {
    let mut f = Fx::new("ua-allow");
    for (i, p) in ["/.env", "/.git/config", "/.aws/credentials"].iter().enumerate() {
        f.req(T0 + i as i64 * S, "198.51.100.200", "shop-ci/2.1", "GET", p, 404, None, None);
    }
    f.tick(&policy(&f), T0 + MIN);
    assert_eq!(f.incidents(), [("198.51.100.200".into(), "high".into(), "open".into())], "a user agent is whatever the sender writes");
}

#[test]
fn severity_never_goes_down_and_one_incident_per_actor_while_open() {
    let mut f = Fx::new("monotonic");
    let p = policy(&f);
    for i in 0..3 {
        f.req(T0 + i * S, ATTACKER, "curl/8", "GET", "/.env", 200, None, None);
    }
    f.tick(&p, T0 + MIN);
    assert_eq!(f.incidents(), [(ATTACKER.into(), "critical".into(), "open".into())], "a served secret is critical");
    // Later evidence is only low; the window has slid past the probes.
    for i in 0..4 {
        f.req(T0 + 20 * MIN + i * S, ATTACKER, "curl/8", "GET", "/about", 200, None, None);
    }
    f.tick(&p, T0 + 21 * MIN);
    assert_eq!(f.incidents(), [(ATTACKER.into(), "critical".into(), "open".into())], "still critical, still one");
    let (sev, last): (String, i64) =
        f.c.with(|x| x.query_row("SELECT severity, last_signal_at FROM security_incidents", [], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
    assert_eq!((sev.as_str(), last), ("critical", T0 + 20 * MIN + 3 * S), "the newer evidence keeps it open");
    // The signal row of the probe keeps its severity when re-seen at a lower one.
    let n: i64 = f.c.with(|x| x.query_row("SELECT count(*) FROM security_incident_signals", [], |r| r.get(0))).unwrap();
    assert_eq!(n, 2, "secrets_probe and non_browser_on_web, each once");
    // Closed, then new activity: a new incident, not the old one reopened.
    f.tick(&p, T0 + 90 * MIN);
    for i in 0..3 {
        f.req(T0 + 95 * MIN + i * S, ATTACKER, "curl/8", "GET", "/.git/HEAD", 404, None, None);
    }
    f.tick(&p, T0 + 96 * MIN);
    assert_eq!(f.incidents(), [(ATTACKER.into(), "critical".into(), "closed".into()), (ATTACKER.into(), "high".into(), "open".into())]);
}

#[test]
fn low_and_medium_incidents_go_to_the_daily_digest_not_an_alert() {
    let mut f = Fx::new("digest");
    let p = format!("{}\n", policy(&f)).replace("[watcher]\n", "[watcher]\ndigest_hour_utc = 23\n");
    for i in 0..25 {
        f.req(T0 + i * 10 * S, ATTACKER, BROWSER, "GET", &format!("/x{}", i % 12), 404, None, None);
    }
    f.tick(&p, T0 + 5 * MIN);
    assert_eq!(f.incidents(), [(ATTACKER.into(), "medium".into(), "open".into())]);
    assert!(f.alerts().is_empty(), "medium alerts nobody at once");
    // 23:00 UTC that day: one digest, once.
    let evening = T0 - T0.rem_euclid(86_400 * S) + 23 * 3600 * S;
    f.tick(&p, evening);
    f.tick(&p, evening + 15 * S);
    let a = f.alerts();
    assert_eq!(a.iter().map(|(e, _, _)| e.as_str()).collect::<Vec<_>>(), ["security_digest"], "{a:?}");
    let message: String = f.c.with(|x| x.query_row("SELECT json_extract(payload, '$.message') FROM alerts", [], |r| r.get(0))).unwrap();
    assert!(message.contains("MEDIUM 203.0.113.66 scanner_shape"), "{message}");
}

#[test]
fn shadow_alerts_go_only_to_the_shadow_integration_and_live_through_rules() {
    let mut f = Fx::new("routing");
    for i in 0..3 {
        f.req(T0 + i * S, ATTACKER, "curl/8", "GET", "/.env", 404, None, None);
    }
    // A live rule exists, but in shadow mode with no shadow integration
    // the incident is recorded and nothing is sent.
    f.c.with(|x| crate::web::integrations::insert_rule(x, f.target.application_id, f.integration, "security_incident", &serde_json::Map::new())).unwrap();
    f.tick("[apps.shop]\nenabled = true\n", T0 + MIN);
    assert_eq!(f.incidents().len(), 1);
    assert!(f.alerts().is_empty(), "shadow with no shadow integration: recorded only");
    // An integration of another account is refused as the shadow target.
    let other =
        f.c.with(|x| {
            x.execute("INSERT INTO accounts (name, slug, created_at, updated_at) VALUES ('Other', 'other', 't', 't')", [])?;
            let acc = x.last_insert_rowid();
            crate::alerts::integration::insert(
                x,
                &crate::alerts::integration::SettingsCipher::new(crate::control::base58(32).as_bytes()),
                acc,
                "webhook",
                "theirs",
                &serde_json::Map::new(),
            )
        })
        .unwrap();
    f.req(T0 + 2 * MIN, "198.51.100.9", "curl/8", "GET", "/.env", 404, None, None);
    f.req(T0 + 2 * MIN + S, "198.51.100.9", "curl/8", "GET", "/.git", 404, None, None);
    f.req(T0 + 2 * MIN + 2 * S, "198.51.100.9", "curl/8", "GET", "/.ssh", 404, None, None);
    f.tick(&format!("[watcher]\nshadow_integration_id = {other}\n[apps.shop]\nenabled = true\n"), T0 + 3 * MIN);
    assert!(f.alerts().is_empty(), "another account's integration never gets this account's incident");
    // Live: through the app's security_incident rule.
    f.req(T0 + 4 * MIN, "198.51.100.10", "curl/8", "GET", "/.env", 200, None, None);
    f.req(T0 + 4 * MIN + S, "198.51.100.10", "curl/8", "GET", "/.git", 404, None, None);
    f.req(T0 + 4 * MIN + 2 * S, "198.51.100.10", "curl/8", "GET", "/.ssh", 404, None, None);
    f.tick("[apps.shop]\nenabled = true\nmode = \"live\"\n", T0 + 5 * MIN);
    let rule: Option<i64> = f.c.with(|x| x.query_row("SELECT alert_rule_id FROM alerts", [], |r| r.get(0))).unwrap();
    assert!(rule.is_some(), "a live alert comes from the rule");
    assert_eq!(f.alerts(), [("security_incident".into(), "opened".into(), "critical".into())]);
}

/// The alert as the relay receives it: signed, with the incident inside.
#[test]
fn the_webhook_carries_the_incident_and_is_signed() {
    let mut f = Fx::new("webhook");
    for i in 0..3 {
        f.req(T0 + i * S, ATTACKER, "curl/8", "GET", "/.env\u{1b}[2J", 404, None, None);
    }
    f.tick(&policy(&f), T0 + MIN);
    f.c.configure_alerting(crate::alerts::integration::Alerting {
        cipher: crate::alerts::integration::SettingsCipher::new(f.key.as_bytes()),
        smtp: None,
        url: "https://rw.example.com".into(),
        deliver: true,
    });
    let conn = f.c.conn();
    let claimed = crate::alerts::outbox::claim_due(&conn, 10).unwrap().pop().unwrap();
    let ctx = crate::alerts::delivery::Context::load(&conn, f.c.alerting(), claimed).unwrap();
    drop(conn);
    assert!(ctx.settings_error.is_none(), "the webhook's secret decrypts: {:?}", ctx.settings_error);
    let body: serde_json::Value = serde_json::from_str(&crate::rjson::generate(&ctx.webhook_body()).unwrap()).unwrap();
    assert_eq!(body["event"], "security_incident");
    let p = &body["payload"];
    assert_eq!((p["transition"].as_str(), p["severity"].as_str(), p["mode"].as_str()), (Some("opened"), Some("high"), Some("shadow")));
    assert_eq!(p["incident"]["actor"], ATTACKER);
    assert_eq!(p["incident"]["signals"][0]["detector"], "secrets_probe");
    assert!(!body.to_string().contains('\u{1b}'), "client text is cleaned before it is stored");
    assert!(ctx.text().starts_with("Security incident: HIGH security incident opened in shop / production: 203.0.113.66"), "{}", ctx.text());
    assert_eq!(ctx.severity().as_deref(), Some("high"));
}

/// Production tenants are per-day directories read through a union reader:
/// the watcher reads them the same way, through the same opener as the loop.
#[test]
fn a_per_day_tenant_is_read_like_a_single_file() {
    let mut f = Fx::new("per-day");
    let now = crate::time::now_micros();
    for (i, p) in ["/.env", "/.git/config", "/.aws/credentials"].iter().enumerate() {
        f.req(now - MIN + i as i64 * S, ATTACKER, "curl/8", "GET", p, 404, None, None);
    }
    f.event(now - MIN, "security.login.failed", json!({"ip": ATTACKER}));
    let slug = f.target.environment_slug.clone();
    let file = f.dir.join("telemetry").join(format!("{slug}.sqlite3"));
    let days = f.dir.join("telemetry").join(&slug);
    crate::storage::export::split(&file, &days).unwrap();
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", file.display()));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let t = super::watcher::open_tenant(&f.dir.join("telemetry"), &slug, deadline).unwrap().expect("the per-day layout is found");
    assert!(crate::storage::reader::is_union(&t));
    let p = Policy::parse(&policy(&f)).unwrap();
    let o = evaluate(&f.c, Some(&t), &f.target, &p.for_app("shop"), &p.watcher, &NoHosting, now, "https://rw.example.com").unwrap();
    assert_eq!((o.requests, o.events), (3, 1));
    assert_eq!(f.incidents(), [(ATTACKER.into(), "high".into(), "open".into())]);
}
