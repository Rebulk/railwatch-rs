//! What the security watcher counts as a signal, and whose traffic it never
//! raises: thresholds per detector and an own-traffic allowlist, per
//! application. Every decision the watcher makes is this policy plus code;
//! nothing here is learned or guessed.
//!
//! The defaults are the thresholds a replay of a real probe was tuned on.
//! An application overrides them in the server's `security_watcher_file`
//! (TOML), keyed by application slug:
//!
//! ```toml
//! [defaults]                   # every application, unless overridden
//! secrets_probe = { min_hits = 3, window_min = 10 }
//!
//! [watcher]                    # the loop itself (all optional)
//! tick_seconds = 15            # how often every watched environment is evaluated
//! lookback_min = 15            # the trailing window each evaluation reads
//! mode = "shadow"              # shadow (default) | live; an app may override
//! shadow_integration_id = 3    # where shadow alerts go (none: recorded only)
//! alert_min_severity = "high"  # an incident at or above this alerts at once
//! digest_hour_utc = 13         # the daily digest of the incidents below it
//!
//! [apps.shop]
//! enabled = true               # watch this application (default: false)
//! mode = "live"                # alerts through the app's security_incident rules
//! app_ua_prefixes = ["ShopMobile/"]   # the app's own clients' user agents
//! allow_cidrs = ["192.0.2.0/28"]
//! allow_user_refs = ["1"]
//! allow_ua_prefixes = ["our-agent/"]
//! scanner_shape = { min_4xx = 30 }
//! ```
//!
//! A table that is not known, a field a detector does not take, or a value
//! of the wrong type refuses the whole file at start: a policy that half
//! applies is worse than none.

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::http::IpNet;

/// A sliding-window count: at least `min_hits` within `window_min` minutes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Burst {
    pub min_hits: usize,
    pub window_min: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScannerShape {
    pub min_4xx: usize,
    pub min_paths: usize,
    pub window_min: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NewAccountProbe {
    pub max_account_age_min: i64,
    pub min_halts: usize,
    pub window_min: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    pub secrets_probe: Burst,
    pub scanner_shape: ScannerShape,
    pub non_browser_on_web: Burst,
    pub auth_refusal_burst: Burst,
    pub new_account_probe: NewAccountProbe,
    pub repeat_signup_lookback_hours: i64,
    /// An incident with no new signal for this long closes.
    pub quiet_close_min: i64,
}

impl Default for Thresholds {
    fn default() -> Thresholds {
        Thresholds {
            secrets_probe: Burst { min_hits: 3, window_min: 10 },
            scanner_shape: ScannerShape { min_4xx: 20, min_paths: 10, window_min: 15 },
            non_browser_on_web: Burst { min_hits: 3, window_min: 30 },
            auth_refusal_burst: Burst { min_hits: 5, window_min: 10 },
            new_account_probe: NewAccountProbe { max_account_age_min: 60, min_halts: 2, window_min: 15 },
            repeat_signup_lookback_hours: 24,
            quiet_close_min: 60,
        }
    }
}

/// Our own agents, CI and checkers: their requests and events are counted
/// as suppressed, never raised.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Allowlist {
    pub cidrs: Vec<IpNet>,
    pub user_refs: Vec<String>,
    pub ua_prefixes: Vec<String>,
}

impl Allowlist {
    pub fn ip(&self, ip: &str) -> bool {
        ip.parse::<IpAddr>().is_ok_and(|a| self.cidrs.iter().any(|n| n.contains(&a)))
    }

    pub fn user(&self, user_ref: &str) -> bool {
        self.user_refs.iter().any(|u| u == user_ref)
    }

    pub fn ua(&self, ua: &str) -> bool {
        self.ua_prefixes.iter().any(|p| !p.is_empty() && ua.starts_with(p.as_str()))
    }

