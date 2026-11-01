//! Uptime monitors (docs/uptime.md), a Rust-only addition: this server
//! requests each monitor's URL on its interval (`probe`), records every
//! check, turns failures into incidents and `monitor_down` /
//! `monitor_recovered` alerts, and serves an optional public status page
//! per environment (web/status_page.rs).
//!
//! Everything lives in the control DB (control_schema migration 4), next to
//! the watchdog's heartbeats and the alert rules it fires: a monitor is
//! configuration plus a little state, a check is one small row, and the
//! probe loop writes for every environment at once, which per-tenant files
//! (meta commits last, under the ingest write lock) would serialize behind
//! ingest. It is bounded by construction, not by the telemetry budget:
//! MAX_MONITORS_PER_ENVIRONMENT x one check every MIN_INTERVAL_SECS x
//! RAW_DAYS of raw checks (about 20 MB per environment at the worst), plus
//! one row per monitor per day for DAILY_DAYS. It is backed up with the
//! control DB.
//!
//! State machine, persisted on the monitor row so a restart neither forgets
//! an outage nor alerts it twice:
//!
//! - pending (no check yet) or up: a failed check counts `failures`; the
//!   FAILURES_TO_DOWN-th in a row marks the monitor down, opens an incident
//!   that starts at the first failed check, and fires `monitor_down`;
//! - down: further failures change nothing; one successful check marks it
//!   up, closes the incident and fires `monitor_recovered`;
//! - paused (disabled): not checked; disabling closes an open incident
//!   without an alert; enabling starts again from pending.

pub mod probe;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::alerts::rules;
use crate::grouping::issue;
use crate::time;

pub const MIN_INTERVAL_SECS: i64 = 30;
pub const MAX_INTERVAL_SECS: i64 = 3600;
pub const DEFAULT_INTERVAL_SECS: i64 = 60;
pub const DEFAULT_TIMEOUT_SECS: i64 = 10;
pub const MAX_TIMEOUT_SECS: i64 = 30;
/// Consecutive failed checks before a monitor is down.
pub const FAILURES_TO_DOWN: i64 = 2;
/// Raw checks are kept this many whole UTC days before today.
pub const RAW_DAYS: i64 = 7;
/// Daily rollups (and closed incidents) are kept this many days.
pub const DAILY_DAYS: i64 = 90;
pub const MAX_MONITORS_PER_ENVIRONMENT: i64 = 20;
pub const MAX_NAME_CHARS: usize = 80;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_KEYWORD_CHARS: usize = 200;

const DAY_US: i64 = 86_400 * 1_000_000;

/// "YYYY-MM-DD" (UTC) of an instant.
pub fn day_of(micros: i64) -> String {
    time::utc_seconds_string(micros.div_euclid(1_000_000))[..10].to_string()
}

/// The UTC midnight starting `micros`' day.
pub fn day_start(micros: i64) -> i64 {
    micros - micros.rem_euclid(DAY_US)
}

/// Percent of checks that passed, None without checks.
pub fn uptime_pct(checks: i64, failures: i64) -> Option<f64> {
    (checks > 0).then(|| (checks - failures.clamp(0, checks)) as f64 * 100.0 / checks as f64)
}

/// Rounded down to 3 decimals, so 99.9996 never shows as 100.
fn pct_value(p: Option<f64>) -> Value {
    p.map_or(Value::Null, |p| json!((p * 1000.0).floor() / 1000.0))
}

// ---------------------------------------------------------------------------
// Monitors

/// What an admin submits.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInput {
    pub name: String,
    pub url: String,
    pub method: String,
    pub expected_status: Option<i64>,
    pub keyword: Option<String>,
    pub timeout_seconds: i64,
    pub interval_seconds: i64,
    pub enabled: bool,
    pub public: bool,
    pub public_name: Option<String>,
    pub public_url: bool,
}

impl Default for MonitorInput {
    fn default() -> Self {
        MonitorInput {
            name: String::new(),
            url: String::new(),
            method: "GET".into(),
            expected_status: None,
            keyword: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECS,
            interval_seconds: DEFAULT_INTERVAL_SECS,
            enabled: true,
            public: false,
            public_name: None,
            public_url: false,
        }
    }
}

