//! The detectors: pure functions of what one evaluation read (requests and
//! `security.*` events in the trailing window, oldest first) and what the
//! control DB already knows (open incidents, accounts seen signing up).
//! Same input, same signals.
//!
//! | detector                | rule                                                            | severity |
//! |-------------------------|-----------------------------------------------------------------|----------|
//! | secrets_probe           | >= min_hits probe paths in window_min                           | high; critical if one was served 2xx |
//! | scanner_shape           | >= min_4xx 4xx over >= min_paths distinct paths in window_min   | medium |
//! | non_browser_on_web      | >= min_hits browser-route requests from curl/python/script/bare Mozilla | low |
//! | auth_refusal_burst      | >= min_hits 401/403 in window_min                               | medium |
//! | spoofed_app_client      | the app's own client user agent and a script's from one address | medium |
//! | login_failure_burst     | >= auth_refusal_burst.min_hits `security.login.failed` events    | medium |
//! | risky_signup            | per signup: hosting address, disposable domain, spoofed app client, address already flagged high | 1 feature high, 2+ or flagged critical, none info |
//! | repeat_signup           | another signup from the address within lookback_hours           | critical |
//! | new_account_probe       | an account under max_account_age_min with >= min_halts privilege halts in window_min | high |
//! | oauth_client_registered | per registration: suspicious name; hosting address or a throwaway redirect | high; medium; else info |
//!
//! The identity detectors (signups, new accounts, OAuth clients, and a
//! served probe) mark their signals `identity`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::lists::{self, UaFamily};
use super::policy::{AppPolicy, HostingClassifier};
use super::{Event, EventKind, Req, Severity, Signal, text};

const MIN_US: i64 = 60_000_000;

/// What the control DB knows before this evaluation.
#[derive(Debug, Default, Clone)]
pub struct History {
    /// Open incidents' severity by actor.
    pub open: HashMap<String, Severity>,
    /// Per actor with an open incident: the earliest time a signal of at
    /// least high, other than a signup's own, was first met. A signup is
    /// "already flagged" only by evidence from before it, never by the
    /// incident it opened itself.
    pub flagged_since: HashMap<String, i64>,
    /// Accounts seen signing up: user ref -> (address, when).
    pub accounts: HashMap<String, (Option<String>, i64)>,
}

/// What one evaluation read, oldest first, own traffic already removed.
#[derive(Debug, Default, Clone)]
pub struct Input {
    pub requests: Vec<Req>,
    pub events: Vec<Event>,
    /// Requests whose only allowlist match was a user-agent prefix: they
    /// still count toward secrets_probe (our tools never ask for /.env, and
    /// a user agent is whatever the sender writes).
    pub ua_allowed: Vec<Req>,
}

/// The first time `times` (ascending) holds `min` inside `window_us`.
pub fn first_crossing(times: &[i64], min: usize, window_us: i64) -> Option<i64> {
    if min == 0 {
        return times.first().copied();
    }
    (min - 1..times.len()).find(|&i| times[i] - times[i + 1 - min] <= window_us).map(|i| times[i])
}

