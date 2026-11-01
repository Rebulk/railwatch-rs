use std::path::PathBuf;
use std::time::Duration;

use super::probe::{self, Failure, Outcome, Policy};
use super::*;
use crate::control::{Control, create_account, create_environment, insert_application, insert_user, test_password};

const T0: i64 = 1_800_000_000_000_000; // 2027-01-15 08:00:00 UTC
const MIN: i64 = 60_000_000;

struct Fixture {
    dir: PathBuf,
    control: Control,
    env: i64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A control DB with one environment and a webhook rule for both monitor events.
fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("rw-uptime-{name}-{}", crate::control::base58(8)));
    std::fs::create_dir_all(&dir).unwrap();
    let control = Control::open(&dir.join("control.sqlite3")).unwrap();
    let user = control.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
    let account = control.tx(|tx| create_account(tx, "Acme", user)).unwrap();
    let app = control.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
    let (env, _) = control.tx(|tx| create_environment(tx, app, "production")).unwrap();
    control
        .with(|x| {
            x.execute_batch(&format!(
                "INSERT INTO integrations (id, account_id, kind, name, settings, created_at, updated_at) VALUES (1, {account}, 'webhook', 'Hook', '{{}}', 't', 't');
                 INSERT INTO alert_rules (application_id, integration_id, event, filters, created_at, updated_at) VALUES
                   ({app}, 1, 'monitor_down', '{{}}', 't', 't'), ({app}, 1, 'monitor_recovered', '{{}}', 't', 't');"
            ))
        })
        .unwrap();
    Fixture { dir, control, env }
}

fn add(f: &Fixture, m: MonitorInput) -> i64 {
    f.control.with(|x| insert(x, f.env, &m)).unwrap().unwrap()
}

fn input(name: &str) -> MonitorInput {
    MonitorInput { name: name.into(), url: "https://shop.example.com/up".into(), ..Default::default() }
}

fn ok(ms: i64) -> Outcome {
    Outcome { status_code: Some(200), latency_ms: ms, failure: None, detail: None }
}

fn fail(f: Failure) -> Outcome {
    Outcome { status_code: (f == Failure::Status).then_some(503), latency_ms: 40, failure: Some(f), detail: Some("status 503, expected 2xx or 3xx".into()) }
}

