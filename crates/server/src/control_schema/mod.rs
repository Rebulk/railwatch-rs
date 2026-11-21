//! The control database's schema: railwatch-cloud's primary database minus
//! billing, as versioned migrations. `migrate` applies the ones a file has
//! not seen yet, each in its own transaction, and records them in
//! `railwatch_migrations`.
//!
//! 1. `phase1`: Phase 1's minimal `environments` + `deploys` (a no-op on a
//!    file Phase 1 already created: CREATE IF NOT EXISTS).
//! 2. `rails_primary`: the Rails tables (rails_primary.sql, copied from the
//!    sqlite_master of a Rails-created production.sqlite3), with Phase 1's
//!    rows moved over: one account + application "default" per distinct
//!    retention, every environment keeping its id, slug and token digest.
//!
//! 3. `issues_alerts`: the rollups/alerts track's tables (issues.rs: issues,
//!    issue_activities, comments, alerts, alert_rules, integrations,
//!    thresholds, anomaly_rules; Rails' own DDL) plus the Rust-only
//!    `environment_heartbeats` (watchdog state).
//! 4. `uptime`: Rust-only uptime monitors (docs/uptime.md): monitors with
//!    their state, raw checks (7 days), daily rollups (90 days), incidents,
//!    and one status page per environment. Every table cascades from its
//!    environment, so deleting an environment deletes its monitors.
//! 5. `trusted_proxy`: the one account a trusted proxy's users belong to
//!    (web::proxy_auth). `railwatch setup` binds the account it creates; an
//!    existing file is bound to its lowest account id, the account the proxy
//!    granted until now. Deliberately no foreign key: deleting that account
//!    leaves the row pointing at nothing, and proxied requests are refused
//!    instead of falling through to whichever account is first.
//! 6. `api_token_role_ceiling`: `api_tokens.role_ceiling` (nullable). A
//!    token made through the trusted proxy keeps the role the proxy signed,
//!    and acts with at most that role even where the user's own membership
//!    is higher. NULL (every existing token): the membership's role.
//! 7. `retired_environment_slugs`: Rust-only. A slug names the environment's
//!    telemetry directory, source maps and backup objects, so a slug once
//!    used is never given to another environment (GA review 2026-11-19 T1:
//!    a recreated slug was served the deleted tenant's telemetry). A trigger
//!    records every deleted environment's slug; `control::retire_unowned`
//!    adds names found on disk or in the backup bucket at startup.
//! 8. `session_token`: Rust-only `sessions.token` (nullable), a random
//!    value the session cookie carries beside the row id
//!    (control::session_cookie). Ids are a counter, and a restore puts the
//!    counter back, so an id alone could name another person's session
//!    after one. NULL (a session from before this, or an imported row):
//!    only a cookie that carries no token opens it.

pub mod issues;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

pub const RAILS_PRIMARY: &str = include_str!("rails_primary.sql");

/// The plan name given to accounts railwatch-rs creates. Not one of Rails'
/// Account::PLANS: a self-hosted install has no billing.
pub const SELF_HOSTED_PLAN: &str = "self-hosted";
/// Account#monthly_event_quota is NOT NULL in Rails. Quota is only enforced
/// with `enforce_quota` (config); this is the value accounts start with.
pub const SELF_HOSTED_QUOTA: i64 = 1_000_000_000_000;

type Step = fn(&Transaction) -> rusqlite::Result<()>;

pub const MIGRATIONS: &[(&str, Step)] = &[
    ("20261013000001_phase1", phase1),
    ("20261013000002_rails_primary", rails_primary),
    ("20261013000003_issues_alerts", issues_alerts),
    ("20261101000001_uptime", uptime),
    ("20261119000001_trusted_proxy", trusted_proxy),
    ("20261119000002_api_token_role_ceiling", api_token_role_ceiling),
    ("20261119000010_retired_environment_slugs", retired_environment_slugs),
    ("20261121000001_session_token", session_token),
];