fn tally<'a>(items: impl Iterator<Item = &'a str>) -> Vec<(String, usize)> {
    let mut m: BTreeMap<&str, usize> = BTreeMap::new();
    for i in items {
        *m.entry(i).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

fn signal(detector: &'static str, severity: Severity, actor: &str, at: i64, last_at: i64, count: usize, summary: String) -> Signal {
    Signal { detector, severity, actor: actor.to_string(), subject: String::new(), at, last_at, count: count as i64, summary, identity: false }
}

pub fn detect(input: &Input, policy: &AppPolicy, hosting: &dyn HostingClassifier, history: &History) -> Vec<Signal> {
    let th = &policy.thresholds;
    let fam = |ua: Option<&str>| lists::ua_family(ua, &policy.app_ua_prefixes);
    let mut out = Vec::new();

    let mut by_ip: BTreeMap<&str, Vec<&Req>> = BTreeMap::new();
    for r in &input.requests {
        if let Some(ip) = r.ip.as_deref() {
            by_ip.entry(ip).or_default().push(r);
        }
    }
    let mut ua_allowed: BTreeMap<&str, Vec<&Req>> = BTreeMap::new();
    for r in &input.ua_allowed {
        if let Some(ip) = r.ip.as_deref() {
            ua_allowed.entry(ip).or_default().push(r);
        }
    }

    // 1. secrets_probe (requests of the address, plus its UA-allowlisted ones)
    let ips: BTreeSet<&str> = by_ip.keys().chain(ua_allowed.keys()).copied().collect();
    for ip in &ips {
        let mut probes: Vec<&Req> =
            by_ip.get(ip).into_iter().flatten().chain(ua_allowed.get(ip).into_iter().flatten()).filter(|r| lists::probe(&r.path)).copied().collect();
        probes.sort_by_key(|r| r.at);
        let times: Vec<i64> = probes.iter().map(|r| r.at).collect();
        if let Some(at) = first_crossing(&times, th.secrets_probe.min_hits, th.secrets_probe.window_min * MIN_US) {
            let served = probes.iter().filter(|r| (200..300).contains(&r.status)).count();
            let mut paths: Vec<String> = Vec::new();
            for r in &probes {
                let p = text(&r.path);
                if !paths.contains(&p) && paths.len() < 4 {
                    paths.push(p);
                }
            }
            let sev = if served > 0 { Severity::Critical } else { Severity::High };
            let mut s = signal(
                "secrets_probe",
                sev,
                ip,
                at,
                *times.last().unwrap(),
                probes.len(),
                format!("{} secret/dotfile probes ({}), {served} served", probes.len(), paths.join(", ")),
            );
            s.identity = served > 0;
            out.push(s);
        }
    }

    for (ip, rs) in &by_ip {
        // 2. scanner_shape: a sliding window of 4xx with its distinct paths
        let c = th.scanner_shape;
        let fx: Vec<&&Req> = rs.iter().filter(|r| (400..500).contains(&r.status)).collect();
        let mut window: VecDeque<&Req> = VecDeque::new();
        let mut paths: HashMap<&str, usize> = HashMap::new();
        for r in &fx {
            window.push_back(r);
            *paths.entry(r.path.as_str()).or_default() += 1;
            while let Some(front) = window.front()
                && r.at - front.at > c.window_min * MIN_US
            {
                let p = front.path.as_str();
                if let Some(n) = paths.get_mut(p) {
                    *n -= 1;
                    if *n == 0 {
                        paths.remove(p);
                    }
                }
                window.pop_front();
            }
            if window.len() >= c.min_4xx && paths.len() >= c.min_paths {
                let distinct: BTreeSet<&str> = fx.iter().map(|r| r.path.as_str()).collect();
                out.push(signal(
                    "scanner_shape",
                    Severity::Medium,
                    ip,
                    r.at,
                    fx.last().unwrap().at,
                    fx.len(),
                    format!("{} 4xx over {} distinct paths ({}+ over {}+ paths in {} min)", fx.len(), distinct.len(), c.min_4xx, c.min_paths, c.window_min),
                ));
                break;
            }
        }

        // 3. non_browser_on_web
        let c = th.non_browser_on_web;
        let nb: Vec<&&Req> = rs.iter().filter(|r| lists::browser_route(&r.path) && fam(r.ua.as_deref()).non_browser()).collect();
        let times: Vec<i64> = nb.iter().map(|r| r.at).collect();
        if let Some(at) = first_crossing(&times, c.min_hits, c.window_min * MIN_US) {
            let fams = tally(nb.iter().map(|r| fam(r.ua.as_deref()).as_str()));
            let fams: Vec<String> = fams.iter().map(|(k, n)| format!("{k}={n}")).collect();
            out.push(signal(
                "non_browser_on_web",
                Severity::Low,
                ip,
                at,
                *times.last().unwrap(),
                nb.len(),
                format!("{} browser-route requests from {}", nb.len(), fams.join(" ")),
            ));
        }

        // 4. auth_refusal_burst
        let c = th.auth_refusal_burst;
        let au: Vec<&&Req> = rs.iter().filter(|r| r.status == 401 || r.status == 403).collect();
        let times: Vec<i64> = au.iter().map(|r| r.at).collect();
        if let Some(at) = first_crossing(&times, c.min_hits, c.window_min * MIN_US) {
            let top = tally(au.iter().map(|r| r.path.as_str())).into_iter().next().map(|(p, _)| text(&p)).unwrap_or_default();
            out.push(signal("auth_refusal_burst", Severity::Medium, ip, at, *times.last().unwrap(), au.len(), format!("{} 401/403 ({top} most)", au.len())));
        }

        // 5. spoofed_app_client: the app's own client and a script, one address
        if !policy.app_ua_prefixes.is_empty() {
            let app: Vec<&&Req> = rs.iter().filter(|r| fam(r.ua.as_deref()) == UaFamily::App).collect();
            let scripted: Vec<&&Req> = rs.iter().filter(|r| fam(r.ua.as_deref()).scripted()).collect();
            if let (Some(a), Some(s)) = (app.first(), scripted.first()) {
                let at = a.at.max(s.at);
                let last = app.last().unwrap().at.max(scripted.last().unwrap().at);
                let script_fams = tally(scripted.iter().map(|r| fam(r.ua.as_deref()).as_str()));
                let script_fams: Vec<&str> = script_fams.iter().map(|(k, _)| k.as_str()).collect();
                out.push(signal(
                    "spoofed_app_client",
                    Severity::Medium,
                    ip,
                    at,
                    last,
                    app.len() + scripted.len(),
                    format!(
                        "{} requests as the app's client ({}) and {} from {}",
                        app.len(),
                        text(a.ua.as_deref().unwrap_or("")),
                        scripted.len(),
                        script_fams.join("/")
                    ),
                ));
            }
        }
    }

    // 6. login_failure_burst (events)
    let mut failures: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for e in input.events.iter().filter(|e| e.kind == EventKind::LoginFailed) {
        if let Some(ip) = e.ip.as_deref() {
            failures.entry(ip).or_default().push(e.at);
        }
    }
    for (ip, times) in &failures {
        let c = th.auth_refusal_burst;
        if let Some(at) = first_crossing(times, c.min_hits, c.window_min * MIN_US) {
            out.push(signal("login_failure_burst", Severity::Medium, ip, at, *times.last().unwrap(), times.len(), format!("{} failed sign-ins", times.len())));
        }
    }

    // Identity: signups, then new-account probes, then OAuth clients. A
    // signup is judged against the address's signals so far (this
    // evaluation's, and the open incident's).
    let address_signals = out.clone();
    let flagged_by = |ip: &str, at: i64| -> Vec<&'static str> {
        let mut d: Vec<&'static str> =
            address_signals.iter().filter(|s| s.actor == ip && s.at <= at && s.severity >= Severity::High).map(|s| s.detector).collect();
        d.dedup();
        if d.is_empty() && history.flagged_since.get(ip).is_some_and(|t| *t < at) {
            d.push("an open incident");
        }
        d
    };
    let signups: Vec<&Event> = input.events.iter().filter(|e| e.kind == EventKind::Signup).collect();
    let mut seen_signups: Vec<(&str, &str, i64)> = Vec::new();
    for e in &signups {
        let actor = actor_of(e.ip.as_deref(), e.user_ref.as_deref());
        let user = e.user_ref.as_deref().unwrap_or("?");
        let mut feats: Vec<String> = Vec::new();
        if let Some(ip) = e.ip.as_deref()
            && hosting.hosting(ip) == Some(true)
        {
            feats.push("hosting address".into());
        }
        if let Some(d) = e.email_domain.as_deref()
            && (e.disposable || lists::disposable_domain(d))
        {
            feats.push(format!("disposable domain {}", text(d)));
        }
        if let Some(ip) = e.ip.as_deref()
            && fam(e.ua.as_deref()) == UaFamily::App
        {
            let other: BTreeSet<&str> = by_ip
                .get(ip)
                .into_iter()
                .flatten()
                .filter(|r| (r.at - e.at).abs() <= 60 * MIN_US)
                .map(|r| fam(r.ua.as_deref()))
                .filter(|f| f.scripted())
                .map(|f| f.as_str())
                .collect();
            if !other.is_empty() {
                feats.push(format!(
                    "app client {} but the address also sent {}",
                    text(e.ua.as_deref().unwrap_or("")),
                    other.into_iter().collect::<Vec<_>>().join("/")
                ));
            }
        }
        let prior = e.ip.as_deref().map(|ip| flagged_by(ip, e.at)).unwrap_or_default();
        if !prior.is_empty() {
            feats.push(format!("address already flagged ({})", prior.join(", ")));
        }
        let sev = if feats.len() >= 2 || !prior.is_empty() {
            Severity::Critical
        } else if feats.is_empty() {
            Severity::Info
        } else {
            Severity::High
        };
        let mut s = signal(
            "risky_signup",
            sev,
            &actor,
            e.at,
            e.at,
            1,
            format!("signup of account {}: {}", text(user), if feats.is_empty() { "no risk features".to_string() } else { feats.join("; ") }),
        );
        s.subject = format!("user:{}", text(user));
        s.identity = true;
        out.push(s);

        // 7. repeat_signup: another account from this address
        if let Some(ip) = e.ip.as_deref() {
            let lookback = th.repeat_signup_lookback_hours * 60 * MIN_US;
            let mut earlier: BTreeSet<&str> =
                seen_signups.iter().filter(|(i, u, t)| *i == ip && *u != user && e.at - t <= lookback).map(|(_, u, _)| *u).collect();
            for (u, (aip, t)) in &history.accounts {
                if aip.as_deref() == Some(ip) && u != user && *t < e.at && e.at - t <= lookback {
                    earlier.insert(u.as_str());
                }
            }
            if !earlier.is_empty() {
                let mut s = signal(
                    "repeat_signup",
                    Severity::Critical,
                    &actor,
                    e.at,
                    e.at,
                    earlier.len() + 1,
                    format!("signup #{} from this address in {} h (account {})", earlier.len() + 1, th.repeat_signup_lookback_hours, text(user)),
                );
                s.subject = format!("user:{}", text(user));
                s.identity = true;
                out.push(s);
            }
            seen_signups.push((ip, user, e.at));
        }
    }

    // 8. new_account_probe: an account's privilege halts, by user ref (works without addresses)
    let c = th.new_account_probe;
    let mut born: HashMap<&str, (Option<&str>, i64)> = history.accounts.iter().map(|(u, (ip, t))| (u.as_str(), (ip.as_deref(), *t))).collect();
    for e in &signups {
        if let Some(u) = e.user_ref.as_deref() {
            born.entry(u).or_insert((e.ip.as_deref(), e.at));
        }
    }
    let mut halts: BTreeMap<&str, Vec<&Req>> = BTreeMap::new();
    for r in &input.requests {
        if let (Some(u), Some(h)) = (r.user_ref.as_deref(), r.halted.as_deref())
            && lists::privilege_halt(h)
        {
            halts.entry(u).or_default().push(r);
        }
    }
    for (user, hs) in &halts {
        let Some(&(signup_ip, birth)) = born.get(user) else { continue };
        let young: Vec<&&Req> = hs.iter().filter(|r| r.at >= birth && r.at - birth <= c.max_account_age_min * MIN_US).collect();
        let times: Vec<i64> = young.iter().map(|r| r.at).collect();
        if let Some(at) = first_crossing(&times, c.min_halts, c.window_min * MIN_US) {
            let ip = signup_ip.or_else(|| young.iter().find_map(|r| r.ip.as_deref()));
            let actor = actor_of(ip, Some(user));
            let names = tally(young.iter().filter_map(|r| r.halted.as_deref()));
            let names: Vec<String> = names.iter().map(|(k, n)| format!("{} x{n}", text(k))).collect();
            let mut paths: Vec<String> = Vec::new();
            for r in &young {
                let p = text(&r.path);
                if !paths.contains(&p) && paths.len() < 4 {
                    paths.push(p);
                }
            }
            let mut s = signal(
                "new_account_probe",
                Severity::High,
                &actor,
                at,
                *times.last().unwrap(),
                young.len(),
                format!("account {}, {} min old: {} ({})", text(user), (at - birth) / MIN_US, names.join(", "), paths.join(" ")),
            );
            s.subject = format!("user:{}", text(user));
            s.identity = true;
            out.push(s);
        }
    }

    // 9. oauth_client_registered
    for e in input.events.iter().filter(|e| e.kind == EventKind::OAuthClientRegistered) {
        let actor = actor_of(e.ip.as_deref(), e.user_ref.as_deref());
        let name = e.client_name.as_deref().unwrap_or("");
        let named = lists::suspicious_client_name(name);
        let mut feats: Vec<String> = Vec::new();
        if named {
            feats.push(format!("name \"{}\"", text(name)));
        }
        if e.ip.as_deref().is_some_and(|ip| hosting.hosting(ip) == Some(true)) {
            feats.push("from a hosting address".into());
        }
        if let Some(u) = e.redirect_uris.iter().find(|u| lists::throwaway_redirect(u)) {
            feats.push(format!("throwaway redirect {}", text(u)));
        }
        let sev = if named {
            Severity::High
        } else if feats.is_empty() {
            Severity::Info
        } else {
            Severity::Medium
        };
        let id = e.client_id.as_deref().map(text).unwrap_or_else(|| "?".into());
        let mut s = signal(
            "oauth_client_registered",
            sev,
            &actor,
            e.at,
            e.at,
            1,
            format!(
                "OAuth client {id} \"{}\" -> {}{}",
                text(name),
                e.redirect_uris.first().map_or_else(|| "(no redirect)".to_string(), |u| text(u)),
                if feats.is_empty() { String::new() } else { format!(" ({})", feats.join(", ")) }
            ),
        );
        s.subject = format!("client:{id}");
        s.identity = true;
        out.push(s);
    }

    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.detector.cmp(b.detector)).then(a.actor.cmp(&b.actor)).then(a.subject.cmp(&b.subject)));
    out
}

