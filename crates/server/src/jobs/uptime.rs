//! The uptime probe loop (uptime/mod.rs has the model, docs/uptime.md the
//! behaviour). Every TICK it reads the enabled monitors from the control DB
//! and starts a check for each one that is due and not already running,
//! at most MAX_CONCURRENT at a time; a monitor is due `interval` after its
//! last check, plus up to a tenth of the interval of jitter, so monitors
//! added together do not stay in lockstep. After a restart the persisted
//! last check decides what is due: nothing is checked twice in a burst and
//! nothing waits more than one interval. Rollups and retention run hourly
//! and once at start (`uptime::maintain`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::control::Control;
use crate::time;
use crate::uptime::{self, probe};

pub const TICK: Duration = Duration::from_secs(1);
pub const MAX_CONCURRENT: usize = 8;
const MAINTAIN_EVERY_SECS: i64 = 3600;

/// Up to a tenth of `interval_secs`, in microseconds.
fn jitter(interval_secs: i64) -> i64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    (u64::from_le_bytes(b) % (interval_secs.max(1) as u64 * 100_000)) as i64
}

/// When each monitor is next due, kept across ticks (in memory: the
/// persisted last check rebuilds it after a restart).
#[derive(Default)]
pub struct Schedule {
    next: HashMap<i64, i64>,
}

impl Schedule {
    /// The monitors due at `now` (epoch micros), dropping removed ones.
    pub fn due(&mut self, monitors: &[uptime::Monitor], now: i64) -> Vec<i64> {
        let ids: HashSet<i64> = monitors.iter().map(|m| m.id).collect();
        self.next.retain(|id, _| ids.contains(id));
        let mut out = Vec::new();
        for m in monitors {
            let every = m.interval_seconds * 1_000_000;
            let next = *self.next.entry(m.id).or_insert_with(|| match m.last_checked_at {
                Some(last) if last + every > now => last + every + jitter(m.interval_seconds),
                // Never checked, or overdue: soon, spread over the first seconds.
                _ => now + jitter(m.interval_seconds.min(30)),
            });
            if next <= now {
                out.push(m.id);
            }
        }
        out
    }

    /// The check of `id` started at `now`: next one an interval later.
    pub fn started(&mut self, id: i64, interval_secs: i64, now: i64) {
        self.next.insert(id, now + interval_secs * 1_000_000 + jitter(interval_secs));
    }
}

/// Runs until `shutdown` changes.
pub async fn run_forever(control: Arc<Control>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let policy = probe::Policy::from_env();
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
    let running: Arc<Mutex<HashSet<i64>>> = Default::default();
    let mut schedule = Schedule::default();
    let mut last_maintain = 0i64;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let now = time::now_micros();
        let c = control.clone();
        let maintain = now / 1_000_000 - last_maintain >= MAINTAIN_EVERY_SECS;
        let listed = tokio::task::spawn_blocking(move || {
            let conn = c.conn();
            if maintain {
                match uptime::maintain(&conn, now) {
                    Ok(m) if m.checks_deleted + m.days_deleted + m.incidents_deleted > 0 => {
                        tracing::info!(checks = m.checks_deleted, days = m.days_deleted, incidents = m.incidents_deleted, "uptime: pruned past retention")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "uptime: rollup/retention failed"),
                }
            }
            uptime::active_monitors(&conn)
        })
        .await;
        if maintain {
            last_maintain = now / 1_000_000;
        }
        let monitors = match listed {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "uptime: could not list monitors");
                continue;
            }
            Err(e) => {
                tracing::error!(error = %e, "uptime: monitor task failed");
                continue;
            }
        };
        let by_id: HashMap<i64, &uptime::Monitor> = monitors.iter().map(|m| (m.id, m)).collect();
        for id in schedule.due(&monitors, now) {
            if running.lock().unwrap().contains(&id) {
                continue;
            }
            // Full: the rest wait for a later tick, still due.
            let Ok(permit) = limit.clone().try_acquire_owned() else { break };
            let m = by_id[&id];
            schedule.started(id, m.interval_seconds, now);
            running.lock().unwrap().insert(id);
            let (c, req, run) = (control.clone(), m.request(), running.clone());
            tokio::spawn(async move {
                let at = time::now_micros();
                let outcome = probe::check(&req, policy).await;
                let recorded = tokio::task::spawn_blocking(move || uptime::record(&c.conn(), id, at, &outcome)).await;
                match recorded {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::error!(monitor = id, error = %e, "uptime: could not record a check"),
                    Err(e) => tracing::error!(monitor = id, error = %e, "uptime: record task failed"),
                }
                run.lock().unwrap().remove(&id);
                drop(permit);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: i64, interval: i64, last: Option<i64>) -> uptime::Monitor {
        uptime::Monitor {
            id,
            environment_id: 1,
            name: format!("m{id}"),
            url: "https://example.com".into(),
            method: "GET".into(),
            expected_status: None,
            keyword: None,
            timeout_seconds: 10,
            interval_seconds: interval,
            enabled: true,
            public: false,
            public_name: None,
            public_url: false,
            status: "up".into(),
            failures: 0,
            failing_since: None,
            last_checked_at: last,
            last_status_code: None,
            last_latency_ms: None,
            last_error: None,
        }
    }

    #[test]
    fn a_monitor_is_due_one_interval_after_its_last_check_with_bounded_jitter() {
        let now = 1_800_000_000_000_000;
        let mut s = Schedule::default();
        // Checked 10 s ago on a 60 s interval: not due for 50-56 s.
        let ms = [monitor(1, 60, Some(now - 10_000_000))];
        assert!(s.due(&ms, now).is_empty());
        assert!(s.due(&ms, now + 49_000_000).is_empty(), "never earlier than the interval");
        assert_eq!(s.due(&ms, now + 56_000_000), vec![1], "never later than interval + a tenth");
        s.started(1, 60, now + 56_000_000);
        assert!(s.due(&ms, now + 56_000_000).is_empty(), "started: the next one is an interval away");
        assert!(s.due(&ms, now + 115_000_000).is_empty());
        assert_eq!(s.due(&ms, now + 122_000_000), vec![1]);
    }

    #[test]
    fn after_a_restart_an_overdue_monitor_is_checked_within_seconds_and_a_removed_one_is_forgotten() {
        let now = 1_800_000_000_000_000;
        let mut s = Schedule::default();
        let ms = [monitor(1, 60, Some(now - 600_000_000)), monitor(2, 300, None)];
        assert!(s.due(&ms, now).is_empty(), "spread over the first seconds, not all at once");
        assert_eq!(s.due(&ms, now + 3_000_001).len(), 2, "overdue and never-checked monitors start within 3 s");
        s.due(&ms[..1], now);
        assert!(!s.next.contains_key(&2));
    }
}
