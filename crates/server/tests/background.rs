//! Phase 2 background work against a real control database and tenant:
//! alert rules (filters, dedupe, burst collapse + summary), the outbox
//! (lease, retry, refusal of private destinations), the watchdog
//! (silent -> recovered, persisted, no re-alert), follow-up drain exactly
//! once, threshold detection off real rollups.

use std::path::PathBuf;

use railwatch_server::alerts::{outbox, rules};
use railwatch_server::control::Control;
use railwatch_server::grouping::{self, issue, Receipt};
use railwatch_server::jobs::{detectors, watchdog};
use rusqlite::{Connection, params};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rw-bg-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn setup(name: &str) -> (Control, i64, Connection, PathBuf) {
    let d = dir(name);
    let control = Control::open(&d.join("control.sqlite3")).unwrap();
    let env = control.create(name, &format!("{:0>64}", name.len()), 30).unwrap();
    let tenant = railwatch_server::tenants::open_tenant(&d.join("t.sqlite3")).unwrap();
    (control, env.id, tenant, d)
}

fn integration(c: &Connection, kind: &str, settings: &str) -> i64 {
    c.execute(
        "INSERT INTO integrations (account_id, created_at, enabled, kind, name, settings, updated_at) VALUES (1, '2026-01-01', 1, ?, 'x', ?, '2026-01-01')",
        params![kind, settings],
    )
    .unwrap();
    c.last_insert_rowid()
}

fn rule(c: &Connection, app: i64, integ: i64, event: &str, filters: &str) -> i64 {
    c.execute(
        "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?, '2026-01-01', ?, ?, ?, '2026-01-01')",
        params![app, event, filters, integ],
    )
    .unwrap();
    c.last_insert_rowid()
}

