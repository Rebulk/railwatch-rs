//! WeeklyDigestJob: the Monday-morning summary of the past week, one email
//! per account member who has `weekly_digest` on (Settings > Profile).
//! recurring.yml runs it "every monday at 8am" in UTC (the app sets no
//! time zone); `run_forever` does the same with `schedule::next_after`.
//! Like Solid Queue, a run missed while the server was down is not caught
//! up.
//!
//! Numbers come from the rollups (requests, job attempts) and the control
//! DB (issues, deploys), never from raw telemetry. Accounts with no
//! recipient, and environments with no tenant file or no request and no
//! job attempt in the window, are skipped; an account left with no
//! environment gets nothing.
//!
//! Delivery follows `alert_delivery`: off (a shadow install), digests are
//! built, counted and logged, never sent. Without SMTP they are logged, not
//! sent. `railwatch digest send --dry-run` prints what a run would send.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::control::Control;
use crate::dashboard::db::{self, qt};
use crate::dashboard::{pool::open_tenant_reader, rb, rollup};
use crate::web::digest_mailer::{self, DigestMail};

/// config/recurring.yml `weekly_digest`.
pub const SCHEDULE: &str = "every monday at 8am";

/// One environment's line in the digest (WeeklyDigestJob#build's hash).
#[derive(Debug, Clone)]
pub struct EnvDigest {
    /// "Application · environment"
    pub name: String,
    pub requests: i64,
    /// Requests with a 5xx (the rollups' error_count).
    pub failed_requests: i64,
    pub p95_ms: f64,
    pub jobs: i64,
    pub failed_jobs: i64,
    pub new_issues: i64,
    pub resolved_issues: i64,
    pub deploys: i64,
    pub top_issues: Vec<TopIssue>,
}

#[derive(Debug, Clone)]
pub struct TopIssue {
    pub id: i64,
    pub key: String,
    /// The first 120 characters.
    pub title: String,
    pub occurrences: i64,
}

/// What a run does for one account.
#[derive(Debug)]
pub enum Outcome {
    NoRecipients,
    /// No environment had requests or job attempts in the window.
    NoTelemetry,
    Mails(Vec<DigestMail>),
    /// Reading the account's numbers failed; nothing is sent for it.
    Failed(String),
}

#[derive(Debug)]
pub struct AccountRun {
    pub id: i64,
    pub name: String,
    pub outcome: Outcome,
}