/// Field errors, empty when valid. `policy` is the private-address rule.
pub fn validate(m: &MonitorInput, policy: probe::Policy) -> Vec<(String, String)> {
    let mut e = Vec::new();
    let mut err = |k: &str, v: String| e.push((k.to_string(), v));
    if m.name.trim().is_empty() {
        err("name", "can't be blank".into());
    } else if m.name.chars().count() > MAX_NAME_CHARS {
        err("name", format!("is too long (maximum is {MAX_NAME_CHARS} characters)"));
    }
    if m.url.trim().is_empty() {
        err("url", "can't be blank".into());
    } else if m.url.len() > MAX_URL_CHARS {
        err("url", format!("is too long (maximum is {MAX_URL_CHARS} characters)"));
    } else if let Err(msg) = probe::validate_url(&m.url, policy) {
        err("url", msg);
    }
    if !matches!(m.method.as_str(), "GET" | "HEAD") {
        err("method", "must be GET or HEAD".into());
    }
    if m.expected_status.is_some_and(|s| !(100..=599).contains(&s)) {
        err("expected_status", "must be an HTTP status between 100 and 599".into());
    }
    if m.keyword.as_ref().is_some_and(|k| k.chars().count() > MAX_KEYWORD_CHARS) {
        err("keyword", format!("is too long (maximum is {MAX_KEYWORD_CHARS} characters)"));
    }
    if m.keyword.is_some() && m.method == "HEAD" {
        err("keyword", "needs GET: a HEAD response has no body".into());
    }
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&m.interval_seconds) {
        err("interval_seconds", format!("must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS} seconds"));
    }
    if !(1..=MAX_TIMEOUT_SECS).contains(&m.timeout_seconds) {
        err("timeout_seconds", format!("must be between 1 and {MAX_TIMEOUT_SECS} seconds"));
    } else if m.timeout_seconds >= m.interval_seconds {
        err("timeout_seconds", "must be shorter than the interval".into());
    }
    if m.public_name.as_ref().is_some_and(|n| n.chars().count() > MAX_NAME_CHARS) {
        err("public_name", format!("is too long (maximum is {MAX_NAME_CHARS} characters)"));
    }
    e
}

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub id: i64,
    pub environment_id: i64,
    pub name: String,
    pub url: String,
    pub method: String,
    pub expected_status: Option<i64>,
    pub keyword: Option<String>,
    pub timeout_seconds: i64,
    pub interval_seconds: i64,
    pub enabled: bool,
    pub public: bool,
    pub public_name: Option<String>,
    pub public_url: bool,
    pub status: String,
    pub failures: i64,
    pub failing_since: Option<i64>,
    pub last_checked_at: Option<i64>,
    pub last_status_code: Option<i64>,
    pub last_latency_ms: Option<i64>,
    pub last_error: Option<String>,
}

impl Monitor {
    pub fn request(&self) -> probe::Request {
        probe::Request {
            url: self.url.clone(),
            head: self.method == "HEAD",
            expected_status: self.expected_status,
            keyword: self.keyword.clone(),
            timeout: std::time::Duration::from_secs(self.timeout_seconds.clamp(1, MAX_TIMEOUT_SECS) as u64),
        }
    }

    pub fn display_name(&self) -> &str {
        self.public_name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&self.name)
    }
}

const MONITOR_COLUMNS: &str = "id, environment_id, name, url, method, expected_status, keyword, timeout_seconds, interval_seconds, enabled, public, \
     public_name, public_url, status, failures, failing_since, last_checked_at, last_status_code, last_latency_ms, last_error";

