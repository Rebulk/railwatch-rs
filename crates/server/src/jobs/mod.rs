//! Background work, in process, no job queue: the tokio scheduler with
//! railwatch-cloud's config/recurring.yml cadences, the tenant worker thread
//! (rollups, grouping, drain, prune), the alert delivery loop, the watchdog
//! and its dead-man switch.
//!
//! | recurring.yml               | here                                       |
//! |-----------------------------|--------------------------------------------|
//! | rollup_catchup 5 min        | rollup_catchup: current + previous hour    |
//! | drain_ingest_followups 1 min| drain per environment                      |
//! | reconcile_alert_deliveries 1 min | delivery loop every 5 s (claims due rows, recovers leases, closes bursts) |
//! | performance_scan 5 min      | detect_performance per env with thresholds |
//! | anomaly_scan 5 min          | detect_anomalies per env with rules        |
//! | silent_host_check 5 min     | silent_hosts (Kamal expected_servers)      |
//! | scheduled_task_check 10 min | check_scheduled_tasks                      |
//! | release_health_check 10 min | release_health_check                       |
//! | prune_telemetry daily 3am   | prune per environment, 03:00 UTC and at start |
//! | auto_resolve_issues daily 4am | auto_resolve, 04:00 UTC                  |
//! | quota_check 15 min          | quota_check                                |
//! | weekly_digest every monday at 8am | weekly_digest::run_forever, 08:00 UTC Mondays |
//! | (new) watchdog              | every 10 s; dead-man ping every 30 s       |
//!
//! Not ported (no counterpart needed): optimize/backup/restore-verification
//! (operational, Litestream-era), operations_status, Linear,
//! Solid Cable/Queue trims.

pub mod detectors;
pub mod drain;
pub mod prune;
pub mod schedule;
pub mod watchdog;
pub mod weekly_digest;
pub mod worker;

/// The environment's retention: its account's retention_days, as Rails'
/// Environment delegates it. Read by the receipt drain and by prune.
pub fn retention_days(control: &rusqlite::Connection, environment_id: i64) -> rusqlite::Result<i64> {
    control.query_row(
        "SELECT a.retention_days FROM environments e JOIN applications ap ON ap.id = e.application_id \
         JOIN accounts a ON a.id = ap.account_id WHERE e.id = ?",
        [environment_id],
        |r| r.get(0),
    )
}

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rusqlite::Connection;

use crate::control::Control;
use crate::followups::{Background, Followup};
use crate::time;

type TenantFn = Box<dyn FnOnce(&Connection, &Connection) + Send>;

/// Work for the tenant worker thread.
pub enum TenantWork {
    Committed(Followup),
    Recompute {
        env: i64,
        slug: String,
        buckets: Vec<i64>,
    },
    RecomputeSessions {
        env: i64,
        slug: String,
        buckets: Vec<i64>,
        retention_floor: i64,
    },
    Drain {
        env: i64,
        slug: String,
    },
    Prune {
        env: i64,
        slug: String,
    },
    /// The disk budget deleted core day files (exceptions with them).
    Trimmed {
        env: i64,
        slug: String,
    },
    /// Day files were deleted: re-attach the union readers
    /// (Worker::reattach), then answer.
    Release(std::sync::mpsc::Sender<()>),
    /// Runs on the worker with the tenant's (read, write) connections.
    With {
        env: i64,
        slug: String,
        f: TenantFn,
    },
}

impl TenantWork {
    pub fn name(&self) -> &'static str {
        match self {
            TenantWork::Committed(_) => "committed",
            TenantWork::Recompute { .. } => "rollup_catchup",
            TenantWork::RecomputeSessions { .. } => "release_health_catchup",
            TenantWork::Drain { .. } => "drain",
            TenantWork::Prune { .. } => "prune",
            TenantWork::Trimmed { .. } => "disk_budget",
            TenantWork::Release(_) => "release_deleted",
            TenantWork::With { .. } => "detector",
        }
    }
}

pub struct Jobs {
    pub background: Background,
    pub threads: Vec<std::thread::JoinHandle<()>>,
}

/// Starts the worker thread and returns the handle the follow-ups use.
pub fn start(control: Arc<Control>, tenants_dir: PathBuf) -> Jobs {
    start_with(control, tenants_dir, crate::storage::retention::Tiers::default(), None)
}

