//! The control database (`control.sqlite3`): railwatch-cloud's primary
//! database minus billing (schema in control_schema/). Column names, value
//! formats (datetime(6) as "YYYY-MM-DD HH:MM:SS.ffffff", dates as
//! "YYYY-MM-DD", JSON as text) are Rails', so a Rails primary file can be
//! imported row for row (import_rails.rs).
//!
//! This file holds what ingest needs (Environment.authenticate, usage,
//! Kamal markers) plus the model rules the web app needs, named after the
//! Ruby models: Account#assign_slug, Application#assign_defaults,
//! Environment#assign_slug/#assign_token, User (has_secure_password),
//! Session, Membership, Invitation, ApiToken.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::control_schema::{self, SELF_HOSTED_PLAN, SELF_HOSTED_QUOTA};

/// What ingest needs of an environment (Environment + its account).
#[derive(Debug, Clone)]
pub struct Environment {
    pub id: i64,
    pub slug: String,
    pub paused: bool,
    /// Account#monthly_event_quota + additional_events_cap when the server
    /// enforces quota (`enforce_quota`); None otherwise.
    pub monthly_event_quota: Option<i64>,
    /// Account#events_this_month: the ledger, summed over the account.
    pub events_this_month: i64,
    pub usage_month: Option<String>,
    pub retention_days: i64,
    pub ingest_followup_gc_before: i64,
    pub raw_pruned_through: Option<i64>,
}

impl Environment {
    /// Account#quota_exhausted?. `events_this_month` is already the current
    /// month's ledger total, so the month argument only guards a stale read.
    pub fn quota_exhausted(&self, current_month: &str) -> bool {
        let used = if self.usage_month.as_deref() == Some(current_month) { self.events_this_month } else { 0 };
        self.monthly_event_quota.is_some_and(|q| used >= q)
    }

    /// Environment#ingest_floor in epoch seconds.
    pub fn ingest_floor(&self, now: f64) -> f64 {
        let retention = now - (self.retention_days * 86_400) as f64;
        match self.raw_pruned_through {
            Some(p) => retention.max(p as f64 / 1e6),
            None => retention,
        }
    }
}

pub struct Control {
    conn: Mutex<Connection>,
    enforce_quota: bool,
    /// The integration-settings key and SMTP for alert delivery, set once
    /// at startup (`configure_alerting`). Unset (tests, CLI), integrations
    /// whose secrets are encrypted fail delivery visibly.
    alerting: std::sync::OnceLock<crate::alerts::integration::Alerting>,
}

pub fn digest(token: &str) -> String {
    crate::ingest::receipt_headers::hex(&Sha256::digest(token.as_bytes()))
}

/// `SecureRandom.base58(n)`
pub fn base58(n: usize) -> String {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut out = String::with_capacity(n);
    let mut buf = [0u8; 64];
    while out.len() < n {
        getrandom::fill(&mut buf).expect("random");
        for b in buf {
            // Rejection sampling keeps every character uniform.
            if (b as usize) < 58 * 4 && out.len() < n {
                out.push(ALPHABET[b as usize % 58] as char);
            }
        }
    }
    out
}

/// `"rw_" + SecureRandom.base58(44)` (Environment#assign_token)
pub fn new_token() -> String {
    format!("rw_{}", base58(44))
}

pub fn now_db() -> String {
    crate::time::quoted_date_micros(crate::time::now_micros())
}

/// "YYYY-MM-DD HH:MM:SS[.ffffff]" -> epoch micros (0 when unparseable).
fn micros(s: &str) -> i64 {
    crate::time::parse_db_time(s).unwrap_or(0)
}

/// Errors a model write can fail with: validation messages per attribute,
/// as `record.errors` (`{"name": ["can't be blank"]}`), or SQLite.
#[derive(Debug)]
pub enum ModelError {
    Invalid(Vec<(String, String)>),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for ModelError {
    fn from(e: rusqlite::Error) -> Self {
        ModelError::Sql(e)
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelError::Invalid(errors) => {
                let parts: Vec<String> = errors.iter().map(|(k, m)| full_message(k, m)).collect();
                f.write_str(&parts.join(", "))
            }
            ModelError::Sql(e) => write!(f, "{e}"),
        }
    }
}

/// ActiveModel's full_message: "Name can't be blank".
pub fn full_message(attr: &str, message: &str) -> String {
    if attr == "base" {
        return message.to_string();
    }
    let human = attr.replace('_', " ");
    let mut c = human.chars();
    let human = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    format!("{human} {message}")
}

pub type ModelResult<T> = Result<T, ModelError>;

fn invalid<T>(attr: &str, message: &str) -> ModelResult<T> {
    Err(ModelError::Invalid(vec![(attr.into(), message.into())]))
}

fn unique_violation(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
}