fn exception(t: &Connection, group: &str, class: &str, user: Option<&str>, at: &str) -> i64 {
    t.execute(
        "INSERT INTO exceptions (occurred_at, class_name, message, group_hash, user_ref, handled, fingerprint, frames, source) VALUES (?, ?, 'boom', ?, ?, 0, '[]', '[]', 'application')",
        params![at, class, group, user],
    )
    .unwrap();
    t.last_insert_rowid()
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn new_issue_alerts_respect_filters_dedupe_and_burst_collapse() {
    let (control, env, t, _d) = setup("alerts");
    let c = control.conn();
    let integ = integration(&c, "webhook", r#"{"url":"https://127.0.0.1/hook","secret":"s"}"#);
    let app = issue::application_of(&c, env).unwrap();
    rule(&c, app, integ, "new_issue", "{}");
    rule(&c, app, integ, "new_issue", r#"{"title_pattern":"/^Never/"}"#); // never matches
    rule(&c, app, integ, "new_issue", &format!(r#"{{"environment_ids":["{}"]}}"#, env + 1)); // other env
    let ids: Vec<i64> = (0..8).map(|i| exception(&t, &format!("{i:032x}"), "ArgumentError", Some("u1"), "2026-10-13 10:00:00")).collect();
    let rows = grouping::load_exceptions(&t, &ids).unwrap();
    let g = grouping::group(&c, &t, env, rows, None).unwrap();
    assert_eq!(g.new, 8);
    // Only the unfiltered rule fired: 5 individually, then collapsed.
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE status = 'pending'"), 5);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE status = 'collapsed'"), 3);
    // Same issues again within 30 min: deduped, no new alerts.
    let rows = grouping::load_exceptions(&t, &ids).unwrap();
    grouping::group(&c, &t, env, rows, None).unwrap();
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts"), 8);
    assert_eq!(count(&c, "SELECT SUM(occurrences) FROM issues"), 16);
    assert_eq!(count(&c, "SELECT MAX(affected_users) FROM issues"), 1);
    // Close the burst window artificially, then summarize.
    c.execute("UPDATE alerts SET created_at = datetime('now', '-2 minutes') WHERE status = 'collapsed'", []).unwrap();
    assert_eq!(outbox::summarize_bursts(&c).unwrap(), 1);
    let msg: String = c.query_row("SELECT json_extract(payload, '$.message') FROM alerts WHERE burst_key IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert!(msg.starts_with("3 more new issues in alerts since "), "{msg}");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE status = 'summarized'"), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn outbox_refuses_private_destination_and_schedules_retry() {
    let (control, env, _t, _d) = setup("outbox");
    let control = std::sync::Arc::new(control);
    {
        let c = control.conn();
        let integ = integration(&c, "webhook", r#"{"url":"https://127.0.0.1/hook"}"#);
        let app = issue::application_of(&c, env).unwrap();
        let r = rules::rules_for(&c, app, "silent_host").unwrap();
        assert!(r.is_empty());
        rule(&c, app, integ, "quota", "{}");
        let payload = railwatch_server::alerts::as_hash(railwatch_server::rhash! { "message" => "m", "environment_id" => env });
        assert_eq!(rules::fire_issueless(&c, app, "quota", &payload).unwrap().len(), 1);
    }
    assert_eq!(railwatch_server::alerts::deliver_pass(&control).await.unwrap(), 1);
    let c = control.conn();
    let (status, attempts, error): (String, i64, String) =
        c.query_row("SELECT status, delivery_attempts, error FROM alerts", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 1));
    assert!(error.contains("non-public"), "{error}");
    let next_in: f64 = c.query_row("SELECT (julianday(next_delivery_at) - julianday('now')) * 86400 FROM alerts", [], |r| r.get(0)).unwrap();
    assert!((50.0..70.0).contains(&next_in), "retry in {next_in} s, want ~60");
    // A lease that expired is recovered on the next claim.
    c.execute("UPDATE alerts SET status = 'delivering', delivery_lease_expires_at = datetime('now', '-1 minute'), next_delivery_at = NULL", []).unwrap();
    let claimed = outbox::claim_due(&c, 10).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].attempts, 2);
}

/// A shadow install (alert_delivery = off) fed a mirror of another server's
/// ingest has that server's integrations: it must record its alerts and
/// send none of them. The same alert with delivery on is attempted (and
/// refused, see the test above), so "skipped" here means nothing went out.
#[tokio::test(flavor = "multi_thread")]
async fn alert_delivery_off_records_alerts_as_skipped_without_sending() {
    let (control, env, _t, _d) = setup("shadow");
    let control = std::sync::Arc::new(control);
    control.configure_alerting(railwatch_server::alerts::integration::Alerting {
        cipher: railwatch_server::alerts::integration::SettingsCipher::new(&[7u8; 64]),
        smtp: None,
        url: "https://rs.example.com".into(),
        deliver: false,
    });
    let integ = {
        let c = control.conn();
        let integ = integration(&c, "webhook", r#"{"url":"https://127.0.0.1/hook"}"#);
        let app = issue::application_of(&c, env).unwrap();
        rule(&c, app, integ, "quota", "{}");
        let payload = railwatch_server::alerts::as_hash(railwatch_server::rhash! { "message" => "m", "environment_id" => env });
        assert_eq!(rules::fire_issueless(&c, app, "quota", &payload).unwrap().len(), 1);
        integ
    };
    assert_eq!(railwatch_server::alerts::deliver_pass(&control).await.unwrap(), 1);
    assert_eq!(railwatch_server::alerts::deliver_test(&control, integ).await.unwrap(), "skipped");
    let c = control.conn();
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE status = 'skipped' AND error IS NULL AND sent_at IS NULL"), 2);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM integrations WHERE last_delivered_at IS NOT NULL OR last_error IS NOT NULL"), 0);
}

#[test]
fn watchdog_alerts_once_on_silence_and_once_on_recovery() {
    let (control, env, _t, _d) = setup("watchdog");
    let c = control.conn();
    let integ = integration(&c, "slack", r#"{"webhook_url":"https://hooks.slack.com/services/T/B/x"}"#);
    let app = issue::application_of(&c, env).unwrap();
    rule(&c, app, integ, "silent_environment", "{}");
    rule(&c, app, integ, "environment_recovered", "{}");
    let hb = watchdog::Heartbeats::default();
    let t0 = 1_791_900_000_000_000i64;
    hb.beat(env, &["web-1".into(), "web-2".into()], t0);
    assert!(watchdog::check(&c, &hb, t0 + 30_000_000, 60).unwrap().is_empty());
    hb.beat(env, &["web-1".into()], t0 + 50_000_000);
    let t = watchdog::check(&c, &hb, t0 + 70_000_000, 60).unwrap();
    assert_eq!(t, vec![watchdog::Transition::Silent { env, server: "web-2".into(), last_seen: t0 }]);
    // Still silent: no repeat; state survives a restart.
    let hb2 = watchdog::Heartbeats::default();
    hb2.restore(&c).unwrap();
    hb2.beat(env, &["web-1".into()], t0 + 100_000_000);
    assert!(watchdog::check(&c, &hb2, t0 + 110_000_000, 60).unwrap().is_empty());
    hb2.beat(env, &["web-2".into()], t0 + 120_000_000);
    let t = watchdog::check(&c, &hb2, t0 + 125_000_000, 60).unwrap();
    assert!(matches!(&t[..], [watchdog::Transition::Recovered { server, .. }] if server == "web-2"), "{t:?}");
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE event = 'silent_environment'"), 1);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE event = 'environment_recovered'"), 1);
}

