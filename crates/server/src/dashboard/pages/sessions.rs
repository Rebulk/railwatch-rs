//! Browser sessions (docs/sessions.md): the list of the window's sessions
//! and one session's journey across its tabs. Not railwatch-cloud pages:
//! this server's own (docs/DIFFERENCES.md).

use rusqlite::Connection;
use serde_json::{Value, json};

use super::*;
use crate::dashboard::db::{jf, jt};
use crate::dashboard::rb;
use crate::dashboard::session_journey::{self as journey, Event, Journey, Kind};

/// Sessions the list shows.
const LIST_LIMIT: usize = 200;

pub fn sessions_index(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let user = ctx.param("user").filter(|u| !u.is_empty());
    let has_errors = ctx.param("errors") == Some("1");
    let (rows, total) = journey::list(conn, ctx.range(), user, has_errors, LIST_LIMIT)?;
    Ok(crate::obj! {
        "sessions" => rows.iter().map(|s| crate::obj! {
            "session_id" => s.session_id.clone(), "first" => jt(Some(s.first)), "last" => jt(Some(s.last)),
            "requests" => s.requests, "visits" => s.visits, "errors" => s.errors(), "tabs" => s.tabs.len() as i64,
            "users" => s.users.iter().cloned().collect::<Vec<_>>(),
        }).collect::<Vec<_>>(),
        "total" => total as i64, "user" => user.unwrap_or(""), "errors" => has_errors,
    })
}

pub fn sessions_show(conn: &Connection, ctx: &PageContext, _control: &dyn ControlData) -> R {
    let id = ctx.param_s("id");
    if !crate::dashboard::filter_query::valid_session_id(&id) {
        return Err(PageError::NotFound);
    }
    let j = journey::read(conn, &id)?;
    if j.events.is_empty() {
        return Err(PageError::NotFound);
    }
    Ok(journey_props(&j))
}

/// The page's props: the tabs (lanes), the events oldest first with each
/// one's lane and run (consecutive alike events of one tab share a run id,
/// so the page can fold them), and the header. The dashboard's own session
/// has security access, so addresses and user agents are shown.
fn journey_props(j: &Journey) -> Value {
    let mut run_of = vec![0usize; j.events.len()];
    let runs = j.runs();
    for (n, r) in runs.iter().enumerate() {
        for &i in &r.events {
            run_of[i] = n;
        }
    }
    let users = j.distinct(|e| match &e.kind {
        Kind::Request(r) => r.user.as_deref(),
        Kind::Visit(v) => v.user.as_deref(),
        Kind::Error(_) => None,
    });
    let ips = j.distinct(|e| match &e.kind {
        Kind::Request(r) => r.ip.as_deref(),
        _ => None,
    });
    let user_agents = j.distinct(|e| match &e.kind {
        Kind::Request(r) => r.user_agent.as_deref(),
        Kind::Visit(v) => v.user_agent.as_deref(),
        Kind::Error(_) => None,
    });
    crate::obj! {
        "session_id" => j.session_id.clone(),
        "started" => jt(j.first()), "last" => jt(j.last()),
        "tabs" => j.tabs.iter().map(|t| crate::obj! {
            "id" => t.id.clone(), "label" => t.label.clone(), "first" => jt(Some(t.first)), "last" => jt(Some(t.last)),
            "requests" => t.requests, "visits" => t.visits, "errors" => t.errors,
        }).collect::<Vec<_>>(),
        "events" => j.events.iter().enumerate().map(|(i, e)| event(e, run_of[i])).collect::<Vec<_>>(),
        "runs" => runs.len() as i64,
        "users" => users, "ips" => ips, "user_agents" => user_agents,
        "capped" => j.capped.requests || j.capped.visits, "cap" => journey::MAX_EVENTS as i64,
    }
}

fn event(e: &Event, run: usize) -> Value {
    let ms = |us: i64| jf(rb::round(us as f64 / 1000.0, 2));
    let mut v = crate::obj! {
        "at" => jt(Some(e.at)), "type" => e.type_name(), "lane" => e.lane.map_or(Value::Null, |l| Value::from(l as i64)),
        "run" => run as i64, "error" => e.is_error(),
    };
    let fields = match &e.kind {
        Kind::Request(r) => json!({
            "execution_id": r.execution_id, "name": r.name, "path": r.path, "status": r.status, "outcome": r.outcome(),
            "duration": ms(r.duration), "exception": r.exception_preview,
        }),
        Kind::Visit(x) => json!({
            "component": x.component, "url": x.url, "method": x.method, "status": x.status, "partial": x.partial,
            "duration": ms(x.duration), "lcp": x.lcp, "inp": x.inp, "ttfb": x.ttfb, "cls": x.cls.map(jf),
        }),
        Kind::Error(x) => json!({
            "class_name": x.class_name, "message": rb::first_chars(&x.message, 500), "execution_id": x.execution_id,
            "group_hash": x.group_hash, "url": x.url, "browser": x.browser,
        }),
    };
    if let (Value::Object(m), Value::Object(f)) = (&mut v, fields) {
        m.extend(f.into_iter().filter(|(_, x)| !x.is_null()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::session_journey::tests::{T0, two_tabs};

    struct NoControl;
    impl ControlData for NoControl {
        fn deploys(&self) -> Vec<crate::dashboard::Deploy> {
            vec![]
        }
        fn issues(&self) -> Vec<crate::dashboard::Issue> {
            vec![]
        }
        fn expected_servers(&self) -> Vec<String> {
            vec![]
        }
    }

    fn ctx(params: &[(&str, &str)]) -> PageContext {
        let p = params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        PageContext::new(1, T0 + 3_600_000_000, p, crate::dashboard::CursorKey(b"k".to_vec()))
    }

    #[test]
    fn the_journey_page_has_lanes_runs_and_every_event() {
        let dir = std::env::temp_dir().join(format!("rw-journey-page-{}", crate::control::base58(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let c = crate::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();
        two_tabs(&c);
        let p = sessions_show(&c, &ctx(&[("id", "s1")]), &NoControl).unwrap();
        assert_eq!(p["tabs"].as_array().unwrap().iter().map(|t| t["label"].as_str().unwrap()).collect::<Vec<_>>(), ["t1", "t2"]);
        let events = p["events"].as_array().unwrap();
        assert_eq!(events.len(), 12);
        let polls: Vec<i64> = events.iter().filter(|e| e["name"] == "GET /api/poll").map(|e| e["run"].as_i64().unwrap()).collect();
        assert_eq!(polls.len(), 5);
        assert!(polls.iter().all(|r| *r == polls[0]), "the polls fold into one run: {polls:?}");
        assert_eq!(events.iter().filter(|e| e["lane"] == 1).count(), 4, "t2: visit, request, exception, browser error");
        assert_eq!(p["ips"], json!(["198.51.100.23"]), "the dashboard session has security access");
        assert!(matches!(sessions_show(&c, &ctx(&[("id", "nope")]), &NoControl), Err(PageError::NotFound)));
        assert!(matches!(sessions_show(&c, &ctx(&[("id", "../x")]), &NoControl), Err(PageError::NotFound)));
        let list = sessions_index(&c, &ctx(&[("window", "24h"), ("errors", "1")]), &NoControl).unwrap();
        assert_eq!(list["sessions"].as_array().unwrap().iter().map(|s| s["session_id"].as_str().unwrap()).collect::<Vec<_>>(), ["s1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