pub fn migrate(conn: &mut Connection) -> rusqlite::Result<Vec<&'static str>> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS railwatch_migrations (version TEXT PRIMARY KEY NOT NULL, applied_at TEXT NOT NULL)")?;
    let mut applied = Vec::new();
    for (version, step) in MIGRATIONS {
        let done: bool = conn.query_row("SELECT 1 FROM railwatch_migrations WHERE version = ?", [version], |_| Ok(true)).optional()?.unwrap_or(false);
        if done {
            continue;
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        step(&tx)?;
        tx.execute(
            "INSERT INTO railwatch_migrations (version, applied_at) VALUES (?, ?)",
            params![version, crate::time::quoted_date_micros(crate::time::now_micros())],
        )?;
        tx.commit()?;
        applied.push(*version);
    }
    Ok(applied)
}

const PHASE1: &str = "
CREATE TABLE IF NOT EXISTS environments (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  slug TEXT NOT NULL UNIQUE,
  token_digest TEXT NOT NULL UNIQUE,
  paused INTEGER NOT NULL DEFAULT 0,
  monthly_event_quota INTEGER,
  events_this_month INTEGER NOT NULL DEFAULT 0,
  usage_month TEXT,
  retention_days INTEGER NOT NULL DEFAULT 30,
  ingest_followup_gc_before INTEGER NOT NULL DEFAULT 0,
  raw_pruned_through INTEGER,
  last_seen_at TEXT,
  expected_servers TEXT NOT NULL DEFAULT '[]',
  kamal_config TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deploys (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  environment_id INTEGER NOT NULL REFERENCES environments(id),
  deploy TEXT NOT NULL,
  ref TEXT, name TEXT, url TEXT, server TEXT, previous_ref TEXT,
  deployed_at TEXT NOT NULL,
  commits TEXT NOT NULL DEFAULT '[]',
  detail TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (environment_id, deploy)
);
";

fn phase1(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(PHASE1)
}

struct Phase1Env {
    id: i64,
    slug: String,
    token_digest: String,
    paused: bool,
    events_this_month: i64,
    usage_month: Option<String>,
    retention_days: i64,
    gc_before: i64,
    raw_pruned_through: Option<i64>,
    last_seen_at: Option<String>,
    expected_servers: String,
    kamal_config: String,
    created_at: String,
}

fn rails_primary(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "ALTER TABLE environments RENAME TO phase1_environments;
         ALTER TABLE deploys RENAME TO phase1_deploys;",
    )?;
    tx.execute_batch(RAILS_PRIMARY)?;

    let envs: Vec<Phase1Env> = {
        let mut s = tx.prepare(
            "SELECT id, slug, token_digest, paused, events_this_month, usage_month, retention_days, ingest_followup_gc_before,
                    raw_pruned_through, last_seen_at, expected_servers, kamal_config, created_at FROM phase1_environments ORDER BY id",
        )?;
        s.query_map([], |r| {
            Ok(Phase1Env {
                id: r.get(0)?,
                slug: r.get(1)?,
                token_digest: r.get(2)?,
                paused: r.get::<_, i64>(3)? != 0,
                events_this_month: r.get(4)?,
                usage_month: r.get(5)?,
                retention_days: r.get(6)?,
                gc_before: r.get(7)?,
                raw_pruned_through: r.get(8)?,
                last_seen_at: r.get(9)?,
                expected_servers: r.get(10)?,
                kamal_config: r.get(11)?,
                created_at: r.get(12)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    let now = crate::time::quoted_date_micros(crate::time::now_micros());
    let mut apps: std::collections::BTreeMap<i64, i64> = Default::default();
    for e in &envs {
        let app_id = match apps.get(&e.retention_days) {
            Some(id) => *id,
            None => {
                let name = if e.retention_days == 30 { "default".to_string() } else { format!("default-{}d", e.retention_days) };
                tx.execute(
                    "INSERT INTO accounts (name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at) VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?5)",
                    params![name, SELF_HOSTED_PLAN, SELF_HOSTED_QUOTA, e.retention_days, now],
                )?;
                let account = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO applications (account_id, name, slug, issue_prefix, created_at, updated_at) VALUES (?, 'default', 'default', 'APP', ?, ?)",
                    params![account, now, now],
                )?;
                let id = tx.last_insert_rowid();
                apps.insert(e.retention_days, id);
                id
            }
        };
        // Phase 1 kept "YYYY-MM"; Rails' usage_month is a date.
        let usage_month = e.usage_month.as_ref().map(|m| if m.len() == 7 { format!("{m}-01") } else { m.clone() });
        tx.execute(
            "INSERT INTO environments (id, application_id, name, slug, token_digest, token_prefix, paused_at, events_this_month, usage_month,
               expected_servers, kamal_config, ingest_followup_gc_before, raw_pruned_through, last_seen_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3, ?4, 'rw_', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                e.id,
                app_id,
                e.slug,
                e.token_digest,
                e.paused.then(|| now.clone()),
                e.events_this_month,
                usage_month,
                e.expected_servers,
                e.kamal_config,
                crate::time::quoted_date_micros(e.gc_before),
                e.raw_pruned_through.map(crate::time::quoted_date_micros),
                e.last_seen_at,
                e.created_at,
            ],
        )?;
        if let Some(month) = usage_month.as_ref().filter(|_| e.events_this_month > 0) {
            tx.execute(
                "INSERT INTO ingest_usage_months (environment_id, month, events, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
                params![e.id, month, e.events_this_month, now, now],
            )?;
        }
    }
    tx.execute_batch(
        "INSERT INTO deploys (id, environment_id, deploy, ref, name, url, server, previous_ref, deployed_at, commits, detail, created_at, updated_at)
           SELECT id, environment_id, deploy, ref, name, url, server, previous_ref, deployed_at, commits, detail, created_at, updated_at FROM phase1_deploys;
         DROP TABLE phase1_deploys;
         DROP TABLE phase1_environments;",
    )?;
    Ok(())
}

/// The rollups/alerts track's tables. `deploys` and `ingest_followup_receipts`
/// are in issues::DDL too, IF NOT EXISTS; rails_primary already made them.
fn issues_alerts(tx: &Transaction) -> rusqlite::Result<()> {
    for sql in issues::DDL {
        tx.execute_batch(sql)?;
    }
    tx.execute_batch(issues::RUST_ONLY)
}

/// Times are epoch microseconds (as environment_heartbeats), days
/// "YYYY-MM-DD" in UTC.
const UPTIME: &str = "
CREATE TABLE uptime_monitors (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  url TEXT NOT NULL,
  method TEXT NOT NULL DEFAULT 'GET',
  -- NULL: any 2xx or 3xx
  expected_status INTEGER,
  keyword TEXT,
  timeout_seconds INTEGER NOT NULL DEFAULT 10,
  interval_seconds INTEGER NOT NULL DEFAULT 60,
  enabled INTEGER NOT NULL DEFAULT 1,
  -- on the environment's status page, as public_name (or name)
  public INTEGER NOT NULL DEFAULT 0,
  public_name TEXT,
  public_url INTEGER NOT NULL DEFAULT 0,
  -- pending (no check yet) | up | down
  status TEXT NOT NULL DEFAULT 'pending',
  failures INTEGER NOT NULL DEFAULT 0,
  failing_since INTEGER,
  last_checked_at INTEGER,
  last_status_code INTEGER,
  last_latency_ms INTEGER,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX index_uptime_monitors_on_environment_id ON uptime_monitors (environment_id);
CREATE TABLE uptime_checks (
  monitor_id INTEGER NOT NULL REFERENCES uptime_monitors(id) ON DELETE CASCADE,
  checked_at INTEGER NOT NULL,
  ok INTEGER NOT NULL,
  status_code INTEGER,
  latency_ms INTEGER NOT NULL,
  -- dns | connect | tls | timeout | status | keyword | blocked; NULL when ok
  error TEXT
);
CREATE INDEX index_uptime_checks_on_monitor_id_and_checked_at ON uptime_checks (monitor_id, checked_at);
CREATE TABLE uptime_daily (
  monitor_id INTEGER NOT NULL REFERENCES uptime_monitors(id) ON DELETE CASCADE,
  day TEXT NOT NULL,
  checks INTEGER NOT NULL,
  failures INTEGER NOT NULL,
  p50_ms INTEGER,
  p95_ms INTEGER,
  PRIMARY KEY (monitor_id, day)
) WITHOUT ROWID;
CREATE TABLE uptime_incidents (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  monitor_id INTEGER NOT NULL REFERENCES uptime_monitors(id) ON DELETE CASCADE,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  cause TEXT NOT NULL,
  detail TEXT
);
CREATE INDEX index_uptime_incidents_on_monitor_id_and_started_at ON uptime_incidents (monitor_id, started_at);
CREATE TABLE status_pages (
  environment_id INTEGER PRIMARY KEY NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
  slug TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
";

fn uptime(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(UPTIME)
}

fn api_token_role_ceiling(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE api_tokens ADD COLUMN role_ceiling varchar")
}

/// One row at most (id 1).
fn trusted_proxy(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE trusted_proxy (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           account_id INTEGER NOT NULL,
           created_at TEXT NOT NULL
         )",
    )?;
    tx.execute(
        "INSERT INTO trusted_proxy (id, account_id, created_at) SELECT 1, min(id), ? FROM accounts HAVING min(id) IS NOT NULL",
        [crate::time::quoted_date_micros(crate::time::now_micros())],
    )?;
    Ok(())
}

const RETIRED_ENVIRONMENT_SLUGS: &str = "
CREATE TABLE retired_environment_slugs (
  slug TEXT PRIMARY KEY NOT NULL,
  retired_at TEXT NOT NULL
);
CREATE TRIGGER retire_environment_slug AFTER DELETE ON environments BEGIN
  INSERT OR IGNORE INTO retired_environment_slugs (slug, retired_at) VALUES (OLD.slug, strftime('%Y-%m-%d %H:%M:%f', 'now'));
END;
";

fn retired_environment_slugs(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(RETIRED_ENVIRONMENT_SLUGS)
}

fn session_token(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE sessions ADD COLUMN token varchar")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slug retirement lives in a trigger on `environments`; a later
    /// migration that rebuilds that table (SQLite's ALTER workaround) would
    /// drop it silently, and deleted slugs would be reused again (PR #107
    /// review note). After every migration it must exist and fire.
    #[test]
    fn the_slug_retirement_trigger_exists_after_every_migration() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys = on").unwrap();
        migrate(&mut c).unwrap();
        let sql: String = c
            .query_row("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'retire_environment_slug' AND tbl_name = 'environments'", [], |r| {
                r.get(0)
            })
            .expect("the retire_environment_slug trigger on environments");
        assert!(sql.contains("AFTER DELETE"), "{sql}");
        c.execute_batch(
            "INSERT INTO accounts (id, name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at) VALUES (1, 'a', 'a', 'p', 1, 30, 'x', 'x');
             INSERT INTO applications (id, account_id, name, slug, issue_prefix, created_at, updated_at) VALUES (1, 1, 'w', 'w', 'W', 'x', 'x');
             INSERT INTO environments (id, application_id, name, slug, token_digest, token_prefix, created_at, updated_at) VALUES (1, 1, 'p', 'a-w-p', 'd', 'rw_', 'x', 'x');
             DELETE FROM environments WHERE id = 1;",
        )
        .unwrap();
        let retired: i64 = c.query_row("SELECT COUNT(*) FROM retired_environment_slugs WHERE slug = 'a-w-p'", [], |r| r.get(0)).unwrap();
        assert_eq!(retired, 1, "and a delete records the slug");
    }

    #[test]
    fn a_phase1_file_moves_forward_keeping_ids_tokens_usage_and_deploys() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(PHASE1).unwrap();
        c.execute_batch(
            "INSERT INTO environments (slug, token_digest, paused, events_this_month, usage_month, retention_days, ingest_followup_gc_before, created_at)
               VALUES ('a', 'da', 0, 12, '2026-10', 30, 1760000000000000, '2026-10-01 00:00:00'),
                      ('b', 'db', 1, 0, NULL, 90, 0, '2026-10-02 00:00:00');
             INSERT INTO deploys (environment_id, deploy, deployed_at, created_at, updated_at) VALUES (2, 'v1', '2026-10-03 00:00:00', 'x', 'x');",
        )
        .unwrap();
        assert_eq!(migrate(&mut c).unwrap().len(), MIGRATIONS.len(), "phase1 (a no-op on this file) and every later migration apply");
        assert!(migrate(&mut c).unwrap().is_empty(), "a second run applies nothing");
        type Row = (i64, String, String, bool, i64, Option<String>, String);
        let rows: Vec<Row> = c
            .prepare(
                "SELECT e.id, e.slug, e.token_digest, e.paused_at IS NOT NULL, a.retention_days, e.usage_month, e.ingest_followup_gc_before
                 FROM environments e JOIN applications ap ON ap.id = e.application_id JOIN accounts a ON a.id = ap.account_id ORDER BY e.id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows[0], (1, "a".into(), "da".into(), false, 30, Some("2026-10-01".into()), "2025-10-09 08:53:20".into()));
        assert_eq!((rows[1].0, rows[1].3, rows[1].4), (2, true, 90));
        let n: i64 = c.query_row("SELECT count(*) FROM deploys WHERE environment_id = 2", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let usage: i64 = c.query_row("SELECT events FROM ingest_usage_months WHERE environment_id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(usage, 12);
    }

    /// A file from before the binding keeps what a trusted proxy granted:
    /// its lowest account. A file with no account yet stays unbound (setup
    /// binds the account it creates).
    #[test]
    fn the_trusted_proxy_is_bound_to_the_account_it_granted_before_the_upgrade() {
        let before: Vec<_> = MIGRATIONS.iter().take_while(|(v, _)| *v != "20261119000001_trusted_proxy").collect();
        let upgrade = |accounts: &[i64]| {
            let mut c = Connection::open_in_memory().unwrap();
            for (_, step) in &before {
                let tx = c.transaction().unwrap();
                step(&tx).unwrap();
                tx.commit().unwrap();
            }
            for id in accounts {
                c.execute(
                    "INSERT INTO accounts (id, name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at) VALUES (?1, 'a', ?1, 'p', 1, 30, 'x', 'x')",
                    [id],
                )
                .unwrap();
            }
            c.execute_batch("CREATE TABLE railwatch_migrations (version TEXT PRIMARY KEY NOT NULL, applied_at TEXT NOT NULL)").unwrap();
            for (version, _) in &before {
                c.execute("INSERT INTO railwatch_migrations (version, applied_at) VALUES (?, 'x')", [version]).unwrap();
            }
            assert_eq!(
                migrate(&mut c).unwrap(),
                vec![
                    "20261119000001_trusted_proxy",
                    "20261119000002_api_token_role_ceiling",
                    "20261119000010_retired_environment_slugs",
                    "20261121000001_session_token"
                ]
            );
            let ceiling: Option<String> = c.query_row("SELECT role_ceiling FROM api_tokens LIMIT 1", [], |r| r.get(0)).optional().unwrap().flatten();
            assert_eq!(ceiling, None, "existing tokens keep no ceiling");
            c.query_row("SELECT account_id FROM trusted_proxy", [], |r| r.get::<_, i64>(0)).optional().unwrap()
        };
        assert_eq!(upgrade(&[7, 3, 9]), Some(3));
        assert_eq!(upgrade(&[]), None);
    }
}