/// ActiveSupport's String#parameterize for the inputs people type as names:
/// ASCII letters and digits kept (lower-cased), every other run becomes one
/// "-", trimmed. (Rails transliterates accented Latin letters first; those
/// are dropped here. docs/DIFFERENCES.md.)
pub fn parameterize(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    out
}

/// URI::MailTo::EMAIL_REGEXP
pub fn valid_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    let local_ok = !local.is_empty() && local.bytes().all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&b));
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = !domain.is_empty()
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-'));
    local_ok && domain_ok
}

const COLUMNS: &str = "e.id, e.slug, e.paused_at IS NOT NULL, a.monthly_event_quota + a.additional_events_cap,
  (SELECT COALESCE(SUM(m.events), 0) FROM ingest_usage_months m JOIN environments e2 ON e2.id = m.environment_id
     JOIN applications ap2 ON ap2.id = e2.application_id WHERE ap2.account_id = a.id AND m.month = ?2),
  a.retention_days, e.ingest_followup_gc_before, e.raw_pruned_through";

const FROM: &str = "FROM environments e JOIN applications ap ON ap.id = e.application_id JOIN accounts a ON a.id = ap.account_id";

fn month_start(month: &str) -> String {
    format!("{month}-01")
}

impl Control {
    pub fn open(path: &Path) -> rusqlite::Result<Control> {
        Self::open_with(path, false)
    }

    pub fn open_with(path: &Path, enforce_quota: bool) -> rusqlite::Result<Control> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.execute_batch("PRAGMA journal_mode = wal; PRAGMA synchronous = normal; PRAGMA foreign_keys = on;")?;
        control_schema::migrate(&mut conn)?;
        Ok(Control { conn: Mutex::new(conn), enforce_quota, alerting: std::sync::OnceLock::new() })
    }

    pub fn configure_alerting(&self, a: crate::alerts::integration::Alerting) {
        let _ = self.alerting.set(a);
    }

    pub fn alerting(&self) -> Option<&crate::alerts::integration::Alerting> {
        self.alerting.get()
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        f(&self.conn.lock().unwrap())
    }

    /// A write that may fail validation, in one IMMEDIATE transaction.
    pub fn tx<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> ModelResult<T>) -> ModelResult<T> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn ingest_row(&self, r: &rusqlite::Row, month: &str) -> rusqlite::Result<Environment> {
        Ok(Environment {
            id: r.get(0)?,
            slug: r.get(1)?,
            paused: r.get(2)?,
            monthly_event_quota: if self.enforce_quota { Some(r.get(3)?) } else { None },
            events_this_month: r.get(4)?,
            usage_month: Some(month.to_string()),
            retention_days: r.get(5)?,
            ingest_followup_gc_before: micros(&r.get::<_, String>(6)?),
            raw_pruned_through: r.get::<_, Option<String>>(7)?.map(|s| micros(&s)),
        })
    }

    /// Environment.authenticate
    pub fn authenticate(&self, token: &str) -> rusqlite::Result<Option<Environment>> {
        if token.is_empty() {
            return Ok(None);
        }
        let d = digest(token);
        let month = month_of(crate::time::now_micros());
        self.with(|c| {
            c.prepare_cached(&format!("SELECT {COLUMNS} {FROM} WHERE e.token_digest = ?1"))?
                .query_row(params![d, month_start(&month)], |r| self.ingest_row(r, &month))
                .optional()
        })
    }

    pub fn find(&self, id: i64) -> rusqlite::Result<Option<Environment>> {
        let month = month_of(crate::time::now_micros());
        self.with(|c| {
            c.prepare_cached(&format!("SELECT {COLUMNS} {FROM} WHERE e.id = ?1"))?
                .query_row(params![id, month_start(&month)], |r| self.ingest_row(r, &month))
                .optional()
        })
    }

    /// `railwatch env create/import`: an environment named `slug` in the
    /// self-hosted account/application for that retention (created on
    /// first use), with the given token digest.
    pub fn create(&self, slug: &str, token_digest: &str, retention_days: i64) -> rusqlite::Result<Environment> {
        validate_slug(slug).map_err(rusqlite::Error::InvalidParameterName)?;
        let id = self
            .tx(|tx| {
                let name = if retention_days == 30 { "default".to_string() } else { format!("default-{retention_days}d") };
                let account = match tx.query_row("SELECT id FROM accounts WHERE slug = ?", [&name], |r| r.get(0)).optional()? {
                    Some(id) => id,
                    None => insert_account(tx, &name, &name, retention_days)?,
                };
                let app = match tx.query_row("SELECT id FROM applications WHERE account_id = ? AND slug = 'default'", [account], |r| r.get(0)).optional()? {
                    Some(id) => id,
                    None => insert_application(tx, account, "default", "default", "APP", None, "main")?,
                };
                insert_environment(tx, app, slug, slug, token_digest, "rw_")
            })
            .map_err(|e| match e {
                ModelError::Sql(e) => e,
                ModelError::Invalid(m) => rusqlite::Error::InvalidParameterName(format!("{m:?}")),
            })?;
        Ok(self.find(id)?.expect("just created"))
    }

    pub fn list(&self) -> rusqlite::Result<Vec<Environment>> {
        let month = month_of(crate::time::now_micros());
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {COLUMNS} {FROM} ORDER BY e.id"))?;
            s.query_map(params![rusqlite::types::Null, month_start(&month)], |r| self.ingest_row(r, &month))?.collect()
        })
    }

    /// Environment#count_events!: the ledger row for the month, then the
    /// cached counter and last_seen_at, forward only (CACHE_FORWARD,
    /// LAST_SEEN_FORWARD), aggregated per second by followups.rs.
    pub fn count_events(&self, id: i64, month: &str, n: i64, last_seen: &str) -> rusqlite::Result<()> {
        let month = month_start(month);
        let now = now_db();
        self.with(|c| {
            c.prepare_cached(
                "INSERT INTO ingest_usage_months (environment_id, month, events, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT (environment_id, month) DO UPDATE SET events = events + excluded.events, updated_at = excluded.updated_at",
            )?
            .execute(params![id, month, n, now])?;
            c.prepare_cached(
                "UPDATE environments SET
                   events_this_month = CASE WHEN usage_month IS NULL OR usage_month < ?2 THEN ?3
                                            WHEN usage_month = ?2 THEN events_this_month + ?3
                                            ELSE events_this_month END,
                   usage_month = CASE WHEN usage_month IS NULL OR usage_month < ?2 THEN ?2 ELSE usage_month END,
                   last_seen_at = CASE WHEN last_seen_at IS NULL OR last_seen_at < ?4 THEN ?4 ELSE last_seen_at END
                 WHERE id = ?1",
            )?
            .execute(params![id, month, n, last_seen])?;
            Ok(())
        })
    }

    pub fn touch_last_seen(&self, id: i64, at: &str) -> rusqlite::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE environments SET last_seen_at = CASE WHEN last_seen_at IS NULL OR last_seen_at < ?2 THEN ?2 ELSE last_seen_at END WHERE id = ?1",
                params![id, at],
            )?;
            Ok(())
        })
    }

    pub fn set_kamal(&self, id: i64, hosts_json: &str, config_json: &str) -> rusqlite::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE environments SET expected_servers = ?2, kamal_config = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, hosts_json, config_json, now_db()],
            )?;
            Ok(())
        })
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Inserts with the models' before_validation defaults.

