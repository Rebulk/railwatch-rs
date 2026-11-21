//! Incidents: one per (environment, actor) while open, in the control DB
//! (control migration `security_watcher`). A run's signals are folded into
//! them in one short transaction:
//!
//! - an actor with no open incident and a signal of at least `low` opens
//!   one (`info` signals alone open nothing; they join an open incident as
//!   context);
//! - a signal already on the incident (same detector and subject) is
//!   updated in place, so the window sliding over the same requests every
//!   tick changes nothing;
//! - the incident's severity is the highest it has seen and never goes
//!   down; a higher one is an escalation;
//! - an incident with no new evidence for `quiet_close_min` (measured on the
//!   evidence's own times, not on ticks) closes. Later activity opens a new
//!   one.
//!
//! An open or escalation at or above `alert_min_severity` fires a
//! `security_incident` alert (once per severity reached), and the close of
//! an incident that alerted fires one too. Incidents that never reached the
//! line are listed once a day in a `security_digest` alert. In shadow mode
//! both go only to the watcher's shadow integration, and only when it
//! belongs to the application's account.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::detect::History;
use super::policy::{AppPolicy, Mode, Watcher};
use super::{Event, EventKind, Severity, Signal};
use crate::alerts::{as_hash, outbox, rules};
use crate::time;

pub const MIGRATION: &str = "
CREATE TABLE security_incidents (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
  -- the client address, or user:<ref> when the app sent none
  actor TEXT NOT NULL,
  -- open | closed
  status TEXT NOT NULL DEFAULT 'open',
  -- info | low | medium | high | critical; only ever raised
  severity TEXT NOT NULL,
  -- touched an account, a session or an OAuth client
  identity INTEGER NOT NULL DEFAULT 0,
  -- times: epoch microseconds; opened_at and last_signal_at are the
  -- evidence's own times, the rest the watcher's clock
  opened_at INTEGER NOT NULL,
  last_signal_at INTEGER NOT NULL,
  escalated_at INTEGER,
  closed_at INTEGER,
  -- the highest severity a security_incident alert went out for
  alerted_severity TEXT,
  digested_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX index_security_incidents_open_actor ON security_incidents (environment_id, actor) WHERE status = 'open';
CREATE INDEX index_security_incidents_on_environment_and_opened ON security_incidents (environment_id, opened_at);
CREATE TABLE security_incident_signals (
  incident_id INTEGER NOT NULL REFERENCES security_incidents(id) ON DELETE CASCADE,
  detector TEXT NOT NULL,
  subject TEXT NOT NULL DEFAULT '',
  severity TEXT NOT NULL,
  first_at INTEGER NOT NULL,
  last_at INTEGER NOT NULL,
  count INTEGER NOT NULL,
  summary TEXT NOT NULL,
  identity INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (incident_id, detector, subject)
) WITHOUT ROWID;
CREATE TABLE security_incident_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  incident_id INTEGER NOT NULL REFERENCES security_incidents(id) ON DELETE CASCADE,
  -- opened | escalated | closed
  kind TEXT NOT NULL,
  severity TEXT NOT NULL,
  previous_severity TEXT,
  -- the signal that caused it (none for a close)
  detector TEXT,
  at INTEGER NOT NULL,
  alert_id INTEGER
);
CREATE INDEX index_security_incident_events_on_incident ON security_incident_events (incident_id, id);
-- Accounts seen signing up, so a later run knows an account's age and an
-- address's earlier signups after their events left the window.
CREATE TABLE security_accounts (
  environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
  user_ref TEXT NOT NULL,
  ip TEXT,
  signed_up_at INTEGER NOT NULL,
  PRIMARY KEY (environment_id, user_ref)
) WITHOUT ROWID;
CREATE INDEX index_security_accounts_on_environment_and_time ON security_accounts (environment_id, signed_up_at);
CREATE TABLE security_digests (
  environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
  day TEXT NOT NULL,
  incidents INTEGER NOT NULL,
  alert_id INTEGER,
  PRIMARY KEY (environment_id, day)
) WITHOUT ROWID;
";

/// Signals one run folds in at most (the most severe first): a flood of
/// distinct addresses cannot make one transaction unbounded.
pub const MAX_SIGNALS_PER_RUN: usize = 2_000;
/// Signals an alert carries.
const ALERT_SIGNALS: usize = 20;
/// Closed incidents are kept this long.
pub const KEEP_CLOSED_DAYS: i64 = 90;
const DAY_US: i64 = 86_400 * 1_000_000;
const MIN_US: i64 = 60_000_000;

