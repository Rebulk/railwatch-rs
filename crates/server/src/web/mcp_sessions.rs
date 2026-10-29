//! Browser sessions for agents (docs/sessions.md): `list_sessions` finds
//! them, `session_journey` reads one across its tabs.
//!
//! A journey is one timeline, oldest first, of the session's page visits,
//! server requests, browser errors and exceptions, each in the lane of its
//! tab (t1, t2, ... in first-seen order). Grouped by default, as #69's
//! compact output is: consecutive events of one tab alike (a request's
//! status, route, outcome, halted callback and exception; a visit's
//! component; an error's class and message) are one run with its exact
//! count, first and last time, cadence and the values that varied, so a
//! tab polling for twenty minutes is one line while the other tab's clicks
//! interleave with it. A request run's `expand` lists exactly its
//! requests through search_requests. `shape: "rows"` lists every event.
//!
//! The client address and user agent are only shown with security access,
//! as everywhere else (mcp_security.rs).

use serde_json::{Map, Value, json};

use super::compact::{self, Shape};
use super::security::access_json;
use super::{Args, EnvRow, Mcp, R, clamp};
use crate::dashboard::db::{self, jf};
use crate::dashboard::filter_query::{self, Parsed};
use crate::dashboard::rb;
use crate::dashboard::request_scan::{FIELD_CHARS, untrusted};
use crate::dashboard::session_journey::{self as journey, Event, Journey, Kind};

const JOURNEY_NEXT: &str = "timeline: the session's events oldest first; lane is the tab (t1, t2, ... in tabs). A run (count > 1) is consecutive events of one tab alike in kind and key, with first/last, cadence and varying values; expand (to search_requests) lists a request run's requests. get_execution on an execution_id for one request in full; get_issue/list_issues for an exception's group. shape rows lists every event.";

/// The newest sessions a user lookup reads.
const USER_SESSIONS: usize = 5;
/// Users, addresses and user agents the summary lists (first seen first);
/// past it the distinct count says how many there were.
const LISTED: usize = 20;