#[test]
fn receipted_grouping_is_claimed_exactly_once() {
    let (control, env, t, _d) = setup("claims");
    let c = control.conn();
    let ids = vec![exception(&t, &format!("{:032x}", 1), "E", None, "2026-10-13 10:00:00"), exception(&t, &format!("{:032x}", 2), "E", None, "2026-10-13 10:00:01")];
    let receipt = Receipt { producer_id: "p".into(), delivery_id: "d".into(), delivery_created_at: railwatch_server::time::now_micros() };
    for _ in 0..3 {
        let rows = grouping::load_exceptions(&t, &ids).unwrap();
        grouping::group(&c, &t, env, rows, Some(&receipt)).unwrap();
    }
    assert_eq!(count(&c, "SELECT SUM(occurrences) FROM issues"), 2);
    assert_eq!(count(&c, "SELECT COUNT(*) FROM ingest_followup_receipts"), 2);
    // Below the GC floor: refused outright.
    c.execute("UPDATE environments SET ingest_followup_gc_before = ?", [railwatch_server::time::quoted_date_micros(receipt.delivery_created_at + 1)]).unwrap();
    let old = Receipt { delivery_id: "old".into(), ..receipt };
    let rows = grouping::load_exceptions(&t, &ids).unwrap();
    assert_eq!(grouping::group(&c, &t, env, rows, Some(&old)).unwrap().claimed_elsewhere, 2);
    assert_eq!(count(&c, "SELECT SUM(occurrences) FROM issues"), 2);
}

#[test]
fn threshold_on_rollups_opens_a_performance_issue_once() {
    let (control, env, t, _d) = setup("threshold");
    let c = control.conn();
    let now = railwatch_server::time::now_micros() / 1_000_000;
    let at = railwatch_server::time::utc_seconds_string(now - 60);
    for d in [900_000, 2_500_000, 3_000_000] {
        t.execute(
            "INSERT INTO executions (occurred_at, kind, name, duration, group_hash, status, counters, detail, stages) VALUES (?, 'request', 'GET /slow', ?, 'g1', 200, '{}', '{}', '[]')",
            params![at, d],
        )
        .unwrap();
    }
    let hour = now - now.rem_euclid(3600);
    let h = if now - 60 < hour { hour - 3600 } else { hour };
    railwatch_server::rollup::rollup_job::perform(&t, &t, h, &mut railwatch_server::rollup::rollup_job::Rng::PerGroup).unwrap();
    c.execute("INSERT INTO thresholds (environment_id, \"limit\", metric, target, target_kind, window_minutes, created_at, updated_at) VALUES (?, 2000, 'p95', '*', 'requests', 5, '2026-01-01', '2026-01-01')", [env]).unwrap();
    assert_eq!(detectors::detect_performance(&c, &t, env, now).unwrap(), 1);
    let title: String = c.query_row("SELECT title FROM issues", [], |r| r.get(0)).unwrap();
    assert_eq!(title, "GET /slow exceeded p95 2000.0ms (3000.0ms)");
    assert_eq!(detectors::detect_performance(&c, &t, env, now + 60).unwrap(), 0);
    assert_eq!(count(&c, "SELECT occurrences FROM issues"), 2);
}