/// Account.create!(name:) with assign_slug; a self-hosted plan, no billing.
pub fn insert_account(tx: &Connection, name: &str, slug: &str, retention_days: i64) -> ModelResult<i64> {
    if name.trim().is_empty() {
        return invalid("name", "can't be blank");
    }
    let now = now_db();
    match tx.execute(
        "INSERT INTO accounts (name, slug, plan, monthly_event_quota, retention_days, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![name, slug, SELF_HOSTED_PLAN, SELF_HOSTED_QUOTA, retention_days, now],
    ) {
        Ok(_) => Ok(tx.last_insert_rowid()),
        Err(e) if unique_violation(&e) => invalid("slug", "has already been taken"),
        Err(e) => Err(e.into()),
    }
}

/// Account#assign_slug: name.parameterize, or 8 random hex characters; a
/// taken slug fails validation in Rails ("Slug has already been taken").
/// Here a numeric suffix is added instead, since every self-hosted user
/// names their account and collisions are ordinary (DIFFERENCES.md).
pub fn create_account(tx: &Connection, name: &str, owner_id: i64) -> ModelResult<i64> {
    let base = Some(parameterize(name)).filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("random");
        crate::ingest::receipt_headers::hex(&b)
    });
    let mut slug = base.clone();
    for n in 2.. {
        let taken: bool = tx.query_row("SELECT 1 FROM accounts WHERE slug = ?", [&slug], |_| Ok(true)).optional()?.unwrap_or(false);
        if !taken {
            break;
        }
        slug = format!("{base}-{n}");
    }
    let id = insert_account(tx, name.trim(), &slug, 30)?;
    insert_membership(tx, id, owner_id, "owner")?;
    Ok(id)
}

pub fn insert_membership(tx: &Connection, account_id: i64, user_id: i64, role: &str) -> ModelResult<i64> {
    let now = now_db();
    tx.execute(
        "INSERT INTO memberships (account_id, user_id, role, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (account_id, user_id) DO NOTHING",
        params![account_id, user_id, role, now],
    )?;
    Ok(tx.query_row("SELECT id FROM memberships WHERE account_id = ? AND user_id = ?", params![account_id, user_id], |r| r.get(0))?)
}

