//! The watchdog: alert when a monitored environment goes quiet, from outside
//! the app (the reason railwatch-rs runs separately). A deliberate addition
//! over railwatch-cloud (docs/DIFFERENCES.md):
//!
//! - every committed batch updates last-seen per (environment, server) in
//!   memory (no database read on the ingest path); a batch whose rows carry
//!   no server counts for the environment as a whole (server "");
//! - every WATCHDOG_TICK the loop checks each pair: silent for longer than its
//!   threshold -> one `silent_environment` alert (payload names the server);
//!   data again -> one `environment_recovered` alert;
//! - threshold: RAILWATCH_SILENCE_SECONDS (default 60, minimum 30). Measured
//!   on real health streams the worst per-host gap was 10.6 s, so 60 s is
//!   ~6x the worst seen; keyed per host, not per process, so restarts do not
//!   look like gaps. Only pairs seen within the last WATCH_WINDOW are watched,
//!   so a retired server stops being watched after a day;
//! - state (last seen, silent since) is persisted to environment_heartbeats
//!   every tick so a restart neither forgets a silence nor re-alerts it;
//! - a dead-man switch (RAILWATCH_DEADMAN_URL): pinged every 30 s only while
//!   this loop runs, the control DB accepts a write and, if any tenant was
//!   active in the last 10 minutes, a commit landed in the last 2 minutes;
//!   `<url>/fail` when the alert outbox has had something undelivered for
//!   10 minutes (healthchecks.io convention).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use rusqlite::{Connection, params};

use crate::alerts::rules;
use crate::grouping::issue;
use crate::time;

pub const WATCH_WINDOW_SECS: i64 = 24 * 3600;

pub fn silence_secs() -> i64 {
    std::env::var("RAILWATCH_SILENCE_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60).max(30)
}

#[derive(Default)]
pub struct Heartbeats {
    /// (env, server) -> last committed micros
    seen: Mutex<HashMap<(i64, String), i64>>,
    pub last_commit_micros: AtomicI64,
}

impl Heartbeats {
    pub fn beat(&self, env: i64, servers: &[String], at: i64) {
        let mut m = self.seen.lock().unwrap();
        for s in servers {
            let e = m.entry((env, s.clone())).or_insert(at);
            *e = (*e).max(at);
        }
        if servers.is_empty() {
            let e = m.entry((env, String::new())).or_insert(at);
            *e = (*e).max(at);
        }
        self.last_commit_micros.fetch_max(at, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Vec<((i64, String), i64)> {
        self.seen.lock().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Reloads persisted last-seen times at boot.
    pub fn restore(&self, c: &Connection) -> rusqlite::Result<()> {
        let mut s = c.prepare("SELECT environment_id, server, last_seen_micros FROM environment_heartbeats")?;
        let rows = s.query_map([], |r| Ok(((r.get::<_, i64>(0)?, r.get::<_, String>(1)?), r.get::<_, i64>(2)?)))?;
        let mut m = self.seen.lock().unwrap();
        for r in rows {
            let (k, v) = r?;
            let e = m.entry(k).or_insert(v);
            *e = (*e).max(v);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq)]
pub enum Transition {
    Silent { env: i64, server: String, last_seen: i64 },
    Recovered { env: i64, server: String, silent_for_secs: i64 },
}

/// One check: persists last-seen, decides transitions against the stored
/// `silent_since`, records them, and fires the alerts. Returns transitions.
pub fn check(c: &Connection, hb: &Heartbeats, now_micros: i64, threshold_secs: i64) -> rusqlite::Result<Vec<Transition>> {
    let snapshot = hb.snapshot();
    let mut out = Vec::new();
    c.execute_batch("BEGIN IMMEDIATE")?;
    let r = (|| {
        for ((env, server), last) in &snapshot {
            c.prepare_cached(
                "INSERT INTO environment_heartbeats (environment_id, server, last_seen_micros) VALUES (?1, ?2, ?3)
                 ON CONFLICT (environment_id, server) DO UPDATE SET last_seen_micros = MAX(last_seen_micros, excluded.last_seen_micros)",
            )?
            .execute(params![env, server, last])?;
        }
        let rows: Vec<(i64, String, i64, Option<i64>)> = {
            let mut s = c.prepare_cached("SELECT environment_id, server, last_seen_micros, silent_since_micros FROM environment_heartbeats")?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (env, server, last, silent_since) in rows {
            let quiet = now_micros - last;
            let paused: bool = c.query_row("SELECT paused_at IS NOT NULL FROM environments WHERE id = ?", [env], |r| r.get::<_, bool>(0)).unwrap_or(true);
            match silent_since {
                None if !paused && quiet > threshold_secs * 1_000_000 && quiet < WATCH_WINDOW_SECS * 1_000_000 => {
                    c.execute(
                        "UPDATE environment_heartbeats SET silent_since_micros = ? WHERE environment_id = ? AND server = ?",
                        params![now_micros, env, server],
                    )?;
                    out.push(Transition::Silent { env, server, last_seen: last });
                }
                Some(since) if last > since - threshold_secs * 1_000_000 && quiet <= threshold_secs * 1_000_000 => {
                    c.execute("UPDATE environment_heartbeats SET silent_since_micros = NULL WHERE environment_id = ? AND server = ?", params![env, server])?;
                    out.push(Transition::Recovered { env, server, silent_for_secs: (now_micros - since) / 1_000_000 + threshold_secs });
                }
                _ => {}
            }
        }
        for t in &out {
            fire(c, t)?;
        }
        Ok(())
    })();
    match r {
        Ok(()) => c.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(out)
}

fn fire(c: &Connection, t: &Transition) -> rusqlite::Result<()> {
    let (env, event, payload) = match t {
        Transition::Silent { env, server, last_seen } => {
            let name = issue::environment_name(c, *env);
            let quiet = (time::now_micros() - last_seen) / 1_000_000;
            let who = if server.is_empty() { name.clone() } else { format!("{server} in {name}") };
            (
                *env,
                "silent_environment",
                crate::rhash! {
                    "environment" => name.clone(), "environment_id" => *env, "server" => server.clone(),
                    "last_seen_at" => time::iso8601(last_seen / 1_000_000),
                    "message" => format!("{who} has sent nothing for {quiet} s (last data {} UTC)", &time::utc_seconds_string(last_seen / 1_000_000)[11..19]),
                },
            )
        }
        Transition::Recovered { env, server, silent_for_secs } => {
            let name = issue::environment_name(c, *env);
            let who = if server.is_empty() { name.clone() } else { format!("{server} in {name}") };
            (
                *env,
                "environment_recovered",
                crate::rhash! {
                    "environment" => name.clone(), "environment_id" => *env, "server" => server.clone(),
                    "message" => format!("{who} is reporting again after about {silent_for_secs} s of silence"),
                },
            )
        }
    };
    let payload = crate::alerts::as_hash(payload);
    tracing::warn!(environment = env, event, "watchdog");
    rules::fire_issueless(c, issue::application_of(c, env)?, event, &payload)?;
    Ok(())
}