#[test]
fn an_alert_in_its_retry_schedule_is_delivered_past_the_reconcile_window_and_retry_resets_a_failed_one() {
    // Rails' 1-hour RECONCILE_WINDOW bounds only the reconciler; an alert in
    // its retry schedule has a job queued and is delivered whatever its age.
    // The fifth attempt is due ~81 min after creation, so a claim that kept
    // the window would silently strand it.
    let (control, env, _t, _d) = setup("retrywin");
    let c = control.conn();
    let integ = integration(&c, "webhook", r#"{"url":"https://example.com/h"}"#);
    let app = issue::application_of(&c, env).unwrap();
    rule(&c, app, integ, "quota", "{}");
    let payload = railwatch_server::alerts::as_hash(railwatch_server::rhash! { "message" => "m", "environment_id" => env });
    let id = rules::fire_issueless(&c, app, "quota", &payload).unwrap()[0];
    c.execute("UPDATE alerts SET created_at = datetime('now', '-90 minutes'), delivery_attempts = 4, next_delivery_at = datetime('now', '-1 second') WHERE id = ?", [id]).unwrap();
    assert_eq!(outbox::claim_due(&c, 10).unwrap().len(), 1, "attempt 5 is still claimed");
    // A never-attempted alert that old is not (it predates the outbox).
    let stale = rules::fire_issueless(&c, app, "quota", &payload).unwrap()[0];
    c.execute("UPDATE alerts SET created_at = datetime('now', '-2 hours') WHERE id = ?", [stale]).unwrap();
    assert!(outbox::claim_due(&c, 10).unwrap().is_empty());
    // Retry: only failed alerts, fresh budget, due now even though it is old.
    c.execute("UPDATE alerts SET status = 'failed', delivery_attempts = 5, delivery_lease_id = NULL WHERE id = ?", [id]).unwrap();
    assert!(!outbox::retry(&c, stale).unwrap(), "a pending alert is not 'retried'");
    assert!(outbox::retry(&c, id).unwrap());
    let claimed = outbox::claim_due(&c, 10).unwrap();
    assert_eq!((claimed.len(), claimed[0].id, claimed[0].attempts), (1, id, 1));
}

/// The affected-users count grouping keeps across batches (AffectedUsers)
/// equals update_affected_users' COUNT(DISTINCT user_ref) after every batch:
/// repeated users, NULLs, other groups interleaved, and a prune in between.
#[test]
fn affected_users_kept_across_batches_equals_the_full_count() {
    let (control, env, t, _d) = setup("affected");
    let c = control.conn();
    let mut users = grouping::AffectedUsers::default();
    let g = |i: usize| format!("{:032x}", i % 3);
    let full = |t: &Connection, g: &str| -> i64 {
        t.query_row("SELECT COUNT(DISTINCT user_ref) FROM exceptions WHERE group_hash = ? AND user_ref IS NOT NULL", [g], |r| r.get(0)).unwrap()
    };
    let mut n = 0;
    for batch in 0..12 {
        if batch == 6 {
            // prune: the owner clears the cache with the deletes.
            t.execute("DELETE FROM exceptions WHERE id % 4 = 0 OR user_ref = 'u3'", []).unwrap();
            users.clear();
        }
        let ids: Vec<i64> = (0..10)
            .map(|_| {
                n += 1;
                let user = if n % 5 == 0 || (batch >= 6 && n % 7 == 3) { None } else { Some(format!("u{}", n % 7)) };
                exception(&t, &g(n), "E", user.as_deref(), "2026-10-13 10:00:00")
            })
            .collect();
        let rows = grouping::load_exceptions(&t, &ids).unwrap();
        grouping::group_with(&c, &t, env, rows, None, &mut users).unwrap();
        for i in 0..3 {
            let got: i64 = c.query_row("SELECT affected_users FROM issues WHERE group_hash = ?", [g(i)], |r| r.get(0)).unwrap();
            assert_eq!(got, full(&t, &g(i)), "batch {batch} group {i}");
        }
    }
}

#[test]
fn a_still_missed_scheduled_task_keeps_its_detection_snapshot() {
    // carry_detection: the 10-minute rescan (:seen) must not drop the
    // snapshot the opening scan captured.
    let (control, env, t, _d) = setup("missed");
    let c = control.conn();
    let now = 1_792_065_600; // 2026-10-15 12:00:00 UTC
    t.execute(
        "INSERT INTO executions (occurred_at, kind, name, duration, group_hash, task_key, status, counters, detail, stages) \
         VALUES (?, 'scheduled_task', 'cleanup', 1000, 'gcleanup', 'cleanup', NULL, '{}', '{\"schedule\":\"every 5 minutes\"}', '[]')",
        [railwatch_server::time::utc_seconds_string(now - 1200)],
    )
    .unwrap();
    assert_eq!(detectors::check_scheduled_tasks(&c, &t, env, now).unwrap(), 1);
    let detection = |c: &Connection| -> Option<String> { c.query_row("SELECT json_extract(sample, '$.detection') FROM issues", [], |r| r.get(0)).unwrap() };
    let first = detection(&c).expect("snapshot on the opening scan");
    assert_eq!(detectors::check_scheduled_tasks(&c, &t, env, now + 600).unwrap(), 0);
    assert_eq!(count(&c, "SELECT occurrences FROM issues"), 2);
    assert_eq!(detection(&c).as_deref(), Some(first.as_str()), "a :seen scan keeps the snapshot");
}

#[test]
fn quota_alerts_fire_only_for_rules_whose_environment_filter_matches_their_application() {
    let (control, env, _t, _d) = setup("quota");
    let c = control.conn();
    let integ = integration(&c, "webhook", r#"{"url":"https://127.0.0.1/hook"}"#);
    let app = issue::application_of(&c, env).unwrap();
    let open = rule(&c, app, integ, "quota", "{}");
    let narrowed = rule(&c, app, integ, "quota", &format!(r#"{{"environment_ids":["{env}"]}}"#));
    let elsewhere = rule(&c, app, integ, "quota", r#"{"environment_ids":[999]}"#);
    let now = 1_792_065_600; // 2026-10-15
    c.execute("UPDATE accounts SET monthly_event_quota = 1000, additional_events_cap = 0", []).unwrap();
    c.execute(
        "INSERT INTO ingest_usage_months (environment_id, month, events, created_at, updated_at) VALUES (?, '2026-10-01', 850, '2026-10-01', '2026-10-01')",
        [env],
    )
    .unwrap();
    assert_eq!(detectors::quota_check(&c, now).unwrap(), 2);
    let fired: Vec<i64> = c
        .prepare("SELECT alert_rule_id FROM alerts ORDER BY alert_rule_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(fired, vec![open, narrowed]);
    assert!(!fired.contains(&elsewhere));
    // The payload is account-level: no environment_id.
    assert_eq!(count(&c, "SELECT COUNT(*) FROM alerts WHERE json_extract(payload, '$.environment_id') IS NOT NULL"), 0);
}

#[test]
fn the_drain_recomputes_a_pre_start_hour_once_and_counts_every_receipts_rows() {
    // Two receipts committed before this process started both owe the same
    // old hour. The per-pass `recomputed` set recomputes it once; that read
    // already sees both receipts' rows (both committed before the pass), so
    // nothing is skipped. A receipt committed during the pass is after
    // process start: the drain leaves it to the accumulator its commit fed.
    let (control, env, t, _d) = setup("drain-late");
    let now = railwatch_server::time::now_micros() / 1_000_000;
    let hour = now - now.rem_euclid(3600) - 3 * 3600;
    let before_start = railwatch_server::time::quoted_date_micros(railwatch_server::jobs::drain::process_started() - 60_000_000);
    let bucket = railwatch_server::time::iso8601(hour);
    for (i, at) in [hour + 600, hour + 1200].into_iter().enumerate() {
        t.execute(
            "INSERT INTO executions (occurred_at, kind, name, duration, group_hash, status, outcome, counters, detail, stages) \
             VALUES (?, 'command', 'db:migrate', 1000, 'gcmd', 0, 'ok', '{}', '{}', '[]')",
            [railwatch_server::time::quoted_date_micros(at * 1_000_000)],
        )
        .unwrap();
        t.execute(
            "INSERT INTO ingest_receipts (ack, body_sha256, committed_at, delivery_created_at, delivery_id, followups, metadata_sha256, policy_version, producer_id, record_count, retain_until) \
             VALUES ('{}', 'b', ?1, ?1, ?2, ?3, 'm', '1', 'p', 1, ?1)",
            params![before_start, format!("d{i}"), format!(r#"{{"version":1,"rollup_buckets":["{bucket}"]}}"#)],
        )
        .unwrap();
    }
    let out = railwatch_server::jobs::drain::drain(&control, (&t, &t), env, &mut Default::default()).unwrap();
    assert_eq!((out.receipts, out.recomputed_hours), (2, 1));
    let n: i64 = t
        .query_row("SELECT count FROM rollups WHERE record_type = 'command' AND bucket = ?", [railwatch_server::time::utc_seconds_string(hour)], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}