    /// Whether any of what a request or event carries names our own traffic.
    pub fn matches(&self, ip: Option<&str>, user_ref: Option<&str>, ua: Option<&str>) -> bool {
        ip.is_some_and(|i| self.ip(i)) || user_ref.is_some_and(|u| self.user(u)) || ua.is_some_and(|u| self.ua(u))
    }
}

/// Where an application's security alerts go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Recorded, visible in the API and MCP; alerts go only to the
    /// watcher's `shadow_integration_id` (none: not sent at all).
    Shadow,
    /// Alerts go through the application's `security_incident` and
    /// `security_digest` alert rules, like any other event.
    Live,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Shadow => "shadow",
            Mode::Live => "live",
        }
    }
}

/// One application's policy.
#[derive(Debug, Clone, PartialEq)]
pub struct AppPolicy {
    pub enabled: bool,
    pub mode: Mode,
    pub thresholds: Thresholds,
    pub allow: Allowlist,
    /// User-agent prefixes of the app's own clients (a mobile app): one of
    /// these from an address that also sends curl is a spoofed client.
    pub app_ua_prefixes: Vec<String>,
}

/// The loop's own settings (`[watcher]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Watcher {
    pub tick_seconds: u64,
    pub lookback_min: i64,
    /// How long one environment's evaluation may read before SQLite
    /// interrupts it.
    pub pass_timeout_seconds: u64,
    /// Requests one evaluation reads at most (the newest).
    pub max_requests: usize,
    /// `security.*` events one evaluation reads at most (the newest).
    pub max_events: usize,
    pub mode: Mode,
    pub shadow_integration_id: Option<i64>,
    pub alert_min_severity: super::Severity,
    pub digest_hour_utc: i64,
}

impl Default for Watcher {
    fn default() -> Watcher {
        Watcher {
            tick_seconds: 15,
            lookback_min: 15,
            pass_timeout_seconds: 10,
            max_requests: 50_000,
            max_events: 5_000,
            mode: Mode::Shadow,
            shadow_integration_id: None,
            alert_min_severity: super::Severity::High,
            digest_hour_utc: 13,
        }
    }
}

/// Is an address a hosting provider's (a VPS, a cloud)? A scanner on a
/// hosting address is not a customer; a residential or mobile one may be.
/// Pluggable: the server ships `NoHosting` (answers None, "unknown"); an
/// offline ASN database can implement this without touching the detectors.
pub trait HostingClassifier: Send + Sync {
    /// Some(true) hosting, Some(false) not, None unknown.
    fn hosting(&self, ip: &str) -> Option<bool>;
}

pub struct NoHosting;

impl HostingClassifier for NoHosting {
    fn hosting(&self, _ip: &str) -> Option<bool> {
        None
    }
}

/// Every application's policy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy {
    pub watcher: Watcher,
    pub defaults: Thresholds,
    pub apps: BTreeMap<String, AppPolicy>,
}

impl Policy {
    /// The policy of application `slug`: its own entry, else the defaults,
    /// not enabled.
    pub fn for_app(&self, slug: &str) -> AppPolicy {
        self.apps.get(slug).cloned().unwrap_or_else(|| self.blank_app())
    }

    fn blank_app(&self) -> AppPolicy {
        AppPolicy { enabled: false, mode: self.watcher.mode, thresholds: self.defaults.clone(), allow: Allowlist::default(), app_ua_prefixes: vec![] }
    }

    /// The slugs of the applications watched.
    pub fn enabled_apps(&self) -> Vec<&str> {
        self.apps.iter().filter(|(_, a)| a.enabled).map(|(s, _)| s.as_str()).collect()
    }