fn monitor_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Monitor> {
    Ok(Monitor {
        id: r.get(0)?,
        environment_id: r.get(1)?,
        name: r.get(2)?,
        url: r.get(3)?,
        method: r.get(4)?,
        expected_status: r.get(5)?,
        keyword: r.get(6)?,
        timeout_seconds: r.get(7)?,
        interval_seconds: r.get(8)?,
        enabled: r.get(9)?,
        public: r.get(10)?,
        public_name: r.get(11)?,
        public_url: r.get(12)?,
        status: r.get(13)?,
        failures: r.get(14)?,
        failing_since: r.get(15)?,
        last_checked_at: r.get(16)?,
        last_status_code: r.get(17)?,
        last_latency_ms: r.get(18)?,
        last_error: r.get(19)?,
    })
}

pub fn monitors(c: &Connection, environment_id: i64) -> rusqlite::Result<Vec<Monitor>> {
    c.prepare_cached(&format!("SELECT {MONITOR_COLUMNS} FROM uptime_monitors WHERE environment_id = ? ORDER BY name, id"))?
        .query_map([environment_id], monitor_row)?
        .collect()
}

/// Every enabled monitor of an environment that is not paused (the probe loop's list).
pub fn active_monitors(c: &Connection) -> rusqlite::Result<Vec<Monitor>> {
    c.prepare_cached(&format!(
        "SELECT {MONITOR_COLUMNS} FROM uptime_monitors m WHERE enabled = 1
           AND EXISTS (SELECT 1 FROM environments e WHERE e.id = m.environment_id AND e.paused_at IS NULL) ORDER BY id"
    ))?
    .query_map([], monitor_row)?
    .collect()
}

pub fn find(c: &Connection, environment_id: i64, id: i64) -> rusqlite::Result<Option<Monitor>> {
    c.prepare_cached(&format!("SELECT {MONITOR_COLUMNS} FROM uptime_monitors WHERE environment_id = ? AND id = ?"))?
        .query_row(params![environment_id, id], monitor_row)
        .optional()
}

/// Inserts a monitor; Err(message) when the environment has its maximum.
pub fn insert(c: &Connection, environment_id: i64, m: &MonitorInput) -> rusqlite::Result<Result<i64, String>> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM uptime_monitors WHERE environment_id = ?", [environment_id], |r| r.get(0))?;
    if n >= MAX_MONITORS_PER_ENVIRONMENT {
        return Ok(Err(format!("An environment can have at most {MAX_MONITORS_PER_ENVIRONMENT} monitors")));
    }
    let now = crate::control::now_db();
    c.execute(
        "INSERT INTO uptime_monitors (environment_id, name, url, method, expected_status, keyword, timeout_seconds, interval_seconds, enabled,
           public, public_name, public_url, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, CASE WHEN ?9 THEN 'pending' ELSE 'paused' END, ?13, ?13)",
        params![
            environment_id,
            m.name.trim(),
            m.url.trim(),
            m.method,
            m.expected_status,
            m.keyword,
            m.timeout_seconds,
            m.interval_seconds,
            m.enabled,
            m.public,
            m.public_name.as_deref().map(str::trim),
            m.public_url,
            now
        ],
    )?;
    Ok(Ok(c.last_insert_rowid()))
}

/// Updates a monitor's settings. Disabling pauses it (an open incident is
/// closed now, without an alert); enabling a paused one starts from pending.
pub fn update(c: &Connection, id: i64, m: &MonitorInput, now_micros: i64) -> rusqlite::Result<()> {
    let was_enabled: bool = c.query_row("SELECT enabled FROM uptime_monitors WHERE id = ?", [id], |r| r.get(0))?;
    c.execute(
        "UPDATE uptime_monitors SET name = ?2, url = ?3, method = ?4, expected_status = ?5, keyword = ?6, timeout_seconds = ?7,
           interval_seconds = ?8, enabled = ?9, public = ?10, public_name = ?11, public_url = ?12, updated_at = ?13 WHERE id = ?1",
        params![
            id,
            m.name.trim(),
            m.url.trim(),
            m.method,
            m.expected_status,
            m.keyword,
            m.timeout_seconds,
            m.interval_seconds,
            m.enabled,
            m.public,
            m.public_name.as_deref().map(str::trim),
            m.public_url,
            crate::control::now_db()
        ],
    )?;
    if was_enabled && !m.enabled {
        c.execute("UPDATE uptime_incidents SET ended_at = ? WHERE monitor_id = ? AND ended_at IS NULL", params![now_micros, id])?;
        c.execute("UPDATE uptime_monitors SET status = 'paused', failures = 0, failing_since = NULL WHERE id = ?", [id])?;
    } else if !was_enabled && m.enabled {
        c.execute("UPDATE uptime_monitors SET status = 'pending', failures = 0, failing_since = NULL WHERE id = ?", [id])?;
    }
    Ok(())
}

