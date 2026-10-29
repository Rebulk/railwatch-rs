//! One browser session across its tabs (docs/sessions.md): the page visits,
//! server requests, browser errors and exceptions that carried one
//! `session_id`, in time order, each in the lane of the tab it came from.
//!
//! The gem tags a request with `session_id` and `tab_id` (kept in
//! `detail`), and a visit with the same two (columns). A request read is a
//! seek on `idx_executions_sessions`, a visit read one on
//! `idx_visits_sessions` (schema::ADDED_INDEXES); rows without a session id
//! (older gems, jobs, API clients) are in neither and never in a journey.
//!
//! Exceptions join through their request's execution_id. A browser error
//! carries the session and tab in its context (`context.browser.session`,
//! `.tab`); one from a gem without the tab goes in the lane of the tab
//! whose visit it names (the latest visit at or before it with the same
//! url), else in no lane.
//!
//! Lanes: tabs labelled t1, t2, ... in the order they were first seen. A
//! request with a session and no tab (a full page load carries the cookie,
//! not the header) is in no lane.

use std::collections::{BTreeSet, HashMap};

use rusqlite::Connection;
use serde_json::Value;

use super::db::{self, Rec, qt};
use super::filter_query;

/// The browser client's idle timeout (docs/sessions.md).
pub const SESSION_IDLE: i64 = 30 * 60 * 1_000_000;

/// Requests, and visits, a journey reads at most (the newest).
pub const MAX_EVENTS: usize = 20_000;

/// One request of the session, with what the gem keeps of it in `detail`.
#[derive(Debug, Clone)]
pub struct Request {
    pub execution_id: String,
    pub name: String,
    pub group_hash: String,
    pub method: Option<String>,
    pub status: Option<i64>,
    pub duration: i64,
    pub user: Option<String>,
    pub tenant: Option<String>,
    pub exception_preview: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub path: Option<String>,
    pub country: Option<String>,
    pub redirect_to: Option<String>,
    pub halted: Option<String>,
    pub rate_limited: bool,
    pub rate_limit: Option<String>,
    /// The dropped keys, as the JSON array the gem recorded.
    pub unpermitted_keys: Option<String>,
    pub response_size: Option<i64>,
}

impl Request {
    /// The request as request_scan's fold reads one.
    pub fn row(&self, at: i64) -> super::request_scan::Row<'_> {
        let exception = self.exception_preview.as_deref().map(|p| p.split_once(": ").map_or(p, |(c, _)| c));
        super::request_scan::Row {
            at,
            execution_id: &self.execution_id,
            name: &self.name,
            group_hash: &self.group_hash,
            method: self.method.as_deref(),
            status: self.status,
            duration: self.duration,
            user: self.user.as_deref(),
            tenant: self.tenant.as_deref(),
            exception,
            exception_preview: self.exception_preview.as_deref(),
            ip: self.ip.as_deref(),
            user_agent: self.user_agent.as_deref(),
            path: self.path.as_deref(),
            country: self.country.as_deref(),
            rate_limited: self.rate_limited,
            rate_limit: self.rate_limit.as_deref(),
            halted: self.halted.as_deref(),
            unpermitted: self.unpermitted_keys.as_deref().is_some_and(|k| k != "[]"),
            unpermitted_keys: self.unpermitted_keys.as_deref(),
            redirect_to: self.redirect_to.as_deref(),
            response_size: self.response_size,
        }
    }

    pub fn outcome(&self) -> Option<&'static str> {
        super::request_scan::outcome(self.status, self.redirect_to.as_deref(), self.halted.as_deref())
    }
}

/// One page visit (an initial load or an Inertia visit).
#[derive(Debug, Clone)]
pub struct Visit {
    pub id: i64,
    pub component: Option<String>,
    pub url: Option<String>,
    pub method: Option<String>,
    /// Microseconds, as stored.
    pub duration: i64,
    pub status: Option<String>,
    pub partial: bool,
    pub lcp: Option<i64>,
    pub cls: Option<f64>,
    pub inp: Option<i64>,
    pub ttfb: Option<i64>,
    pub user: Option<String>,
    pub user_agent: Option<String>,
}

/// An exception: one a request of the session raised, or a browser error.
#[derive(Debug, Clone)]
pub struct Error {
    pub id: i64,
    pub class_name: String,
    pub message: String,
    pub browser: bool,
    pub handled: bool,
    pub execution_id: Option<String>,
    pub group_hash: Option<String>,
    /// A browser error's page (its visit's url, else the url it was raised at).
    pub url: Option<String>,
    pub component: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Request(Request),
    Visit(Visit),
    Error(Error),
}

#[derive(Debug, Clone)]
pub struct Event {
    pub at: i64,
    /// The gem's tab id.
    pub tab: Option<String>,
    /// The index in Journey::tabs of `tab` (None: no tab).
    pub lane: Option<usize>,
    pub kind: Kind,
}

impl Event {
    pub fn type_name(&self) -> &'static str {
        match &self.kind {
            Kind::Request(_) => "request",
            Kind::Visit(_) => "visit",
            Kind::Error(e) if e.browser => "browser_error",
            Kind::Error(_) => "exception",
        }
    }

    /// What makes two events in a row of one tab the same, to collapse:
    /// a request's status, route, outcome, halted callback and exception
    /// class (as mcp_compact keys requests), a visit's component, method,
    /// status and partial flag, an exception's class and message.
    pub fn key(&self) -> Vec<String> {
        let o = |s: &Option<String>| s.clone().unwrap_or_default();
        match &self.kind {
            Kind::Request(r) => {
                let exception = r.exception_preview.as_deref().map(|p| p.split_once(": ").map_or(p, |(c, _)| c)).unwrap_or("");
                vec![
                    "request".into(),
                    r.status.map(|s| s.to_string()).unwrap_or_default(),
                    r.name.clone(),
                    r.outcome().unwrap_or("").into(),
                    o(&r.halted),
                    exception.into(),
                ]
            }
            Kind::Visit(v) => vec!["visit".into(), o(&v.component), o(&v.method), o(&v.status), v.partial.to_string()],
            Kind::Error(e) => vec![self.type_name().into(), e.class_name.clone(), e.message.clone()],
        }
    }

    /// Requests sort before the exceptions they raised and visits at the same instant.
    fn rank(&self) -> u8 {
        match self.kind {
            Kind::Request(_) => 0,
            Kind::Error(_) => 1,
            Kind::Visit(_) => 2,
        }
    }

    /// Whether the event is drawn as an error (an exception included).
    pub fn is_error(&self) -> bool {
        match &self.kind {
            Kind::Request(r) => r.status.is_some_and(|s| s >= 500),
            Kind::Visit(v) => v.status.as_deref() == Some("error"),
            Kind::Error(_) => true,
        }
    }
}