impl Mcp<'_> {
    /// Recent browser sessions in the window, newest activity first.
    pub(super) fn list_sessions(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let limit = clamp(a.get("limit"), 20, 100)? as usize;
        let user = a.presence("user");
        let has_errors = a.get("has_errors").as_bool().unwrap_or(false) || a.s("has_errors") == "true";
        let (from, to) = self.window(a);
        let window = crate::dashboard::window::Window::preset(Some(&a.s("window")), self.now).key;
        self.with_telemetry(&env, |conn| {
            let (sessions, total) = journey::list(conn, (from, to), user.as_deref(), has_errors, limit)?;
            let rows: Vec<Value> = sessions
                .iter()
                .map(|s| {
                    let mut v = crate::obj! {
                        "session_id" => s.session_id.clone(), "first" => compact::time(s.first), "last" => compact::time(s.last),
                        "requests" => s.requests, "visits" => s.visits, "tabs" => s.tabs.len() as i64,
                    };
                    if s.errors() > 0 {
                        db::merge_into(
                            &mut v,
                            "errors",
                            json!({"total": s.errors(), "server": s.server_errors, "visits": s.visit_errors, "browser": s.browser_errors}),
                        );
                    }
                    if !s.users.is_empty() {
                        db::merge_into(&mut v, "users", json!(s.users.iter().take(LISTED).map(|u| untrusted(u, FIELD_CHARS)).collect::<Vec<_>>()));
                        if s.users.len() > LISTED {
                            db::merge_into(&mut v, "distinct_users", Value::from(s.users.len() as i64));
                        }
                    }
                    db::merge_into(&mut v, "url", self.url(&session_path(&env, &s.session_id)));
                    v
                })
                .collect();
            let mut v = crate::obj! { "window" => window, "sessions" => rows, "total" => total as i64 };
            if total > sessions.len() {
                db::merge_into(
                    &mut v,
                    "note",
                    Value::from(format!("The {} with the newest activity of {total}; pass a larger limit, user, or has_errors.", sessions.len())),
                );
            }
            db::merge_into(&mut v, "next", Value::from("session_journey with a session_id for its timeline across tabs."));
            Ok(v.to_string())
        })
    }

    /// One session's timeline across its tabs, by session_id, by
    /// execution_id (the session that request carried), or by user (their
    /// newest sessions in the window).
    pub(super) fn session_journey(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let shape = match compact::shape(a, Shape::Grouped) {
            Ok(Shape::Compact) => return Ok(json!({"error": "session_journey's shape is grouped (runs, the default) or rows"}).to_string()),
            Ok(s) => s,
            Err(e) => return Ok(e.to_string()),
        };
        let budget = compact::budget(a)?;
        let given = ["session_id", "execution_id", "user"].iter().filter(|k| a.present(k)).count();
        if given != 1 {
            return Ok(json!({"error": "pass exactly one of session_id, execution_id or user",
                "hint": "list_sessions finds sessions (by user, or has_errors); an execution_id from search_requests or get_execution resolves to its session."})
            .to_string());
        }
        let access = self.security_access();
        let (from, to) = self.window(a);
        self.with_telemetry(&env, |conn| {
            let session_id = if let Some(s) = a.presence("session_id") {
                if !filter_query::valid_session_id(&s) {
                    return Ok(json!({"error": "session_id is letters, digits, - and _, 64 at most"}).to_string());
                }
                s
            } else if let Some(eid) = a.presence("execution_id") {
                match journey::session_of(conn, &eid)? {
                    None => return Ok(json!({"error": "execution not found", "hint": "Executions are kept for the environment's retention period; search recent ones with search_requests."}).to_string()),
                    Some(None) => {
                        return Ok(json!({"error": "that execution carried no browser session",
                            "hint": "Only requests from a browser running the railwatch client (gem 1.5+) carry one; jobs, API clients and older requests do not."})
                        .to_string());
                    }
                    Some(Some(s)) => s,
                }
            } else {
                let user = a.s("user");
                let (sessions, total) = journey::list(conn, (from, to), Some(&user), false, USER_SESSIONS)?;
                if sessions.is_empty() {
                    return Ok(json!({"error": format!("no browser session for user {} in the window", untrusted(&user, 255)),
                        "hint": "Pass a longer window (7d, 30d), or check the user_ref with search_requests user:<ref>."})
                    .to_string());
                }
                if sessions.len() > 1 {
                    // Several: say which, and read the newest.
                    let others: Vec<Value> = sessions
                        .iter()
                        .map(|s| json!({"session_id": s.session_id, "first": compact::time(s.first), "last": compact::time(s.last), "requests": s.requests, "visits": s.visits}))
                        .collect();
                    let j = journey::read(conn, &sessions[0].session_id)?;
                    let mut v = self.journey_json(&env, &j, shape, budget, access);
                    db::merge_into(&mut v, "user_sessions", json!({"total": total, "newest": others}));
                    db::merge_into(&mut v, "note", Value::from("The user's newest session; user_sessions lists the others to pass as session_id."));
                    return Ok(v.to_string());
                }
                sessions[0].session_id.clone()
            };
            let j = journey::read(conn, &session_id)?;
            if j.events.is_empty() {
                return Ok(json!({"error": "no events for that session",
                    "hint": "Sessions are kept for the environment's retention period; list_sessions lists recent ones."})
                .to_string());
            }
            Ok(self.journey_json(&env, &j, shape, budget, access).to_string())
        })
    }

    fn journey_json(&self, env: &EnvRow, j: &Journey, shape: Shape, budget: Option<usize>, access: bool) -> Value {
        let mut v = crate::obj! { "session_id" => j.session_id.clone(), "summary" => summary(j, access) };
        db::merge_into(
            &mut v,
            "tabs",
            json!(j.tabs.iter().map(|t| json!({"lane": t.label, "first": compact::time(t.first), "last": compact::time(t.last), "requests": t.requests, "visits": t.visits, "errors": t.errors})).collect::<Vec<_>>()),
        );
        let base = filter_query::parse(Some(&filter_query::token("session", &j.session_id)));
        let (timeline, shape_name) = if shape == Shape::Rows {
            (j.events.iter().map(|e| self.event_row(env, j, e, access)).collect::<Vec<_>>(), "rows")
        } else {
            let entries: Vec<compact::Entry> = j.runs().iter().map(|r| run_entry(j, r, &base, access)).collect();
            (entries.into_iter().map(|e| Value::Object(e.json)).collect(), "grouped")
        };
        // Within max_tokens (or the result cap), less what the header and
        // tabs take: the newest are kept, the older counted, so the end of
        // the session (where it went wrong) is always there.
        let budget = budget.map(|b| b.saturating_sub(v.to_string().len() + 2_000).max(1_000));
        let (timeline, left_out) = fit(timeline, budget);
        db::merge_into(&mut v, "shape", Value::from(shape_name));
        db::merge_into(&mut v, "timeline", Value::Array(timeline));
        if let Some(n) = left_out {
            db::merge_into(&mut v, "timeline_left_out", n);
        }
        if j.capped.requests || j.capped.visits {
            db::merge_into(
                &mut v,
                "capped",
                Value::from(format!("The session has more than {} requests or visits: the newest are read, the older left out.", journey::MAX_EVENTS)),
            );
        }
        db::merge_into(&mut v, "url", self.url(&session_path(env, &j.session_id)));
        db::merge_into(&mut v, "next", Value::from(JOURNEY_NEXT));
        db::merge_into(&mut v, "access", access_json(access));
        v
    }

    /// One event as a row (shape rows).
    fn event_row(&self, env: &EnvRow, j: &Journey, e: &Event, access: bool) -> Value {
        let mut m = Map::new();
        m.insert("at".into(), Value::from(compact::time(e.at)));
        m.insert("type".into(), Value::from(e.type_name()));
        if let Some(l) = j.label(e.lane) {
            m.insert("lane".into(), Value::from(l));
        }
        for (k, v) in event_fields(e, access) {
            m.insert(k.into(), v);
        }
        if let Kind::Request(r) = &e.kind {
            m.insert("url".into(), self.execution_url(env, Some("request"), Some(&r.execution_id)));
        }
        Value::Object(m)
    }
}