/// Deletes a monitor with its checks, rollups and incidents (ON DELETE CASCADE).
pub fn delete(c: &Connection, id: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM uptime_monitors WHERE id = ?", [id]).map(|_| ())
}

// ---------------------------------------------------------------------------
// Recording a check

#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    Down { monitor_id: i64, incident_id: i64 },
    Recovered { monitor_id: i64, incident_id: Option<i64> },
}

/// Records one check of monitor `id` taken at `at`: the raw row, the day's
/// counts, the state machine (module docs) and its incident and alert, in
/// one transaction. None when nothing changed state, or the monitor was
/// deleted or disabled while the check ran (nothing is recorded then).
pub fn record(c: &Connection, id: i64, at: i64, o: &probe::Outcome) -> rusqlite::Result<Option<Transition>> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    // A failed COMMIT (SQLITE_BUSY) leaves the transaction open on the
    // shared control connection: roll back on any error, commit included.
    let r = record_in(c, id, at, o).and_then(|t| c.execute_batch("COMMIT").map(|_| t));
    if r.is_err() && !c.is_autocommit() {
        let _ = c.execute_batch("ROLLBACK");
    }
    r
}

fn record_in(c: &Connection, id: i64, at: i64, o: &probe::Outcome) -> rusqlite::Result<Option<Transition>> {
    let Some(m) = c.prepare_cached(&format!("SELECT {MONITOR_COLUMNS} FROM uptime_monitors WHERE id = ?"))?.query_row([id], monitor_row).optional()? else {
        return Ok(None);
    };
    if !m.enabled {
        return Ok(None);
    }
    let error = o.failure.map(|f| f.as_str());
    c.prepare_cached("INSERT INTO uptime_checks (monitor_id, checked_at, ok, status_code, latency_ms, error) VALUES (?, ?, ?, ?, ?, ?)")?.execute(params![
        id,
        at,
        o.ok(),
        o.status_code,
        o.latency_ms,
        error
    ])?;
    c.prepare_cached(
        "INSERT INTO uptime_daily (monitor_id, day, checks, failures) VALUES (?1, ?2, 1, ?3)
         ON CONFLICT (monitor_id, day) DO UPDATE SET checks = checks + 1, failures = failures + excluded.failures",
    )?
    .execute(params![id, day_of(at), !o.ok() as i64])?;
    c.prepare_cached("UPDATE uptime_monitors SET last_checked_at = ?2, last_status_code = ?3, last_latency_ms = ?4, last_error = ?5 WHERE id = ?1")?
        .execute(params![id, at, o.status_code, o.latency_ms, o.detail])?;
    let transition = if o.ok() {
        c.execute("UPDATE uptime_monitors SET status = 'up', failures = 0, failing_since = NULL WHERE id = ?", [id])?;
        if m.status == "down" {
            let incident: Option<i64> = c
                .query_row("SELECT id FROM uptime_incidents WHERE monitor_id = ? AND ended_at IS NULL ORDER BY id DESC LIMIT 1", [id], |r| r.get(0))
                .optional()?;
            c.execute("UPDATE uptime_incidents SET ended_at = ? WHERE monitor_id = ? AND ended_at IS NULL", params![at, id])?;
            Some(Transition::Recovered { monitor_id: id, incident_id: incident })
        } else {
            None
        }
    } else {
        let failures = m.failures + 1;
        let since = m.failing_since.unwrap_or(at);
        c.execute("UPDATE uptime_monitors SET failures = ?2, failing_since = ?3 WHERE id = ?1", params![id, failures, since])?;
        if m.status != "down" && failures >= FAILURES_TO_DOWN {
            c.execute("UPDATE uptime_monitors SET status = 'down' WHERE id = ?", [id])?;
            c.execute(
                "INSERT INTO uptime_incidents (monitor_id, started_at, cause, detail) VALUES (?, ?, ?, ?)",
                params![id, since, error.unwrap_or("unknown"), o.detail],
            )?;
            Some(Transition::Down { monitor_id: id, incident_id: c.last_insert_rowid() })
        } else {
            None
        }
    };
    if let Some(t) = &transition {
        let m = Monitor { failing_since: m.failing_since.or(Some(at)), ..m };
        fire(c, &m, t, at, o)?;
    }
    Ok(transition)
}