/// The environment a run is for, with what alerts name.
#[derive(Debug, Clone)]
pub struct Target {
    pub environment_id: i64,
    pub environment_slug: String,
    pub environment: String,
    pub application_id: i64,
    pub application: String,
    pub account_id: i64,
    pub account: String,
}

/// What one run changed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Applied {
    pub opened: Vec<i64>,
    pub escalated: Vec<i64>,
    pub closed: Vec<i64>,
    pub alerts: Vec<i64>,
    pub signals_dropped: usize,
}

fn sev(s: &str) -> Severity {
    Severity::parse(s).unwrap_or(Severity::Info)
}

/// What the detectors need from earlier runs.
pub fn history(c: &Connection, environment_id: i64, now: i64, lookback_hours: i64) -> rusqlite::Result<History> {
    let mut h = History::default();
    let mut s = c.prepare_cached("SELECT actor, severity FROM security_incidents WHERE environment_id = ? AND status = 'open' LIMIT 10000")?;
    for r in s.query_map([environment_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (a, v) = r?;
        h.open.insert(a, sev(&v));
    }
    let mut s = c.prepare_cached(
        "SELECT user_ref, ip, signed_up_at FROM security_accounts WHERE environment_id = ? AND signed_up_at >= ? ORDER BY signed_up_at DESC LIMIT 10000",
    )?;
    for r in s.query_map(params![environment_id, now - lookback_hours.max(1) * 3600 * 1_000_000], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?))
    })? {
        let (u, ip, at) = r?;
        h.accounts.insert(u, (ip, at));
    }
    Ok(h)
}