/// `on_release`: after a prune deleted day files, closes the process's other
/// handles to them (worker.rs `on_release`).
pub fn start_with(control: Arc<Control>, tenants_dir: PathBuf, tiers: crate::storage::retention::Tiers, on_release: Option<Box<dyn Fn() + Send>>) -> Jobs {
    let (tx, rx) = std::sync::mpsc::channel();
    let heartbeats = Arc::new(watchdog::Heartbeats::default());
    if let Err(e) = heartbeats.restore(&control.conn()) {
        tracing::error!(error = %e, "watchdog: could not restore heartbeats");
    }
    let mut w = worker::Worker::new(control, tenants_dir);
    w.tiers = tiers;
    w.on_release = on_release;
    let t = std::thread::Builder::new().name("rollups".into()).spawn(move || w.run(rx)).expect("spawn worker");
    Jobs { background: Background { heartbeats, work: tx }, threads: vec![t] }
}

struct Env {
    id: i64,
    slug: String,
    retention_days: i64,
    last_seen_micros: Option<i64>,
    paused: bool,
}

fn environments(control: &Control) -> Vec<Env> {
    let c = control.conn();
    let r = c
        .prepare(
            "SELECT e.id, e.slug, a.retention_days, e.last_seen_at, e.paused_at IS NOT NULL \
         FROM environments e JOIN applications ap ON ap.id = e.application_id JOIN accounts a ON a.id = ap.account_id ORDER BY e.id",
        )
        .and_then(|mut s| {
            s.query_map([], |r| {
                let ls: Option<String> = r.get(3)?;
                Ok(Env {
                    id: r.get(0)?,
                    slug: r.get(1)?,
                    retention_days: r.get(2)?,
                    last_seen_micros: ls.and_then(|s| time::parse_db_time(&s)),
                    paused: r.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        });
    r.unwrap_or_else(|e| {
        tracing::error!(error = %e, "could not list environments");
        vec![]
    })
}

fn has_rows(control: &Control, sql: &str, env: i64) -> bool {
    control.conn().query_row(sql, [env], |r| r.get::<_, bool>(0)).unwrap_or(false)
}

type DetectorFn = fn(&Connection, &Connection, i64, i64) -> Result<usize, String>;

/// A recurring.yml scan job and the per-environment detector it enqueues.
pub struct Scan {
    pub name: &'static str,
    pub every_secs: u64,
    /// The environment needs a row here (PerformanceScanJob's joins).
    gate: Option<&'static str>,
    /// The environment reported within this many seconds (last_seen_at).
    recent_secs: Option<i64>,
    /// `where(last_seen_at: t..)` includes t; `where("last_seen_at > ?", t)` does not.
    recent_inclusive: bool,
    pub run: DetectorFn,
}

pub const SCANS: &[Scan] = &[
    // PerformanceScanJob: Environment.active.joins(:thresholds)
    Scan {
        name: "performance_scan",
        every_secs: 300,
        gate: Some("SELECT EXISTS (SELECT 1 FROM thresholds WHERE environment_id = ?)"),
        recent_secs: None,
        recent_inclusive: false,
        run: detectors::detect_performance,
    },
    // AnomalyScanJob: Environment.active.joins(:anomaly_rules).where(enabled: true)
    Scan {
        name: "anomaly_scan",
        every_secs: 300,
        gate: Some("SELECT EXISTS (SELECT 1 FROM anomaly_rules WHERE environment_id = ? AND enabled = 1)"),
        recent_secs: None,
        recent_inclusive: false,
        run: detectors::detect_anomalies,
    },
    // SilentHostCheckJob: Environment.active.where(last_seen_at: 24.hours.ago..)
    Scan { name: "silent_host_check", every_secs: 300, gate: None, recent_secs: Some(86_400), recent_inclusive: true, run: detectors::silent_hosts },
    // ScheduledTaskScanJob: Environment.active.where("last_seen_at > ?", 1.day.ago)
    Scan {
        name: "scheduled_task_check",
        every_secs: 600,
        gate: None,
        recent_secs: Some(86_400),
        recent_inclusive: false,
        run: detectors::check_scheduled_tasks,
    },
    // ReleaseHealthCheckJob: Environment.active.where(last_seen_at: 24.hours.ago..)
    Scan { name: "release_health_check", every_secs: 600, gate: None, recent_secs: Some(86_400), recent_inclusive: true, run: detectors::release_health_check },
];

/// The environments `scan` runs its detector for at `now` (epoch seconds).
pub fn scan_targets(control: &Control, scan: &Scan, now: i64) -> Vec<(i64, String)> {
    environments(control)
        .into_iter()
        .filter(|e| {
            !e.paused
                && scan.recent_secs.is_none_or(|r| {
                    let since = (now - r) * 1_000_000;
                    e.last_seen_micros.is_some_and(|l| l > since || (scan.recent_inclusive && l == since))
                })
        })
        .filter(|e| scan.gate.is_none_or(|g| has_rows(control, g, e.id)))
        .map(|e| (e.id, e.slug))
        .collect()
}

/// Every recurring task on its cadence. Returns when `shutdown` resolves.
pub async fn scheduler(control: Arc<Control>, bg: Background, tenants_dir: PathBuf, shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut handles = Vec::new();
    let every = |secs: u64, name: &'static str, f: Box<dyn Fn() + Send + Sync>| {
        let mut sd = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        tracing::debug!(task = name, "scheduled");
                        f();
                    }
                    _ = sd.changed() => return,
                }
            }
        })
    };
    let send = |bg: &Background, w: TenantWork| {
        let _ = bg.work.send(w);
    };

    // rollup_catchup: active envs seen in the last 2 hours, current + previous hour.
    {
        let (c, bg) = (control.clone(), bg.clone());
        handles.push(every(
            300,
            "rollup_catchup",
            Box::new(move || {
                let now = time::now_micros() / 1_000_000;
                let hour = now - now.rem_euclid(3600);
                for e in environments(&c) {
                    if e.paused || e.last_seen_micros.is_none_or(|l| l <= (now - 7200) * 1_000_000) {
                        continue;
                    }
                    send(&bg, TenantWork::Recompute { env: e.id, slug: e.slug.clone(), buckets: vec![hour, hour - 3600] });
                    let floor = now - e.retention_days * 86_400;
                    send(
                        &bg,
                        TenantWork::RecomputeSessions {
                            env: e.id,
                            slug: e.slug,
                            buckets: vec![hour, hour - 3600],
                            retention_floor: floor - floor.rem_euclid(3600),
                        },
                    );
                }
            }),
        ));
    }
    // drain_ingest_followups: every environment, paused included.
    {
        let (c, bg) = (control.clone(), bg.clone());
        handles.push(every(
            60,
            "drain_ingest_followups",
            Box::new(move || {
                for e in environments(&c) {
                    send(&bg, TenantWork::Drain { env: e.id, slug: e.slug });
                }
            }),
        ));
    }
    // Detectors that read telemetry run on the worker (its read connection).
    for scan in SCANS {
        let (c, bg) = (control.clone(), bg.clone());
        handles.push(every(
            scan.every_secs,
            scan.name,
            Box::new(move || {
                let now = time::now_micros() / 1_000_000;
                for (id, slug) in scan_targets(&c, scan, now) {
                    let c2 = c.clone();
                    send(
                        &bg,
                        TenantWork::With {
                            env: id,
                            slug,
                            f: Box::new(move |read, _write| {
                                let conn = c2.conn();
                                if let Err(err) = (scan.run)(&conn, read, id, now) {
                                    tracing::error!(task = scan.name, environment = id, error = %err, "detector failed");
                                }
                            }),
                        },
                    );
                }
            }),
        ));
    }
    // quota_check
    {
        let c = control.clone();
        handles.push(every(
            900,
            "quota_check",
            Box::new(move || {
                if let Err(e) = detectors::quota_check(&c.conn(), time::now_micros() / 1_000_000) {
                    tracing::error!(error = %e, "quota check failed");
                }
            }),
        ));
    }
    // weekly_digest: every monday at 8am (UTC).
    handles.push(tokio::spawn(weekly_digest::run_forever(control.clone(), tenants_dir.clone(), shutdown.clone())));
    // Daily: prune at 03:00 UTC, auto-resolve at 04:00 UTC. Prune also runs
    // once at start: a server that was not up at 03:00 (an app's dev box,
    // a deploy at the wrong minute) would otherwise never prune. It is
    // idempotent; on a pruned tenant it deletes nothing.
    {
        let (c, bg) = (control.clone(), bg.clone());
        let mut sd = shutdown.clone();
        handles.push(tokio::spawn(async move {
            for e in environments(&c) {
                let _ = bg.work.send(TenantWork::Prune { env: e.id, slug: e.slug });
            }
            loop {
                let now = time::now_micros() / 1_000_000;
                let next = |hour: i64| {
                    let d = now - now.rem_euclid(86_400) + hour * 3600;
                    if d > now { d } else { d + 86_400 }
                };
                let (prune_at, resolve_at) = (next(3), next(4));
                let wake = prune_at.min(resolve_at);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs((wake - now).max(1) as u64)) => {}
                    _ = sd.changed() => return,
                }
                if wake == prune_at {
                    for e in environments(&c) {
                        let _ = bg.work.send(TenantWork::Prune { env: e.id, slug: e.slug });
                    }
                } else {
                    let c2 = c.clone();
                    let r = tokio::task::spawn_blocking(move || detectors::auto_resolve(&c2.conn(), time::now_micros() / 1_000_000)).await;
                    if let Ok(Err(e)) = r {
                        tracing::error!(error = %e, "auto-resolve failed");
                    }
                }
            }
        }));
    }
    // Alert delivery loop (reconcile + deliver), every 5 s.
    let outbox = Arc::new(crate::alerts::OutboxHealth::default());
    {
        let (c, o, dir) = (control.clone(), outbox.clone(), tenants_dir.clone());
        let mut sd = shutdown.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        match crate::alerts::deliver_pass(&c, &dir).await {
                            Ok(n) => {
                                o.last_pass.store(time::now_micros() / 1_000_000, Ordering::Relaxed);
                                if n > 0 { tracing::info!(alerts = n, "alerts delivered or retried"); }
                            }
                            Err(e) => tracing::error!(error = %e, "alert delivery pass failed"),
                        }
                    }
                    _ = sd.changed() => return,
                }
            }
        }));
    }
    // Watchdog every 10 s + dead-man switch every 30 s.
    {
        let (c, hb) = (control.clone(), bg.heartbeats.clone());
        let deadman = std::env::var("RAILWATCH_DEADMAN_URL").ok().filter(|u| !u.is_empty());
        let mut sd = shutdown.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let threshold = watchdog::silence_secs();
            let mut last_ping = 0i64;
            let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok();
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = sd.changed() => return,
                }
                let (c2, hb2) = (c.clone(), hb.clone());
                let ok = tokio::task::spawn_blocking(move || {
                    let conn = c2.conn();
                    let checked = watchdog::check(&conn, &hb2, time::now_micros(), threshold);
                    let stuck = crate::alerts::outbox::stuck(&conn, 600).unwrap_or(true);
                    (checked.is_ok(), stuck)
                })
                .await;
                let (watchdog_ok, outbox_stuck) = ok.unwrap_or((false, true));
                let now = time::now_micros() / 1_000_000;
                let (Some(url), Some(http)) = (&deadman, &http) else { continue };
                if now - last_ping < 30 {
                    continue;
                }
                // Recent tenant commits: if anything committed in the last
                // 10 min, the newest must be under 2 min old.
                let last_commit = hb.last_commit_micros.load(Ordering::Relaxed) / 1_000_000;
                let ingest_ok = last_commit == 0 || now - last_commit > 600 || now - last_commit <= 120;
                let outbox_ok = !outbox_stuck && now - outbox.last_pass.load(Ordering::Relaxed) <= 60;
                let target = if watchdog_ok && ingest_ok && outbox_ok {
                    Some(url.clone())
                } else if watchdog_ok && !outbox_ok {
                    Some(format!("{}/fail", url.trim_end_matches('/')))
                } else {
                    None // wedged: stop pinging, like a dead process would
                };
                if let Some(t) = target {
                    last_ping = now;
                    if let Err(e) = http.get(&t).send().await {
                        tracing::warn!(error = %e, "dead-man ping failed");
                    }
                }
            }
        }));
    }
    let mut sd = shutdown;
    let _ = sd.changed().await;
    for h in handles {
        let _ = h.await;
    }
}

#[cfg(test)]
mod retention_tests {
    use crate::control::{Control, create_account, create_environment, insert_application, insert_user, test_password};

    #[test]
    fn retention_is_read_from_the_account() {
        // drain and prune both read this; a malformed query stopped every receipt drain and every prune.
        let dir = std::env::temp_dir().join(format!("rw-retention-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let user = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", user)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        c.with(|x| x.execute("UPDATE accounts SET retention_days = 14 WHERE id = ?", [account])).unwrap();
        assert_eq!(super::retention_days(&c.conn(), env).unwrap(), 14);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