/// The incident key: the address, else the account.
pub fn actor_of(ip: Option<&str>, user_ref: Option<&str>) -> String {
    match (ip.filter(|i| !i.is_empty()), user_ref.filter(|u| !u.is_empty())) {
        (Some(ip), _) => text(ip),
        (None, Some(u)) => format!("user:{}", text(u)),
        (None, None) => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::policy::{NoHosting, Policy};
    use super::*;

    const T0: i64 = 1_792_000_000_000_000;
    const S: i64 = 1_000_000;

    fn req(at_s: i64, ip: &str, ua: &str, path: &str, status: i64) -> Req {
        Req { at: T0 + at_s * S, ip: Some(ip.into()), ua: Some(ua.into()), path: path.into(), status, user_ref: None, halted: None }
    }

    fn app() -> AppPolicy {
        let mut p = Policy::default().for_app("shop");
        p.app_ua_prefixes = vec!["ShopMobile/".into()];
        p
    }

    fn run(requests: Vec<Req>, events: Vec<Event>) -> Vec<Signal> {
        detect(&Input { requests, events, ua_allowed: vec![] }, &app(), &NoHosting, &History::default())
    }

    fn names(s: &[Signal]) -> Vec<(&str, &str)> {
        s.iter().map(|s| (s.detector, s.severity.as_str())).collect()
    }

    #[test]
    fn first_crossing_is_the_moment_the_rule_is_met() {
        assert_eq!(first_crossing(&[1, 2, 3], 3, 10), Some(3));
        assert_eq!(first_crossing(&[1, 20, 21, 22], 3, 5), Some(22), "the first two are too far apart");
        assert_eq!(first_crossing(&[1, 2], 3, 10), None);
        assert_eq!(first_crossing(&[], 1, 10), None);
    }

    #[test]
    fn a_secrets_probe_is_high_and_critical_once_one_is_served() {
        let ip = "203.0.113.5";
        let ff = "Mozilla/5.0 (X11; Linux) Gecko/20100101 Firefox/131.0";
        let rs = |served: i64| vec![req(0, ip, ff, "/.env", 404), req(5, ip, ff, "/.git/config", 404), req(9, ip, ff, "/.aws/credentials", served)];
        let s = run(rs(404), vec![]);
        assert_eq!(names(&s), [("secrets_probe", "high")]);
        assert_eq!(s[0].at, T0 + 9 * S, "fires at the third probe");
        assert!(s[0].summary.contains("0 served") && !s[0].identity);
        let s = run(rs(200), vec![]);
        assert_eq!(names(&s), [("secrets_probe", "critical")]);
        assert!(s[0].identity, "a served secret is an identity-grade incident");
        assert!(run(rs(404)[..2].to_vec(), vec![]).is_empty(), "two probes are under the threshold");
    }

    #[test]
    fn a_scanner_needs_many_4xx_over_many_paths() {
        let ip = "198.51.100.20";
        let many: Vec<Req> = (0..25).map(|i| req(i * 10, ip, "Mozilla/5.0 (X11; Linux) Gecko/20100101 Firefox/131.0", &format!("/x{}", i % 12), 404)).collect();
        assert_eq!(names(&run(many, vec![])), [("scanner_shape", "medium")]);
        let same: Vec<Req> = (0..25).map(|i| req(i * 10, ip, "Mozilla/5.0 (X11) Gecko Firefox", "/missing", 404)).collect();
        assert!(run(same, vec![]).is_empty(), "one broken link reloaded is not a scanner");
        let slow: Vec<Req> = (0..25).map(|i| req(i * 120, ip, "Mozilla/5.0 (X11) Gecko Firefox", &format!("/x{i}"), 404)).collect();
        assert!(run(slow, vec![]).is_empty(), "25 over 50 minutes never has 20 inside 15");
    }

    #[test]
    fn scripts_on_browser_pages_and_refusal_bursts() {
        let ip = "192.0.2.30";
        let rs: Vec<Req> = (0..6).map(|i| req(i, ip, if i % 2 == 0 { "curl/8" } else { "Mozilla/5.0" }, "/admin", 401)).collect();
        let s = run(rs, vec![]);
        assert_eq!(names(&s), [("non_browser_on_web", "low"), ("auth_refusal_burst", "medium")]);
        assert!(s[0].summary.contains("bare-mozilla=3") && s[0].summary.contains("curl=3"), "{}", s[0].summary);
        let api: Vec<Req> = (0..6).map(|i| req(i, ip, "curl/8", "/api/v1/things", 200)).collect();
        assert!(run(api, vec![]).is_empty(), "scripts on the API are expected");
    }

    #[test]
    fn the_apps_own_client_from_a_scripting_address_is_spoofed() {
        let ip = "203.0.113.40";
        let s = run(vec![req(0, ip, "python-requests/2.31", "/api/v1/me", 401), req(30, ip, "ShopMobile/1.0", "/api/v1/registration", 201)], vec![]);
        assert_eq!(names(&s), [("spoofed_app_client", "medium")]);
        assert!(run(vec![req(0, ip, "ShopMobile/1.0", "/api/v1/me", 200)], vec![]).is_empty(), "the real app alone is fine");
    }

    fn signup(at_s: i64, ip: &str, user: &str, ua: &str, domain: &str) -> Event {
        let mut e = Event::new(T0 + at_s * S, "security.signup");
        e.ip = Some(ip.into());
        e.user_ref = Some(user.into());
        e.ua = Some(ua.into());
        e.email_domain = Some(domain.into());
        e
    }

    #[test]
    fn a_signup_is_scored_by_its_features() {
        let ip = "198.51.100.77";
        let browser = "Mozilla/5.0 (Macintosh) AppleWebKit/605 Safari/605";
        assert_eq!(names(&run(vec![], vec![signup(0, ip, "41", browser, "example.com")])), [("risky_signup", "info")]);
        assert_eq!(names(&run(vec![], vec![signup(0, ip, "41", browser, "mailinator.com")])), [("risky_signup", "high")]);
        let curl = req(-30, ip, "curl/8", "/api/v1/registration", 422);
        let s = run(vec![curl], vec![signup(0, ip, "41", "ShopMobile/1.0", "mailinator.com")]);
        assert_eq!(names(&s), [("risky_signup", "critical")], "disposable + spoofed client");
        assert!(s[0].summary.contains("also sent curl"), "{}", s[0].summary);
        let mut disposable = signup(0, ip, "41", browser, "corp.example");
        disposable.disposable = true;
        assert_eq!(names(&run(vec![], vec![disposable])), [("risky_signup", "high")], "the app's own verdict counts");
        // An address already flagged high makes any signup critical.
        let probes: Vec<Req> = (0..3).map(|i| req(-60 + i, ip, browser, "/.env", 404)).collect();
        let s = run(probes, vec![signup(0, ip, "41", browser, "example.com")]);
        assert_eq!(names(&s), [("secrets_probe", "high"), ("risky_signup", "critical")]);
        let mut h = History::default();
        h.open.insert(ip.into(), Severity::High);
        h.flagged_since.insert(ip.into(), T0 - 60 * S);
        let s = detect(&Input { events: vec![signup(0, ip, "41", browser, "example.com")], ..Default::default() }, &app(), &NoHosting, &h);
        assert_eq!(names(&s), [("risky_signup", "critical")], "flagged in an earlier evaluation");
        h.flagged_since.insert(ip.into(), T0 + 60 * S);
        let s = detect(&Input { events: vec![signup(0, ip, "41", browser, "example.com")], ..Default::default() }, &app(), &NoHosting, &h);
        assert_eq!(names(&s), [("risky_signup", "info")], "evidence after the signup does not flag it");
    }

    #[test]
    fn a_second_signup_from_one_address_repeats() {
        let ip = "198.51.100.77";
        let b = "Mozilla/5.0 (X11) Gecko Firefox";
        let s = run(vec![], vec![signup(0, ip, "41", b, "example.com"), signup(60, ip, "42", b, "example.com")]);
        assert_eq!(names(&s), [("risky_signup", "info"), ("repeat_signup", "critical"), ("risky_signup", "info")]);
        let mut h = History::default();
        h.accounts.insert("40".into(), (Some(ip.into()), T0 - 3600 * S));
        let s = detect(&Input { events: vec![signup(0, ip, "41", b, "example.com")], ..Default::default() }, &app(), &NoHosting, &h);
        assert!(s.iter().any(|s| s.detector == "repeat_signup"), "an account seen an hour ago, from the control DB");
        h.accounts.insert("40".into(), (Some(ip.into()), T0 - 25 * 3600 * S));
        let s = detect(&Input { events: vec![signup(0, ip, "41", b, "example.com")], ..Default::default() }, &app(), &NoHosting, &h);
        assert!(!s.iter().any(|s| s.detector == "repeat_signup"), "past the 24 h lookback");
    }

    #[test]
    fn a_young_account_probing_privileged_pages() {
        let halt = |at_s: i64, h: &str, path: &str| Req {
            user_ref: Some("41".into()),
            halted: Some(h.into()),
            ..req(at_s, "198.51.100.77", "Mozilla/5.0 (X11) Gecko", path, 302)
        };
        let mut h = History::default();
        h.accounts.insert("41".into(), (Some("198.51.100.77".into()), T0 - 60 * S));
        let input = Input {
            requests: vec![halt(0, "require_superadmin", "/admin/console"), halt(20, "set_current_organization", "/admin/users")],
            ..Default::default()
        };
        let s = detect(&input, &app(), &NoHosting, &h);
        assert_eq!(names(&s), [("new_account_probe", "high")]);
        assert!(s[0].summary.contains("1 min old") && s[0].summary.contains("/admin/console"), "{}", s[0].summary);
        assert_eq!(s[0].actor, "198.51.100.77");
        h.accounts.insert("41".into(), (None, T0 - 2 * 3600 * S));
        assert!(detect(&input, &app(), &NoHosting, &h).is_empty(), "an account two hours old is not new");
        h.accounts.clear();
        assert!(detect(&input, &app(), &NoHosting, &h).is_empty(), "no signup seen: no age to judge");
    }

    #[test]
    fn oauth_registrations_by_what_they_announce() {
        let reg = |name: &str, uri: &str| {
            let mut e = Event::new(T0, "security.oauth_client.registered");
            e.ip = Some("203.0.113.9".into());
            e.client_id = Some("7".into());
            e.client_name = Some(name.into());
            e.redirect_uris = vec![uri.into()];
            e
        };
        assert_eq!(names(&run(vec![], vec![reg("pentest-tool", "https://example.invalid/cb")])), [("oauth_client_registered", "high")]);
        assert_eq!(names(&run(vec![], vec![reg("Notes", "https://example.invalid/cb")])), [("oauth_client_registered", "medium")]);
        assert_eq!(names(&run(vec![], vec![reg("Claude", "https://claude.ai/api/mcp/auth_callback")])), [("oauth_client_registered", "info")]);
    }

    #[test]
    fn failed_sign_in_events_burst() {
        let ev: Vec<Event> = (0..5)
            .map(|i| {
                let mut e = Event::new(T0 + i * S, "security.login.failed");
                e.ip = Some("192.0.2.9".into());
                e
            })
            .collect();
        assert_eq!(names(&run(vec![], ev.clone())), [("login_failure_burst", "medium")]);
        assert!(run(vec![], ev[..4].to_vec()).is_empty());
    }

    #[test]
    fn client_text_in_a_summary_is_cut_and_cleaned() {
        let ip = "203.0.113.5";
        let evil = format!("/.env\u{1b}[2J\u{202E}{}", "A".repeat(500));
        let s = run((0..3).map(|i| req(i, ip, "curl/8", &evil, 404)).collect(), vec![]);
        assert!(!s[0].summary.contains('\u{1b}') && !s[0].summary.contains('\u{202E}'));
        assert!(s[0].summary.len() < 1_000, "{}", s[0].summary.len());
    }
}