fn fire(c: &Connection, m: &Monitor, t: &Transition, at: i64, o: &probe::Outcome) -> rusqlite::Result<()> {
    use crate::rjson::Value as R;
    let env = m.environment_id;
    let environment = issue::environment_name(c, env);
    let (event, payload) = match t {
        Transition::Down { .. } => {
            let since = m.failing_since.unwrap_or(at);
            let cause = o.failure.map_or("unknown", |f| f.as_str());
            let detail = o.detail.clone().unwrap_or_default();
            (
                "monitor_down",
                crate::rhash! {
                    "monitor" => m.name.clone(), "monitor_id" => m.id, "url" => m.url.clone(),
                    "environment" => environment.clone(), "environment_id" => env,
                    "cause" => cause, "detail" => detail.clone(), "status_code" => o.status_code.map_or(R::Nil, R::from),
                    "since" => time::iso8601(since.div_euclid(1_000_000)),
                    "message" => format!(
                        "{} ({environment}) is down: {detail} [{cause}], failing since {} UTC. {}",
                        m.name, &time::utc_seconds_string(since.div_euclid(1_000_000))[11..19], m.url
                    ),
                },
            )
        }
        Transition::Recovered { incident_id, .. } => {
            let started: Option<i64> = match incident_id {
                Some(i) => c.query_row("SELECT started_at FROM uptime_incidents WHERE id = ?", [i], |r| r.get(0)).optional()?,
                None => None,
            };
            let down_for = started.map_or(0, |s| (at - s).div_euclid(1_000_000));
            (
                "monitor_recovered",
                crate::rhash! {
                    "monitor" => m.name.clone(), "monitor_id" => m.id, "url" => m.url.clone(),
                    "environment" => environment.clone(), "environment_id" => env,
                    "down_for_seconds" => down_for,
                    "since" => started.map_or(R::Nil, |s| R::from(time::iso8601(s.div_euclid(1_000_000)))),
                    "message" => format!("{} ({environment}) is up again after {} down. {}", m.name, duration_words(down_for), m.url),
                },
            )
        }
    };
    tracing::warn!(monitor = m.id, environment = env, event, "uptime");
    rules::fire_issueless(c, issue::application_of(c, env)?, event, &crate::alerts::as_hash(payload))?;
    Ok(())
}

/// "45 s", "12 min", "3 h 5 min", "2 d 4 h".
pub fn duration_words(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86_400 => {
            let (h, m) = (s / 3600, s % 3600 / 60);
            if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
        }
        s => {
            let (d, h) = (s / 86_400, s % 86_400 / 3600);
            if h == 0 { format!("{d} d") } else { format!("{d} d {h} h") }
        }
    }
}

// ---------------------------------------------------------------------------
// Rollups and retention

/// What `maintain` did.
#[derive(Debug, Default, PartialEq)]
pub struct Maintained {
    pub days_rolled: usize,
    pub checks_deleted: usize,
    pub days_deleted: usize,
    pub incidents_deleted: usize,
}