/// Application#assign_defaults + validations.
pub fn application_defaults(name: &str, issue_prefix: Option<&str>) -> (String, String) {
    let slug = parameterize(name);
    let prefix = issue_prefix.filter(|p| !p.is_empty()).map(str::to_string).unwrap_or_else(|| {
        let p: String = name.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_uppercase().chars().take(4).collect();
        if p.is_empty() { "APP".into() } else { p }
    });
    (slug, prefix)
}

pub fn validate_application(name: &str, slug: &str, issue_prefix: &str, repository_url: Option<&str>) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if name.trim().is_empty() {
        e.push(("name".into(), "can't be blank".into()));
    }
    if slug.is_empty() {
        e.push(("slug".into(), "can't be blank".into()));
    } else if !slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        e.push(("slug".into(), "is invalid".into()));
    }
    if issue_prefix.is_empty() {
        e.push(("issue_prefix".into(), "can't be blank".into()));
    } else {
        if issue_prefix.chars().count() > 8 {
            e.push(("issue_prefix".into(), "is too long (maximum is 8 characters)".into()));
        }
        if !issue_prefix.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            e.push(("issue_prefix".into(), "is invalid".into()));
        }
    }
    if let Some(u) = repository_url.filter(|u| !u.trim().is_empty()) {
        let ok = (u.starts_with("http://") || u.starts_with("https://"))
            && !u.chars().any(char::is_whitespace)
            && u.split_once("://").is_some_and(|(_, r)| !r.is_empty());
        if !ok {
            e.push(("repository_url".into(), "must be a single http:// or https:// URL".into()));
        }
    }
    e
}

pub fn insert_application(
    tx: &Connection,
    account_id: i64,
    name: &str,
    slug: &str,
    issue_prefix: &str,
    repository_url: Option<&str>,
    default_branch: &str,
) -> ModelResult<i64> {
    let errors = validate_application(name, slug, issue_prefix, repository_url);
    if !errors.is_empty() {
        return Err(ModelError::Invalid(errors));
    }
    let now = now_db();
    match tx.execute(
        "INSERT INTO applications (account_id, name, slug, issue_prefix, repository_url, default_branch, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![account_id, name, slug, issue_prefix, repository_url.filter(|u| !u.is_empty()), default_branch, now],
    ) {
        Ok(_) => {
            touch_account(tx, account_id)?;
            Ok(tx.last_insert_rowid())
        }
        Err(e) if unique_violation(&e) => invalid("slug", "has already been taken"),
        Err(e) => Err(e.into()),
    }
}

/// `belongs_to :account, touch: true`: the navigation cache key.
pub fn touch_account(tx: &Connection, account_id: i64) -> rusqlite::Result<()> {
    tx.execute("UPDATE accounts SET updated_at = ? WHERE id = ?", params![now_db(), account_id])?;
    Ok(())
}

pub fn touch_application(tx: &Connection, application_id: i64) -> rusqlite::Result<()> {
    let now = now_db();
    tx.execute("UPDATE applications SET updated_at = ? WHERE id = ?", params![now, application_id])?;
    tx.execute("UPDATE accounts SET updated_at = ? WHERE id = (SELECT account_id FROM applications WHERE id = ?)", params![now, application_id])?;
    Ok(())
}

/// Environment name validation: presence, format /\A[a-z0-9_-]+\z/.
pub fn validate_environment_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("can't be blank")
    } else if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        Some("is invalid")
    } else {
        None
    }
}