/// One tab of the session, in first-seen order.
#[derive(Debug, Clone, Default)]
pub struct Tab {
    pub id: String,
    pub label: String,
    pub first: i64,
    pub last: i64,
    pub requests: i64,
    pub visits: i64,
    pub errors: i64,
}

/// A session's errors (Journey::errors).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ErrorCounts {
    pub server: i64,
    pub visits: i64,
    pub browser: i64,
    /// Exceptions the session's requests raised: not in `total`.
    pub exceptions: i64,
}

impl ErrorCounts {
    pub fn total(&self) -> i64 {
        self.server + self.visits + self.browser
    }
}

/// The newest MAX_EVENTS of a kind were read and older ones exist.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Capped {
    pub requests: bool,
    pub visits: bool,
}

#[derive(Debug, Clone)]
pub struct Journey {
    pub session_id: String,
    /// Oldest first.
    pub events: Vec<Event>,
    pub tabs: Vec<Tab>,
    pub capped: Capped,
}

/// Consecutive steps of one lane with the same key: one run, its events'
/// indexes into Journey::events (oldest first).
#[derive(Debug, Clone)]
pub struct Run {
    pub lane: Option<usize>,
    pub events: Vec<usize>,
}

/// How long after a visit starts its own request may arrive.
const STEP_GAP: i64 = 5_000_000;

/// The path of a visit's url (the browser client sends it absolute or not).
fn url_path(url: &str) -> &str {
    // Query and fragment off first: either may hold a "://" of its own.
    let url = url.split(['?', '#']).next().unwrap_or(url);
    match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => url,
    }
}

impl Journey {
    /// The lane's steps: a page visit together with its own request (the
    /// next request in the same tab, within STEP_GAP, to the visit's path),
    /// or any other event alone. Each step is its events' indexes.
    fn steps(&self) -> Vec<Vec<usize>> {
        let mut steps: Vec<Vec<usize>> = vec![];
        // lane -> (the step of a visit still waiting for its request, the visit's path, its time)
        let mut waiting: HashMap<Option<usize>, (usize, String, i64)> = HashMap::new();
        for (i, e) in self.events.iter().enumerate() {
            match &e.kind {
                Kind::Visit(v) => {
                    waiting.insert(e.lane, (steps.len(), url_path(v.url.as_deref().unwrap_or("")).to_string(), e.at));
                    steps.push(vec![i]);
                }
                Kind::Request(r) => match waiting.remove(&e.lane) {
                    Some((step, path, at)) if e.at - at <= STEP_GAP && r.path.as_deref().map(url_path) == Some(path.as_str()) => steps[step].push(i),
                    _ => steps.push(vec![i]),
                },
                Kind::Error(_) => {
                    // An error ends the visit's wait: a request after it is not the visit's.
                    waiting.remove(&e.lane);
                    steps.push(vec![i]);
                }
            }
        }
        steps.sort_by_key(|s| s[0]);
        steps
    }

    /// The runs, by their first event (oldest first): consecutive steps of
    /// one lane alike in every event's key. A run only ends when its own tab
    /// does something else, so a tab polling (an Inertia reload is a visit
    /// and its request) while another is used stays one run.
    pub fn runs(&self) -> Vec<Run> {
        let mut runs: Vec<(Vec<Vec<String>>, Run)> = vec![];
        // lane -> the index in `runs` of its open run.
        let mut open: HashMap<Option<usize>, usize> = HashMap::new();
        for step in self.steps() {
            let lane = self.events[step[0]].lane;
            let key: Vec<Vec<String>> = step.iter().map(|&i| self.events[i].key()).collect();
            match open.get(&lane) {
                Some(&r) if runs[r].0 == key => runs[r].1.events.extend(step),
                _ => {
                    open.insert(lane, runs.len());
                    runs.push((key, Run { lane, events: step }));
                }
            }
        }
        runs.into_iter().map(|(_, r)| r).collect()
    }

    /// Error counts, one definition for the summary, the page and
    /// list_sessions: 5xx requests, failed visits and browser errors. An
    /// exception a request raised is part of that request's error, counted
    /// apart (`exceptions`) and never added to the total.
    pub fn errors(&self) -> ErrorCounts {
        let mut c = ErrorCounts::default();
        for e in &self.events {
            match &e.kind {
                Kind::Request(r) if r.status.is_some_and(|s| s >= 500) => c.server += 1,
                Kind::Visit(v) if v.status.as_deref() == Some("error") => c.visits += 1,
                Kind::Error(x) if x.browser => c.browser += 1,
                Kind::Error(_) => c.exceptions += 1,
                _ => {}
            }
        }
        c
    }

    /// A lane's label (t1, t2, ...), from a label or a gem tab id.
    pub fn lane_of(&self, tab: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.label == tab || t.id == tab)
    }

    pub fn label(&self, lane: Option<usize>) -> Option<&str> {
        lane.map(|l| self.tabs[l].label.as_str())
    }

    pub fn first(&self) -> Option<i64> {
        self.events.first().map(|e| e.at)
    }

    pub fn last(&self) -> Option<i64> {
        self.events.last().map(|e| e.at)
    }

    /// Distinct values of a request or visit field, in first-seen order.
    pub fn distinct(&self, f: impl Fn(&Event) -> Option<&str>) -> Vec<String> {
        let mut seen = indexmap::IndexSet::new();
        for e in &self.events {
            if let Some(v) = f(e).filter(|v| !v.is_empty()) {
                seen.insert(v.to_string());
            }
        }
        seen.into_iter().collect()
    }
}

