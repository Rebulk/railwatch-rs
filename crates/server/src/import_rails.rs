//! `railwatch import-rails --primary <production.sqlite3> --telemetry-dir <dir>`:
//! the self-host migration from a railwatch-cloud install. Copies the
//! primary database's rows for every table the control DB shares with it
//! (same names, same columns, ids kept, so every foreign key and every
//! tenant file's slug still lines up), then adopts the tenant files where
//! they are: `telemetry/<slug>.sqlite3` already has the gem's schema.
//!
//! The source is never opened: it and its WAL are copied to a snapshot
//! beside the control DB and the snapshot is read (see `snapshot`). The target
//! must be a fresh control DB (no accounts yet): merging two installs is not
//! a goal. Each table is copied column by column by name: columns the Rails
//! table has that the control DB lacks (none today) are reported and
//! skipped, never silently.
//!
//! Tables copied: accounts, users, sessions, memberships, invitations,
//! applications, environments, api_tokens, deploys, saved_views,
//! ingest_usage_months, ingest_followup_receipts, plus whatever p2-rollups'
//! issue/alert tables the control DB has by the time this runs (the copy
//! is driven by the target's table list).
//!
//! Integration secrets: Rails encrypted `integrations.encrypted_settings`
//! with Active Record encryption. Given Rails' `active_record_encryption`
//! primary_key and key_derivation_salt (`ArKeys`), each one is decrypted
//! (active_record_encryption.rs) and re-encrypted with this install's
//! SettingsCipher in the same transaction. Without them, or for a row no
//! key opens, the Rails ciphertext is copied as is and reported: delivery
//! then fails with a message saying to re-enter it.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::active_record_encryption::Keys;
use crate::alerts::integration::SettingsCipher;
use crate::control::Control;

/// What `import-rails` needs to carry Rails-encrypted integration secrets
/// over: Rails' keys to read them, this install's cipher to write them.
pub struct Secrets {
    pub rails: Keys,
    pub cipher: SettingsCipher,
}

/// Parents before children, so foreign keys hold during the copy.
const ORDER: &[&str] = &[
    "accounts",
    "users",
    "sessions",
    "memberships",
    "invitations",
    "applications",
    "environments",
    "api_tokens",
    "deploys",
    "saved_views",
    "ingest_usage_months",
    "ingest_followup_receipts",
    // p2-rollups' tables, when present in the control DB.
    "integrations",
    "alert_rules",
    "issues",
    "issue_activities",
    "comments",
    "thresholds",
    "anomaly_rules",
    "alerts",
];

#[derive(Debug, Default)]
pub struct Report {
    pub tables: Vec<(String, usize)>,
    pub skipped_columns: Vec<(String, String)>,
    pub skipped_tables: Vec<String>,
    pub tenants_found: usize,
    pub tenants_missing: Vec<String>,
    pub tenants_dir: PathBuf,
    /// Integrations whose Rails-encrypted secrets were re-encrypted here.
    pub secrets_converted: usize,
    /// (integration id, why) for Rails-encrypted secrets left unreadable.
    pub secrets_unreadable: Vec<(i64, String)>,
}

fn columns(c: &Connection, schema: &str, table: &str) -> rusqlite::Result<Vec<String>> {
    c.prepare(&format!("SELECT name FROM {schema}.pragma_table_info(?)"))?.query_map([table], |r| r.get(0))?.collect()
}

fn tables(c: &Connection, schema: &str) -> rusqlite::Result<Vec<String>> {
    c.prepare(&format!("SELECT name FROM {schema}.sqlite_master WHERE type = 'table'"))?.query_map([], |r| r.get(0))?.collect()
}