pub fn insert_environment(tx: &Connection, application_id: i64, name: &str, slug: &str, token_digest: &str, token_prefix: &str) -> ModelResult<i64> {
    if let Some(m) = validate_environment_name(name) {
        return invalid("name", m);
    }
    validate_slug(slug).or_else(|_| invalid("slug", "is invalid"))?;
    let taken_name: bool = tx
        .query_row("SELECT 1 FROM environments WHERE application_id = ? AND name = ?", params![application_id, name], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if taken_name {
        return invalid("name", "has already been taken");
    }
    let now = now_db();
    match tx.execute(
        "INSERT INTO environments (application_id, name, slug, token_digest, token_prefix, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![application_id, name, slug, token_digest, token_prefix, now],
    ) {
        Ok(_) => {
            touch_application(tx, application_id)?;
            Ok(tx.last_insert_rowid())
        }
        Err(e) if unique_violation(&e) => invalid("slug", "has already been taken"),
        Err(e) => Err(e.into()),
    }
}

/// Environment#assign_slug + #assign_token for a new environment of an
/// application: returns (id, plaintext token).
pub fn create_environment(tx: &Connection, application_id: i64, name: &str) -> ModelResult<(i64, String)> {
    let (account_slug, app_slug): (String, String) =
        tx.query_row("SELECT a.slug, ap.slug FROM applications ap JOIN accounts a ON a.id = ap.account_id WHERE ap.id = ?", [application_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let slug = format!("{account_slug}-{app_slug}-{name}");
    let token = new_token();
    let id = insert_environment(tx, application_id, name, &slug, &digest(&token), &token[..12])?;
    Ok((id, token))
}

/// Environment#rotate_token!
pub fn rotate_token(tx: &Connection, environment_id: i64) -> rusqlite::Result<String> {
    let token = new_token();
    tx.execute(
        "UPDATE environments SET token_digest = ?, token_prefix = ?, updated_at = ? WHERE id = ?",
        params![digest(&token), &token[..12], now_db(), environment_id],
    )?;
    Ok(token)
}

/// Slugs name tenant files and directories, so they must be path-safe.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    if !slug.is_empty() && slug.len() <= 128 && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        Ok(())
    } else {
        Err(format!("invalid slug {slug:?}: use a-z, 0-9, - and _"))
    }
}

/// "YYYY-MM" of a UTC microsecond time; usage is counted per month.
pub fn month_of(micros: i64) -> String {
    crate::time::utc_seconds_string(micros.div_euclid(1_000_000))[..7].to_string()
}

// ---------------------------------------------------------------------------
// Users (has_secure_password: a bcrypt digest Rails can verify too).

/// ActiveModel::SecurePassword uses BCrypt::Engine.cost (12 in production).
pub const BCRYPT_COST: u32 = 12;
/// User: `validates :password, length: {minimum: 12}`.
pub const MIN_PASSWORD: usize = 12;
/// has_secure_password: passwords over 72 bytes are invalid.
pub const MAX_PASSWORD_BYTES: usize = 72;

pub fn hash_password(password: &str) -> String {
    // bcrypt crate writes $2b$; Ruby's bcrypt reads $2a$/$2b$/$2y$ alike.
    bcrypt::hash(password, BCRYPT_COST).expect("bcrypt")
}

pub fn verify_password(password: &str, digest: &str) -> bool {
    bcrypt::verify(password, digest).unwrap_or(false)
}

/// A digest to compare against when the user does not exist, so a miss
/// costs what a hit costs (authenticate_by's timing equalisation).
pub fn dummy_digest() -> &'static str {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| hash_password(&base58(16)))
}

/// User validations for sign-up / password change; email normalised.
pub fn validate_new_user(name: &str, email: &str, password: &str, confirmation: Option<&str>) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if confirmation.is_some_and(|c| c != password) {
        e.push(("password_confirmation".into(), "doesn't match Password".into()));
    }
    if name.trim().is_empty() {
        e.push(("name".into(), "can't be blank".into()));
    }
    if email.is_empty() {
        e.push(("email".into(), "can't be blank".into()));
    } else if !valid_email(email) {
        e.push(("email".into(), "is invalid".into()));
    }
    e.extend(password_errors(password));
    e
}

pub fn password_errors(password: &str) -> Vec<(String, String)> {
    let mut e = Vec::new();
    if password.is_empty() {
        e.push(("password".into(), "can't be blank".into()));
    } else if password.chars().count() < MIN_PASSWORD {
        e.push(("password".into(), format!("is too short (minimum is {MIN_PASSWORD} characters)")));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        e.push(("password".into(), format!("is too long (maximum is {MAX_PASSWORD_BYTES} bytes)")));
    }
    e
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub password_digest: String,
    pub verified: bool,
    pub provider: Option<String>,
    pub editor: String,
    pub editor_root: Option<String>,
    pub weekly_digest: bool,
    pub created_at: String,
    pub updated_at: String,
    pub password_reset_sent_at: Option<String>,
}

const USER_COLUMNS: &str =
    "id, name, email, password_digest, verified, provider, editor, editor_root, weekly_digest, created_at, updated_at, password_reset_sent_at";

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        email: r.get(2)?,
        password_digest: r.get(3)?,
        verified: r.get(4)?,
        provider: r.get(5)?,
        editor: r.get(6)?,
        editor_root: r.get(7)?,
        weekly_digest: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        password_reset_sent_at: r.get(11)?,
    })
}