fn session_path(env: &EnvRow, session_id: &str) -> String {
    format!("/apps/{}/envs/{}/sessions/{}", env.application_id, env.id, super::segment(session_id))
}

/// The header: when, how long, tabs, counts, user; address and browser with
/// security access.
fn summary(j: &Journey, access: bool) -> Value {
    let (first, last) = (j.first().unwrap_or(0), j.last().unwrap_or(0));
    let count = |t: &str| j.events.iter().filter(|e| e.type_name() == t).count() as i64;
    let e = j.errors();
    let mut v = crate::obj! {
        "started" => compact::time(first), "last" => compact::time(last),
        "duration" => span(last - first), "tabs" => j.tabs.len() as i64,
        "requests" => count("request"), "visits" => count("visit"),
        "errors" => json!({"total": e.total(), "server": e.server, "visits": e.visits, "browser": e.browser, "exceptions": e.exceptions}),
    };
    let untabbed = j.events.iter().filter(|e| e.lane.is_none()).count();
    if untabbed > 0 {
        db::merge_into(&mut v, "without_tab", Value::from(untabbed as i64));
    }
    let users = j.distinct(|e| match &e.kind {
        Kind::Request(r) => r.user.as_deref(),
        Kind::Visit(x) => x.user.as_deref(),
        Kind::Error(_) => None,
    });
    if !users.is_empty() {
        db::merge_into(&mut v, "users", json!(users.iter().take(LISTED).map(|u| untrusted(u, FIELD_CHARS)).collect::<Vec<_>>()));
        if users.len() > LISTED {
            db::merge_into(&mut v, "distinct_users", Value::from(users.len() as i64));
        }
    }
    if access {
        let ips = j.distinct(|e| match &e.kind {
            Kind::Request(r) => r.ip.as_deref(),
            _ => None,
        });
        let uas = j.distinct(|e| match &e.kind {
            Kind::Request(r) => r.user_agent.as_deref(),
            Kind::Visit(x) => x.user_agent.as_deref(),
            Kind::Error(_) => None,
        });
        db::merge_into(&mut v, "ips", json!(ips.iter().take(LISTED).map(|x| untrusted(x, 64)).collect::<Vec<_>>()));
        db::merge_into(&mut v, "user_agents", json!(uas.iter().take(LISTED).map(|x| untrusted(x, FIELD_CHARS)).collect::<Vec<_>>()));
        for (k, n) in [("distinct_ips", ips.len()), ("distinct_user_agents", uas.len())] {
            if n > LISTED {
                db::merge_into(&mut v, k, Value::from(n as i64));
            }
        }
    }
    v
}