fn alerts(c: &Control) -> Vec<(String, String)> {
    c.with(|x| x.prepare("SELECT event, payload FROM alerts ORDER BY id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()).unwrap()
}

fn status(c: &Control, id: i64) -> String {
    c.with(|x| x.query_row("SELECT status FROM uptime_monitors WHERE id = ?", [id], |r| r.get(0))).unwrap()
}

#[test]
fn one_failed_check_is_a_blip_two_in_a_row_are_down_and_one_success_recovers() {
    let f = fixture("states");
    let id = add(&f, input("Shop"));
    let rec = |at: i64, o: Outcome| f.control.with(|x| record(x, id, at, &o)).unwrap();

    assert_eq!(rec(T0, ok(80)), None);
    assert_eq!(status(&f.control, id), "up");
    // A blip: one failure, then success. No incident, no alert.
    assert_eq!(rec(T0 + MIN, fail(Failure::Timeout)), None);
    assert_eq!(status(&f.control, id), "up", "one failure does not mark a monitor down");
    assert_eq!(rec(T0 + 2 * MIN, ok(90)), None);
    assert!(alerts(&f.control).is_empty());
    // Two in a row: down, from the first failed check.
    assert_eq!(rec(T0 + 3 * MIN, fail(Failure::Status)), None);
    let Some(Transition::Down { incident_id, .. }) = rec(T0 + 4 * MIN, fail(Failure::Status)) else { panic!("second failure in a row is down") };
    assert_eq!(status(&f.control, id), "down");
    let (started, cause): (i64, String) =
        f.control.with(|x| x.query_row("SELECT started_at, cause FROM uptime_incidents WHERE id = ?", [incident_id], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
    assert_eq!((started, cause.as_str()), (T0 + 3 * MIN, "status"), "the incident starts at the first failed check");
    // Still failing: no second alert, no second incident.
    assert_eq!(rec(T0 + 5 * MIN, fail(Failure::Connect)), None);
    // One success: up, incident closed.
    assert!(matches!(rec(T0 + 6 * MIN, ok(70)), Some(Transition::Recovered { incident_id: Some(i), .. }) if i == incident_id));
    assert_eq!(status(&f.control, id), "up");
    let ended: Option<i64> = f.control.with(|x| x.query_row("SELECT ended_at FROM uptime_incidents WHERE id = ?", [incident_id], |r| r.get(0))).unwrap();
    assert_eq!(ended, Some(T0 + 6 * MIN));

    let a = alerts(&f.control);
    assert_eq!(a.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(), ["monitor_down", "monitor_recovered"]);
    let down: serde_json::Value = serde_json::from_str(&a[0].1).unwrap();
    assert_eq!(down["monitor"], "Shop");
    assert_eq!(down["url"], "https://shop.example.com/up");
    assert_eq!(down["cause"], "status");
    assert_eq!(down["status_code"], 503);
    assert_eq!(down["since"], crate::time::iso8601((T0 + 3 * MIN) / 1_000_000));
    assert_eq!(down["environment_id"], f.env, "rules filtered by environment match it");
    let up: serde_json::Value = serde_json::from_str(&a[1].1).unwrap();
    assert_eq!(up["down_for_seconds"], 180);
    assert!(up["message"].as_str().unwrap().contains("up again after 3 min down"), "{}", up["message"]);
}

#[test]
fn a_restart_neither_forgets_an_outage_nor_alerts_it_again() {
    let f = fixture("restart");
    let id = add(&f, input("Shop"));
    f.control.with(|x| record(x, id, T0, &fail(Failure::Connect))).unwrap();
    // Restart between the two failures: the count survives.
    let path = f.dir.join("control.sqlite3");
    let c2 = Control::open(&path).unwrap();
    assert!(matches!(c2.with(|x| record(x, id, T0 + MIN, &fail(Failure::Connect))).unwrap(), Some(Transition::Down { .. })));
    drop(c2);
    // Restart while down: still down, further failures do not alert again.
    let c3 = Control::open(&path).unwrap();
    assert_eq!(status(&c3, id), "down");
    for i in 2..6 {
        assert_eq!(c3.with(|x| record(x, id, T0 + i * MIN, &fail(Failure::Connect))).unwrap(), None);
    }
    assert!(matches!(c3.with(|x| record(x, id, T0 + 6 * MIN, &ok(50))).unwrap(), Some(Transition::Recovered { .. })));
    let events: Vec<String> = alerts(&c3).into_iter().map(|(e, _)| e).collect();
    assert_eq!(events, ["monitor_down", "monitor_recovered"], "one alert each way across two restarts");
    let incidents: i64 = c3.with(|x| x.query_row("SELECT COUNT(*) FROM uptime_incidents", [], |r| r.get(0))).unwrap();
    assert_eq!(incidents, 1);
}

#[test]
fn a_disabled_monitor_records_nothing_and_closes_its_incident_without_an_alert() {
    let f = fixture("disable");
    let id = add(&f, input("Shop"));
    for i in 0..2 {
        f.control.with(|x| record(x, id, T0 + i * MIN, &fail(Failure::Dns))).unwrap();
    }
    let off = MonitorInput { enabled: false, ..input("Shop") };
    f.control.with(|x| update(x, id, &off, T0 + 3 * MIN)).unwrap();
    assert_eq!(status(&f.control, id), "paused");
    assert_eq!(f.control.with(|x| record(x, id, T0 + 4 * MIN, &ok(10))).unwrap(), None, "a check finishing after disable is dropped");
    let open: i64 = f.control.with(|x| x.query_row("SELECT COUNT(*) FROM uptime_incidents WHERE ended_at IS NULL", [], |r| r.get(0))).unwrap();
    assert_eq!(open, 0);
    assert_eq!(alerts(&f.control).len(), 1, "only the down alert");
    f.control.with(|x| update(x, id, &input("Shop"), T0 + 5 * MIN)).unwrap();
    assert_eq!(status(&f.control, id), "pending");
}

#[test]
fn uptime_percentages_and_daily_rollups_add_up() {
    let f = fixture("rollup");
    let id = add(&f, input("Shop"));
    let day = 86_400 * 1_000_000;
    let today = day_start(T0);
    // Yesterday: 100 checks, 5 failed; latencies 1..=95 ms on the successes.
    for i in 0..100 {
        let o = if i < 95 { ok(i + 1) } else { fail(Failure::Timeout) };
        f.control.with(|x| record(x, id, today - day + i * MIN, &o)).unwrap();
    }
    // Today: 10 checks, all fine at 200 ms.
    for i in 0..10 {
        f.control.with(|x| record(x, id, today + i * MIN, &ok(200))).unwrap();
    }
    let now = today + 12 * MIN;
    let m = f.control.with(|x| maintain(x, now)).unwrap();
    assert_eq!(m.days_rolled, 2);
    let days = f.control.with(|x| daily(x, id, now, 2)).unwrap();
    assert_eq!(days[0]["checks"], 100);
    assert_eq!(days[0]["failures"], 5);
    assert_eq!(days[0]["uptime"], 95.0);
    assert_eq!((days[0]["p50_ms"].as_i64(), days[0]["p95_ms"].as_i64()), (Some(48), Some(91)), "percentiles of the successful checks only");
    assert_eq!(days[1]["uptime"], 100.0);
    // 7 days: 105 of 110 checks.
    let w = f.control.with(|x| uptime_windows(x, id, now)).unwrap();
    assert_eq!(w["7d"], 95.454, "105/110 rounded down to 3 places");
    assert_eq!(w["90d"], w["7d"]);
    assert_eq!(uptime_pct(0, 0), None, "no checks is no number, not 100%");
    assert_eq!(uptime_pct(3, 3), Some(0.0));
    // 99.9996% never displays as 100.
    assert_eq!(pct_value(uptime_pct(1_000_000, 4)), json!(99.999));
}

#[test]
fn retention_keeps_7_days_of_checks_and_90_of_rollups() {
    let f = fixture("retention");
    let id = add(&f, input("Shop"));
    let day = 86_400 * 1_000_000;
    let today = day_start(T0);
    // Each day: down (two failures) and up again, one incident.
    for d in [0, 7, 8, 89, 90] {
        f.control.with(|x| record(x, id, today - d * day + MIN, &fail(Failure::Dns))).unwrap();
        f.control.with(|x| record(x, id, today - d * day + 2 * MIN, &fail(Failure::Dns))).unwrap();
        f.control.with(|x| record(x, id, today - d * day + 3 * MIN, &ok(5))).unwrap();
    }
    let m = f.control.with(|x| maintain(x, today + 4 * MIN)).unwrap();
    assert_eq!(m.checks_deleted, 3 * 3, "checks of days 8, 89 and 90 go; day 7 stays");
    assert_eq!(m.days_deleted, 1, "only day 90 is past 90 days of rollups");
    assert_eq!(m.incidents_deleted, 1, "the incident closed over 90 days ago goes, day 89's stays");
    let oldest: String = f.control.with(|x| x.query_row("SELECT MIN(day) FROM uptime_daily", [], |r| r.get(0))).unwrap();
    assert_eq!(oldest, day_of(today - 89 * day));
}

#[test]
fn the_interval_minimum_and_other_limits_are_enforced() {
    let errs = |m: MonitorInput| validate(&m, Policy::default()).into_iter().map(|(k, _)| k).collect::<Vec<_>>();
    assert_eq!(errs(MonitorInput { interval_seconds: 29, ..input("a") }), ["interval_seconds"]);
    assert!(errs(MonitorInput { interval_seconds: 30, timeout_seconds: 10, ..input("a") }).is_empty(), "30 s is allowed");
    assert_eq!(errs(MonitorInput { interval_seconds: 3601, ..input("a") }), ["interval_seconds"]);
    assert_eq!(errs(MonitorInput { interval_seconds: 30, timeout_seconds: 30, ..input("a") }), ["timeout_seconds"]);
    assert_eq!(errs(MonitorInput { method: "POST".into(), ..input("a") }), ["method"]);
    assert_eq!(errs(MonitorInput { method: "HEAD".into(), keyword: Some("ok".into()), ..input("a") }), ["keyword"]);
    assert_eq!(errs(MonitorInput { expected_status: Some(42), ..input("a") }), ["expected_status"]);
    assert_eq!(errs(MonitorInput { url: "http://10.0.0.8/up".into(), ..input("a") }), ["url"], "a private address is refused at save");
    assert_eq!(errs(input(" ")), ["name"]);
    let f = fixture("max");
    for i in 0..MAX_MONITORS_PER_ENVIRONMENT {
        add(&f, input(&format!("m{i}")));
    }
    assert!(f.control.with(|x| insert(x, f.env, &input("one too many"))).unwrap().is_err());
}

#[test]
fn the_public_view_shows_only_public_monitors_and_public_fields() {
    let f = fixture("public");
    let shown = add(&f, MonitorInput { public: true, public_name: Some("Storefront".into()), ..input("shop-web internal") });
    let with_url = add(&f, MonitorInput { public: true, public_url: true, url: "https://api.example.com/health".into(), ..input("API") });
    let hidden = add(&f, MonitorInput { url: "https://admin.example.com/secret".into(), ..input("Admin backend") });
    for (i, id) in [shown, with_url, hidden].into_iter().enumerate() {
        f.control.with(|x| record(x, id, T0 + i as i64, &fail(Failure::Status))).unwrap();
        f.control.with(|x| record(x, id, T0 + MIN + i as i64, &fail(Failure::Status))).unwrap();
    }
    f.control.with(|x| record(x, with_url, T0 + 3 * MIN, &ok(10))).unwrap();
    f.control.with(|x| save_status_page(x, f.env, "acme-status", "Acme status", false)).unwrap().unwrap();
    assert!(f.control.with(|x| public_status(x, "acme-status", T0 + 4 * MIN)).unwrap().is_none(), "a disabled page is not served");
    assert!(f.control.with(|x| public_status(x, "nope", T0)).unwrap().is_none());
    f.control.with(|x| save_status_page(x, f.env, "acme-status", "Acme status", true)).unwrap().unwrap();
    let v = f.control.with(|x| public_status(x, "acme-status", T0 + 4 * MIN)).unwrap().unwrap();
    let text = v.to_string();
    assert_eq!(v["components"].as_array().unwrap().len(), 2);
    assert_eq!(v["components"][0]["name"], "API");
    assert_eq!(v["components"][0]["url"], "https://api.example.com/health", "shown only because public_url is on");
    assert_eq!(v["components"][1]["name"], "Storefront", "the public name, not the internal one");
    assert!(v["components"][1].get("url").is_none());
    assert_eq!(v["status"], "degraded", "one public monitor down, one up");
    assert_eq!(v["incidents"].as_array().unwrap().len(), 2, "the hidden monitor's incident is not listed");
    assert_eq!(v["components"][0]["days"].as_array().unwrap().len(), 90);
    for leak in
        ["Admin backend", "admin.example.com", "shop-web internal", "shop.example.com", "status 503", "\"cause\"", "\"id\"", "monitor_id", "environment"]
    {
        assert!(!text.contains(leak), "public view leaks {leak:?}: {text}");
    }
    // Another environment cannot take the slug.
    let other = f.control.with(|x| x.query_row("SELECT application_id FROM environments WHERE id = ?", [f.env], |r| r.get::<_, i64>(0))).unwrap();
    let (env2, _) = f.control.tx(|tx| create_environment(tx, other, "staging")).unwrap();
    assert!(f.control.with(|x| save_status_page(x, env2, "acme-status", "x", true)).unwrap().is_err());
}

#[test]
fn overall_status_reads_the_components() {
    assert_eq!(overall(&["up", "up"]), "operational");
    assert_eq!(overall(&["up", "down"]), "degraded");
    assert_eq!(overall(&["down", "down"]), "down");
    assert_eq!(overall(&["pending", "up"]), "operational", "an unchecked component does not make it degraded");
    assert_eq!(overall(&[]), "unknown");
    assert!(validate_slug("acme-status").is_none());
    for bad in ["ab", "-acme", "acme-", "Acme", "acme/x", "a..b"] {
        assert!(validate_slug(bad).is_some(), "{bad}");
    }
    assert_eq!(new_slug().len(), 20);
}

#[test]
fn deleting_an_environment_deletes_its_monitors() {
    let f = fixture("cascade");
    let id = add(&f, input("Shop"));
    f.control.with(|x| record(x, id, T0, &ok(5))).unwrap();
    f.control.with(|x| save_status_page(x, f.env, "acme", "Acme", true)).unwrap().unwrap();
    f.control.with(|x| x.execute("DELETE FROM environments WHERE id = ?", [f.env])).unwrap();
    let left: i64 = f
        .control
        .with(|x| {
            x.query_row("SELECT (SELECT COUNT(*) FROM uptime_monitors) + (SELECT COUNT(*) FROM uptime_checks) + (SELECT COUNT(*) FROM status_pages)", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(left, 0);
}

// ---------------------------------------------------------------------------
// Probes against a local server (allowed only with the self-host policy).

async fn local_server() -> String {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/ok", get(|| async { "all systems fine" }))
        .route("/fail", get(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "maintenance") }))
        .route("/teapot", get(|| async { (axum::http::StatusCode::IM_A_TEAPOT, "short and stout") }))
        .route("/redirect", get(|| async { axum::response::Redirect::to("http://169.254.169.254/") }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        )
        .route("/ua", get(|headers: axum::http::HeaderMap| async move { headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn req(url: String) -> probe::Request {
    probe::Request { url, head: false, expected_status: None, keyword: None, timeout: Duration::from_secs(2) }
}

const LAN: Policy = Policy { allow_private: true };

#[tokio::test]
async fn probes_classify_each_way_a_check_fails() {
    let base = local_server().await;
    let ok = probe::check(&probe::Request { keyword: Some("fine".into()), ..req(format!("{base}/ok")) }, LAN).await;
    assert_eq!((ok.failure, ok.status_code), (None, Some(200)), "{ok:?}");
    assert!(ok.latency_ms < 2000);

    let s = probe::check(&req(format!("{base}/fail")), LAN).await;
    assert_eq!((s.failure, s.status_code), (Some(Failure::Status), Some(503)));
    let teapot = probe::check(&probe::Request { expected_status: Some(418), ..req(format!("{base}/teapot")) }, LAN).await;
    assert_eq!(teapot.failure, None, "an expected status passes");
    let kw = probe::check(&probe::Request { keyword: Some("healthy".into()), ..req(format!("{base}/ok")) }, LAN).await;
    assert_eq!(kw.failure, Some(Failure::Keyword));
    let head = probe::check(&probe::Request { head: true, ..req(format!("{base}/ok")) }, LAN).await;
    assert_eq!(head.failure, None);
    // A redirect is an answer (3xx passes); it is never followed.
    let r = probe::check(&req(format!("{base}/redirect")), LAN).await;
    assert_eq!((r.failure, r.status_code), (None, Some(303)));
    let slow = probe::check(&probe::Request { timeout: Duration::from_millis(300), ..req(format!("{base}/slow")) }, LAN).await;
    assert_eq!(slow.failure, Some(Failure::Timeout));
    assert!(slow.latency_ms < 2000);

    // Nothing listens on the port of a socket just closed.
    let closed = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap() };
    let refused = probe::check(&req(format!("http://{closed}/")), LAN).await;
    assert_eq!(refused.failure, Some(Failure::Connect), "{refused:?}");
    let dns = probe::check(&req("http://does-not-exist.invalid/".into()), LAN).await;
    assert_eq!(dns.failure, Some(Failure::Dns), "{dns:?}");
    // Plain HTTP spoken to by a TLS client.
    let tls = probe::check(&req(base.replace("http://", "https://") + "/ok"), LAN).await;
    assert_eq!(tls.failure, Some(Failure::Tls), "{tls:?}");

    let ua = reqwest::get(format!("{base}/ua")).await.unwrap().text().await.unwrap();
    assert!(!ua.starts_with("Railwatch"), "control: reqwest's own agent");
    let mut b = probe::Request { keyword: Some("Railwatch-Uptime/".into()), ..req(format!("{base}/ua")) };
    assert_eq!(probe::check(&b, LAN).await.failure, None, "the probe says it is Railwatch");
    b.keyword = Some(crate::SOURCE_URL.into());
    assert_eq!(probe::check(&b, LAN).await.failure, None);
}

#[tokio::test]
async fn without_the_self_host_flag_a_local_server_is_never_contacted() {
    let base = local_server().await;
    let out = probe::check(&req(format!("{base}/ok")), Policy::default()).await;
    assert_eq!(out.failure, Some(Failure::Blocked), "{out:?}");
    assert_eq!(out.status_code, None, "refused before connecting");
}
