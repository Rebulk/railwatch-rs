//! What one evaluation reads from a tenant: the trailing window's requests
//! (through the request index, as request_scan does: no row's `detail` is
//! parsed for the fields it keeps) and its `security.*` events, each capped
//! at the newest N. The reader the watcher passes is its own, with a time
//! limit (watcher.rs), so a slow tenant stops here, not in ingest.

use rusqlite::Connection;

use super::policy::Allowlist;
use super::{Event, Req};
use crate::dashboard::db::qt;
use crate::dashboard::filter_query;

/// What a read found, and whether a cap cut it.
#[derive(Debug, Default)]
pub struct Read {
    pub requests: Vec<Req>,
    pub events: Vec<Event>,
    /// UA-allowlisted requests, kept for secrets_probe only.
    pub ua_allowed: Vec<Req>,
    pub suppressed: i64,
    pub requests_capped: bool,
    pub events_capped: bool,
}

/// Requests in [from, to], oldest first, at most `cap` (the newest).
pub fn requests(conn: &Connection, from: i64, to: i64, cap: usize) -> rusqlite::Result<(Vec<Req>, bool)> {
    let from_sql = filter_query::request_index(conn)?.unwrap_or_else(|| "\"executions\"".into());
    let d = |p: &str| filter_query::detail_sql("executions", p);
    let sql = format!(
        "SELECT \"executions\".\"occurred_at\", {ip}, {ua}, {path}, \"executions\".\"status\", \"executions\".\"user_ref\", {halted} \
         FROM {from_sql} WHERE \"executions\".\"kind\" = 'request' AND \"executions\".\"occurred_at\" BETWEEN ?1 AND ?2 \
         ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {}",
        cap + 1,
        ip = d("$.ip"),
        ua = d("$.user_agent"),
        path = d("$.path"),
        halted = d("$.halted_callback"),
    );
    // db::prepare: a per-day reader's views take the hint per branch.
    let mut stmt = crate::dashboard::db::prepare(conn, &sql)?;
    let mut rows = stmt.query(rusqlite::params![qt(from), qt(to)])?;
    let mut out = Vec::new();
    let text = |r: &rusqlite::Row<'_>, i: usize| -> Option<String> {
        match r.get_ref(i) {
            Ok(rusqlite::types::ValueRef::Text(t)) => std::str::from_utf8(t).ok().filter(|s| !s.is_empty()).map(str::to_string),
            _ => None,
        }
    };
    let mut capped = false;
    while let Some(r) = rows.next()? {
        if out.len() == cap {
            capped = true;
            break;
        }
        let at = text(r, 0).and_then(|s| crate::time::parse_db_time(&s)).unwrap_or(0);
        out.push(Req {
            at,
            ip: text(r, 1),
            ua: text(r, 2),
            path: text(r, 3).unwrap_or_default(),
            status: r.get::<_, Option<i64>>(4).ok().flatten().unwrap_or(0),
            user_ref: text(r, 5),
            halted: text(r, 6),
        });
    }
    out.reverse();
    Ok((out, capped))
}

/// `security.*` events in [from, to], oldest first, at most `cap`.
pub fn events(conn: &Connection, from: i64, to: i64, cap: usize) -> rusqlite::Result<(Vec<Event>, bool)> {
    let sql = format!(
        "SELECT occurred_at, message, context, user_ref FROM logs \
         WHERE level = 'event' AND occurred_at BETWEEN ?1 AND ?2 AND message LIKE 'security.%' \
         ORDER BY occurred_at DESC LIMIT {}",
        cap + 1
    );
    let mut stmt = crate::dashboard::db::prepare(conn, &sql)?;
    let mut rows = stmt.query(rusqlite::params![qt(from), qt(to)])?;
    let mut out = Vec::new();
    let mut capped = false;
    while let Some(r) = rows.next()? {
        if out.len() == cap {
            capped = true;
            break;
        }
        let at = r.get::<_, String>(0).ok().and_then(|s| crate::time::parse_db_time(&s)).unwrap_or(0);
        let name: String = r.get(1)?;
        let context: Option<String> = r.get(2)?;
        let row_user: Option<String> = r.get(3)?;
        out.push(event(at, &name, context.as_deref(), row_user.as_deref()));
    }
    out.reverse();
    Ok((out, capped))
}