/// Folds one run's signals into the environment's incidents, closes the
/// quiet ones and fires what that calls for, in one transaction.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    c: &Connection,
    t: &Target,
    app: &AppPolicy,
    w: &Watcher,
    signals: &[Signal],
    events: &[Event],
    now: i64,
    base_url: &str,
) -> rusqlite::Result<Applied> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    match apply_in(c, t, app, w, signals, events, now, base_url) {
        Ok(a) => {
            c.execute_batch("COMMIT")?;
            Ok(a)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_in(
    c: &Connection,
    t: &Target,
    app: &AppPolicy,
    w: &Watcher,
    signals: &[Signal],
    events: &[Event],
    now: i64,
    base_url: &str,
) -> rusqlite::Result<Applied> {
    let mut out = Applied::default();
    for e in events.iter().filter(|e| e.kind == EventKind::Signup) {
        if let Some(u) = e.user_ref.as_deref().filter(|u| !u.is_empty()) {
            c.prepare_cached("INSERT OR IGNORE INTO security_accounts (environment_id, user_ref, ip, signed_up_at) VALUES (?, ?, ?, ?)")?.execute(params![
                t.environment_id,
                super::text(u),
                e.ip.as_deref().map(super::text),
                e.at
            ])?;
        }
    }
    let mut kept: Vec<&Signal> = signals.iter().collect();
    if kept.len() > MAX_SIGNALS_PER_RUN {
        kept.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.at.cmp(&b.at)));
        out.signals_dropped = kept.len() - MAX_SIGNALS_PER_RUN;
        kept.truncate(MAX_SIGNALS_PER_RUN);
        tracing::warn!(
            environment = t.environment_id,
            dropped = out.signals_dropped,
            "security watcher: more signals than one run folds; the least severe wait"
        );
    }
    let mut by_actor: BTreeMap<&str, Vec<&Signal>> = BTreeMap::new();
    for s in kept {
        by_actor.entry(s.actor.as_str()).or_default().push(s);
    }
    let shadow = shadow_integration(c, t, w)?;
    for (actor, sigs) in by_actor {
        let top = sigs.iter().copied().max_by(|a, b| a.severity.cmp(&b.severity).then(b.at.cmp(&a.at))).expect("non-empty");
        let last = sigs.iter().map(|s| s.last_at).max().unwrap_or(now);
        let identity = sigs.iter().any(|s| s.identity);
        let open: Option<(i64, String, i64, Option<String>)> = c
            .prepare_cached(
                "SELECT id, severity, last_signal_at, alerted_severity FROM security_incidents WHERE environment_id = ? AND actor = ? AND status = 'open'",
            )?
            .query_row(params![t.environment_id, actor], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let (id, before, alerted) = match open {
            None if top.severity < Severity::Low => continue,
            None => {
                let first = sigs.iter().map(|s| s.at).min().unwrap_or(now);
                c.prepare_cached(
                    "INSERT INTO security_incidents (environment_id, actor, status, severity, identity, opened_at, last_signal_at, created_at, updated_at)
                     VALUES (?, ?, 'open', ?, ?, ?, ?, ?, ?)",
                )?
                .execute(params![t.environment_id, actor, top.severity.as_str(), identity, first, last, now, now])?;
                let id = c.last_insert_rowid();
                upsert_signals(c, id, &sigs)?;
                let alert = if top.severity >= w.alert_min_severity { fire(c, t, app, shadow, id, "opened", None, base_url)? } else { None };
                record(c, id, "opened", top.severity, None, Some(top.detector), now, alert)?;
                out.opened.push(id);
                if let Some(a) = alert {
                    c.execute("UPDATE security_incidents SET alerted_severity = ? WHERE id = ?", params![top.severity.as_str(), id])?;
                    out.alerts.push(a);
                }
                continue;
            }
            Some((id, before, last_before, alerted)) => {
                upsert_signals(c, id, &sigs)?;
                c.prepare_cached("UPDATE security_incidents SET last_signal_at = ?, identity = MAX(identity, ?), updated_at = ? WHERE id = ?")?
                    .execute(params![last.max(last_before), identity, now, id])?;
                (id, sev(&before), alerted.map(|a| sev(&a)))
            }
        };
        if top.severity > before {
            c.prepare_cached("UPDATE security_incidents SET severity = ?, escalated_at = ?, updated_at = ? WHERE id = ?")?.execute(params![
                top.severity.as_str(),
                now,
                now,
                id
            ])?;
            let alert = if top.severity >= w.alert_min_severity && alerted.is_none_or(|a| top.severity > a) {
                fire(c, t, app, shadow, id, "escalated", Some(before), base_url)?
            } else {
                None
            };
            record(c, id, "escalated", top.severity, Some(before), Some(top.detector), now, alert)?;
            out.escalated.push(id);
            if let Some(a) = alert {
                c.execute("UPDATE security_incidents SET alerted_severity = ? WHERE id = ?", params![top.severity.as_str(), id])?;
                out.alerts.push(a);
            }
        }
    }
    // Quiet ones close: no evidence newer than quiet_close_min.
    let quiet: Vec<(i64, String, Option<String>)> = c
        .prepare_cached(
            "SELECT id, severity, alerted_severity FROM security_incidents WHERE environment_id = ? AND status = 'open' AND last_signal_at < ? LIMIT 1000",
        )?
        .query_map(params![t.environment_id, now - app.thresholds.quiet_close_min * MIN_US], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, severity, alerted) in quiet {
        c.prepare_cached("UPDATE security_incidents SET status = 'closed', closed_at = ?, updated_at = ? WHERE id = ?")?.execute(params![now, now, id])?;
        let alert = if alerted.is_some() { fire(c, t, app, shadow, id, "closed", None, base_url)? } else { None };
        record(c, id, "closed", sev(&severity), None, None, now, alert)?;
        out.closed.push(id);
        out.alerts.extend(alert);
    }
    Ok(out)
}

fn upsert_signals(c: &Connection, incident: i64, sigs: &[&Signal]) -> rusqlite::Result<()> {
    // Severity and count only rise; the summary follows the newest evidence.
    let mut s = c.prepare_cached(
        "INSERT INTO security_incident_signals (incident_id, detector, subject, severity, first_at, last_at, count, summary, identity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (incident_id, detector, subject) DO UPDATE SET
           severity = CASE WHEN ?10 > (CASE severity WHEN 'info' THEN 0 WHEN 'low' THEN 1 WHEN 'medium' THEN 2 WHEN 'high' THEN 3 ELSE 4 END) THEN excluded.severity ELSE severity END,
           first_at = MIN(first_at, excluded.first_at),
           summary = CASE WHEN excluded.last_at >= last_at THEN excluded.summary ELSE summary END,
           last_at = MAX(last_at, excluded.last_at),
           count = MAX(count, excluded.count),
           identity = MAX(identity, excluded.identity)",
    )?;
    for g in sigs {
        s.execute(params![incident, g.detector, g.subject, g.severity.as_str(), g.at, g.last_at, g.count, g.summary, g.identity, g.severity as i64])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record(
    c: &Connection,
    id: i64,
    kind: &str,
    s: Severity,
    previous: Option<Severity>,
    detector: Option<&str>,
    now: i64,
    alert: Option<i64>,
) -> rusqlite::Result<()> {
    c.prepare_cached(
        "INSERT INTO security_incident_events (incident_id, kind, severity, previous_severity, detector, at, alert_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?
    .execute(params![id, kind, s.as_str(), previous.map(Severity::as_str), detector, now, alert])?;
    Ok(())
}

/// The shadow integration, when it is set and belongs to this account.
fn shadow_integration(c: &Connection, t: &Target, w: &Watcher) -> rusqlite::Result<Option<i64>> {
    let Some(id) = w.shadow_integration_id else { return Ok(None) };
    let owner: Option<i64> = c.query_row("SELECT account_id FROM integrations WHERE id = ?", [id], |r| r.get(0)).optional()?;
    if owner != Some(t.account_id) {
        tracing::warn!(integration = id, application = %t.application, "security watcher: shadow_integration_id is not an integration of this application's account; shadow alerts are recorded only");
        return Ok(None);
    }
    Ok(Some(id))
}

/// The alert for one transition: to the shadow integration in shadow mode,
/// through the application's security_incident rules when live.
#[allow(clippy::too_many_arguments)]
fn fire(
    c: &Connection,
    t: &Target,
    app: &AppPolicy,
    shadow: Option<i64>,
    id: i64,
    transition: &str,
    previous: Option<Severity>,
    base_url: &str,
) -> rusqlite::Result<Option<i64>> {
    let Some(incident) = incident_json(c, id, ALERT_SIGNALS)? else { return Ok(None) };
    let severity = incident["severity"].as_str().unwrap_or("info").to_string();
    let lead = incident["signals"].as_array().and_then(|s| s.first()).cloned().unwrap_or(Value::Null);
    let verb = match transition {
        "opened" => "opened".to_string(),
        "escalated" => format!("escalated from {}", previous.map_or("?", Severity::as_str)),
        _ => "closed (quiet)".to_string(),
    };
    let message = format!(
        "{} security incident {verb} in {} / {}: {} - {}: {}",
        severity.to_uppercase(),
        t.application,
        t.environment,
        incident["actor"].as_str().unwrap_or(""),
        lead["detector"].as_str().unwrap_or(""),
        lead["summary"].as_str().unwrap_or("")
    );
    let body = json!({
        "transition": transition,
        "mode": app.mode.as_str(),
        "severity": severity,
        "previous_severity": previous.map(Severity::as_str),
        "message": message,
        "application": t.application,
        "application_id": t.application_id,
        "environment": t.environment,
        "environment_id": t.environment_id,
        "incident": incident,
        "incident_url": format!("{base_url}/api/v1/applications/{}/environments/{}/security_incidents/{id}", t.application_id, t.environment_id),
        "mcp": {"tool": "get_security_incident", "arguments": {"application_id": t.application_id, "environment": t.environment, "incident_id": id}},
    });
    let payload = as_hash(crate::rjson::parse(body.to_string().as_bytes()).unwrap_or(crate::rjson::Value::Nil));
    Ok(match app.mode {
        Mode::Shadow => match shadow {
            Some(integration) => Some(outbox::create(c, None, integration, "security_incident", None, &crate::rjson::Value::Hash(payload), "pending", None)?),
            None => None,
        },
        Mode::Live => rules::fire_issueless(c, t.application_id, "security_incident", &payload)?.first().copied(),
    })
}

/// One incident with its signals (the most severe and newest first, at
/// most `max_signals`) and its timeline. Every string a client wrote in
/// it was cut and cleaned when it was recorded (super::text).
pub fn incident_json(c: &Connection, id: i64, max_signals: usize) -> rusqlite::Result<Option<Value>> {
    let t = |m: Option<i64>| m.map_or(Value::Null, |m| json!(time::iso8601(m.div_euclid(1_000_000))));
    let row = c
        .query_row(
            "SELECT environment_id, actor, status, severity, identity, opened_at, last_signal_at, escalated_at, closed_at, alerted_severity FROM security_incidents WHERE id = ?",
            [id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((env, actor, status, severity, identity, opened, last, escalated, closed, alerted)) = row else { return Ok(None) };
    let total: i64 = c.query_row("SELECT count(*) FROM security_incident_signals WHERE incident_id = ?", [id], |r| r.get(0))?;
    let signals: Vec<Value> = c
        .prepare_cached(
            "SELECT detector, subject, severity, first_at, last_at, count, summary, identity FROM security_incident_signals WHERE incident_id = ?
             ORDER BY CASE severity WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END, last_at DESC LIMIT ?",
        )?
        .query_map(params![id, max_signals as i64], |r| {
            let subject: String = r.get(1)?;
            let mut v = json!({
                "detector": r.get::<_, String>(0)?, "severity": r.get::<_, String>(2)?,
                "first_at": t(Some(r.get(3)?)), "last_at": t(Some(r.get(4)?)), "count": r.get::<_, i64>(5)?,
                "summary": r.get::<_, String>(6)?, "identity": r.get::<_, bool>(7)?,
            });
            if !subject.is_empty() {
                v["subject"] = json!(subject);
            }
            Ok(v)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let timeline: Vec<Value> = c
        .prepare_cached(
            "SELECT kind, severity, previous_severity, detector, at, alert_id FROM security_incident_events WHERE incident_id = ? ORDER BY id LIMIT 50",
        )?
        .query_map([id], |r| {
            let mut v = json!({"kind": r.get::<_, String>(0)?, "severity": r.get::<_, String>(1)?, "at": t(Some(r.get(4)?))});
            if let Some(p) = r.get::<_, Option<String>>(2)? {
                v["previous_severity"] = json!(p);
            }
            if let Some(d) = r.get::<_, Option<String>>(3)? {
                v["detector"] = json!(d);
            }
            v["alerted"] = json!(r.get::<_, Option<i64>>(5)?.is_some());
            Ok(v)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut v = json!({
        "id": id, "environment_id": env, "actor": actor, "status": status, "severity": severity, "identity": identity,
        "opened_at": t(Some(opened)), "last_signal_at": t(Some(last)), "escalated_at": t(escalated), "closed_at": t(closed),
        "alerted_severity": alerted, "signals": signals, "timeline": timeline,
    });
    if total as usize > max_signals {
        v["signals_omitted"] = json!(total as usize - max_signals);
    }
    Ok(Some(v))
}

/// Incidents of an environment, newest first: (id, actor, status,
/// severity, identity, opened_at, last_signal_at, closed_at, detectors).
pub fn list(c: &Connection, environment_id: i64, status: Option<&str>, min: Severity, since: i64, limit: i64) -> rusqlite::Result<Vec<Value>> {
    let t = |m: Option<i64>| m.map_or(Value::Null, |m| json!(time::iso8601(m.div_euclid(1_000_000))));
    let mut sql = String::from(
        "SELECT i.id, i.actor, i.status, i.severity, i.identity, i.opened_at, i.last_signal_at, i.closed_at,
           (SELECT group_concat(detector, ',') FROM (SELECT DISTINCT detector FROM security_incident_signals s WHERE s.incident_id = i.id ORDER BY detector)),
           (SELECT summary FROM security_incident_signals s WHERE s.incident_id = i.id
              ORDER BY CASE severity WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'medium' THEN 2 WHEN 'low' THEN 3 ELSE 4 END, last_at DESC LIMIT 1)
         FROM security_incidents i WHERE i.environment_id = ?1 AND i.last_signal_at >= ?2
           AND (CASE i.severity WHEN 'info' THEN 0 WHEN 'low' THEN 1 WHEN 'medium' THEN 2 WHEN 'high' THEN 3 ELSE 4 END) >= ?3",
    );
    if status.is_some() {
        sql.push_str(" AND i.status = ?5");
    }
    sql.push_str(" ORDER BY i.opened_at DESC, i.id DESC LIMIT ?4");
    let mut stmt = c.prepare(&sql)?;
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
        let detectors: Option<String> = r.get(8)?;
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?, "severity": r.get::<_, String>(3)?,
            "identity": r.get::<_, bool>(4)?, "opened_at": t(Some(r.get(5)?)), "last_signal_at": t(Some(r.get(6)?)), "closed_at": t(r.get(7)?),
            "detectors": detectors.map(|d| d.split(',').map(str::to_string).collect::<Vec<_>>()).unwrap_or_default(),
            "lead": r.get::<_, Option<String>>(9)?,
        }))
    };
    let rows = match status {
        Some(s) => stmt.query_map(params![environment_id, since, min as i64, limit, s], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
        None => stmt.query_map(params![environment_id, since, min as i64, limit], map)?.collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows)
}

/// The daily digest of incidents that never reached the alert line (at
/// least medium), once per environment and UTC day; deterministic text,
/// no model. Also prunes what the watcher no longer needs.
pub fn digest(c: &Connection, t: &Target, app: &AppPolicy, w: &Watcher, now: i64, base_url: &str) -> rusqlite::Result<Option<i64>> {
    let day = &time::utc_seconds_string(now.div_euclid(1_000_000))[..10];
    // Read first: after the digest hour this runs every tick, and a done
    // day must not take the write lock each time.
    let done: bool =
        c.query_row("SELECT EXISTS (SELECT 1 FROM security_digests WHERE environment_id = ? AND day = ?)", params![t.environment_id, day], |r| r.get(0))?;
    if done {
        return Ok(None);
    }
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| -> rusqlite::Result<Option<i64>> {
        let done: bool =
            c.query_row("SELECT EXISTS (SELECT 1 FROM security_digests WHERE environment_id = ? AND day = ?)", params![t.environment_id, day], |r| r.get(0))?;
        if done {
            return Ok(None);
        }
        c.execute(
            "DELETE FROM security_incidents WHERE environment_id = ? AND status = 'closed' AND closed_at < ?",
            params![t.environment_id, now - KEEP_CLOSED_DAYS * DAY_US],
        )?;
        c.execute("DELETE FROM security_accounts WHERE environment_id = ? AND signed_up_at < ?", params![t.environment_id, now - 7 * DAY_US])?;
        c.execute(
            "DELETE FROM security_digests WHERE environment_id = ? AND day < ?",
            params![t.environment_id, &time::utc_seconds_string((now - 30 * DAY_US).div_euclid(1_000_000))[..10]],
        )?;
        let rows: Vec<(i64, String, String, String, i64, Option<i64>)> = c
            .prepare(
                "SELECT i.id, i.actor, i.severity, i.status, i.opened_at,
                   (SELECT count(*) FROM security_incident_signals s WHERE s.incident_id = i.id)
                 FROM security_incidents i WHERE i.environment_id = ? AND i.alerted_severity IS NULL AND i.digested_at IS NULL
                   AND i.opened_at >= ? AND i.severity IN ('medium', 'high', 'critical')
                 ORDER BY CASE i.severity WHEN 'critical' THEN 0 WHEN 'high' THEN 1 ELSE 2 END, i.opened_at LIMIT 200",
            )?
            .query_map(params![t.environment_id, now - DAY_US], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut alert = None;
        if !rows.is_empty() {
            let mut lines = Vec::new();
            let mut incidents = Vec::new();
            for (id, actor, severity, status, opened, n) in &rows {
                let detectors: Vec<String> = c
                    .prepare_cached("SELECT DISTINCT detector FROM security_incident_signals WHERE incident_id = ? ORDER BY detector")?
                    .query_map([id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                lines.push(format!(
                    "{} {actor} {} (opened {} UTC, {status})",
                    severity.to_uppercase(),
                    detectors.join(", "),
                    &time::utc_seconds_string(opened.div_euclid(1_000_000))[11..16]
                ));
                incidents.push(json!({"id": id, "actor": actor, "severity": severity, "status": status, "detectors": detectors, "signals": n}));
                c.execute("UPDATE security_incidents SET digested_at = ? WHERE id = ?", params![now, id])?;
            }
            let message =
                format!("{} security incidents below the alert line in {} / {} (24 h): {}", rows.len(), t.application, t.environment, lines.join("; "));
            let body = json!({
                "mode": app.mode.as_str(), "message": message, "day": day, "incidents": incidents,
                "application": t.application, "application_id": t.application_id, "environment": t.environment, "environment_id": t.environment_id,
                "incidents_url": format!("{base_url}/api/v1/applications/{}/environments/{}/security_incidents", t.application_id, t.environment_id),
            });
            let payload = as_hash(crate::rjson::parse(body.to_string().as_bytes()).unwrap_or(crate::rjson::Value::Nil));
            alert = match app.mode {
                Mode::Shadow => match shadow_integration(c, t, w)? {
                    Some(i) => Some(outbox::create(c, None, i, "security_digest", None, &crate::rjson::Value::Hash(payload), "pending", None)?),
                    None => None,
                },
                Mode::Live => rules::fire_issueless(c, t.application_id, "security_digest", &payload)?.first().copied(),
            };
        }
        c.execute(
            "INSERT INTO security_digests (environment_id, day, incidents, alert_id) VALUES (?, ?, ?, ?)",
            params![t.environment_id, day, rows.len() as i64, alert],
        )?;
        Ok(alert)
    })();
    match r {
        Ok(v) => {
            c.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