/// The session the request `execution_id` carried: Ok(None) when there is
/// no such request, Ok(Some(None)) when it carried no session.
pub fn session_of(conn: &Connection, execution_id: &str) -> rusqlite::Result<Option<Option<String>>> {
    let row = db::query_one(
        conn,
        "SELECT json_extract(\"executions\".\"detail\", '$.session_id') AS \"session_id\" FROM \"executions\" \
         WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1",
        [execution_id],
    )?;
    Ok(row.map(|r| r.s("session_id").map(str::to_string)))
}

/// One session's events across its tabs (at most MAX_EVENTS requests and
/// MAX_EVENTS visits, the newest), oldest first.
pub fn read(conn: &Connection, session_id: &str) -> rusqlite::Result<Journey> {
    read_capped(conn, session_id, MAX_EVENTS)
}

pub fn read_capped(conn: &Connection, session_id: &str, cap: usize) -> rusqlite::Result<Journey> {
    let mut events = vec![];
    let mut capped = Capped::default();

    let from = filter_query::session_index(conn)?.unwrap_or_else(|| "\"executions\"".into());
    let rows = db::query(
        conn,
        &format!(
            "SELECT \"executions\".\"occurred_at\", \"executions\".\"execution_id\", \"executions\".\"name\", \"executions\".\"group_hash\", \
             \"executions\".\"method\", \"executions\".\"status\", \"executions\".\"duration\", \"executions\".\"user_ref\", \"executions\".\"app_tenant\", \
             \"executions\".\"exception_preview\", \"executions\".\"detail\" FROM {from} \
             WHERE \"executions\".\"kind\" = 'request' AND {} = ?1 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {}",
            filter_query::detail_sql("executions", "$.session_id"),
            cap + 1
        ),
        [session_id],
    )?;
    capped.requests = rows.len() > cap;
    for r in rows.iter().take(cap) {
        let detail = r.json("detail");
        let s = |k: &str| detail.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
        let rate = detail.get("rate_limited").filter(|v| !v.is_null() && **v != Value::Bool(false));
        let keys = detail.get("unpermitted_parameters").filter(|v| v.as_array().is_some_and(|a| !a.is_empty())).map(Value::to_string);
        events.push(Event {
            at: r.t("occurred_at").unwrap_or(0),
            tab: s("tab_id"),
            lane: None,
            kind: Kind::Request(Request {
                execution_id: r.s("execution_id").unwrap_or("").into(),
                name: r.s("name").unwrap_or("").into(),
                group_hash: r.s("group_hash").unwrap_or("").into(),
                method: r.s("method").map(str::to_string),
                status: r.i("status"),
                duration: r.i("duration").unwrap_or(0),
                user: r.s("user_ref").map(str::to_string),
                tenant: r.s("app_tenant").map(str::to_string),
                exception_preview: r.s("exception_preview").map(str::to_string),
                ip: s("ip"),
                user_agent: s("user_agent"),
                path: s("path"),
                country: detail.get("headers").and_then(|h| h.get("Cf-Ipcountry")).and_then(Value::as_str).map(str::to_string),
                redirect_to: s("redirect_to"),
                halted: s("halted_callback"),
                rate_limited: rate.is_some(),
                rate_limit: rate.map(|r| r.get("name").and_then(Value::as_str).unwrap_or("").to_string()),
                unpermitted_keys: keys,
                response_size: detail.get("response_size").and_then(Value::as_i64),
            }),
        });
    }

    let from = filter_query::visit_session_index(conn)?.unwrap_or_else(|| "\"visits\"".into());
    let rows = db::query(
        conn,
        &format!("SELECT \"visits\".* FROM {from} WHERE \"visits\".\"session_id\" = ?1 ORDER BY \"visits\".\"occurred_at\" DESC LIMIT {}", cap + 1),
        [session_id],
    )?;
    capped.visits = rows.len() > cap;
    for r in rows.iter().take(cap) {
        let s = |k: &str| r.s(k).filter(|v| !v.is_empty()).map(str::to_string);
        events.push(Event {
            at: r.t("occurred_at").unwrap_or(0),
            tab: s("tab_id"),
            lane: None,
            kind: Kind::Visit(Visit {
                id: r.i("id").unwrap_or(0),
                component: s("component"),
                url: s("url"),
                method: s("method"),
                duration: r.i("duration").unwrap_or(0),
                status: s("status"),
                partial: r.b("partial").unwrap_or(false),
                lcp: r.i("lcp"),
                cls: r.f("cls"),
                inp: r.i("inp"),
                ttfb: r.i("ttfb"),
                user: s("user_ref"),
                user_agent: s("user_agent"),
            }),
        });
    }

    // What the session's requests raised, and the browser's own errors.
    let tab_of: HashMap<String, Option<String>> = events
        .iter()
        .filter_map(|e| match &e.kind {
            Kind::Request(r) => Some((r.execution_id.clone(), e.tab.clone())),
            _ => None,
        })
        .collect();
    let mut errors: Vec<(Rec, Option<String>)> = vec![];
    if !tab_of.is_empty() {
        let ids = serde_json::to_string(&tab_of.keys().collect::<Vec<_>>()).expect("json");
        for r in db::query(
            conn,
            &format!(
                "SELECT {ERROR_COLUMNS} FROM \"exceptions\" WHERE \"exceptions\".\"execution_id\" IN (SELECT value FROM json_each(?1)) \
                 AND \"exceptions\".\"source\" IS NOT 'browser'"
            ),
            [ids],
        )? {
            let tab = r.s("execution_id").and_then(|id| tab_of.get(id)).cloned().flatten();
            errors.push((r, tab));
        }
    }
    if let (Some(lo), Some(hi)) = (events.iter().map(|e| e.at).min(), events.iter().map(|e| e.at).max()) {
        // A browser error may be all the session did in a tab for a while,
        // and is beaconed late: up to the idle timeout (30 min) either side,
        // past which the browser would have started a new session.
        let slack = SESSION_IDLE;
        let browser = db::query(
            conn,
            &format!(
                "SELECT {ERROR_COLUMNS} FROM \"exceptions\" WHERE \"exceptions\".\"occurred_at\" BETWEEN ?1 AND ?2 \
                 AND \"exceptions\".\"source\" = 'browser' AND json_extract(\"exceptions\".\"context\", '$.browser.session') = ?3"
            ),
            rusqlite::params![qt(lo - slack), qt(hi + slack), session_id],
        )?;
        // (The query above left browser errors out: ids repeat across day files.)
        errors.extend(browser.into_iter().map(|r| (r, None)));
    }
    for (r, tab) in errors {
        let context: Value = r.s("context").and_then(|c| serde_json::from_str(c).ok()).unwrap_or(Value::Null);
        let browser_cx = context.get("browser");
        let bs = |k: &str| browser_cx.and_then(|b| b.get(k)).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
        let browser = r.s("source") == Some("browser");
        // A browser error's own tab (gem 1.5+: `context.browser.tab`); an
        // older one's is worked out from its page below.
        let tab = if browser { bs("tab").filter(|t| filter_query::valid_session_id(t)) } else { tab };
        events.push(Event {
            at: r.t("occurred_at").unwrap_or(0),
            tab,
            lane: None,
            kind: Kind::Error(Error {
                id: r.i("id").unwrap_or(0),
                class_name: r.s("class_name").unwrap_or("").into(),
                message: r.s("message").unwrap_or("").into(),
                browser,
                handled: r.b("handled").unwrap_or(false),
                execution_id: if browser { None } else { r.s("execution_id").map(str::to_string) },
                group_hash: r.s("group_hash").map(str::to_string),
                url: bs("visit").or_else(|| bs("url")),
                component: bs("component"),
            }),
        });
    }

    let id_of = |e: &Event| match &e.kind {
        Kind::Request(r) => r.execution_id.clone(),
        Kind::Visit(v) => format!("{:020}", v.id),
        Kind::Error(x) => format!("{:020}", x.id),
    };
    events.sort_by(|a, b| a.at.cmp(&b.at).then(a.rank().cmp(&b.rank())).then_with(|| id_of(a).cmp(&id_of(b))));

    // A browser error without its tab goes in the lane of the visit it names.
    let mut last_visit_tab: HashMap<String, String> = HashMap::new();
    for e in events.iter_mut() {
        match &e.kind {
            Kind::Visit(v) => {
                if let (Some(url), Some(tab)) = (&v.url, &e.tab) {
                    last_visit_tab.insert(url.clone(), tab.clone());
                }
            }
            Kind::Error(x) if x.browser && e.tab.is_none() => e.tab = x.url.as_ref().and_then(|u| last_visit_tab.get(u)).cloned(),
            _ => {}
        }
    }

    let mut tabs: Vec<Tab> = vec![];
    for e in events.iter_mut() {
        let Some(id) = &e.tab else { continue };
        let lane = match tabs.iter().position(|t| &t.id == id) {
            Some(l) => l,
            None => {
                tabs.push(Tab { id: id.clone(), label: format!("t{}", tabs.len() + 1), first: e.at, ..Tab::default() });
                tabs.len() - 1
            }
        };
        e.lane = Some(lane);
        let t = &mut tabs[lane];
        t.last = e.at;
        match e.kind {
            Kind::Request(_) => t.requests += 1,
            Kind::Visit(_) => t.visits += 1,
            Kind::Error(_) => {}
        }
        // Journey::errors' definition: an exception is its request's error.
        if e.is_error() && !matches!(&e.kind, Kind::Error(x) if !x.browser) {
            t.errors += 1;
        }
    }
    Ok(Journey { session_id: session_id.to_string(), events, tabs, capped })
}