    pub fn parse(text: &str) -> Result<Policy, String> {
        let t: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
        let mut p = Policy::default();
        for (k, v) in &t {
            match k.as_str() {
                "watcher" => p.watcher = watcher(table(v, "watcher")?)?,
                "defaults" => apply_thresholds(&mut p.defaults, table(v, "defaults")?, "defaults", &[])?,
                "apps" => {}
                other => return Err(format!("unknown table [{other}] (expected [watcher], [defaults] and [apps.<slug>])")),
            }
        }
        if let Some(apps) = t.get("apps") {
            for (slug, v) in table(apps, "apps")? {
                let at = format!("apps.{slug}");
                let entry = table(v, &at)?;
                let mut app = p.blank_app();
                apply_thresholds(
                    &mut app.thresholds,
                    entry,
                    &at,
                    &["enabled", "mode", "app_ua_prefixes", "allow_cidrs", "allow_user_refs", "allow_ua_prefixes"],
                )?;
                if let Some(e) = entry.get("enabled") {
                    app.enabled = e.as_bool().ok_or_else(|| format!("{at}.enabled: expected true or false"))?;
                }
                if let Some(m) = entry.get("mode") {
                    app.mode = mode(m, &format!("{at}.mode"))?;
                }
                app.app_ua_prefixes = strings(entry.get("app_ua_prefixes"), &format!("{at}.app_ua_prefixes"))?;
                if app.app_ua_prefixes.iter().any(|p| p.len() < 4) {
                    return Err(format!("{at}.app_ua_prefixes: a prefix must be at least 4 characters"));
                }
                if app.thresholds.quiet_close_min < p.watcher.lookback_min {
                    return Err(format!(
                        "{at}.quiet_close_min: at least lookback_min ({}), or evidence still in the window reopens what closed",
                        p.watcher.lookback_min
                    ));
                }
                for cidr in strings(entry.get("allow_cidrs"), &format!("{at}.allow_cidrs"))? {
                    app.allow.cidrs.push(IpNet::parse(&cidr).ok_or_else(|| format!("{at}.allow_cidrs: {cidr:?} is not an address or network"))?);
                }
                app.allow.user_refs = strings(entry.get("allow_user_refs"), &format!("{at}.allow_user_refs"))?;
                app.allow.ua_prefixes = strings(entry.get("allow_ua_prefixes"), &format!("{at}.allow_ua_prefixes"))?;
                if app.allow.ua_prefixes.iter().any(|p| p.len() < 4) {
                    return Err(format!("{at}.allow_ua_prefixes: a prefix must be at least 4 characters (a short one allowlists browsers)"));
                }
                p.apps.insert(slug.clone(), app);
            }
        }
        Ok(p)
    }

    pub fn load(path: &std::path::Path) -> Result<Policy, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Policy::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn mode(v: &toml::Value, at: &str) -> Result<Mode, String> {
    match v.as_str() {
        Some("shadow") => Ok(Mode::Shadow),
        Some("live") => Ok(Mode::Live),
        _ => Err(format!("{at}: expected \"shadow\" or \"live\"")),
    }
}

fn watcher(t: &toml::Table) -> Result<Watcher, String> {
    let mut w = Watcher::default();
    for (k, v) in t {
        let at = format!("watcher.{k}");
        match k.as_str() {
            "tick_seconds" => w.tick_seconds = int(v, &at)? as u64,
            "lookback_min" => w.lookback_min = int(v, &at)?,
            "pass_timeout_seconds" => w.pass_timeout_seconds = int(v, &at)? as u64,
            "max_requests" => w.max_requests = int(v, &at)? as usize,
            "max_events" => w.max_events = int(v, &at)? as usize,
            "mode" => w.mode = mode(v, &at)?,
            "shadow_integration_id" => w.shadow_integration_id = Some(int(v, &at)?),
            "alert_min_severity" => {
                w.alert_min_severity = v
                    .as_str()
                    .and_then(super::Severity::parse)
                    .filter(|s| *s >= super::Severity::Low)
                    .ok_or_else(|| format!("{at}: expected low, medium, high or critical"))?
            }
            "digest_hour_utc" => w.digest_hour_utc = v.as_integer().filter(|h| (0..24).contains(h)).ok_or_else(|| format!("{at}: expected 0-23"))?,
            other => return Err(format!("watcher.{other}: unknown")),
        }
    }
    if !(1..=3600).contains(&w.tick_seconds) {
        return Err("watcher.tick_seconds: 1 to 3600".into());
    }
    if !(1..=super::MAX_WINDOW_MIN).contains(&w.lookback_min) {
        return Err(format!("watcher.lookback_min: 1 to {}", super::MAX_WINDOW_MIN));
    }
    if !(1..=60).contains(&w.pass_timeout_seconds) {
        return Err("watcher.pass_timeout_seconds: 1 to 60".into());
    }
    if w.max_requests > 500_000 || w.max_events > 50_000 {
        return Err("watcher.max_requests is at most 500000 and max_events at most 50000".into());
    }
    Ok(w)
}

fn table<'a>(v: &'a toml::Value, at: &str) -> Result<&'a toml::Table, String> {
    v.as_table().ok_or_else(|| format!("{at}: expected a table"))
}