/// WeeklyDigestJob#perform without the delivery: every account (or just
/// `only`), in id order, with the mails it would send at `now` (micros).
pub fn build_all(control: &Control, tenants_dir: &Path, base_url: &str, now: i64, only: Option<i64>) -> rusqlite::Result<Vec<AccountRun>> {
    let accounts: Vec<(i64, String)> = control.with(|c| {
        c.prepare(r#"SELECT "accounts"."id", "accounts"."name" FROM "accounts" WHERE ?1 IS NULL OR "accounts"."id" = ?1 ORDER BY "accounts"."id""#)?
            .query_map([only], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    })?;
    let since = now - 7 * 86_400 * 1_000_000;
    Ok(accounts
        .into_iter()
        .map(|(id, name)| {
            let outcome = match account(control, tenants_dir, base_url, id, &name, since, now) {
                Ok(o) => o,
                Err(e) => Outcome::Failed(e.to_string()),
            };
            AccountRun { id, name, outcome }
        })
        .collect())
}

fn account(control: &Control, tenants_dir: &Path, base_url: &str, id: i64, name: &str, since: i64, now: i64) -> rusqlite::Result<Outcome> {
    // account.users.where(weekly_digest: true), the SQL Rails runs.
    let recipients: Vec<(String, String)> = control.with(|c| {
        c.prepare(
            r#"SELECT "users"."name", "users"."email" FROM "users" INNER JOIN "memberships" ON "users"."id" = "memberships"."user_id" WHERE "memberships"."account_id" = ? AND "users"."weekly_digest" = TRUE"#,
        )?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
    })?;
    if recipients.is_empty() {
        return Ok(Outcome::NoRecipients);
    }
    let environments = build(control, tenants_dir, id, since, now)?;
    if environments.is_empty() {
        return Ok(Outcome::NoTelemetry);
    }
    Ok(Outcome::Mails(recipients.iter().map(|(user, email)| digest_mailer::weekly(base_url, user, email, name, &environments)).collect()))
}

struct EnvRow {
    id: i64,
    name: String,
    slug: String,
    app_name: String,
}

/// WeeklyDigestJob#build.
pub fn build(control: &Control, tenants_dir: &Path, account_id: i64, since: i64, now: i64) -> rusqlite::Result<Vec<EnvDigest>> {
    // account.environments.includes(:application). Rails has no ORDER BY:
    // its plan walks index_applications_on_account_id then
    // index_environments_on_application_id, i.e. application id, then
    // environment id. This control DB holds the same indexes created in
    // another order, so SQLite picks the (account_id, slug) and
    // (application_id, name) ones instead; the order is spelled out.
    let envs: Vec<EnvRow> = control.with(|c| {
        let rows = db::query(
            c,
            r#"SELECT "environments".* FROM "environments" INNER JOIN "applications" ON "environments"."application_id" = "applications"."id" WHERE "applications"."account_id" = ? ORDER BY "applications"."id", "environments"."id""#,
            [account_id],
        )?;
        rows.iter()
            .map(|e| {
                let app_name = c.query_row(r#"SELECT "applications"."name" FROM "applications" WHERE "applications"."id" = ?"#, [e.i("application_id")], |r| r.get(0))?;
                Ok(EnvRow {
                    id: e.i("id").unwrap_or_default(),
                    name: e.s("name").unwrap_or_default().to_string(),
                    slug: e.s("slug").unwrap_or_default().to_string(),
                    app_name,
                })
            })
            .collect()
    })?;
    let mut out = Vec::new();
    for env in envs {
        // select(&:telemetry_exists?)
        if crate::storage::Layout::detect(tenants_dir, &env.slug).is_none() {
            continue;
        }
        let (requests, jobs) = {
            let t = open_tenant_reader(tenants_dir, &env.slug)?;
            (rollup::summarize_owned(&rollup::between(&t, "request", since, now)?), rollup::summarize_owned(&rollup::between(&t, "job_attempt", since, now)?))
        };
        if requests.count == 0 && jobs.count == 0 {
            continue;
        }
        out.push(control.with(|c| env_digest(c, &env, &requests, &jobs, since))?);
    }
    Ok(out)
}

fn env_digest(c: &Connection, env: &EnvRow, requests: &rollup::Summary, jobs: &rollup::Summary, since: i64) -> rusqlite::Result<EnvDigest> {
    let count = |sql: &str| c.query_row(sql, params![env.id, qt(since)], |r| r.get::<_, i64>(0));
    // env.issues.open.order(occurrences: :desc).limit(3)
    let top = db::query(
        c,
        r#"SELECT "issues".* FROM "issues" WHERE "issues"."environment_id" = ? AND "issues"."status" = 'open' ORDER BY "issues"."occurrences" DESC LIMIT 3"#,
        [env.id],
    )?
    .iter()
    .map(|i| {
        // Issue#key: the issue's own application's prefix.
        let prefix: String =
            c.query_row(r#"SELECT "applications"."issue_prefix" FROM "applications" WHERE "applications"."id" = ?"#, [i.i("application_id")], |r| r.get(0))?;
        Ok(TopIssue {
            id: i.i("id").unwrap_or_default(),
            key: format!("{prefix}-{}", i.i("number").unwrap_or_default()),
            title: rb::first_chars(i.s("title").unwrap_or_default(), 120).to_string(),
            occurrences: i.i("occurrences").unwrap_or_default(),
        })
    })
    .collect::<rusqlite::Result<_>>()?;
    Ok(EnvDigest {
        name: format!("{} · {}", env.app_name, env.name),
        requests: requests.count,
        failed_requests: requests.errors,
        p95_ms: rb::round(requests.p95 as f64 / 1000.0, 1),
        jobs: jobs.count,
        failed_jobs: jobs.errors,
        new_issues: count(r#"SELECT COUNT(*) FROM "issues" WHERE "issues"."environment_id" = ? AND (first_seen_at >= ?)"#)?,
        resolved_issues: count(r#"SELECT COUNT(*) FROM "issues" WHERE "issues"."environment_id" = ? AND (resolved_at >= ?)"#)?,
        deploys: count(r#"SELECT COUNT(*) FROM "deploys" WHERE "deploys"."environment_id" = ? AND (deployed_at >= ?)"#)?,
        top_issues: top,
    })
}

/// What one run did.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub built: usize,
    pub sent: usize,
    pub failed: usize,
    /// Why nothing was sent, when the digests were withheld.
    pub withheld: Option<&'static str>,
}

/// One run at `now`: builds every account's digests and sends them, or
/// with delivery off or no SMTP logs that it did not. Never silent.
pub async fn run_once(control: std::sync::Arc<Control>, tenants_dir: std::path::PathBuf, now: i64) -> Report {
    let mut report = Report::default();
    let Some(alerting) = control.alerting().cloned() else {
        tracing::error!("weekly digest: the server's mail settings are not configured; nothing sent");
        report.withheld = Some("mail settings not configured");
        return report;
    };
    let url = alerting.url.clone();
    let c = control.clone();
    let runs = match tokio::task::spawn_blocking(move || build_all(&c, &tenants_dir, &url, now, None)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "weekly digest failed");
            report.withheld = Some("build failed");
            return report;
        }
        Err(e) => {
            tracing::error!(error = %e, "weekly digest panicked");
            report.withheld = Some("build failed");
            return report;
        }
    };
    let mut mails = Vec::new();
    for run in runs {
        match run.outcome {
            Outcome::Mails(m) => mails.extend(m),
            Outcome::Failed(e) => {
                report.failed += 1;
                tracing::error!(account = run.id, error = e, "weekly digest: account skipped");
            }
            Outcome::NoRecipients | Outcome::NoTelemetry => {}
        }
    }
    report.built = mails.len();
    if mails.is_empty() {
        tracing::info!("weekly digest: nothing to send");
        return report;
    }
    if !alerting.deliver {
        tracing::warn!(digests = mails.len(), "weekly digest: alert_delivery = off, not sent");
        report.withheld = Some("alert_delivery = off");
        return report;
    }
    let mailer = match alerting.smtp.as_ref().map(crate::web::mailer::Mailer::new) {
        None => {
            tracing::warn!(digests = mails.len(), "weekly digest: SMTP is not configured, not sent");
            report.withheld = Some("SMTP is not configured");
            return report;
        }
        Some(Err(e)) => {
            tracing::error!(digests = mails.len(), error = e, "weekly digest: SMTP settings are invalid, not sent");
            report.withheld = Some("SMTP settings are invalid");
            return report;
        }
        Some(Ok(m)) => m,
    };
    for m in &mails {
        match mailer.send_alternative(&m.to, &m.subject, &m.text, &m.html).await {
            Ok(()) => report.sent += 1,
            Err(e) => {
                report.failed += 1;
                tracing::error!(to = m.to, subject = m.subject, error = e, "weekly digest delivery failed");
            }
        }
    }
    tracing::info!(sent = report.sent, failed = report.failed, "weekly digest sent");
    report
}

/// Runs `run_once` at every SCHEDULE time until `shutdown` changes. Wakes
/// at least hourly, so a wall-clock jump cannot push a run a week late.
pub async fn run_forever(control: std::sync::Arc<Control>, tenants_dir: std::path::PathBuf, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let now_s = || crate::time::now_micros() / 1_000_000;
    let Some(mut next) = super::schedule::next_after(SCHEDULE, now_s()) else { return };
    loop {
        let now = now_s();
        if now >= next {
            run_once(control.clone(), tenants_dir.clone(), crate::time::now_micros()).await;
            next = super::schedule::next_after(SCHEDULE, now_s()).unwrap_or(next + 7 * 86_400);
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs((next - now).clamp(1, 3600) as u64)) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// `railwatch digest send --dry-run`: what a run now would send, as text.
pub fn dry_run(control: &Control, tenants_dir: &Path, base_url: &str, now: i64, only: Option<i64>, delivery: &str) -> rusqlite::Result<String> {
    let runs = build_all(control, tenants_dir, base_url, now, only)?;
    if let Some(id) = only
        && runs.is_empty()
        && control.with(|c| c.query_row("SELECT id FROM accounts WHERE id = ?", [id], |r| r.get::<_, i64>(0)).optional())?.is_none()
    {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    let mut out = format!("weekly digest at {} UTC (schedule: {SCHEDULE}, UTC); delivery: {delivery}\n", crate::time::utc_seconds_string(now / 1_000_000));
    let total: usize = runs.iter().map(|r| if let Outcome::Mails(m) = &r.outcome { m.len() } else { 0 }).sum();
    for run in &runs {
        let head = format!("account {} {:?}", run.id, run.name);
        match &run.outcome {
            Outcome::NoRecipients => out += &format!("\n{head}: skipped, no member has the weekly digest on\n"),
            Outcome::NoTelemetry => out += &format!("\n{head}: skipped, no requests or job attempts in the last 7 days\n"),
            Outcome::Failed(e) => out += &format!("\n{head}: FAILED, nothing would be sent: {e}\n"),
            Outcome::Mails(mails) => {
                out += &format!("\n{head}: {} digest(s)\n", mails.len());
                for m in mails {
                    out += &format!("\n--- To: {}\n--- Subject: {}\n{}", m.to, m.subject, m.text);
                }
            }
        }
    }
    out += &format!("\n{total} digest(s) would be sent.\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway password for fixture users (random, so no credential is
    /// written into the source).
    fn test_password() -> String {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("random");
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    use crate::control::{create_account, create_environment, insert_application, insert_membership, insert_user};
    use crate::rollup::random::RubyRandom;
    use crate::rollup::tdigest::TDigest;

    const NOW: i64 = 1_792_396_800_000_000; // Monday 2026-10-19 08:00 UTC
    const HOUR: i64 = 3_600_000_000;

    struct Fixture {
        dir: std::path::PathBuf,
        control: Control,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("rw-digest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let control = Control::open(&dir.join("control.sqlite3")).unwrap();
        Fixture { dir, control }
    }

    /// A tenant file with one rollup row per (type, bucket, count, errors, millis).
    fn tenant(f: &Fixture, slug: &str, rows: &[(&str, i64, i64, i64, i64)]) {
        let mut t = Connection::open(f.dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        crate::schema::create_tenant(&mut t, "2026-10-01 00:00:00").unwrap();
        let mut rng = RubyRandom::new(1);
        for (i, (kind, bucket, count, errors, ms)) in rows.iter().enumerate() {
            let mut d = TDigest::new(0.01);
            for _ in 0..*count {
                d.push_int(ms * 1000, &mut rng);
            }
            t.execute(
                "INSERT INTO rollups (record_type, group_hash, name, bucket, count, error_count, client_error_count, duration_sum, duration_max, digest, extra, p50, p95, p99) VALUES (?, ?, 'x', ?, ?, ?, 0, ?, ?, ?, '{}', 0, 0, 0)",
                params![kind, format!("{i:032}"), qt(*bucket), count, errors, count * ms * 1000, ms * 1000, d.as_small_bytes()],
            )
            .unwrap();
        }
    }

    #[test]
    fn who_gets_what() {
        let f = fixture("who");
        let c = &f.control;
        let (ada, bob, cyd) = c
            .tx(|tx| {
                Ok((
                    insert_user(tx, "Ada", "ada@example.com", &test_password(), true)?,
                    insert_user(tx, "Bob", "bob@example.com", &test_password(), true)?,
                    insert_user(tx, "Cyd", "cyd@example.com", &test_password(), true)?,
                ))
            })
            .unwrap();
        c.with(|x| x.execute("UPDATE users SET weekly_digest = FALSE WHERE id = ?", [bob])).unwrap();
        let acme = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        c.tx(|tx| insert_membership(tx, acme, bob, "member")).unwrap();
        let shop = c.tx(|tx| insert_application(tx, acme, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (prod, _) = c.tx(|tx| create_environment(tx, shop, "production")).unwrap();
        let (_staging, _) = c.tx(|tx| create_environment(tx, shop, "staging")).unwrap();
        let (_old, _) = c.tx(|tx| create_environment(tx, shop, "old")).unwrap();
        // Bob alone, digest off: no recipients. Cyd's account has no telemetry.
        let solo = c.tx(|tx| create_account(tx, "Solo", bob)).unwrap();
        let none = c.tx(|tx| create_account(tx, "Empty", cyd)).unwrap();
        c.tx(|tx| insert_application(tx, none, "E", "e", "E", None, "main")).unwrap();

        tenant(
            &f,
            "acme-shop-production",
            &[("request", NOW - 2 * HOUR, 100, 7, 120), ("job_attempt", NOW - 3 * HOUR, 40, 2, 900), ("request", NOW - 8 * 24 * HOUR, 999, 999, 5)],
        );
        tenant(&f, "acme-shop-staging", &[]); // a tenant file, but nothing in the window
        // acme-shop-old: no tenant file at all.
        let at = |h: i64| qt(NOW - h * HOUR);
        c.with(|x| {
            x.execute_batch(&format!(
                "INSERT INTO issues (application_id, environment_id, number, title, group_hash, occurrences, status, first_seen_at, last_seen_at, resolved_at, created_at, updated_at, sample) VALUES
                 ({shop}, {prod}, 1, 'Boom <b>', 'a', 50, 'open', '{a}', '{a}', NULL, '{a}', '{a}', '{{}}'),
                 ({shop}, {prod}, 2, 'Old', 'b', 70, 'open', '{b}', '{a}', NULL, '{a}', '{a}', '{{}}'),
                 ({shop}, {prod}, 3, 'Fixed', 'c', 900, 'resolved', '{b}', '{a}', '{a}', '{a}', '{a}', '{{}}');
                 INSERT INTO deploys (environment_id, deploy, deployed_at, created_at, updated_at) VALUES ({prod}, 'v1', '{a}', '{a}', '{a}'), ({prod}, 'v0', '{b}', '{b}', '{b}');",
                a = at(5),
                b = at(24 * 10),
            ))
        })
        .unwrap();

        let runs = build_all(c, &f.dir.join("telemetry"), "https://rw.example.com", NOW, None).unwrap();
        let ids: Vec<i64> = runs.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![acme, solo, none]);
        let Outcome::Mails(mails) = &runs[0].outcome else { panic!("{:?}", runs[0].outcome) };
        assert!(matches!(runs[1].outcome, Outcome::NoRecipients));
        assert!(matches!(runs[2].outcome, Outcome::NoTelemetry));
        // Only Ada (Bob has it off), and only production (staging is empty in the window, old has no file).
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].to, "ada@example.com");
        assert_eq!(mails[0].subject, "[Railwatch] Acme: your week in 1 environment");
        assert_eq!(
            mails[0].text,
            "Hey Ada,\n\nHere's what happened in Acme over the last 7 days.\n\nShop · production\n  100 requests, p95 120.0ms, 7.0% 5xx\n  40 job attempts, 2 failed\n  1 new issues, 1 resolved, 1 deploys\n  SHOP-2 Old (70) https://rw.example.com/issues/2\n  SHOP-1 Boom <b> (50) https://rw.example.com/issues/1\n\n\nOpen Railwatch: https://rw.example.com/dashboard\nTurn off this digest: https://rw.example.com/settings/profile\n\n"
        );
        assert!(mails[0].html.contains("<li>1 new issue, 1 resolved, 1 deploy</li>"));
        assert!(mails[0].html.contains("SHOP-1</a> Boom &lt;b&gt; (50)"));

        let only = build_all(c, &f.dir.join("telemetry"), "https://rw.example.com", NOW, Some(solo)).unwrap();
        assert_eq!(only.len(), 1);
        let out = dry_run(c, &f.dir.join("telemetry"), "https://rw.example.com", NOW, None, "off (test)").unwrap();
        assert!(out.starts_with("weekly digest at 2026-10-19 08:00:00 UTC (schedule: every monday at 8am, UTC); delivery: off (test)\n"), "{out}");
        assert!(out.contains(&format!("account {solo} \"Solo\": skipped, no member has the weekly digest on")));
        assert!(out.contains(&format!("account {none} \"Empty\": skipped, no requests or job attempts in the last 7 days")));
        assert!(out.ends_with("\n1 digest(s) would be sent.\n"));
        assert!(matches!(dry_run(c, &f.dir.join("telemetry"), "u", NOW, Some(9999), "x"), Err(rusqlite::Error::QueryReturnedNoRows)));
    }

    #[tokio::test]
    async fn alert_delivery_off_withholds_every_digest() {
        let f = fixture("off");
        let c = &f.control;
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let acme = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let shop = c.tx(|tx| insert_application(tx, acme, "Shop", "shop", "SHOP", None, "main")).unwrap();
        c.tx(|tx| create_environment(tx, shop, "production")).unwrap();
        tenant(&f, "acme-shop-production", &[("request", NOW - HOUR, 10, 0, 50)]);
        // Nothing listens on port 9: an attempted send fails and is counted.
        let smtp = crate::config::Smtp {
            address: "127.0.0.1".into(),
            port: 9,
            username: None,
            password: None,
            security: "none".into(),
            from: "Railwatch <rw@example.com>".into(),
        };
        let run = |deliver: bool| {
            let control = std::sync::Arc::new(Control::open(&f.dir.join("control.sqlite3")).unwrap());
            control.configure_alerting(crate::alerts::integration::Alerting {
                cipher: crate::alerts::integration::SettingsCipher::new(b"k"),
                smtp: Some(smtp.clone()),
                url: "https://rw.example.com".into(),
                deliver,
            });
            run_once(control, f.dir.join("telemetry"), NOW)
        };
        assert_eq!(run(false).await, Report { built: 1, sent: 0, failed: 0, withheld: Some("alert_delivery = off") });
        assert_eq!(run(true).await, Report { built: 1, sent: 0, failed: 1, withheld: None });
    }
}