const ERROR_COLUMNS: &str = "\"exceptions\".\"id\", \"exceptions\".\"occurred_at\", \"exceptions\".\"class_name\", \"exceptions\".\"message\", \
     \"exceptions\".\"execution_id\", \"exceptions\".\"handled\", \"exceptions\".\"group_hash\", \"exceptions\".\"source\", \"exceptions\".\"context\"";

// -- Discovery ----------------------------------------------------------------

/// One session as a window's activity shows it.
#[derive(Debug, Clone, Default)]
pub struct Listed {
    pub session_id: String,
    pub first: i64,
    pub last: i64,
    pub requests: i64,
    /// 5xx requests.
    pub server_errors: i64,
    pub visits: i64,
    /// Visits the browser client marked failed.
    pub visit_errors: i64,
    pub browser_errors: i64,
    pub tabs: BTreeSet<String>,
    pub users: BTreeSet<String>,
}

impl Listed {
    pub fn errors(&self) -> i64 {
        self.server_errors + self.visit_errors + self.browser_errors
    }
}

/// The sessions active in [from, to] (a request, visit or browser error
/// in it), newest activity first: at most `limit`, and how many there are
/// in all. `user` keeps the sessions any of whose records names that user,
/// counted whole: a visit or browser error the beacon could not attribute
/// is still its session's. `has_errors` keeps the ones with an error
/// (Journey::errors' definition: a 5xx, a failed visit or a browser error).
/// Counts are of the window's activity.
pub fn list(conn: &Connection, (from, to): (i64, i64), user: Option<&str>, has_errors: bool, limit: usize) -> rusqlite::Result<(Vec<Listed>, usize)> {
    let mut by: HashMap<String, Listed> = HashMap::new();
    let sid = filter_query::detail_sql("executions", "$.session_id");
    let tab = filter_query::detail_sql("executions", "$.tab_id");
    let binds: Vec<rusqlite::types::Value> = vec![qt(from).into(), qt(to).into()];
    // Requests: on the session index, which holds the tab, status and user,
    // in the files the window touches.
    let inner = format!(
        "SELECT {sid} AS \"sid\", {tab} AS \"tab\", MIN(\"executions\".\"occurred_at\") AS \"first\", MAX(\"executions\".\"occurred_at\") AS \"last\", \
         COUNT(*) AS \"n\", SUM(\"executions\".\"status\" >= 500) AS \"errors\", group_concat(DISTINCT \"executions\".\"user_ref\") AS \"users\" FROM {{t}} \
         WHERE \"executions\".\"kind\" = 'request' AND {sid} IS NOT NULL AND \"executions\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY 1, 2"
    );
    let hint = crate::storage::reader::Hint { window: (from, to), inside: crate::schema::SESSION_INDEX, outside: "index_executions_on_kind_and_occurred_at" };
    let rows = match crate::storage::reader::aggregate_hinted(conn, "executions", Some(hint), &inner, "SELECT * FROM {u}", &binds)? {
        Some(rows) => rows,
        None => {
            let table = filter_query::session_index(conn)?.unwrap_or_else(|| "\"executions\"".into());
            db::query(conn, &inner.replace("{t}", &table), rusqlite::params_from_iter(binds.iter()))?
        }
    };
    // Only ids the session pages accept (ingest keeps whatever the gem sent).
    for r in rows.into_iter().filter(|r| r.s("sid").is_some_and(filter_query::valid_session_id)) {
        let s = add(&mut by, &r);
        s.requests += r.i("n").unwrap_or(0);
        s.server_errors += r.i("errors").unwrap_or(0);
    }
    let inner = "SELECT \"visits\".\"session_id\" AS \"sid\", \"visits\".\"tab_id\" AS \"tab\", MIN(\"visits\".\"occurred_at\") AS \"first\", \
         MAX(\"visits\".\"occurred_at\") AS \"last\", COUNT(*) AS \"n\", SUM(\"visits\".\"status\" = 'error') AS \"errors\", \
         group_concat(DISTINCT \"visits\".\"user_ref\") AS \"users\" \
         FROM {t} WHERE \"visits\".\"session_id\" IS NOT NULL AND \"visits\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY 1, 2";
    let hint = crate::storage::reader::Hint { window: (from, to), inside: "index_visits_on_occurred_at", outside: "index_visits_on_occurred_at" };
    let rows = match crate::storage::reader::aggregate_hinted(conn, "visits", Some(hint), inner, "SELECT * FROM {u}", &binds)? {
        Some(rows) => rows,
        None => db::query(conn, &inner.replace("{t}", "\"visits\""), rusqlite::params_from_iter(binds.iter()))?,
    };
    // Only ids the session pages accept (ingest keeps whatever the gem sent).
    for r in rows.into_iter().filter(|r| r.s("sid").is_some_and(filter_query::valid_session_id)) {
        let s = add(&mut by, &r);
        s.visits += r.i("n").unwrap_or(0);
        s.visit_errors += r.i("errors").unwrap_or(0);
    }
    // Browser errors are few: read them through the view.
    let browser = db::query(
        conn,
        "SELECT json_extract(\"exceptions\".\"context\", '$.browser.session') AS \"sid\", COUNT(*) AS \"n\", MIN(\"exceptions\".\"occurred_at\") AS \"first\", \
         MAX(\"exceptions\".\"occurred_at\") AS \"last\", group_concat(DISTINCT \"exceptions\".\"user_ref\") AS \"users\" FROM \"exceptions\" \
         WHERE \"exceptions\".\"source\" = 'browser' AND \"exceptions\".\"occurred_at\" BETWEEN ?1 AND ?2 GROUP BY 1",
        rusqlite::params_from_iter(binds.iter()),
    )?;
    for r in browser {
        if r.s("sid").is_some_and(filter_query::valid_session_id) {
            let s = add(&mut by, &r);
            s.browser_errors += r.i("n").unwrap_or(0);
        }
    }
    let mut all: Vec<Listed> = by.into_values().filter(|s| !has_errors || s.errors() > 0).filter(|s| user.is_none_or(|u| s.users.contains(u))).collect();
    all.sort_by(|a, b| b.last.cmp(&a.last).then(a.session_id.cmp(&b.session_id)));
    let total = all.len();
    all.truncate(limit);
    Ok((all, total))
}

