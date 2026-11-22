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
//! - at most MAX_SERVERS_PER_ENVIRONMENT named servers are watched per
//!   environment: `server` is client data, and every pair is a row in the
//!   control database every account shares, scanned each tick with its one
//!   connection held. Further names count for the environment as a whole
//!   (server ""). Rows past WATCH_WINDOW are deleted, except a bounded
//!   number still marked silent (so a server back after a long outage still
//!   gets its recovery alert), and a deleted environment's rows go with it;
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

/// Named servers watched per environment. Kamal reports at most 100 hosts
/// and the servers page lists 200 processes; 500 leaves room for a pod per
/// deploy over the day the window covers. Past it, a batch's other servers
/// count for the environment as a whole (server "").
pub const MAX_SERVERS_PER_ENVIRONMENT: usize = 500;

pub fn silence_secs() -> i64 {
    std::env::var("RAILWATCH_SILENCE_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60).max(30)
}

#[derive(Default)]
pub struct Heartbeats {
    /// env -> server -> last committed micros
    seen: Mutex<HashMap<i64, HashMap<String, i64>>>,
    /// Environments already logged as over MAX_SERVERS_PER_ENVIRONMENT.
    capped: Mutex<std::collections::HashSet<i64>>,
    pub last_commit_micros: AtomicI64,
}

/// Records `at` for (env, server) unless that would be a named server past
/// the cap; "" (the environment as a whole) is always taken.
fn note(m: &mut HashMap<i64, HashMap<String, i64>>, env: i64, server: &str, at: i64) -> bool {
    let servers = m.entry(env).or_default();
    if let Some(e) = servers.get_mut(server) {
        *e = (*e).max(at);
        return true;
    }
    let named = servers.len() - usize::from(servers.contains_key(""));
    if !server.is_empty() && named >= MAX_SERVERS_PER_ENVIRONMENT {
        return false;
    }
    servers.insert(server.to_string(), at);
    true
}

impl Heartbeats {
    pub fn beat(&self, env: i64, servers: &[String], at: i64) {
        let mut m = self.seen.lock().unwrap();
        let mut over = false;
        for s in servers {
            over |= !note(&mut m, env, s, at);
        }
        // The environment-wide entry ("") is created for a batch that names no
        // server or names one past the cap, and then refreshed by every
        // batch: otherwise an environment that once went past the cap would
        // be reported silent while its watched servers keep reporting.
        let tracks_environment = m.get(&env).is_some_and(|servers| servers.contains_key(""));
        if servers.is_empty() || over || tracks_environment {
            note(&mut m, env, "", at);
        }
        drop(m);
        if over && self.capped.lock().unwrap().insert(env) {
            tracing::warn!(
                environment = env,
                max = MAX_SERVERS_PER_ENVIRONMENT,
                "watchdog: environment reports more servers than are watched; the rest count for the environment as a whole"
            );
        }
        self.last_commit_micros.fetch_max(at, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Vec<((i64, String), i64)> {
        self.seen.lock().unwrap().iter().flat_map(|(env, servers)| servers.iter().map(|(s, at)| ((*env, s.clone()), *at))).collect()
    }

    /// Forgets pairs last seen before `before` (micros): they are past
    /// WATCH_WINDOW, so no longer watched, and their last-seen time is
    /// already in environment_heartbeats. Without this, every server name
    /// ever seen (a pod per deploy) stayed in memory and was upserted every
    /// tick. Returns how many went.
    pub fn forget_before(&self, before: i64) -> usize {
        let mut m = self.seen.lock().unwrap();
        let mut gone = 0;
        m.retain(|_, servers| {
            let n = servers.len();
            servers.retain(|_, at| *at >= before);
            gone += n - servers.len();
            !servers.is_empty()
        });
        // retain keeps the peak's allocation; give it back after a big expiry.
        if m.capacity() > 2 * m.len() + 64 {
            m.shrink_to_fit();
        }
        gone
    }

    /// Reloads persisted last-seen times at boot, newest first and within
    /// the cap. Rows written before there was a cap are trimmed here, once:
    /// those of deleted environments, and named servers past the cap.
    pub fn restore(&self, c: &Connection) -> rusqlite::Result<()> {
        c.execute("DELETE FROM environment_heartbeats WHERE environment_id NOT IN (SELECT id FROM environments)", [])?;
        c.execute(
            "DELETE FROM environment_heartbeats WHERE rowid IN (
               SELECT rowid FROM (SELECT rowid, ROW_NUMBER() OVER (PARTITION BY environment_id ORDER BY last_seen_micros DESC, server) AS n
                                  FROM environment_heartbeats WHERE server != '') WHERE n > ?)",
            [MAX_SERVERS_PER_ENVIRONMENT as i64],
        )?;
        let mut s = c.prepare("SELECT environment_id, server, last_seen_micros FROM environment_heartbeats ORDER BY last_seen_micros DESC, server")?;
        let rows = s.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        let mut m = self.seen.lock().unwrap();
        for r in rows {
            let (env, server, at) = r?;
            note(&mut m, env, &server, at);
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
    let window_start = now_micros - WATCH_WINDOW_SECS * 1_000_000;
    let r = (|| {
        for ((env, server), last) in &snapshot {
            // A deleted environment's pairs stay in memory until they age
            // out; they must not put its rows back.
            c.prepare_cached(
                "INSERT INTO environment_heartbeats (environment_id, server, last_seen_micros)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM environments WHERE id = ?1)
                 ON CONFLICT (environment_id, server) DO UPDATE SET last_seen_micros = MAX(last_seen_micros, excluded.last_seen_micros)",
            )?
            .execute(params![env, server, last])?;
        }
        // Past the window nothing is watched: those rows go, except the
        // newest MAX_SERVERS_PER_ENVIRONMENT per environment still marked
        // silent, which a server's return turns into a recovery alert.
        c.prepare_cached(
            "DELETE FROM environment_heartbeats WHERE last_seen_micros < ?1 AND (silent_since_micros IS NULL OR rowid IN (
               SELECT rowid FROM (SELECT rowid, ROW_NUMBER() OVER (PARTITION BY environment_id ORDER BY last_seen_micros DESC, server) AS n
                                  FROM environment_heartbeats WHERE last_seen_micros < ?1 AND silent_since_micros IS NOT NULL) WHERE n > ?2))",
        )?
        .execute(params![window_start, MAX_SERVERS_PER_ENVIRONMENT as i64])?;
        let rows: Vec<(i64, String, i64, Option<i64>, bool)> = {
            let mut s = c.prepare_cached(
                "SELECT h.environment_id, h.server, h.last_seen_micros, h.silent_since_micros, e.id IS NULL OR e.paused_at IS NOT NULL
                 FROM environment_heartbeats h LEFT JOIN environments e ON e.id = h.environment_id WHERE h.last_seen_micros >= ?",
            )?;
            s.query_map([window_start], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (env, server, last, silent_since, paused) in rows {
            let quiet = now_micros - last;
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