pub fn find_user(c: &Connection, id: i64) -> rusqlite::Result<Option<User>> {
    c.prepare_cached(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?"))?.query_row([id], user_row).optional()
}

pub fn find_user_by_email(c: &Connection, email: &str) -> rusqlite::Result<Option<User>> {
    c.prepare_cached(&format!("SELECT {USER_COLUMNS} FROM users WHERE email = ?"))?.query_row([normalize_email(email)], user_row).optional()
}

/// User.authenticate_by(email:, password:)
pub fn authenticate_by(c: &Connection, email: &str, password: &str) -> rusqlite::Result<Option<User>> {
    match find_user_by_email(c, email)? {
        Some(u) if verify_password(password, &u.password_digest) => Ok(Some(u)),
        Some(_) => Ok(None),
        None => {
            verify_password(password, dummy_digest());
            Ok(None)
        }
    }
}

/// User.create! for sign-up and setup. The caller validated.
pub fn insert_user(tx: &Connection, name: &str, email: &str, password: &str, verified: bool) -> ModelResult<i64> {
    let now = now_db();
    match tx.execute(
        "INSERT INTO users (name, email, password_digest, verified, terms_accepted_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?5)",
        params![name.trim(), normalize_email(email), hash_password(password), verified, now],
    ) {
        Ok(_) => Ok(tx.last_insert_rowid()),
        Err(e) if unique_violation(&e) => invalid("email", "has already been taken"),
        Err(e) => Err(e.into()),
    }
}

/// `after_update if: :password_digest_previously_changed?`: every other
/// session of the user is signed out.
pub fn set_password(tx: &Connection, user_id: i64, password: &str, keep_session: Option<i64>) -> rusqlite::Result<()> {
    tx.execute("UPDATE users SET password_digest = ?, updated_at = ? WHERE id = ?", params![hash_password(password), now_db(), user_id])?;
    tx.execute("DELETE FROM sessions WHERE user_id = ? AND id IS NOT ?", params![user_id, keep_session])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sessions

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    pub authenticated_at: Option<String>,
}

pub fn create_session(c: &Connection, user_id: i64, user_agent: Option<&str>, ip: Option<&str>) -> rusqlite::Result<i64> {
    let now = now_db();
    c.execute(
        "INSERT INTO sessions (user_id, user_agent, ip_address, authenticated_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4, ?4)",
        params![user_id, user_agent, ip, now],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn find_session(c: &Connection, id: i64) -> rusqlite::Result<Option<Session>> {
    c.prepare_cached("SELECT id, user_id, authenticated_at FROM sessions WHERE id = ?")?
        .query_row([id], |r| Ok(Session { id: r.get(0)?, user_id: r.get(1)?, authenticated_at: r.get(2)? }))
        .optional()
}

/// Session::RECENT_AUTHENTICATION_WINDOW (10 minutes).
pub fn recently_authenticated(s: &Session) -> bool {
    s.authenticated_at.as_deref().and_then(crate::time::parse_db_time).is_some_and(|t| crate::time::now_micros() - t < 600_000_000)
}

// ---------------------------------------------------------------------------
// Memberships / invitations / api tokens

pub const ROLES: [&str; 3] = ["owner", "admin", "member"];

pub fn membership_role(c: &Connection, account_id: i64, user_id: i64) -> rusqlite::Result<Option<String>> {
    c.prepare_cached("SELECT role FROM memberships WHERE account_id = ? AND user_id = ?")?.query_row(params![account_id, user_id], |r| r.get(0)).optional()
}

pub struct NewInvitation {
    pub id: i64,
    pub token: String,
}

/// Invitation.create!(email:, role:, inviter:): token base58(32), digest
/// stored, expires in 7 days.
pub fn create_invitation(tx: &Connection, account_id: i64, inviter_id: i64, email: &str, role: &str) -> ModelResult<NewInvitation> {
    let email = normalize_email(email);
    if email.is_empty() {
        return invalid("email", "can't be blank");
    }
    if !valid_email(&email) {
        return invalid("email", "is invalid");
    }
    if !["admin", "member"].contains(&role) {
        return invalid("role", "is not included in the list");
    }
    let token = base58(32);
    let now_us = crate::time::now_micros();
    let now = crate::time::quoted_date_micros(now_us);
    let expires = crate::time::quoted_date_micros(now_us + 7 * 86_400_000_000);
    match tx.execute(
        "INSERT INTO invitations (account_id, inviter_id, email, role, token_digest, expires_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![account_id, inviter_id, email, role, digest(&token), expires, now],
    ) {
        Ok(_) => Ok(NewInvitation { id: tx.last_insert_rowid(), token }),
        Err(e) if unique_violation(&e) => invalid("email", "already has a pending invitation"),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone)]
pub struct Invitation {
    pub id: i64,
    pub account_id: i64,
    pub inviter_id: i64,
    pub email: String,
    pub role: String,
}

/// Invitation.find_by_token (pending only).
pub fn find_invitation_by_token(c: &Connection, raw: &str) -> rusqlite::Result<Option<Invitation>> {
    if raw.is_empty() {
        return Ok(None);
    }
    c.query_row(
        "SELECT id, account_id, inviter_id, email, role FROM invitations WHERE token_digest = ? AND accepted_at IS NULL AND expires_at > ?",
        params![digest(raw), now_db()],
        |r| Ok(Invitation { id: r.get(0)?, account_id: r.get(1)?, inviter_id: r.get(2)?, email: r.get(3)?, role: r.get(4)? }),
    )
    .optional()
}

#[derive(Debug, PartialEq)]
pub enum AcceptError {
    RecipientMismatch,
    Unavailable,
}

/// Invitation#accept!: the conditional UPDATE is the linearisation point.
pub fn accept_invitation(tx: &Connection, inv: &Invitation, user: &User, raw: &str) -> ModelResult<Result<(), AcceptError>> {
    if inv.email != normalize_email(&user.email) {
        return Ok(Err(AcceptError::RecipientMismatch));
    }
    let now = now_db();
    let claimed = tx.execute(
        "UPDATE invitations SET accepted_at = ?1, updated_at = ?1 WHERE id = ?2 AND token_digest = ?3 AND accepted_at IS NULL AND expires_at > ?1",
        params![now, inv.id, digest(raw)],
    )?;
    if claimed != 1 {
        return Ok(Err(AcceptError::Unavailable));
    }
    insert_membership(tx, inv.account_id, user.id, &inv.role)?;
    Ok(Ok(()))
}

/// ApiToken.issue!: "rwp_" + base58(40), SHA-256 digest stored.
pub fn issue_api_token(
    tx: &Connection,
    user_id: i64,
    name: &str,
    scopes: &[String],
    account_id: Option<i64>,
    application_id: Option<i64>,
    expires_at: Option<String>,
) -> ModelResult<(i64, String)> {
    const SCOPES: [&str; 5] = ["api", "mcp", "read", "write", "security"];
    if name.trim().is_empty() {
        return invalid("name", "can't be blank");
    }
    if name.chars().count() > 100 {
        return invalid("name", "is too long (maximum is 100 characters)");
    }
    if scopes.is_empty() {
        return invalid("scopes", "can't be blank");
    }
    let unknown: Vec<&str> = scopes.iter().map(String::as_str).filter(|s| !SCOPES.contains(s)).collect();
    if !unknown.is_empty() {
        return invalid("scopes", &format!("contains unsupported values: {}", unknown.join(", ")));
    }
    let raw = format!("rwp_{}", base58(40));
    let now = now_db();
    let scopes_json = serde_json::to_string(scopes).expect("json");
    tx.execute(
        "INSERT INTO api_tokens (user_id, name, scopes, account_id, application_id, expires_at, token_digest, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![user_id, name, scopes_json, account_id, application_id, expires_at, digest(&raw), now],
    )?;
    Ok((tx.last_insert_rowid(), raw))
}

/// A throwaway password for fixture users (random, so no credential is
/// written into the source).
#[cfg(test)]
pub(crate) fn test_password() -> String {
    base58(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (std::path::PathBuf, Control) {
        let dir = std::env::temp_dir().join(format!("rw-control-{}-{}", std::process::id(), base58(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        (dir, c)
    }

    #[test]
    fn tokens_look_like_rails_tokens() {
        let t = new_token();
        assert_eq!(t.len(), 47);
        assert!(t.starts_with("rw_"));
        assert!(t[3..].bytes().all(|b| b.is_ascii_alphanumeric() && !b"0OIl".contains(&b)));
    }

    #[test]
    fn authenticate_by_digest_and_count_usage_into_the_ledger() {
        let (dir, c) = temp();
        let env = c.create("acme-web-production", &digest("rw_secret"), 30).unwrap();
        assert_eq!(c.authenticate("rw_secret").unwrap().unwrap().id, env.id);
        assert!(c.authenticate("rw_other").unwrap().is_none());
        assert!(c.authenticate("").unwrap().is_none());
        let month = month_of(crate::time::now_micros());
        c.count_events(env.id, &month, 5, "2026-10-13 00:00:00").unwrap();
        c.count_events(env.id, &month, 7, "2026-10-13 00:00:01").unwrap();
        assert_eq!(c.find(env.id).unwrap().unwrap().events_this_month, 12);
        // A charge for an older month goes to the ledger only.
        c.count_events(env.id, "2000-01", 1, "2000-01-01 00:00:00").unwrap();
        let cached: (i64, String) =
            c.with(|x| x.query_row("SELECT events_this_month, usage_month FROM environments WHERE id = ?", [env.id], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
        assert_eq!(cached, (12, format!("{month}-01")));
        assert!(c.create("../etc", "d", 30).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quota_is_the_accounts_and_only_enforced_when_configured() {
        let dir = std::env::temp_dir().join(format!("rw-control-q-{}", base58(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open_with(&dir.join("control.sqlite3"), true).unwrap();
        let a = c.create("a", &digest("rw_a"), 30).unwrap();
        let b = c.create("b", &digest("rw_b"), 30).unwrap();
        c.with(|x| x.execute("UPDATE accounts SET monthly_event_quota = 10", [])).unwrap();
        let month = month_of(crate::time::now_micros());
        c.count_events(a.id, &month, 6, "2026-10-13 00:00:00").unwrap();
        assert!(!c.find(b.id).unwrap().unwrap().quota_exhausted(&month));
        c.count_events(b.id, &month, 4, "2026-10-13 00:00:00").unwrap();
        assert!(c.find(a.id).unwrap().unwrap().quota_exhausted(&month), "sums every environment of the account");
        let off = Control::open(&dir.join("control.sqlite3")).unwrap();
        assert!(!off.find(a.id).unwrap().unwrap().quota_exhausted(&month));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn model_defaults_follow_the_ruby_models() {
        assert_eq!(parameterize("Acme Web!  2"), "acme-web-2");
        assert_eq!(application_defaults("Vossloh Tracks", None), ("vossloh-tracks".into(), "VOSS".into()));
        assert_eq!(application_defaults("!!", None).1, "APP");
        assert!(valid_email("a.b+c@example.co"));
        assert!(!valid_email("bad"));
        assert!(!valid_email("a@-x.com"));
        let (dir, c) = temp();
        let password = test_password();
        let user = c.tx(|tx| insert_user(tx, "Ada", " Ada@Example.com ", &password, false)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", user)).unwrap();
        let account2 = c.tx(|tx| create_account(tx, "Acme", user)).unwrap();
        let slugs: Vec<String> =
            c.with(|x| x.prepare("SELECT slug FROM accounts WHERE id IN (?, ?) ORDER BY id")?.query_map([account, account2], |r| r.get(0))?.collect()).unwrap();
        assert_eq!(slugs, ["acme", "acme-2"]);
        let app = c.tx(|tx| insert_application(tx, account, "Web", "web", "WEB", None, "main")).unwrap();
        let (env, token) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let (slug, prefix): (String, String) =
            c.with(|x| x.query_row("SELECT slug, token_prefix FROM environments WHERE id = ?", [env], |r| Ok((r.get(0)?, r.get(1)?)))).unwrap();
        assert_eq!((slug.as_str(), prefix.as_str()), ("acme-web-production", &token[..12]));
        assert!(matches!(c.tx(|tx| create_environment(tx, app, "production")), Err(ModelError::Invalid(e)) if e[0].1 == "has already been taken"));
        assert!(matches!(c.tx(|tx| create_environment(tx, app, "Prod")), Err(ModelError::Invalid(_))));
        let u = c.with(|x| authenticate_by(x, "ADA@example.com", &password)).unwrap().unwrap();
        assert_eq!(u.email, "ada@example.com");
        assert!(u.password_digest.starts_with("$2b$12$"));
        assert!(c.with(|x| authenticate_by(x, "ada@example.com", &format!("{password}x"))).unwrap().is_none());
        // A digest Ruby's bcrypt wrote (has_secure_password, cost 12, rw-baseline).
        let rails = "$2a$12$1EzDBnePTwOGg6V1MTqICOaxRDkMD1vVk5RM0fB6JBBMam5iMRCly";
        assert!(verify_password("correct horse battery staple", rails));
        assert!(!verify_password("correct horse battery stapl", rails));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invitations_are_single_use_and_bound_to_their_recipient() {
        let (dir, c) = temp();
        let owner = c.tx(|tx| insert_user(tx, "Owner", "o@example.com", &test_password(), true)).unwrap();
        let other = c.tx(|tx| insert_user(tx, "Other", "x@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", owner)).unwrap();
        let inv = c.tx(|tx| create_invitation(tx, account, owner, "New@Example.com", "member")).unwrap();
        assert!(matches!(c.tx(|tx| create_invitation(tx, account, owner, "new@example.com", "member")), Err(ModelError::Invalid(_))));
        let found = c.with(|x| find_invitation_by_token(x, &inv.token)).unwrap().unwrap();
        let wrong = c.with(|x| find_user(x, other)).unwrap().unwrap();
        assert_eq!(c.tx(|tx| accept_invitation(tx, &found, &wrong, &inv.token)).unwrap(), Err(AcceptError::RecipientMismatch));
        let newbie = c.tx(|tx| insert_user(tx, "New", "new@example.com", &test_password(), true)).unwrap();
        let newbie = c.with(|x| find_user(x, newbie)).unwrap().unwrap();
        assert_eq!(c.tx(|tx| accept_invitation(tx, &found, &newbie, &inv.token)).unwrap(), Ok(()));
        assert_eq!(c.tx(|tx| accept_invitation(tx, &found, &newbie, &inv.token)).unwrap(), Err(AcceptError::Unavailable));
        assert_eq!(c.with(|x| membership_role(x, account, newbie.id)).unwrap().as_deref(), Some("member"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