fn strings(v: Option<&toml::Value>, at: &str) -> Result<Vec<String>, String> {
    match v {
        None => Ok(vec![]),
        Some(toml::Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| format!("{at}: expected strings"))).collect(),
        Some(_) => Err(format!("{at}: expected an array of strings")),
    }
}

fn int(v: &toml::Value, at: &str) -> Result<i64, String> {
    v.as_integer().filter(|n| *n > 0).ok_or_else(|| format!("{at}: expected a positive integer"))
}

/// Applies the detector tables in `t` onto `th`; `extra` keys are the
/// caller's own (skipped here, not unknown).
fn apply_thresholds(th: &mut Thresholds, t: &toml::Table, at: &str, extra: &[&str]) -> Result<(), String> {
    for (k, v) in t {
        let here = format!("{at}.{k}");
        let fields = |known: &[&str]| -> Result<&toml::Table, String> {
            let f = table(v, &here)?;
            if let Some(bad) = f.keys().find(|x| !known.contains(&x.as_str())) {
                return Err(format!("{here}.{bad}: unknown (takes {})", known.join(", ")));
            }
            Ok(f)
        };
        let get = |f: &toml::Table, name: &str| f.get(name).map(|x| int(x, &format!("{here}.{name}"))).transpose();
        match k.as_str() {
            "secrets_probe" | "non_browser_on_web" | "auth_refusal_burst" => {
                let f = fields(&["min_hits", "window_min"])?;
                let b = match k.as_str() {
                    "secrets_probe" => &mut th.secrets_probe,
                    "non_browser_on_web" => &mut th.non_browser_on_web,
                    _ => &mut th.auth_refusal_burst,
                };
                if let Some(n) = get(f, "min_hits")? {
                    b.min_hits = n as usize;
                }
                if let Some(n) = get(f, "window_min")? {
                    b.window_min = n;
                }
            }
            "scanner_shape" => {
                let f = fields(&["min_4xx", "min_paths", "window_min"])?;
                if let Some(n) = get(f, "min_4xx")? {
                    th.scanner_shape.min_4xx = n as usize;
                }
                if let Some(n) = get(f, "min_paths")? {
                    th.scanner_shape.min_paths = n as usize;
                }
                if let Some(n) = get(f, "window_min")? {
                    th.scanner_shape.window_min = n;
                }
            }
            "new_account_probe" => {
                let f = fields(&["max_account_age_min", "min_halts", "window_min"])?;
                if let Some(n) = get(f, "max_account_age_min")? {
                    th.new_account_probe.max_account_age_min = n;
                }
                if let Some(n) = get(f, "min_halts")? {
                    th.new_account_probe.min_halts = n as usize;
                }
                if let Some(n) = get(f, "window_min")? {
                    th.new_account_probe.window_min = n;
                }
            }
            "repeat_signup" => {
                let f = fields(&["lookback_hours"])?;
                if let Some(n) = get(f, "lookback_hours")? {
                    th.repeat_signup_lookback_hours = n;
                }
            }
            "quiet_close_min" => th.quiet_close_min = int(v, &here)?,
            other if extra.contains(&other) => {}
            other => return Err(format!("{at}.{other}: unknown detector")),
        }
    }
    // A window longer than the lookback could never be satisfied whole.
    let windows = [th.secrets_probe.window_min, th.scanner_shape.window_min, th.non_browser_on_web.window_min, th.auth_refusal_burst.window_min];
    if windows.iter().any(|w| *w > super::MAX_WINDOW_MIN) {
        return Err(format!("{at}: a detector window is at most {} minutes", super::MAX_WINDOW_MIN));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_inherits_the_defaults_and_overrides_what_it_names() {
        let p = Policy::parse(
            r#"
            [watcher]
            tick_seconds = 5
            shadow_integration_id = 9
            [defaults]
            secrets_probe = { min_hits = 4 }
            [apps.shop]
            enabled = true
            mode = "live"
            app_ua_prefixes = ["ShopMobile/"]
            scanner_shape = { min_4xx = 30 }
            allow_cidrs = ["192.0.2.0/28", "2001:db8::/32"]
            allow_user_refs = ["7"]
            allow_ua_prefixes = ["ci-runner/"]
            "#,
        )
        .unwrap();
        assert_eq!((p.watcher.tick_seconds, p.watcher.lookback_min, p.watcher.shadow_integration_id), (5, 15, Some(9)));
        assert_eq!(p.watcher.mode, Mode::Shadow, "shadow unless an app says otherwise");
        let shop = p.for_app("shop");
        assert!(shop.enabled);
        assert_eq!(shop.mode, Mode::Live);
        assert_eq!(shop.app_ua_prefixes, ["ShopMobile/"]);
        assert_eq!(shop.thresholds.secrets_probe, Burst { min_hits: 4, window_min: 10 }, "from [defaults]");
        assert_eq!(shop.thresholds.scanner_shape.min_4xx, 30);
        assert_eq!(shop.thresholds.scanner_shape.min_paths, 10, "untouched fields keep the default");
        assert!(shop.allow.ip("192.0.2.9") && !shop.allow.ip("192.0.2.16") && shop.allow.ip("2001:db8::1"));
        assert!(shop.allow.user("7") && !shop.allow.user("8"));
        assert!(shop.allow.ua("ci-runner/2.1") && !shop.allow.ua("Mozilla/5.0 ci-runner/2.1"), "a prefix, not a substring");
        let other = p.for_app("blog");
        assert!(!other.enabled, "an application not named is not watched");
        assert_eq!(other.mode, Mode::Shadow);
        assert_eq!(p.enabled_apps(), ["shop"]);
        assert_eq!(Policy::parse("").unwrap().watcher, Watcher::default(), "an empty file is every default, nothing watched");
        assert_eq!(other.thresholds.secrets_probe.min_hits, 4);
    }

    #[test]
    fn a_policy_that_would_half_apply_is_refused() {
        for (text, says) in [
            ("[apps.shop]\nscanner_shap = { min_4xx = 3 }", "unknown detector"),
            ("[apps.shop]\nscanner_shape = { min_4xxx = 3 }", "unknown"),
            ("[apps.shop]\nscanner_shape = { min_4xx = -3 }", "positive integer"),
            ("[apps.shop]\nallow_cidrs = [\"not-a-net\"]", "not an address"),
            ("[apps.shop]\nallow_ua_prefixes = [\"Moz\"]", "at least 4"),
            ("[apps.shop]\nenabled = \"yes\"", "true or false"),
            ("[watch]\nx = 1", "unknown table"),
            ("[apps.shop]\nsecrets_probe = { window_min = 120 }", "at most"),
            ("[apps.shop]\nquiet_close_min = 5", "at least lookback_min"),
            ("[apps.shop]\nmode = \"loud\"", "shadow"),
            ("[watcher]\ntick_seconds = 0", "positive"),
            ("[watcher]\nalert_min_severity = \"info\"", "expected low"),
            ("[watcher]\ntick = 5", "unknown"),
        ] {
            let e = Policy::parse(text).unwrap_err();
            assert!(e.contains(says), "{text:?}: {e}");
        }
    }
}