/// One event from its name and payload. Keys are read as the gem forwards
/// `Rails.event.notify(name, **payload)`, in their common spellings; a
/// missing one is simply absent.
pub fn event(at: i64, name: &str, context: Option<&str>, row_user: Option<&str>) -> Event {
    let mut e = Event::new(at, name);
    let p: serde_json::Value = context.and_then(|c| serde_json::from_str(c).ok()).unwrap_or_default();
    let s = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| match p.get(*k) {
            Some(serde_json::Value::String(v)) if !v.is_empty() => Some(v.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
    };
    e.ip = s(&["ip", "remote_ip", "client_ip"]);
    e.ua = s(&["ua", "user_agent"]);
    e.user_ref = s(&["user_ref", "user_id"]).or_else(|| row_user.filter(|u| !u.is_empty()).map(str::to_string));
    e.email_domain = s(&["email_domain"]).or_else(|| s(&["email"]).and_then(|m| m.rsplit_once('@').map(|(_, d)| d.to_string())));
    e.disposable =
        matches!(p.get("disposable"), Some(serde_json::Value::Bool(true))) || matches!(p.get("disposable_email"), Some(serde_json::Value::Bool(true)));
    e.client_id = s(&["client_id", "oauth_client_id", "id"]);
    e.client_name = s(&["client_name", "name"]);
    e.redirect_uris = match p.get("redirect_uris").or_else(|| p.get("redirect_uri")) {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).take(10).collect(),
        Some(serde_json::Value::String(u)) => vec![u.clone()],
        _ => vec![],
    };
    e
}

/// Reads a window and removes our own traffic.
pub fn window(conn: &Connection, from: i64, to: i64, max_requests: usize, max_events: usize, allow: &Allowlist) -> rusqlite::Result<Read> {
    let (all, requests_capped) = requests(conn, from, to, max_requests)?;
    let (events, events_capped) = match events(conn, from, to, max_events) {
        Ok(v) => v,
        // A tenant with no logs table yet (an empty one) has no events.
        Err(rusqlite::Error::SqliteFailure(_, Some(m))) if m.contains("no such table") => (vec![], false),
        Err(e) => return Err(e),
    };
    let mut out = Read { requests_capped, events_capped, ..Default::default() };
    for r in all {
        let by_net_or_user = r.ip.as_deref().is_some_and(|i| allow.ip(i)) || r.user_ref.as_deref().is_some_and(|u| allow.user(u));
        if by_net_or_user {
            out.suppressed += 1;
        } else if r.ua.as_deref().is_some_and(|u| allow.ua(u)) {
            out.suppressed += 1;
            out.ua_allowed.push(r);
        } else {
            out.requests.push(r);
        }
    }
    for e in events {
        if allow.matches(e.ip.as_deref(), e.user_ref.as_deref(), e.ua.as_deref()) {
            out.suppressed += 1;
        } else {
            out.events.push(e);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_payload_in_the_shapes_apps_send() {
        let e = event(5, "security.signup", Some(r#"{"ip":"198.51.100.7","user_agent":"curl/8","user_id":41,"email":"x@mailinator.com","via":"api"}"#), None);
        assert_eq!(e.kind, super::super::EventKind::Signup);
        assert_eq!(
            (e.ip.as_deref(), e.ua.as_deref(), e.user_ref.as_deref(), e.email_domain.as_deref()),
            (Some("198.51.100.7"), Some("curl/8"), Some("41"), Some("mailinator.com"))
        );
        let e = event(
            5,
            "security.oauth_client.registered",
            Some(r#"{"client_id":"c1","client_name":"x","redirect_uris":["https://a.example/cb"],"disposable":true}"#),
            Some("9"),
        );
        assert_eq!(e.kind, super::super::EventKind::OAuthClientRegistered);
        assert_eq!((e.client_id.as_deref(), e.redirect_uris.len(), e.user_ref.as_deref(), e.disposable), (Some("c1"), 1, Some("9"), true));
        assert_eq!(
            super::super::EventKind::of("security.oauth_client.token_issued"),
            super::super::EventKind::Other,
            "a token for a client is not a registration"
        );
        assert_eq!(super::super::EventKind::of("security.oauth_client.created"), super::super::EventKind::OAuthClientRegistered);
        let e = event(5, "security.login.failed", Some("not json"), None);
        assert_eq!((e.kind, e.ip), (super::super::EventKind::LoginFailed, None), "a payload that does not parse is an event with no fields");
    }
}