pub fn run(control_path: &Path, primary: &Path, telemetry_dir: &Path, target_tenants: &Path, secrets: Option<&Secrets>) -> Result<Report, String> {
    if !primary.is_file() {
        return Err(format!("{}: no such file", primary.display()));
    }
    // Opening runs the control migrations, so the tables exist.
    drop(Control::open(control_path).map_err(|e| e.to_string())?);
    let mut c = Connection::open(control_path).map_err(|e| e.to_string())?;
    harden(&c).map_err(|e| e.to_string())?;
    let existing: i64 = c.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    let existing_envs: i64 = c.query_row("SELECT count(*) FROM environments", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if existing > 0 || existing_envs > 0 {
        return Err(format!("{} already has {existing} account(s) and {existing_envs} environment(s); import into a fresh data dir", control_path.display()));
    }
    // Even a read-only open of a WAL database writes its -shm, and the last
    // connection to close may checkpoint. So the source is never opened at
    // all: its file and WAL are copied to a snapshot beside the control DB,
    // and the snapshot is what gets attached (then deleted).
    let snapshot = snapshot(primary, control_path)?;
    let result = copy(&mut c, &snapshot.0, telemetry_dir, target_tenants, secrets);
    drop(snapshot);
    result
}

/// The Rails file is attached to this connection and comes from elsewhere,
/// so treat its schema as untrusted: no functions with side effects in its
/// views, triggers or generated columns (trusted_schema = OFF), and no
/// writable_schema or other ways to corrupt a file (DEFENSIVE).
fn harden(c: &Connection) -> rusqlite::Result<()> {
    use rusqlite::config::DbConfig;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    Ok(())
}

/// A copy of `primary` (+ its -wal) next to the control DB, removed on drop.
struct Snapshot(PathBuf);

impl Drop for Snapshot {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn snapshot(primary: &Path, control_path: &Path) -> Result<Snapshot, String> {
    let dst = control_path.with_file_name("import-rails-snapshot.sqlite3");
    let s = Snapshot(dst.clone());
    std::fs::copy(primary, &dst).map_err(|e| format!("copy {}: {e}", primary.display()))?;
    let wal = PathBuf::from(format!("{}-wal", primary.display()));
    if wal.is_file() {
        std::fs::copy(&wal, format!("{}-wal", dst.display())).map_err(|e| format!("copy {}: {e}", wal.display()))?;
    }
    Ok(s)
}

fn copy(c: &mut Connection, snapshot: &Path, telemetry_dir: &Path, target_tenants: &Path, secrets: Option<&Secrets>) -> Result<Report, String> {
    c.execute("ATTACH DATABASE ? AS rails", [snapshot.display().to_string()]).map_err(|e| format!("attach the snapshot: {e}"))?;
    let source_tables = tables(c, "rails").map_err(|e| e.to_string())?;
    let target_tables = tables(c, "main").map_err(|e| e.to_string())?;
    let mut report = Report { tenants_dir: target_tenants.to_path_buf(), ..Default::default() };

    let tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute_batch("PRAGMA defer_foreign_keys = on").map_err(|e| e.to_string())?;
    for table in ORDER {
        if !target_tables.iter().any(|t| t == table) {
            if source_tables.iter().any(|t| t == table) {
                let n: i64 = tx.query_row(&format!("SELECT count(*) FROM rails.\"{table}\""), [], |r| r.get(0)).map_err(|e| e.to_string())?;
                if n > 0 {
                    report.skipped_tables.push(format!("{table} ({n} rows; not in the control DB yet)"));
                }
            }
            continue;
        }
        if !source_tables.iter().any(|t| t == table) {
            continue;
        }
        let src = columns(&tx, "rails", table).map_err(|e| e.to_string())?;
        let dst = columns(&tx, "main", table).map_err(|e| e.to_string())?;
        let shared: Vec<&String> = src.iter().filter(|c| dst.contains(c)).collect();
        for col in src.iter().filter(|c| !dst.contains(c)) {
            report.skipped_columns.push(((*table).into(), col.clone()));
        }
        let cols = shared.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
        let n = tx
            .execute(&format!("INSERT INTO main.\"{table}\" ({cols}) SELECT {cols} FROM rails.\"{table}\" ORDER BY id"), [])
            .map_err(|e| format!("copying {table}: {e}"))?;
        report.tables.push(((*table).into(), n));
    }
    // AUTOINCREMENT counters, so new rows never reuse an imported id.
    tx.execute(
        "INSERT OR REPLACE INTO main.sqlite_sequence (name, seq) SELECT name, seq FROM rails.sqlite_sequence
         WHERE name IN (SELECT name FROM main.sqlite_master WHERE type = 'table')",
        [],
    )
    .map_err(|e| format!("sqlite_sequence: {e}"))?;
    if target_tables.iter().any(|t| t == "integrations") {
        convert_secrets(&tx, secrets, &mut report)?;
    }
    let fk: Vec<String> = tx
        .prepare("PRAGMA main.foreign_key_check")
        .and_then(|mut s| s.query_map([], |r| Ok(format!("{}#{}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect())
        .map_err(|e| e.to_string())?;
    if !fk.is_empty() {
        return Err(format!("foreign keys broken after the copy (nothing was written): {}", fk.join(", ")));
    }
    // Slugs name files and directories under telemetry/; the copy bypassed
    // insert_environment's check, so it runs here.
    let slugs: Vec<String> =
        tx.prepare("SELECT slug FROM main.environments ORDER BY id").and_then(|mut s| s.query_map([], |r| r.get(0))?.collect()).map_err(|e| e.to_string())?;
    if let Some(e) = slugs.iter().find_map(|s| crate::control::validate_slug(s).err()) {
        return Err(format!("environments: {e} (nothing was written)"));
    }
    tx.commit().map_err(|e| e.to_string())?;
    c.execute("DETACH DATABASE rails", []).map_err(|e| e.to_string())?;

    // Adopt the tenant files: link each environment's file into the data
    // dir's telemetry/ (a symlink when the directories differ, so nothing
    // is copied and the Rails install's files stay where they are).
    std::fs::create_dir_all(target_tenants).map_err(|e| e.to_string())?;
    let slugs: Vec<String> =
        c.prepare("SELECT slug FROM environments ORDER BY id").and_then(|mut s| s.query_map([], |r| r.get(0))?.collect()).map_err(|e| e.to_string())?;
    let same_dir = telemetry_dir.canonicalize().ok() == target_tenants.canonicalize().ok();
    for slug in slugs {
        let src = telemetry_dir.join(format!("{slug}.sqlite3"));
        // Importing the control DB into an install that already has its own
        // per-day telemetry for these environments (a server that ran beside
        // Rails, fed the same ingest): that directory is the environment's
        // data, adopted as is.
        if same_dir && crate::storage::Layout::detect(target_tenants, &slug).is_some() {
            report.tenants_found += 1;
            continue;
        }
        if !src.is_file() {
            report.tenants_missing.push(slug);
            continue;
        }
        report.tenants_found += 1;
        if same_dir {
            continue;
        }
        let dst = target_tenants.join(format!("{slug}.sqlite3"));
        if dst.exists() {
            return Err(format!("{} already exists", dst.display()));
        }
        let abs = src.canonicalize().map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&abs, &dst).map_err(|e| format!("link {}: {e}", dst.display()))?;
    }
    Ok(report)
}

/// Rails-encrypted `encrypted_settings` -> this install's SettingsCipher.
fn convert_secrets(tx: &Connection, secrets: Option<&Secrets>, report: &mut Report) -> Result<(), String> {
    let rows: Vec<(i64, String)> = tx
        .prepare("SELECT id, encrypted_settings FROM main.integrations WHERE encrypted_settings IS NOT NULL AND encrypted_settings != '' ORDER BY id")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
        .map_err(|e| e.to_string())?;
    for (id, stored) in rows {
        if !Keys::is_message(&stored) {
            continue;
        }
        let Some(s) = secrets else {
            report.secrets_unreadable.push((id, "no Rails active_record_encryption keys given".into()));
            continue;
        };
        let plain = s.rails.decrypt(&stored).and_then(|json| match serde_json::from_str(&json) {
            Ok(serde_json::Value::Object(m)) => Ok(m),
            _ => Err("decrypted, but not a JSON object".into()),
        });
        match plain {
            Ok(m) => {
                tx.execute("UPDATE main.integrations SET encrypted_settings = ? WHERE id = ?", rusqlite::params![s.cipher.encrypt(&m), id])
                    .map_err(|e| e.to_string())?;
                report.secrets_converted += 1;
            }
            Err(e) => report.secrets_unreadable.push((id, e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The import connection refuses writable_schema edits and treats an
    /// attached schema as untrusted.
    #[test]
    fn import_connection_is_defensive() {
        let c = Connection::open_in_memory().unwrap();
        harden(&c).unwrap();
        let trusted: i64 = c.query_row("PRAGMA trusted_schema", [], |r| r.get(0)).unwrap();
        assert_eq!(trusted, 0);
        c.execute_batch("CREATE TABLE t (x)").unwrap();
        let edit = c.execute_batch("PRAGMA writable_schema = ON; UPDATE sqlite_schema SET sql = 'CREATE TABLE t (y)' WHERE name = 't';");
        assert!(edit.is_err(), "DEFENSIVE refuses writable_schema edits");
    }

    #[test]
    fn copies_rails_rows_keeping_ids_and_refuses_a_used_target() {
        let dir = std::env::temp_dir().join(format!("rw-import-{}", crate::control::base58(8)));
        std::fs::create_dir_all(dir.join("rails-telemetry")).unwrap();
        let primary = dir.join("production.sqlite3");
        let r = Connection::open(&primary).unwrap();
        r.execute_batch(crate::control_schema::RAILS_PRIMARY).unwrap();
        r.execute_batch(
            "INSERT INTO accounts (id, name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at)
               VALUES (7, 'Acme', 'acme', 'team', 25000000, 30, '2026-01-01 00:00:00', '2026-01-01 00:00:00');
             INSERT INTO users (id, name, email, password_digest, verified, created_at, updated_at)
               VALUES (3, 'Ada', 'ada@example.com', '$2a$12$abcdefghijklmnopqrstuuJ8P5mTKMr0zTqYwT1sTQf4vCQiR1WZK', 1, '2026-01-01 00:00:00', '2026-01-01 00:00:00');
             INSERT INTO memberships (account_id, user_id, role, created_at, updated_at) VALUES (7, 3, 'owner', 'x', 'x');
             INSERT INTO applications (id, account_id, name, slug, issue_prefix, created_at, updated_at) VALUES (11, 7, 'Web', 'web', 'WEB', 'x', 'x');
             INSERT INTO environments (id, application_id, name, slug, token_digest, token_prefix, created_at, updated_at)
               VALUES (42, 11, 'production', 'acme-web-production', 'd1', 'rw_abc', 'x', 'x');",
        )
        .unwrap();
        // A live WAL, as a running Rails install has: rows only in the -wal.
        r.execute_batch("PRAGMA journal_mode = wal; PRAGMA wal_autocheckpoint = 0;").unwrap();
        r.execute("UPDATE users SET name = 'Ada Lovelace' WHERE id = 3", []).unwrap();
        let wal = std::fs::read(format!("{}-wal", primary.display())).unwrap();
        let main = std::fs::read(&primary).unwrap();
        // Copy the files while the writer still holds them open, as an
        // operator copying a live install's volume does.
        let copied = dir.join("copy.sqlite3");
        std::fs::copy(&primary, &copied).unwrap();
        std::fs::write(format!("{}-wal", copied.display()), &wal).unwrap();
        drop(r);
        let primary = copied;
        let (main_before, wal_before) = (std::fs::read(&primary).unwrap(), wal);
        assert_eq!(main_before, main);
        Connection::open(dir.join("rails-telemetry/acme-web-production.sqlite3")).unwrap().execute_batch("CREATE TABLE t (x)").unwrap();
        let control = dir.join("data/control.sqlite3");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let rep = run(&control, &primary, &dir.join("rails-telemetry"), &dir.join("data/telemetry"), None).unwrap();
        assert!(rep.tables.contains(&("environments".into(), 1)));
        assert_eq!((rep.tenants_found, rep.tenants_missing.len()), (1, 0));
        assert!(dir.join("data/telemetry/acme-web-production.sqlite3").is_symlink());
        assert_eq!(std::fs::read(&primary).unwrap(), main_before, "the source file is never written");
        assert_eq!(std::fs::read(format!("{}-wal", primary.display())).unwrap(), wal_before, "nor its WAL");
        assert!(!std::path::Path::new(&format!("{}-shm", primary.display())).exists(), "nor an -shm created");
        let c = Control::open(&control).unwrap();
        let env = c.authenticate("nope").unwrap();
        assert!(env.is_none());
        let id: i64 = c.with(|x| x.query_row("SELECT id FROM environments WHERE token_digest = 'd1'", [], |r| r.get(0))).unwrap();
        assert_eq!(id, 42);
        let name: String = c.with(|x| x.query_row("SELECT name FROM users WHERE id = 3", [], |r| r.get(0))).unwrap();
        assert_eq!(name, "Ada Lovelace", "rows still in the WAL are imported");
        // New rows continue after the imported ids.
        let next = c.create("fresh", "d2", 30).unwrap();
        assert!(next.id > 42);
        drop(c);
        assert!(run(&control, &primary, &dir.join("rails-telemetry"), &dir.join("data/telemetry"), None).is_err(), "a used target is refused");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_an_environment_slug_that_would_leave_the_telemetry_dir() {
        let dir = std::env::temp_dir().join(format!("rw-import-slug-{}", crate::control::base58(8)));
        std::fs::create_dir_all(dir.join("rails-telemetry")).unwrap();
        let primary = dir.join("production.sqlite3");
        let r = Connection::open(&primary).unwrap();
        r.execute_batch(crate::control_schema::RAILS_PRIMARY).unwrap();
        r.execute_batch(
            "INSERT INTO accounts (id, name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at)
               VALUES (7, 'Acme', 'acme', 'team', 25000000, 30, 'x', 'x');
             INSERT INTO applications (id, account_id, name, slug, issue_prefix, created_at, updated_at) VALUES (11, 7, 'Web', 'web', 'WEB', 'x', 'x');
             INSERT INTO environments (id, application_id, name, slug, token_digest, token_prefix, created_at, updated_at)
               VALUES (42, 11, 'production', '../escape', 'd1', 'rw_abc', 'x', 'x');",
        )
        .unwrap();
        drop(r);
        std::fs::write(dir.join("escape.sqlite3"), b"").unwrap();
        let control = dir.join("data/control.sqlite3");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let err = run(&control, &primary, &dir.join("rails-telemetry"), &dir.join("data/telemetry"), None).unwrap_err();
        assert!(err.contains("invalid slug \"../escape\""), "{err}");
        let c = Control::open(&control).unwrap();
        let n: i64 = c.with(|x| x.query_row("SELECT COUNT(*) FROM environments", [], |r| r.get(0))).unwrap();
        assert_eq!(n, 0, "nothing was written");
        assert!(!dir.join("data/escape.sqlite3").exists(), "no link outside telemetry/");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adopts_per_day_directories_already_in_the_data_dir() {
        let dir = std::env::temp_dir().join(format!("rw-import-days-{}", crate::control::base58(8)));
        let tenants = dir.join("data/telemetry");
        std::fs::create_dir_all(&tenants).unwrap();
        let primary = dir.join("production.sqlite3");
        let r = Connection::open(&primary).unwrap();
        r.execute_batch(crate::control_schema::RAILS_PRIMARY).unwrap();
        r.execute_batch(
            "INSERT INTO accounts (id, name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at)
               VALUES (7, 'Acme', 'acme', 'team', 25000000, 30, 'x', 'x');
             INSERT INTO applications (id, account_id, name, slug, issue_prefix, created_at, updated_at) VALUES (11, 7, 'Web', 'web', 'WEB', 'x', 'x');
             INSERT INTO environments (id, application_id, name, slug, token_digest, token_prefix, created_at, updated_at)
               VALUES (42, 11, 'production', 'acme-web-production', 'd1', 'rw_abc', 'x', 'x'),
                      (43, 11, 'staging', 'acme-web-staging', 'd2', 'rw_def', 'x', 'x');",
        )
        .unwrap();
        drop(r);
        // This install already holds production's per-day data; staging has none.
        crate::storage::layout::ensure(&tenants.join("acme-web-production")).unwrap();
        let rep = run(&dir.join("data/control.sqlite3"), &primary, &tenants, &tenants, None).unwrap();
        assert_eq!(rep.tenants_found, 1);
        assert_eq!(rep.tenants_missing, vec!["acme-web-staging".to_string()]);
        assert!(crate::storage::layout::meta_path(&tenants.join("acme-web-production")).is_file(), "left in place");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
