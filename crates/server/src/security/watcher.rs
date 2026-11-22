//! The watcher loop. Every `tick_seconds` (default 15) it evaluates every
//! watched environment over the trailing `lookback_min` (default 15); an
//! ingest batch carrying a `security.*` event wakes it for that
//! environment at once (at most every WAKE_MIN_GAP).
//!
//! It must never stall ingest (a hung monitoring server has blocked app
//! deploys before), so:
//!
//! - it runs on its own task; the ingest side only sets a flag (`wake`),
//!   which never blocks and never allocates past a bounded set;
//! - telemetry is read on a connection of its own, read-only, which SQLite
//!   interrupts after `pass_timeout_seconds`; the control DB (shared with
//!   ingest's authentication) is held only for the bounded writes;
//! - each environment's evaluation runs on the blocking pool under a
//!   timeout, one at a time, and an environment still busy (a timed-out
//!   evaluation not yet interrupted) is skipped, never stacked;
//! - a panic inside an evaluation is caught and logged; the loop goes on.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::detect::{self, Input};
use super::incidents::{self, Applied, Target};
use super::policy::{AppPolicy, HostingClassifier, NoHosting, Policy, Watcher as Settings};
use super::{Wake, read};
use crate::control::Control;

/// The soonest an environment is evaluated again after a wake.
pub const WAKE_MIN_GAP: Duration = Duration::from_secs(2);

static WAKE: OnceLock<Arc<Wake>> = OnceLock::new();

/// Whether the watcher runs in this process (ingest skips looking for
/// `security.*` events otherwise).
pub fn running() -> bool {
    WAKE.get().is_some()
}

/// Called by the ingest follow-ups when a committed batch held a
/// `security.*` event: marks the environment for the watcher. A no-op when
/// the watcher is not running.
pub fn wake(environment_id: i64) {
    if let Some(w) = WAKE.get() {
        w.wake(environment_id);
    }
}

/// What one environment's evaluation did.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub requests: usize,
    pub events: usize,
    pub suppressed: i64,
    pub signals: usize,
    pub capped: bool,
    pub applied: Applied,
}