/// The session `r` (a grouped row: sid, tab, first, last, users) counts
/// toward, with its times, tab and user added.
fn add<'a>(by: &'a mut HashMap<String, Listed>, r: &Rec) -> &'a mut Listed {
    let sid = r.s("sid").unwrap_or("").to_string();
    let (first, last) = (r.t("first").unwrap_or(0), r.t("last").unwrap_or(0));
    let s = by.entry(sid.clone()).or_insert_with(|| Listed { session_id: sid, first, last, ..Listed::default() });
    s.first = s.first.min(first);
    s.last = s.last.max(last);
    if let Some(t) = r.s("tab").filter(|t| !t.is_empty()) {
        s.tabs.insert(t.to_string());
    }
    // group_concat's list (user refs hold no commas in practice; one that
    // does is split, which only widens the user filter's match).
    for u in r.s("users").unwrap_or("").split(',').filter(|u| !u.is_empty()) {
        s.users.insert(u.to_string());
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::params;
    use serde_json::json;

    use super::*;

    pub const S: i64 = 1_000_000;
    pub const T0: i64 = 1_792_000_000_000_000 - 3_600 * S;

    pub fn request(c: &Connection, id: &str, at: i64, route: &str, status: i64, session: Option<&str>, tab: Option<&str>) {
        let mut detail = json!({"path": route, "ip": "198.51.100.23", "user_agent": "Mozilla/5.0 (Journey)"});
        if let Some(s) = session {
            detail["session_id"] = json!(s);
        }
        if let Some(t) = tab {
            detail["tab_id"] = json!(t);
        }
        c.execute(
            "INSERT INTO executions (execution_id, kind, name, method, route, group_hash, status, occurred_at, duration, user_ref, detail)
             VALUES (?1, 'request', 'GET ' || ?2, 'GET', ?2, 'g', ?3, ?4, 2000, 'ada', ?5)",
            params![id, route, status, qt(at), detail.to_string()],
        )
        .unwrap();
    }

    pub fn visit(c: &Connection, at: i64, component: &str, url: &str, session: Option<&str>, tab: Option<&str>) {
        c.execute(
            "INSERT INTO visits (occurred_at, component, url, method, duration, status, user_ref, user_agent, session_id, tab_id)
             VALUES (?1, ?2, ?3, 'GET', 120000, '200', 'ada', 'Mozilla/5.0 (Journey)', ?4, ?5)",
            params![qt(at), component, url, session, tab],
        )
        .unwrap();
    }

    /// Session s1, two tabs interleaved, oldest first:
    ///   t1 (tab-A): visit /orders, GET /orders, then GET /api/poll x5 (every 10 s)
    ///   t2 (tab-B): opens at +12 s: visit /orders/7, GET /orders/7 (500, raises), a browser error on /orders/7
    ///   no tab: a full page load request of s1
    /// And noise: session s2's requests and visits, and an old request
    /// without any session.
    pub fn two_tabs(c: &Connection) {
        visit(c, T0, "orders/index", "/orders", Some("s1"), Some("tab-A"));
        request(c, "a1", T0 + S, "/orders", 200, Some("s1"), Some("tab-A"));
        for i in 0..5 {
            request(c, &format!("poll{i}"), T0 + (5 + 10 * i) * S, "/api/poll", 200, Some("s1"), Some("tab-A"));
        }
        visit(c, T0 + 12 * S, "orders/show", "/orders/7", Some("s1"), Some("tab-B"));
        request(c, "b1", T0 + 13 * S, "/orders/7", 500, Some("s1"), Some("tab-B"));
        c.execute("UPDATE executions SET exception_preview = 'NoMethodError: undefined method' WHERE execution_id = 'b1'", []).unwrap();
        c.execute(
            "INSERT INTO exceptions (execution_id, execution_source, class_name, message, handled, occurred_at, source)
             VALUES ('b1', 'request', 'NoMethodError', 'undefined method', 0, ?1, 'app/models/order.rb:3')",
            [qt(T0 + 13 * S + 500)],
        )
        .unwrap();
        c.execute(
            "INSERT INTO exceptions (class_name, message, handled, occurred_at, source, context)
             VALUES ('TypeError', 'x is undefined', 0, ?1, 'browser', ?2)",
            params![qt(T0 + 20 * S), json!({"browser": {"session": "s1", "visit": "/orders/7", "component": "orders/show"}}).to_string()],
        )
        .unwrap();
        request(c, "full", T0 + 30 * S, "/settings", 200, Some("s1"), None);
        // Another session at the same time, and a request from before sessions.
        visit(c, T0 + 2 * S, "orders/index", "/orders", Some("s2"), Some("tab-A"));
        request(c, "other", T0 + 3 * S, "/orders", 200, Some("s2"), Some("tab-A"));
        request(c, "old", T0 - 60 * S, "/orders", 200, None, None);
        c.execute(
            "INSERT INTO visits (occurred_at, component, url, method, duration, status) VALUES (?1, 'orders/index', '/orders', 'GET', 1, '200')",
            [qt(T0)],
        )
        .unwrap();
    }

    /// What the real browser client records when a tab reloads its page on
    /// a timer (an Inertia poll): a visit from the beacon (absolute url, as
    /// the client sends it after the first load) and, a few ms later, the
    /// request it made. Session p1: tab-C polls /three 15 times, 3 s apart;
    /// tab-D clicks /one then /two in between; an asset request (no tab)
    /// lands right after one poll's visit.
    pub fn polling(c: &Connection) {
        visit(c, T0, "journey/three", "/three", Some("p1"), Some("tab-C"));
        request(c, "first", T0 + 1_000, "/three", 200, Some("p1"), None);
        for i in 0..15 {
            let at = T0 + (3 + 3 * i) * S;
            visit(c, at, "journey/three", "http://localhost:3123/three", Some("p1"), Some("tab-C"));
            request(c, &format!("poll{i}"), at + 2_000, "/three", 200, Some("p1"), Some("tab-C"));
        }
        visit(c, T0 + 7 * S + 300_000, "journey/one", "http://localhost:3123/one", Some("p1"), Some("tab-D"));
        request(c, "one", T0 + 7 * S + 302_000, "/one", 200, Some("p1"), Some("tab-D"));
        request(c, "asset", T0 + 7 * S + 310_000, "/vite/one.js", 200, Some("p1"), None);
        visit(c, T0 + 31 * S + 300_000, "journey/two", "http://localhost:3123/two", Some("p1"), Some("tab-D"));
        request(c, "two", T0 + 31 * S + 302_000, "/two", 200, Some("p1"), Some("tab-D"));
    }

    fn tenant(name: &str) -> (std::path::PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!("rw-journey-{name}-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let c = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        (dir, c)
    }

    fn describe(j: &Journey) -> Vec<String> {
        j.events
            .iter()
            .map(|e| {
                let what = match &e.kind {
                    Kind::Request(r) => r.execution_id.clone(),
                    Kind::Visit(v) => v.url.clone().unwrap_or_default(),
                    Kind::Error(x) => x.class_name.clone(),
                };
                format!("{} {} {what}", j.label(e.lane).unwrap_or("-"), e.type_name())
            })
            .collect()
    }

    #[test]
    fn two_tabs_interleave_in_time_order_each_in_its_own_lane() {
        let (dir, c) = tenant("lanes");
        two_tabs(&c);
        let j = read(&c, "s1").unwrap();
        assert_eq!(
            describe(&j),
            [
                "t1 visit /orders",
                "t1 request a1",
                "t1 request poll0",
                "t2 visit /orders/7",
                "t2 request b1",
                "t2 exception NoMethodError",
                "t1 request poll1",
                "t2 browser_error TypeError",
                "t1 request poll2",
                "- request full",
                "t1 request poll3",
                "t1 request poll4",
            ]
        );
        assert_eq!(j.tabs.iter().map(|t| (t.label.as_str(), t.id.as_str())).collect::<Vec<_>>(), [("t1", "tab-A"), ("t2", "tab-B")], "first seen first");
        assert_eq!((j.tabs[0].requests, j.tabs[0].visits, j.tabs[0].errors), (6, 1, 0));
        assert_eq!((j.tabs[1].requests, j.tabs[1].visits, j.tabs[1].errors), (1, 1, 2), "the 500 and the browser error; the exception is the 500's");
        assert_eq!(j.capped, Capped::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A browser error that says its tab is in that tab's lane, even when
    /// another tab's visit has the page it names; one without falls back to
    /// that visit's tab.
    #[test]
    fn a_browser_error_goes_in_the_lane_of_its_own_tab_else_of_its_page() {
        let (dir, c) = tenant("errtab");
        two_tabs(&c);
        // tab-A opened /orders/7 too, after tab-B: the page alone would say tab-A.
        visit(&c, T0 + 18 * S, "orders/show", "/orders/7", Some("s1"), Some("tab-A"));
        for (at, tab) in [(T0 + 19 * S, Some("tab-B")), (T0 + 21 * S, None)] {
            let mut browser = json!({"session": "s1", "visit": "/orders/7"});
            if let Some(t) = tab {
                browser["tab"] = json!(t);
            }
            c.execute(
                "INSERT INTO exceptions (class_name, message, handled, occurred_at, source, context) VALUES ('RangeError', 'tab test', 0, ?1, 'browser', ?2)",
                params![qt(at), json!({"browser": browser}).to_string()],
            )
            .unwrap();
        }
        let j = read(&c, "s1").unwrap();
        let lanes: Vec<Option<&str>> =
            j.events.iter().filter(|e| matches!(&e.kind, Kind::Error(x) if x.class_name == "RangeError")).map(|e| j.label(e.lane)).collect();
        assert_eq!(lanes, [Some("t2"), Some("t1")], "its own tab; then the page's latest visit (tab-A)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A browser error beaconed ten minutes after the session's last
    /// request is still the session's (as list_sessions counts it); one
    /// past the idle timeout is not.
    #[test]
    fn a_late_browser_error_is_in_the_journey_within_the_idle_timeout() {
        let (dir, c) = tenant("late");
        two_tabs(&c);
        for (at, class) in [(T0 + 45 * S + 10 * 60 * S, "Late"), (T0 + 45 * S + 31 * 60 * S, "TooLate")] {
            c.execute(
                "INSERT INTO exceptions (class_name, message, handled, occurred_at, source, context) VALUES (?1, 'x', 0, ?2, 'browser', ?3)",
                params![class, qt(at), json!({"browser": {"session": "s1", "tab": "tab-A"}}).to_string()],
            )
            .unwrap();
        }
        let j = read(&c, "s1").unwrap();
        let classes: Vec<&str> = j.events.iter().filter_map(|e| if let Kind::Error(x) = &e.kind { Some(x.class_name.as_str()) } else { None }).collect();
        assert!(classes.contains(&"Late") && !classes.contains(&"TooLate"), "{classes:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_sessions_and_rows_without_a_session_are_not_in_a_journey() {
        let (dir, c) = tenant("exclude");
        two_tabs(&c);
        let j = read(&c, "s1").unwrap();
        let text = describe(&j).join("\n");
        assert!(!text.contains("other") && !text.contains("old"), "{text}");
        assert_eq!(j.events.iter().filter(|e| e.type_name() == "visit").count(), 2, "s2's visit and the sessionless one are out");
        assert_eq!(describe(&read(&c, "s2").unwrap()), ["t1 visit /orders", "t1 request other"]);
        assert!(read(&c, "nope").unwrap().events.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tab_polling_while_another_is_used_is_one_run() {
        let (dir, c) = tenant("runs");
        two_tabs(&c);
        let j = read(&c, "s1").unwrap();
        let runs = j.runs();
        let shown: Vec<String> =
            runs.iter().map(|r| format!("{} {}x{}", j.label(r.lane).unwrap_or("-"), j.events[r.events[0]].type_name(), r.events.len())).collect();
        assert_eq!(
            shown,
            ["t1 visitx2", "t1 requestx5", "t2 visitx2", "t2 exceptionx1", "t2 browser_errorx1", "- requestx1"],
            "a visit and its own request are one step; the five polls are one run though t2 and the untabbed load happened between them"
        );
        let total: usize = runs.iter().map(|r| r.events.len()).sum();
        assert_eq!(total, j.events.len(), "every event is in exactly one run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An Inertia poll is a visit and its request, alternating: the pairs
    /// fold into one run per tab, the other tab's clicks and an asset
    /// request between them do not break it, and nothing is lost.
    #[test]
    fn a_tab_reloading_its_page_on_a_timer_is_one_run_of_visit_and_request_pairs() {
        let (dir, c) = tenant("polling");
        polling(&c);
        let j = read(&c, "p1").unwrap();
        let runs = j.runs();
        let shown: Vec<String> = runs
            .iter()
            .map(|r| {
                let kinds: Vec<&str> = r.events.iter().map(|&i| j.events[i].type_name()).collect();
                format!(
                    "{} {}x{} {}",
                    j.label(r.lane).unwrap_or("-"),
                    j.events[r.events[0]].type_name(),
                    r.events.len(),
                    kinds.iter().filter(|k| **k == "request").count()
                )
            })
            .collect();
        assert_eq!(
            shown,
            ["t1 visitx1 0", "- requestx1 1", "t1 visitx30 15", "t2 visitx2 1", "- requestx1 1", "t2 visitx2 1"],
            "15 polls are one run of 30 events (15 visits, 15 requests); the first load's request has no tab"
        );
        let total: usize = runs.iter().map(|r| r.events.len()).sum();
        assert_eq!(total, j.events.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_visits_path_matches_its_requests() {
        assert_eq!(url_path("http://localhost:3123/three?x=1#y"), "/three");
        assert_eq!(url_path("/three?page=2"), "/three");
        assert_eq!(url_path("https://shop.example"), "/");
        // A "://" in the query or fragment is not the url's own.
        assert_eq!(url_path("/login?return_to=https://app/x"), "/login");
        assert_eq!(url_path("https://host?next=/a"), "/");
        assert_eq!(url_path("https://host/p#https://x/y"), "/p");
    }

    #[test]
    fn an_execution_id_resolves_to_its_session() {
        let (dir, c) = tenant("resolve");
        two_tabs(&c);
        assert_eq!(session_of(&c, "b1").unwrap(), Some(Some("s1".into())));
        assert_eq!(session_of(&c, "other").unwrap(), Some(Some("s2".into())));
        assert_eq!(session_of(&c, "old").unwrap(), Some(None), "a request from before sessions");
        assert_eq!(session_of(&c, "missing").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_events_are_kept_past_the_cap() {
        let (dir, c) = tenant("cap");
        two_tabs(&c);
        let j = read_capped(&c, "s1", 3).unwrap();
        assert_eq!(j.capped, Capped { requests: true, visits: false });
        let ids: Vec<String> = j
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                Kind::Request(r) => Some(r.execution_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["full", "poll3", "poll4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One session's requests and visits are a seek on the session
    /// indexes, never a scan: the statements `read` runs, and the
    /// session: filter's, as EXPLAIN QUERY PLAN reports them.
    #[test]
    fn a_journey_reads_through_the_session_indexes() {
        let (dir, c) = tenant("plan");
        two_tabs(&c);
        let plan = |sql: &str, binds: &[rusqlite::types::Value]| -> String {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            stmt.query_map(rusqlite::params_from_iter(binds), |r| r.get::<_, String>(3)).unwrap().map(Result::unwrap).collect::<Vec<_>>().join("; ")
        };
        let sid = filter_query::detail_sql("executions", "$.session_id");
        let from = filter_query::session_index(&c).unwrap().unwrap();
        let requests = plan(
            &format!(
                "SELECT \"executions\".\"detail\" FROM {from} WHERE \"executions\".\"kind\" = 'request' AND {sid} = ?1 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT 10"
            ),
            &["s1".to_string().into()],
        );
        assert_eq!(requests, format!("SEARCH executions USING INDEX {} (<expr>=?)", crate::schema::SESSION_INDEX), "no sort, no scan");
        let from = filter_query::visit_session_index(&c).unwrap().unwrap();
        let visits = plan(
            &format!("SELECT \"visits\".* FROM {from} WHERE \"visits\".\"session_id\" = ?1 ORDER BY \"visits\".\"occurred_at\" DESC"),
            &["s1".to_string().into()],
        );
        assert_eq!(visits, format!("SEARCH visits USING INDEX {} (session_id=?)", crate::schema::VISIT_SESSION_INDEX));
        // search_requests q=session:s1, over a week: the same seek.
        let p = filter_query::parse(Some("session:s1 status:5xx"));
        let cond = filter_query::requests(&c, &p, T0 - 7 * 86_400 * S, T0 + 3_600 * S).unwrap();
        let sql = crate::dashboard::request_scan::scan_sql(&c, &cond, 10).unwrap();
        let q = plan(&sql, &cond.binds);
        assert!(
            q.starts_with(&format!("SEARCH executions USING INDEX {} (<expr>=? AND occurred_at>? AND occurred_at<?)", crate::schema::SESSION_INDEX)),
            "{q}"
        );
        // Several sessions, a tab, a route too: every one plans on the
        // index (INDEXED BY fails the statement when it cannot).
        for q in ["session:s1|s2", "session:s1 tab:tab-A", "session:s1 tab:none", "session:s1 route:/orders"] {
            let cond = filter_query::requests(&c, &filter_query::parse(Some(q)), T0 - 86_400 * S, T0 + 3_600 * S).unwrap();
            let sql = crate::dashboard::request_scan::scan_sql(&c, &cond, 10).unwrap();
            assert!(plan(&sql, &cond.binds).contains(crate::schema::SESSION_INDEX), "{q}");
        }
        // The records of a session's requests (search_telemetry).
        let cond = filter_query::apply(&c, filter_query::Resource::Exceptions, Some("session:s1"), T0 - 86_400 * S, T0 + 3_600 * S, &[]).unwrap();
        let n = db::count(&c, &format!("SELECT COUNT(*) FROM \"exceptions\" WHERE {}", cond.sql()), rusqlite::params_from_iter(cond.binds.iter())).unwrap();
        assert_eq!(n, 1, "the exception of s1's 500 (not the browser error, which has no request)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ingest keeps a session id as the client sent it: a number, an
    /// overlong or malformed string never lists as a session (the session
    /// page would refuse it).
    #[test]
    fn listing_skips_session_ids_the_session_page_would_refuse() {
        let (dir, c) = tenant("badids");
        two_tabs(&c);
        let window = (T0 - 3_600 * S, T0 + 3_600 * S);
        let long = "x".repeat(65);
        for (id, sid) in [("n", json!(42)), ("l", json!(long)), ("m", json!("a b/c"))] {
            c.execute(
                "INSERT INTO executions (execution_id, kind, name, status, occurred_at, duration, detail) VALUES (?1, 'request', 'GET /', 200, ?2, 1, ?3)",
                params![id, qt(T0), json!({"session_id": sid}).to_string()],
            )
            .unwrap();
        }
        c.execute("INSERT INTO visits (occurred_at, duration, session_id) VALUES (?1, 1, 'bad id!')", [qt(T0)]).unwrap();
        let (all, total) = list(&c, window, None, false, 10).unwrap();
        let ids: Vec<&str> = all.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!((total, ids), (2, vec!["s1", "s2"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_counts_each_sessions_window_activity() {
        let (dir, c) = tenant("list");
        two_tabs(&c);
        let window = (T0 - 3_600 * S, T0 + 3_600 * S);
        let (all, total) = list(&c, window, None, false, 10).unwrap();
        assert_eq!(total, 2);
        let s1 = all.iter().find(|s| s.session_id == "s1").unwrap();
        assert_eq!((s1.requests, s1.server_errors, s1.visits, s1.browser_errors), (8, 1, 2, 1));
        assert_eq!(s1.tabs.iter().map(String::as_str).collect::<Vec<_>>(), ["tab-A", "tab-B"]);
        assert_eq!((s1.first, s1.last), (T0, T0 + 45 * S));
        assert_eq!(all[0].session_id, "s1", "newest activity first");
        let (errored, n) = list(&c, window, None, true, 10).unwrap();
        assert_eq!((errored.len(), n, errored[0].session_id.as_str()), (1, 1, "s1"));
        let (none, _) = list(&c, window, Some("bob"), false, 10).unwrap();
        assert!(none.is_empty());
        assert_eq!(list(&c, window, Some("ada"), false, 10).unwrap().1, 2);
        // The browser error and a visit carry no user_ref (the beacon could
        // not attribute them): a user's session still counts them.
        c.execute("UPDATE visits SET user_ref = NULL WHERE session_id = 's1' AND url = '/orders/7'", []).unwrap();
        let (mine, _) = list(&c, window, Some("ada"), false, 10).unwrap();
        let s1 = mine.iter().find(|s| s.session_id == "s1").unwrap();
        assert_eq!((s1.visits, s1.browser_errors, s1.errors()), (2, 1, 2), "every visit and error of the user's session");
        let (one, total) = list(&c, window, None, false, 1).unwrap();
        assert_eq!((one.len(), total), (1, 2), "limited, and the total says so");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