/// p50/p95 latency (successful checks) of today and yesterday from the raw
/// checks, then retention: raw checks before the RAW_DAYS-th day before
/// today, rollups and closed incidents older than DAILY_DAYS. Counts and
/// failures are kept current by `record`; only the percentiles need the
/// raw rows, and those two days still have all of theirs.
pub fn maintain(c: &Connection, now: i64) -> rusqlite::Result<Maintained> {
    let today = day_start(now);
    let mut out = Maintained::default();
    for start in [today - DAY_US, today] {
        let day = day_of(start);
        let ids: Vec<i64> =
            c.prepare("SELECT monitor_id FROM uptime_daily WHERE day = ?")?.query_map([&day], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for id in ids {
            let mut lat: Vec<i64> = c
                .prepare_cached("SELECT latency_ms FROM uptime_checks WHERE monitor_id = ? AND checked_at >= ? AND checked_at < ? AND ok = 1")?
                .query_map(params![id, start, start + DAY_US], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            lat.sort_unstable();
            let p = |q: usize| (!lat.is_empty()).then(|| crate::dashboard::request_scan::percentile(&lat, q));
            c.execute("UPDATE uptime_daily SET p50_ms = ?, p95_ms = ? WHERE monitor_id = ? AND day = ?", params![p(50), p(95), id, day])?;
            out.days_rolled += 1;
        }
    }
    out.checks_deleted = c.execute("DELETE FROM uptime_checks WHERE checked_at < ?", [today - RAW_DAYS * DAY_US])?;
    out.days_deleted = c.execute("DELETE FROM uptime_daily WHERE day < ?", [day_of(today - (DAILY_DAYS - 1) * DAY_US)])?;
    out.incidents_deleted = c.execute("DELETE FROM uptime_incidents WHERE ended_at IS NOT NULL AND ended_at < ?", [now - DAILY_DAYS * DAY_US])?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Reads (dashboard, MCP, API, status page)

/// Uptime over the last `days` UTC days including today, from the rollups.
pub fn uptime_days(c: &Connection, id: i64, now: i64, days: i64) -> rusqlite::Result<Option<f64>> {
    let (checks, failures): (i64, i64) = c
        .prepare_cached("SELECT COALESCE(SUM(checks), 0), COALESCE(SUM(failures), 0) FROM uptime_daily WHERE monitor_id = ? AND day >= ?")?
        .query_row(params![id, day_of(day_start(now) - (days - 1) * DAY_US)], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(uptime_pct(checks, failures))
}

/// Uptime over the last 24 hours, from the raw checks.
pub fn uptime_24h(c: &Connection, id: i64, now: i64) -> rusqlite::Result<Option<f64>> {
    let (checks, failures): (i64, i64) = c
        .prepare_cached("SELECT COUNT(*), COALESCE(SUM(ok = 0), 0) FROM uptime_checks WHERE monitor_id = ? AND checked_at > ?")?
        .query_row(params![id, now - DAY_US], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(uptime_pct(checks, failures))
}

/// The four windows the page, MCP and API show.
pub fn uptime_windows(c: &Connection, id: i64, now: i64) -> rusqlite::Result<Value> {
    Ok(json!({
        "24h": pct_value(uptime_24h(c, id, now)?),
        "7d": pct_value(uptime_days(c, id, now, 7)?),
        "30d": pct_value(uptime_days(c, id, now, 30)?),
        "90d": pct_value(uptime_days(c, id, now, 90)?),
    }))
}

/// One entry per day of the last `days` (oldest first): uptime (null
/// without checks), checks, failures, p50/p95.
pub fn daily(c: &Connection, id: i64, now: i64, days: i64) -> rusqlite::Result<Vec<Value>> {
    let first = day_start(now) - (days - 1) * DAY_US;
    let mut rows: std::collections::HashMap<String, (i64, i64, Option<i64>, Option<i64>)> = c
        .prepare_cached("SELECT day, checks, failures, p50_ms, p95_ms FROM uptime_daily WHERE monitor_id = ? AND day >= ?")?
        .query_map(params![id, day_of(first)], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((0..days)
        .map(|i| {
            let day = day_of(first + i * DAY_US);
            let (checks, failures, p50, p95) = rows.remove(&day).unwrap_or((0, 0, None, None));
            json!({"date": day, "uptime": pct_value(uptime_pct(checks, failures)), "checks": checks, "failures": failures, "p50_ms": p50, "p95_ms": p95})
        })
        .collect())
}

/// Average latency of successful checks per `bucket_secs` since `from`.
pub fn latency_series(c: &Connection, id: i64, from: i64, bucket_secs: i64) -> rusqlite::Result<Vec<Value>> {
    let b = bucket_secs * 1_000_000;
    c.prepare_cached(
        "SELECT (checked_at / ?1) * ?1, CAST(ROUND(AVG(latency_ms)) AS INTEGER) FROM uptime_checks
         WHERE monitor_id = ?2 AND checked_at >= ?3 AND ok = 1 GROUP BY 1 ORDER BY 1",
    )?
    .query_map(params![b, id, from], |r| Ok(json!({"t": time::iso8601(r.get::<_, i64>(0)?.div_euclid(1_000_000)), "value": r.get::<_, i64>(1)?})))?
    .collect()
}

#[derive(Debug, Clone)]
pub struct Incident {
    pub id: i64,
    pub monitor_id: i64,
    pub monitor: String,
    pub public_name: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub cause: String,
    pub detail: Option<String>,
}

impl Incident {
    pub fn duration_secs(&self, now: i64) -> i64 {
        (self.ended_at.unwrap_or(now) - self.started_at).div_euclid(1_000_000)
    }
}

/// The newest `limit` incidents of an environment's monitors (only the
/// public monitors' when `public_only`, only `monitor`'s when given), open
/// ones first.
pub fn incidents(c: &Connection, environment_id: i64, limit: i64, public_only: bool, monitor: Option<i64>) -> rusqlite::Result<Vec<Incident>> {
    let public = if public_only { " AND m.public = 1 AND m.enabled = 1" } else { "" };
    c.prepare(&format!(
        "SELECT i.id, i.monitor_id, m.name, COALESCE(NULLIF(TRIM(m.public_name), ''), m.name), i.started_at, i.ended_at, i.cause, i.detail
         FROM uptime_incidents i JOIN uptime_monitors m ON m.id = i.monitor_id
         WHERE m.environment_id = ?1 AND (?2 IS NULL OR m.id = ?2){public} ORDER BY i.ended_at IS NOT NULL, i.started_at DESC LIMIT ?3"
    ))?
    .query_map(params![environment_id, monitor, limit], |r| {
        Ok(Incident {
            id: r.get(0)?,
            monitor_id: r.get(1)?,
            monitor: r.get(2)?,
            public_name: r.get(3)?,
            started_at: r.get(4)?,
            ended_at: r.get(5)?,
            cause: r.get(6)?,
            detail: r.get(7)?,
        })
    })?
    .collect()
}

/// When the monitor's current status began: the open incident's start
/// while down, else the end of its last incident.
pub fn status_since(c: &Connection, m: &Monitor) -> rusqlite::Result<Option<i64>> {
    let sql = if m.status == "down" {
        "SELECT started_at FROM uptime_incidents WHERE monitor_id = ? AND ended_at IS NULL ORDER BY id DESC LIMIT 1"
    } else {
        "SELECT MAX(ended_at) FROM uptime_incidents WHERE monitor_id = ?"
    };
    Ok(c.query_row(sql, [m.id], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten())
}

// ---------------------------------------------------------------------------
// Status pages

#[derive(Debug, Clone, PartialEq)]
pub struct StatusPage {
    pub environment_id: i64,
    pub slug: String,
    pub title: String,
    pub enabled: bool,
}

pub const SLUG_MIN: usize = 3;
pub const SLUG_MAX: usize = 64;

/// A new unguessable slug: 20 lowercase base58 characters (~100 bits).
pub fn new_slug() -> String {
    crate::control::base58(20).to_ascii_lowercase()
}

/// A chosen slug: lowercase letters, digits and inner hyphens.
pub fn validate_slug(s: &str) -> Option<String> {
    let ok = (SLUG_MIN..=SLUG_MAX).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    (!ok).then(|| format!("must be {SLUG_MIN}-{SLUG_MAX} lowercase letters, digits or hyphens"))
}

pub fn status_page(c: &Connection, environment_id: i64) -> rusqlite::Result<Option<StatusPage>> {
    c.query_row("SELECT environment_id, slug, title, enabled FROM status_pages WHERE environment_id = ?", [environment_id], |r| {
        Ok(StatusPage { environment_id: r.get(0)?, slug: r.get(1)?, title: r.get(2)?, enabled: r.get(3)? })
    })
    .optional()
}

/// Creates or updates an environment's status page. Err(field, message)
/// when the slug is taken by another environment.
pub fn save_status_page(c: &Connection, environment_id: i64, slug: &str, title: &str, enabled: bool) -> rusqlite::Result<Result<(), (String, String)>> {
    let taken: bool =
        c.query_row("SELECT EXISTS (SELECT 1 FROM status_pages WHERE slug = ? AND environment_id != ?)", params![slug, environment_id], |r| r.get(0))?;
    if taken {
        return Ok(Err(("slug".into(), "has already been taken".into())));
    }
    let now = crate::control::now_db();
    c.execute(
        "INSERT INTO status_pages (environment_id, slug, title, enabled, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT (environment_id) DO UPDATE SET slug = excluded.slug, title = excluded.title, enabled = excluded.enabled, updated_at = excluded.updated_at",
        params![environment_id, slug, title.trim(), enabled, now],
    )?;
    Ok(Ok(()))
}

/// Overall status from component statuses: every one up is operational,
/// some down degraded, all down down; nothing checked yet is unknown.
pub fn overall(statuses: &[&str]) -> &'static str {
    let checked: Vec<&&str> = statuses.iter().filter(|s| matches!(**s, "up" | "down")).collect();
    let down = checked.iter().filter(|s| ***s == "down").count();
    match (checked.len(), down) {
        (0, _) => "unknown",
        (_, 0) => "operational",
        (n, d) if d == n => "down",
        _ => "degraded",
    }
}

/// The public view of an enabled status page, None for an unknown or
/// disabled slug. Holds only what the admin marked public: display names,
/// URLs only where allowed, statuses, uptime and incident times. No ids,
/// no causes, no error text.
pub fn public_status(c: &Connection, slug: &str, now: i64) -> rusqlite::Result<Option<Value>> {
    let Some((env, title)) = c
        .query_row(
            "SELECT p.environment_id, p.title FROM status_pages p JOIN environments e ON e.id = p.environment_id WHERE p.slug = ? AND p.enabled = 1",
            [slug],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let shown: Vec<Monitor> = monitors(c, env)?.into_iter().filter(|m| m.public && m.enabled).collect();
    let mut components = Vec::new();
    for m in &shown {
        let days: Vec<Value> = daily(c, m.id, now, DAILY_DAYS)?.into_iter().map(|d| json!({"date": d["date"], "uptime": d["uptime"]})).collect();
        let mut v = json!({
            "name": m.display_name(),
            "status": if matches!(m.status.as_str(), "up" | "down") { m.status.as_str() } else { "unknown" },
            "uptime_90d": pct_value(uptime_days(c, m.id, now, DAILY_DAYS)?),
            "days": days,
        });
        if m.public_url {
            v["url"] = json!(m.url);
        }
        components.push(v);
    }
    let statuses: Vec<&str> = shown.iter().map(|m| m.status.as_str()).collect();
    let incidents: Vec<Value> = incidents(c, env, 10, true, None)?
        .iter()
        .map(|i| {
            json!({"component": i.public_name, "started_at": time::iso8601(i.started_at.div_euclid(1_000_000)),
                   "ended_at": i.ended_at.map(|e| time::iso8601(e.div_euclid(1_000_000))), "duration_seconds": i.duration_secs(now)})
        })
        .collect();
    Ok(Some(json!({
        "title": title,
        "status": overall(&statuses),
        "updated_at": time::iso8601(now.div_euclid(1_000_000)),
        "components": components,
        "incidents": incidents,
    })))
}

#[cfg(test)]
mod tests;