/// Every watched environment with its policy. A policy entry is keyed by
/// application slug, or `<account slug>/<application slug>` where two
/// accounts share one (that form wins).
pub fn targets(c: &Connection, policy: &Policy) -> rusqlite::Result<Vec<(Target, AppPolicy)>> {
    if policy.enabled_apps().is_empty() {
        return Ok(vec![]);
    }
    let mut s = c.prepare(
        "SELECT e.id, e.slug, e.name, ap.id, ap.slug, ac.id, ac.slug FROM environments e
         JOIN applications ap ON ap.id = e.application_id JOIN accounts ac ON ac.id = ap.account_id
         WHERE e.paused_at IS NULL ORDER BY e.id",
    )?;
    let rows = s.query_map([], |r| {
        Ok(Target {
            environment_id: r.get(0)?,
            environment_slug: r.get(1)?,
            environment: r.get(2)?,
            application_id: r.get(3)?,
            application: r.get(4)?,
            account_id: r.get(5)?,
            account: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for t in rows {
        let t = t?;
        let qualified = format!("{}/{}", t.account, t.application);
        let app = policy.apps.get(&qualified).or_else(|| policy.apps.get(&t.application));
        if let Some(app) = app.filter(|a| a.enabled) {
            out.push((t, app.clone()));
        }
    }
    Ok(out)
}

/// A read-only connection to the environment's telemetry that SQLite
/// interrupts at `deadline`. None when the environment has none yet.
pub fn open_tenant(tenants_dir: &Path, slug: &str, deadline: Instant) -> rusqlite::Result<Option<Connection>> {
    if crate::storage::Layout::detect(tenants_dir, slug).is_none() {
        return Ok(None);
    }
    let mut conn = crate::dashboard::pool::open_tenant_reader(tenants_dir, slug)?;
    conn.progress_handler(10_000, Some(move || Instant::now() > deadline))?;
    Ok(Some(conn))
}

/// One environment's evaluation at `now` (epoch micros): read the window
/// from `tenant` (none: nothing to read, quiet incidents still close),
/// detect, and fold the signals into its incidents.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    control: &Control,
    tenant: Option<&Connection>,
    target: &Target,
    app: &AppPolicy,
    w: &Settings,
    hosting: &dyn HostingClassifier,
    now: i64,
    base_url: &str,
) -> Result<Outcome, String> {
    let e = |e: rusqlite::Error| e.to_string();
    // Accounts far enough back for both the repeat-signup lookback and the new-account age.
    let hours = app.thresholds.repeat_signup_lookback_hours.max((app.thresholds.new_account_probe.max_account_age_min + 59) / 60);
    let history = control.with(|c| incidents::history(c, target.environment_id, now, hours, app.thresholds.quiet_close_min)).map_err(e)?;
    // Back far enough for every detector's window, and lookback_min at least.
    let from = now - w.lookback_min.max(app.thresholds.longest_window_min()) * 60_000_000;
    let r = match tenant {
        Some(t) => read::window(t, from, now, w.max_requests, w.max_events, &app.allow).map_err(e)?,
        None => read::Read::default(),
    };
    #[cfg(test)]
    tests::fault(base_url);
    let input = Input { requests: r.requests, events: r.events, ua_allowed: r.ua_allowed };
    let signals = detect::detect(&input, app, hosting, &history);
    let applied = control.with(|c| incidents::apply(c, target, app, w, &signals, &input.events, now, base_url)).map_err(e)?;
    let hour = (now.div_euclid(1_000_000).rem_euclid(86_400)) / 3600;
    if hour >= w.digest_hour_utc {
        control.with(|c| incidents::digest(c, target, app, w, now, base_url)).map_err(e)?;
    }
    if r.requests_capped || r.events_capped {
        tracing::warn!(environment = target.environment_id, "security watcher: the window held more than max_requests/max_events; the newest were read");
    }
    Ok(Outcome {
        requests: input.requests.len(),
        events: input.events.len(),
        suppressed: r.suppressed,
        signals: signals.len(),
        capped: r.requests_capped || r.events_capped,
        applied,
    })
}

/// The running watcher.
pub struct SecurityWatcher {
    control: Arc<Control>,
    tenants_dir: PathBuf,
    policy: Arc<Policy>,
    hosting: Arc<dyn HostingClassifier>,
    base_url: String,
    busy: Arc<Mutex<HashSet<i64>>>,
    last_wake: Mutex<HashMap<i64, Instant>>,
    /// A wake that came inside WAKE_MIN_GAP: kept and run once the gap is
    /// over, never dropped.
    deferred: Mutex<HashSet<i64>>,
    /// The environment listing is running (single flight: one that timed
    /// out may still hold a blocking thread).
    listing: Arc<std::sync::atomic::AtomicBool>,
}

impl SecurityWatcher {
    pub fn new(control: Arc<Control>, tenants_dir: PathBuf, policy: Policy, base_url: String) -> SecurityWatcher {
        SecurityWatcher {
            control,
            tenants_dir,
            policy: Arc::new(policy),
            hosting: Arc::new(NoHosting),
            base_url: base_url.trim_end_matches('/').to_string(),
            busy: Default::default(),
            last_wake: Default::default(),
            deferred: Default::default(),
            listing: Default::default(),
        }
    }

    /// Runs until `shutdown` changes.
    pub async fn run_forever(self: Arc<Self>, shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut sd = shutdown.clone();
        let wake = WAKE.get_or_init(|| Arc::new(Wake::default())).clone();
        let w = &self.policy.watcher;
        tracing::info!(apps = ?self.policy.enabled_apps(), tick_seconds = w.tick_seconds, lookback_min = w.lookback_min, mode = w.mode.as_str(), "security watcher on");
        let mut tick = tokio::time::interval(Duration::from_secs(w.tick_seconds));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut retry = tokio::time::interval(WAKE_MIN_GAP);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let started = Instant::now();
            tokio::select! {
                _ = tick.tick() => {
                    self.pass(None, &shutdown).await;
                    let took = started.elapsed();
                    if took > Duration::from_secs(w.lookback_min as u64 * 60) {
                        tracing::error!(seconds = took.as_secs(), lookback_min = w.lookback_min, "security watcher: one pass took longer than the lookback; requests between windows were not read (raise lookback_min or watch fewer environments)");
                    }
                }
                _ = wake.notified() => {
                    let due = self.due(wake.take());
                    if !due.is_empty() {
                        self.pass(Some(due), &shutdown).await;
                    }
                }
                _ = retry.tick() => {
                    let deferred = std::mem::take(&mut *self.deferred.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
                    let due = self.due(deferred);
                    if !due.is_empty() {
                        self.pass(Some(due), &shutdown).await;
                    }
                }
                _ = sd.changed() => return,
            }
        }
    }

    /// The woken environments not evaluated in the last WAKE_MIN_GAP; the
    /// others are deferred to the next retry.
    fn due(&self, woken: HashSet<i64>) -> HashSet<i64> {
        let now = Instant::now();
        let mut last = self.last_wake.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        last.retain(|_, t| now.duration_since(*t) < WAKE_MIN_GAP * 10);
        let (due, later): (HashSet<i64>, HashSet<i64>) = woken.into_iter().partition(|e| last.get(e).is_none_or(|t| now.duration_since(*t) >= WAKE_MIN_GAP));
        for e in &due {
            last.insert(*e, now);
        }
        drop(last);
        if !later.is_empty() {
            let mut d = self.deferred.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if d.len() < 10_000 {
                d.extend(later);
            }
        }
        due
    }

    /// Evaluates every watched environment (or only `only`), one at a
    /// time, each bounded by the pass timeout. Returns how many finished.
    /// Stops starting evaluations once `shutdown` is set.
    pub async fn pass(&self, only: Option<HashSet<i64>>, shutdown: &tokio::sync::watch::Receiver<bool>) -> usize {
        use std::sync::atomic::Ordering;
        let limit = Duration::from_secs(self.policy.watcher.pass_timeout_seconds);
        if self.listing.swap(true, Ordering::SeqCst) {
            tracing::warn!("security watcher: the previous environment listing is still running; pass skipped");
            return 0;
        }
        let (c, p, flag) = (self.control.clone(), self.policy.clone(), self.listing.clone());
        let listed = tokio::time::timeout(
            limit,
            tokio::task::spawn_blocking(move || {
                struct Done(Arc<std::sync::atomic::AtomicBool>);
                impl Drop for Done {
                    fn drop(&mut self) {
                        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let _done = Done(flag);
                c.with(|x| targets(x, &p))
            }),
        )
        .await;
        let targets = match listed {
            Ok(Ok(Ok(t))) => t,
            Ok(Ok(Err(e))) => {
                tracing::error!(error = %e, "security watcher: cannot list environments");
                return 0;
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "security watcher: listing environments panicked");
                return 0;
            }
            Err(_) => {
                tracing::error!("security watcher: listing environments timed out");
                return 0;
            }
        };
        let mut done = 0;
        for (target, app) in targets {
            if *shutdown.borrow() {
                break;
            }
            if only.as_ref().is_some_and(|o| !o.contains(&target.environment_id)) {
                continue;
            }
            let env = target.environment_id;
            if !self.busy.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(env) {
                tracing::warn!(environment = env, "security watcher: the previous evaluation is still running; skipped");
                continue;
            }
            let (c, dir, policy, hosting, base, busy) =
                (self.control.clone(), self.tenants_dir.clone(), self.policy.clone(), self.hosting.clone(), self.base_url.clone(), self.busy.clone());
            let job = tokio::task::spawn_blocking(move || {
                struct Release(Arc<Mutex<HashSet<i64>>>, i64);
                impl Drop for Release {
                    fn drop(&mut self) {
                        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.1);
                    }
                }
                let _release = Release(busy, env);
                let deadline = Instant::now() + limit;
                let now = crate::time::now_micros();
                let tenant = open_tenant(&dir, &target.environment_slug, deadline).map_err(|e| e.to_string())?;
                evaluate(&c, tenant.as_ref(), &target, &app, &policy.watcher, hosting.as_ref(), now, &base)
            });
            match tokio::time::timeout(limit + Duration::from_secs(5), job).await {
                Ok(Ok(Ok(o))) => {
                    done += 1;
                    let a = &o.applied;
                    if !(a.opened.is_empty() && a.escalated.is_empty() && a.closed.is_empty()) {
                        tracing::warn!(environment = env, opened = ?a.opened, escalated = ?a.escalated, closed = ?a.closed, alerts = a.alerts.len(), "security watcher: incidents changed");
                    }
                }
                Ok(Ok(Err(e))) => tracing::error!(environment = env, error = %e, "security watcher: evaluation failed"),
                Ok(Err(e)) => tracing::error!(environment = env, error = %e, "security watcher: evaluation panicked"),
                Err(_) => tracing::error!(environment = env, "security watcher: evaluation timed out"),
            }
        }
        done
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::control::{create_account, create_environment, insert_application, insert_user, test_password};

    /// A fault the next evaluation for `BASE` hits after its telemetry
    /// read (tests run in parallel; only this module's watcher uses BASE).
    #[derive(Clone, Copy)]
    pub enum Fault {
        Panic,
        Sleep(Duration),
    }

    const BASE: &str = "https://faults.example.com";
    static FAULT: Mutex<Option<Fault>> = Mutex::new(None);

    pub fn fault(base_url: &str) {
        if base_url != BASE {
            return;
        }
        let f = FAULT.lock().unwrap().take();
        match f {
            Some(Fault::Panic) => panic!("injected"),
            Some(Fault::Sleep(d)) => std::thread::sleep(d),
            None => {}
        }
    }

    fn watched(name: &str, timeout_s: u64) -> (PathBuf, Arc<Control>, SecurityWatcher) {
        let dir = std::env::temp_dir().join(format!("rw-secw-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let c = Arc::new(Control::open(&dir.join("control.sqlite3")).unwrap());
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _token) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let slug: String = c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0))).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        for i in 0..3 {
            t.execute(
                "INSERT INTO executions (execution_id, kind, name, status, occurred_at, duration, detail) VALUES (?1, 'request', 'GET /x', 404, ?2, 1, ?3)",
                rusqlite::params![
                    format!("e{i}"),
                    crate::time::quoted_date_micros(crate::time::now_micros() - 60_000_000 + i),
                    r#"{"ip":"203.0.113.5","path":"/.env"}"#
                ],
            )
            .unwrap();
        }
        let policy = Policy::parse(&format!("[watcher]\npass_timeout_seconds = {timeout_s}\n[apps.shop]\nenabled = true\n")).unwrap();
        let w = SecurityWatcher::new(c.clone(), dir.join("telemetry"), policy, BASE.into());
        (dir, c, w)
    }

    /// The watcher can never hold up ingest: a slow evaluation does not hold
    /// the control lock ingest authenticates through, the pass gives up on
    /// it, the environment is skipped (not stacked) while it still runs,
    /// and a panicking one is contained. The next pass works.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_or_panicking_evaluation_never_stalls_the_rest() {
        let (dir, c, w) = watched("stall", 1);
        *FAULT.lock().unwrap() = Some(Fault::Sleep(Duration::from_secs(8)));
        let started = Instant::now();
        let w = Arc::new(w);
        let w2 = w.clone();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let rx2 = rx.clone();
        let pass = tokio::spawn(async move { w2.pass(None, &rx2).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Mid-evaluation: the control DB answers at once (ingest's path).
        let t = Instant::now();
        c.with(|x| x.query_row("SELECT count(*) FROM environments", [], |r| r.get::<_, i64>(0))).unwrap();
        assert!(t.elapsed() < Duration::from_millis(200), "control lock held during the telemetry read: {:?}", t.elapsed());
        assert_eq!(pass.await.unwrap(), 0, "the timed-out evaluation does not count");
        assert!(started.elapsed() < Duration::from_secs(7), "the pass gave up at its timeout: {:?}", started.elapsed());
        assert_eq!(w.pass(None, &rx).await, 0, "still running: skipped, not stacked");
        // Wait for the stuck one to finish, then a panic is contained too.
        while !w.busy.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        *FAULT.lock().unwrap() = Some(Fault::Panic);
        assert_eq!(w.pass(None, &rx).await, 0);
        assert!(w.busy.lock().unwrap().is_empty(), "a panic releases the environment");
        assert_eq!(w.pass(None, &rx).await, 1, "and the next pass evaluates");
        let n: i64 = c.with(|x| x.query_row("SELECT count(*) FROM security_incidents", [], |r| r.get(0))).unwrap();
        assert_eq!(n, 1, "the probes opened one incident");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The SQLite deadline interrupts a telemetry read that runs too long.
    #[test]
    fn a_read_past_its_deadline_is_interrupted() {
        let (dir, _c, _w) = watched("deadline", 1);
        let slug: String = _c.with(|x| x.query_row("SELECT slug FROM environments", [], |r| r.get(0))).unwrap();
        let conn = open_tenant(&dir.join("telemetry"), &slug, Instant::now() + Duration::from_millis(200)).unwrap().unwrap();
        let t = Instant::now();
        let r = conn.query_row("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) FROM n", [], |r| r.get::<_, i64>(0));
        assert!(r.is_err(), "an endless query is interrupted");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wake inside WAKE_MIN_GAP is deferred, never dropped; shutdown
    /// stops a pass from starting more evaluations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_throttled_wake_is_deferred_and_shutdown_stops_a_pass() {
        let (dir, _c, w) = watched("defer", 5);
        let first = w.due([7, 8].into_iter().collect());
        assert_eq!(first.len(), 2, "the first wake is immediate");
        assert!(w.due([7].into_iter().collect()).is_empty(), "inside the gap: not now");
        assert!(w.deferred.lock().unwrap().contains(&7), "but kept for the retry");
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();
        assert_eq!(w.pass(None, &rx).await, 0, "shut down: nothing more starts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waking_never_blocks_and_is_bounded() {
        let w = Wake::default();
        for i in 0..20_000 {
            w.wake(i);
        }
        assert_eq!(w.take().len(), 10_000, "a flood of environments is capped");
        assert!(w.take().is_empty());
    }
}