/// "45s", "12m", "3h 5m".
fn span(us: i64) -> String {
    let s = us / 1_000_000;
    match s {
        ..120 => format!("{s}s"),
        120..7_200 => format!("{}m", s / 60),
        _ => format!("{}h {}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// An event's own fields (its type, time and lane aside).
fn event_fields(e: &Event, access: bool) -> Vec<(&'static str, Value)> {
    let ms = |us: i64| jf(rb::round(us as f64 / 1000.0, 2));
    let s = |v: &Option<String>| v.as_deref().map(|x| Value::from(untrusted(x, FIELD_CHARS)));
    let mut out: Vec<(&'static str, Option<Value>)> = vec![];
    match &e.kind {
        Kind::Request(r) => {
            out.push(("route", Some(Value::from(untrusted(&r.name, FIELD_CHARS)))));
            out.push(("path", s(&r.path)));
            out.push(("status", r.status.map(Value::from)));
            out.push(("outcome", r.outcome().map(Value::from)));
            out.push(("duration_ms", Some(ms(r.duration))));
            out.push(("execution_id", Some(Value::from(r.execution_id.clone()))));
            out.push(("exception", s(&r.exception_preview)));
            out.push(("halted", s(&r.halted)));
            out.push(("redirect_to", s(&r.redirect_to)));
            if access {
                out.push(("ip", s(&r.ip)));
                out.push(("user_agent", s(&r.user_agent)));
            }
        }
        Kind::Visit(v) => {
            out.push(("component", s(&v.component)));
            out.push(("page", s(&v.url)));
            out.push(("method", s(&v.method)));
            out.push(("status", s(&v.status)));
            out.push(("duration_ms", Some(ms(v.duration))));
            out.push(("partial", v.partial.then_some(Value::Bool(true))));
            out.push(("lcp_ms", v.lcp.map(Value::from)));
            out.push(("inp_ms", v.inp.map(Value::from)));
            out.push(("ttfb_ms", v.ttfb.map(Value::from)));
            out.push(("cls", v.cls.map(jf)));
        }
        Kind::Error(x) => {
            out.push(("class_name", Some(Value::from(untrusted(&x.class_name, FIELD_CHARS)))));
            // A browser error's message is the client's text; a server
            // exception's the app's (as every exception read shows it).
            let message = if x.browser { untrusted(&x.message, 500) } else { rb::first_chars(&x.message, 500).to_string() };
            out.push(("message", Some(Value::from(message))));
            out.push(("handled", x.handled.then_some(Value::Bool(true))));
            out.push(("execution_id", x.execution_id.clone().map(Value::from)));
            out.push(("group_hash", x.group_hash.clone().map(Value::from)));
            out.push(("page", s(&x.url)));
            out.push(("component", s(&x.component)));
        }
    }
    out.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect()
}

/// A run as an entry. Its requests through mcp_compact (the fold every
/// grouped request read uses, with expand narrowed to the session, the tab
/// and the run's times); its visits, errors (a run of them alone) through a
/// plain fold with count, times, cadence and the fields that varied. A run
/// of page visits each with its own request (an Inertia reload) is one
/// entry: the requests' entry, `type` "page", the visits in `visits`, and
/// `count` all its events (visits and requests), so counts still add up to
/// the rows.
fn run_entry(j: &Journey, run: &journey::Run, base: &Parsed, access: bool) -> compact::Entry {
    let events: Vec<&Event> = run.events.iter().map(|&i| &j.events[i]).collect();
    let lane = j.label(run.lane);
    let rows: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Request(r) => Some(r.row(e.at)),
            _ => None,
        })
        .collect();
    let others: Vec<&Event> = events.iter().copied().filter(|e| !matches!(e.kind, Kind::Request(_))).collect();
    let mut entry = if rows.is_empty() {
        other_entry(&others, access)
    } else {
        let mut e = compact::request_entry(&rows, access);
        if e.count > 1 {
            let mut p = base.clone();
            let tab = run.lane.map_or("none", |l| j.tabs[l].id.as_str());
            p.fields.insert("tab".into(), tab.into());
            let x = compact::expand(&p, &e);
            e.json.insert("expand".into(), x);
        }
        if !others.is_empty() {
            let v = other_entry(&others, access);
            let mut visits = Map::new();
            for k in ["count", "component", "first", "last", "at", "duration_ms", "varying", "not_listed"] {
                if let Some(x) = v.json.get(k) {
                    visits.insert(k.into(), x.clone());
                }
            }
            e.json.insert("requests".into(), Value::from(e.count));
            e.json.insert("visits".into(), Value::Object(visits));
            e.count += v.count;
            e.first = e.first.min(v.first);
            e.json.insert("count".into(), Value::from(e.count));
        }
        e
    };
    // The entry's own lead: type and lane, before its fields.
    let paired = !rows.is_empty() && !others.is_empty();
    let mut m = Map::new();
    m.insert("type".into(), Value::from(if paired { "page" } else { events[0].type_name() }));
    if let Some(l) = lane {
        m.insert("lane".into(), Value::from(l));
    }
    m.extend(std::mem::take(&mut entry.json));
    entry.json = m;
    entry
}

/// Visits or errors folded: count, times, cadence, durations, the fields
/// that varied, keyed by what makes them alike.
fn other_entry(events: &[&Event], access: bool) -> compact::Entry {
    const KEY: [&str; 5] = ["component", "class_name", "status", "method", "partial"];
    let mut fold = compact::Fold::default();
    for e in events {
        let duration = match &e.kind {
            Kind::Visit(v) => Some(v.duration),
            _ => None,
        };
        fold.add(e.at, duration, None);
        for (name, v) in event_fields(e, access) {
            if name == "duration_ms" || KEY.contains(&name) {
                continue;
            }
            let text = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            fold.field(name, Some(&text), FIELD_CHARS, false);
        }
    }
    let mut key = Map::new();
    for (k, v) in event_fields(events[0], access) {
        if KEY.contains(&k) {
            key.insert(k.into(), v);
        }
    }
    compact::entry(String::new(), key, fold)
}

/// The newest entries that fit `budget` bytes (the last always); how many
/// older ones were left out, and their events.
fn fit(entries: Vec<Value>, budget: Option<usize>) -> (Vec<Value>, Option<Value>) {
    let Some(budget) = budget else { return (entries, None) };
    let mut used = 0;
    let mut keep = 0;
    for (i, e) in entries.iter().enumerate().rev() {
        used += e.to_string().len() + 1;
        if used > budget && i + 1 < entries.len() {
            break;
        }
        keep += 1;
    }
    let cut = entries.len() - keep;
    if cut == 0 {
        return (entries, None);
    }
    let events: i64 = entries[..cut].iter().map(|e| e.get("count").and_then(Value::as_i64).unwrap_or(1)).sum();
    let until = entries[cut - 1].get("last").or_else(|| entries[cut - 1].get("at")).cloned().unwrap_or(Value::Null);
    let mut rest = entries;
    let kept = rest.split_off(cut);
    (
        kept,
        Some(
            json!({"entries": cut as i64, "events": events, "until": until, "note": "Older entries past max_tokens (or the result cap); pass a larger max_tokens, or search_requests with q session:<id> and before: this until for the older requests."}),
        ),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::tests::{Seeded, seeded};
    use crate::dashboard::session_journey::tests::{S, T0, request, two_tabs, visit};

    fn fixture(name: &str) -> Seeded {
        let f = seeded(name);
        two_tabs(&f.tenant());
        f
    }

    /// The execution_ids a grouped timeline stands for: a one-request
    /// entry's own, a run's through its expand (search_requests, every page),
    /// which must list exactly `count` of them.
    fn request_ids(f: &Seeded, v: &Value) -> Vec<String> {
        let mut out = vec![];
        for e in v["timeline"].as_array().unwrap_or_else(|| panic!("{v}")) {
            // A page entry's requests are `requests` of its `count`.
            let count = match e["type"].as_str() {
                Some("request") => e["count"].as_i64().unwrap(),
                Some("page") => e["requests"].as_i64().unwrap(),
                _ => continue,
            };
            if count == 1 {
                out.push(e["execution_id"].as_str().unwrap().to_string());
                continue;
            }
            let mut args = e["expand"].clone();
            let mut got = vec![];
            loop {
                let page = f.tool("search_requests", args.clone());
                got.extend(page["rows"].as_array().unwrap_or_else(|| panic!("{page}")).iter().map(|r| r["execution_id"].as_str().unwrap().to_string()));
                match page.get("next") {
                    Some(next) => args = next.clone(),
                    None => break,
                }
            }
            assert_eq!(got.len() as i64, count, "{e}");
            out.extend(got);
        }
        out.sort();
        out
    }

    #[test]
    fn a_journey_is_one_timeline_across_tabs_and_collapsing_loses_no_count() {
        let f = fixture("journey");
        let v = f.tool("session_journey", json!({"session_id": "s1"}));
        assert_eq!(v["shape"], "grouped");
        let shown: Vec<String> = v["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| format!("{} {} x{}", e["lane"].as_str().unwrap_or("-"), e["type"].as_str().unwrap(), e["count"]))
            .collect();
        assert_eq!(shown, ["t1 page x2", "t1 request x5", "t2 page x2", "t2 exception x1", "t2 browser_error x1", "- request x1",]);
        let polls = &v["timeline"][1];
        let t = |s: i64| super::compact::time(T0 + s * S);
        assert_eq!(
            (polls["route"].as_str(), polls["first"].as_str(), polls["last"].as_str()),
            (Some("GET /api/poll"), Some(t(5).as_str()), Some(t(45).as_str()))
        );
        assert_eq!(polls["cadence"], "every ~10s, regular");
        // Lossless: the counts add up to the rows, and every run expands to
        // exactly its requests (and no other session's or tab's).
        let rows = f.tool("session_journey", json!({"session_id": "s1", "shape": "rows"}));
        let all = rows["timeline"].as_array().unwrap();
        let counted: i64 = v["timeline"].as_array().unwrap().iter().map(|e| e["count"].as_i64().unwrap()).sum();
        assert_eq!(counted, all.len() as i64);
        let mut want: Vec<String> = all.iter().filter(|r| r["type"] == "request").map(|r| r["execution_id"].as_str().unwrap().to_string()).collect();
        want.sort();
        assert_eq!(request_ids(&f, &v), want);
        assert!(!want.contains(&"other".to_string()) && !want.contains(&"old".to_string()));
        let s = &v["summary"];
        assert_eq!((s["tabs"].as_i64(), s["requests"].as_i64(), s["visits"].as_i64(), s["without_tab"].as_i64()), (Some(2), Some(8), Some(2), Some(1)));
        assert_eq!(s["errors"], json!({"total": 2, "server": 1, "visits": 0, "browser": 1, "exceptions": 1}), "the exception is the 500's, not another error");
        assert_eq!(v["tabs"][1]["lane"], "t2");
        assert!(v["url"].as_str().unwrap().ends_with("/sessions/s1"));
    }

    /// The real client's poll (a visit, then its request, every 3 s): one
    /// entry for the 15, with the visits counted inside it, and lossless.
    #[test]
    fn a_polling_tab_is_one_page_entry_and_loses_nothing() {
        let f = seeded("journey-poll");
        crate::dashboard::session_journey::tests::polling(&f.tenant());
        let v = f.tool("session_journey", json!({"session_id": "p1"}));
        let shown: Vec<String> = v["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{} {} {} x{}",
                    e["lane"].as_str().unwrap_or("-"),
                    e["type"].as_str().unwrap(),
                    e["route"].as_str().or(e["component"].as_str()).unwrap_or(""),
                    e["count"]
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                "t1 visit journey/three x1",
                "- request GET /three x1",
                "t1 page GET /three x30",
                "t2 page GET /one x2",
                "- request GET /vite/one.js x1",
                "t2 page GET /two x2"
            ]
        );
        let polls = &v["timeline"][2];
        assert_eq!(
            (polls["requests"].as_i64(), polls["visits"]["count"].as_i64(), polls["visits"]["component"].as_str()),
            (Some(15), Some(15), Some("journey/three"))
        );
        assert_eq!(polls["cadence"], "every ~3s, regular");
        let rows = f.tool("session_journey", json!({"session_id": "p1", "shape": "rows"}));
        let all = rows["timeline"].as_array().unwrap();
        let counted: i64 = v["timeline"].as_array().unwrap().iter().map(|e| e["count"].as_i64().unwrap()).sum();
        assert_eq!(counted, all.len() as i64, "visits and requests all counted");
        let mut want: Vec<String> = all.iter().filter(|r| r["type"] == "request").map(|r| r["execution_id"].as_str().unwrap().to_string()).collect();
        want.sort();
        assert_eq!(request_ids(&f, &v), want, "each run expands to exactly its requests");
    }

    /// Two tabs polling the same route at once are two runs, and each
    /// run's expand lists its own tab's requests only.
    #[test]
    fn two_tabs_polling_the_same_route_expand_to_their_own_requests() {
        let f = seeded("journey-dup");
        let c = f.tenant();
        for i in 0..4 {
            request(&c, &format!("a{i}"), T0 + 10 * i * S, "/api/poll", 200, Some("dup"), Some("tab-A"));
            request(&c, &format!("b{i}"), T0 + (10 * i + 3) * S, "/api/poll", 200, Some("dup"), Some("tab-B"));
        }
        request(&c, "c0", T0 + 5 * S, "/api/poll", 200, Some("dup"), None);
        request(&c, "c1", T0 + 6 * S, "/api/poll", 200, Some("dup"), None);
        let v = f.tool("session_journey", json!({"session_id": "dup"}));
        let runs: Vec<(String, i64)> =
            v["timeline"].as_array().unwrap().iter().map(|e| (e["lane"].as_str().unwrap_or("-").to_string(), e["count"].as_i64().unwrap())).collect();
        assert_eq!(runs, [("t1".into(), 4), ("t2".into(), 4), ("-".into(), 2)]);
        let mut want: Vec<String> = (0..4).flat_map(|i| [format!("a{i}"), format!("b{i}")]).chain(["c0".into(), "c1".into()]).collect();
        want.sort();
        assert_eq!(request_ids(&f, &v), want, "each run expands to exactly its own tab's");
    }

    #[test]
    fn a_session_is_found_from_one_of_its_requests_or_its_user() {
        let f = fixture("journey-resolve");
        let by_id = f.tool("session_journey", json!({"session_id": "s1"}));
        assert_eq!(f.tool("session_journey", json!({"execution_id": "b1"})), by_id, "b1 resolves to s1");
        assert_eq!(f.tool("session_journey", json!({"execution_id": "other"}))["session_id"], "s2");
        let old = f.tool("session_journey", json!({"execution_id": "old"}));
        assert!(old["error"].as_str().unwrap().contains("no browser session"), "{old}");
        assert!(f.tool("session_journey", json!({"execution_id": "nope"}))["error"].as_str().unwrap().contains("not found"));
        let by_user = f.tool("session_journey", json!({"user": "ada", "window": "24h"}));
        assert_eq!(by_user["session_id"], "s1", "the newest activity");
        assert_eq!(by_user["user_sessions"]["total"], 2);
        for bad in [json!({}), json!({"session_id": "s1", "user": "ada"})] {
            assert!(f.tool("session_journey", bad.clone())["error"].as_str().unwrap().contains("exactly one"), "{bad}");
        }
        assert!(f.tool("session_journey", json!({"session_id": "a b"}))["error"].is_string());
    }

    #[test]
    fn addresses_and_user_agents_only_with_the_security_scope() {
        let f = fixture("journey-access");
        for shape in ["grouped", "rows"] {
            let v = f.text("session_journey", json!({"session_id": "s1", "shape": shape}));
            assert!(!v.contains("198.51.100.23") && !v.contains("Journey)"), "{shape}: {v}");
            let v: Value = serde_json::from_str(&v).unwrap();
            assert_eq!(v["access"]["client_fields"], false);
        }
        let refused = f.tool("search_requests", json!({"q": "session:s1 ip:198.51.100.23"}));
        assert!(refused["error"].as_str().unwrap().contains("security access"));
        f.grant_security();
        let v = f.tool("session_journey", json!({"session_id": "s1"}));
        assert_eq!(v["summary"]["ips"], json!(["198.51.100.23"]));
        assert_eq!(v["summary"]["user_agents"], json!(["Mozilla/5.0 (Journey)"]));
        let rows = f.tool("session_journey", json!({"session_id": "s1", "shape": "rows"}));
        assert_eq!(rows["timeline"][1]["ip"], "198.51.100.23");
    }

    #[test]
    fn max_tokens_keeps_the_newest_entries_and_counts_the_rest() {
        let f = seeded("journey-budget");
        let c = f.tenant();
        // One tab clicking through 300 different pages: nothing to fold.
        for i in 0..300 {
            request(&c, &format!("p{i}"), T0 + i * S, &format!("/page/{i}"), 200, Some("big"), Some("tab-A"));
        }
        let v = f.tool("session_journey", json!({"session_id": "big", "max_tokens": 1000}));
        let shown = v["timeline"].as_array().unwrap();
        let left = &v["timeline_left_out"];
        assert!(shown.len() < 300 && !shown.is_empty());
        assert_eq!(shown.last().unwrap()["execution_id"], "p299", "the end of the session is kept");
        assert_eq!(left["events"].as_i64().unwrap() + shown.len() as i64, 300, "{left}");
        assert!(v.to_string().len() < 4_000 + 2_000);
    }

    /// A day-long session behind many addresses still fits the result cap
    /// without max_tokens: the summary lists 20 and counts the rest, and the
    /// timeline keeps its newest entries and counts the older.
    #[test]
    fn a_huge_session_fits_the_cap_with_its_end_and_counts() {
        let f = seeded("journey-huge");
        f.grant_security();
        let c = f.tenant();
        c.execute_batch("BEGIN").unwrap();
        for i in 0..2_000 {
            c.execute(
                "INSERT INTO executions (execution_id, kind, name, method, route, group_hash, status, occurred_at, duration, user_ref, detail)
                 VALUES (?1, 'request', 'GET /p' || ?2, 'GET', '/p', 'g', 200, ?3, 2000, 'u' || ?2, ?4)",
                rusqlite::params![
                    format!("h{i}"),
                    i,
                    crate::dashboard::db::qt(T0 + i * S),
                    json!({"session_id": "huge", "tab_id": "tab-A", "ip": format!("10.0.{}.{}", i / 250, i % 250), "user_agent": format!("Mozilla/5.0 {i} {}", "x".repeat(400)), "path": format!("/p/{i}/{}", "y".repeat(300))})
                        .to_string()
                ],
            )
            .unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
        let text = f.text("session_journey", json!({"session_id": "huge"}));
        assert!(text.len() <= super::super::mcp_summary::MAX_RESULT_BYTES, "{}", text.len());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v.get("truncated").is_none(), "fits without the generic cap");
        let s = &v["summary"];
        assert_eq!((s["ips"].as_array().unwrap().len(), s["distinct_ips"].as_i64()), (20, Some(2_000)));
        assert_eq!((s["users"].as_array().unwrap().len(), s["distinct_users"].as_i64()), (20, Some(2_000)));
        let shown = v["timeline"].as_array().unwrap();
        assert_eq!(shown.last().unwrap()["execution_id"], "h1999");
        assert_eq!(v["timeline_left_out"]["events"].as_i64().unwrap() + shown.len() as i64, 2_000);
    }

    /// A user ref is app-written text: list_sessions cuts it and replaces
    /// control and bidi characters, as the journey's summary does.
    #[test]
    fn list_sessions_shows_users_as_untrusted_text() {
        let f = seeded("journey-list-users");
        let c = f.tenant();
        crate::dashboard::session_journey::tests::request(&c, "u", T0, "/", 200, Some("su"), Some("t"));
        c.execute("UPDATE executions SET user_ref = ?1 WHERE execution_id = 'u'", ["ev\u{202E}il\nx"]).unwrap();
        let v = f.tool("list_sessions", json!({"window": "24h"}));
        let users = v["sessions"][0]["users"][0].as_str().unwrap();
        assert!(!users.contains('\u{202E}') && !users.contains('\n'), "{users:?}");
    }

    #[test]
    fn list_sessions_by_user_and_errors() {
        let f = fixture("journey-list");
        let v = f.tool("list_sessions", json!({"window": "24h"}));
        let ids: Vec<&str> = v["sessions"].as_array().unwrap().iter().map(|s| s["session_id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        assert_eq!(v["sessions"][0]["errors"], json!({"total": 2, "server": 1, "visits": 0, "browser": 1}));
        assert_eq!(v["sessions"][0]["tabs"], 2);
        assert_eq!(f.tool("list_sessions", json!({"window": "24h", "has_errors": true}))["total"], 1);
        assert_eq!(f.tool("list_sessions", json!({"window": "24h", "user": "nobody"}))["total"], 0);
        assert_eq!(f.tool("list_sessions", json!({"window": "24h", "limit": 1}))["note"].as_str().map(|n| n.contains("of 2")), Some(true));
    }

    /// A per-day tenant answers the same with the session indexes in every
    /// day file, in none (the state before the background build), and in
    /// some, as the single file does.
    #[test]
    fn a_per_day_tenant_reads_the_same_journey_with_and_without_the_index() {
        let want = {
            let f = fixture("journey-single");
            spread(&f);
            (f.tool("session_journey", json!({"session_id": "s1", "shape": "rows"})), f.tool("list_sessions", json!({"window": "7d"})))
        };
        assert_eq!(want.0["timeline"].as_array().unwrap().len(), 14);
        for (name, pick) in [("all", (|_| true) as fn(usize) -> bool), ("none", |_| false), ("some", |i| i % 2 == 0)] {
            let f = fixture(&format!("journey-days-{name}"));
            spread(&f);
            f.split_into_days(|_| true);
            let telemetry = f.days_dir();
            for (i, day) in crate::storage::layout::days(&telemetry, crate::storage::layout::Tier::Core).iter().enumerate() {
                let c = rusqlite::Connection::open(crate::storage::layout::day_path(&telemetry, crate::storage::layout::Tier::Core, day)).unwrap();
                if !pick(i) {
                    c.execute_batch(&format!("DROP INDEX {}; DROP INDEX {};", crate::schema::SESSION_INDEX, crate::schema::VISIT_SESSION_INDEX)).unwrap();
                }
            }
            let got = (f.tool("session_journey", json!({"session_id": "s1", "shape": "rows"})), f.tool("list_sessions", json!({"window": "7d"})));
            assert_eq!(got, want, "{name}");
        }
    }

    /// The session's requests the day before, so a per-day tenant has two
    /// day files of it.
    fn spread(f: &Seeded) {
        let c = f.tenant();
        request(&c, "yesterday", T0 - 86_400 * S, "/orders", 200, Some("s1"), Some("tab-A"));
        visit(&c, T0 - 86_400 * S - S, "orders/index", "/orders", Some("s1"), Some("tab-A"));
    }
}
