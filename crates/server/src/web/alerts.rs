//! AlertsController (railwatch-cloud @ 464f0fd): fired-alert history,
//! account-wide at /alerts and per environment, and retry.
//!
//! One deliberate difference (docs/DIFFERENCES.md): the environment page
//! also lists issue-less alerts whose payload names that environment
//! (`environment_id`: silent_environment, environment_recovered,
//! silent_host, crash_free_drop, quota). Rails filters the env page through
//! the alert's issue, so those only showed account-wide; here "my app went
//! silent" is on the page of the app that went silent.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery};
use axum::response::Response;
use axum::routing::{get, post};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::WebState;
use super::controllers::{guard, internal, not_found, require_account};
use super::inertia::{Ctx, Redirect, t_json, t_json_opt};
use super::params::Params;
use crate::alerts::rules;

type S = Arc<WebState>;

/// Alert::STATUSES
pub const STATUSES: [&str; 7] = ["pending", "delivering", "sent", "failed", "skipped", "collapsed", "summarized"];

pub fn routes() -> Router<S> {
    Router::new().route("/alerts", get(index)).route("/alerts/{id}/retry", post(retry))
}

/// `account_alerts` (scoped through the integration, so "Send test" alerts
/// with no rule show too), the q filter's event/status, newest 200.
/// `environment`: the env page's scope.
pub fn rows(x: &Connection, account: i64, environment: Option<i64>, q: &str) -> rusqlite::Result<Vec<Value>> {
    let parsed = crate::dashboard::filter_query::parse(Some(q));
    let mut sql = String::from(
        "SELECT a.id, a.event, a.status, a.sent_at, a.error, a.payload, a.created_at, i.kind, i.name, s.id, ap.issue_prefix || '-' || s.number, s.title
         FROM alerts a JOIN integrations i ON i.id = a.integration_id
         LEFT JOIN issues s ON s.id = a.issue_id LEFT JOIN applications ap ON ap.id = s.application_id
         WHERE i.account_id = ?",
    );
    let mut binds: Vec<rusqlite::types::Value> = vec![account.into()];
    if let Some(env) = environment {
        sql.push_str(" AND (s.environment_id = ? OR (a.issue_id IS NULL AND json_extract(a.payload, '$.environment_id') = ?))");
        binds.push(env.into());
        binds.push(env.into());
    }
    if let Some(e) = parsed.fields.get("event").filter(|e| rules::EVENTS.contains(&e.as_str())) {
        sql.push_str(" AND a.event = ?");
        binds.push(e.clone().into());
    }
    if let Some(st) = parsed.fields.get("status").filter(|s| STATUSES.contains(&s.as_str())) {
        sql.push_str(" AND a.status = ?");
        binds.push(st.clone().into());
    }
    sql.push_str(" ORDER BY a.created_at DESC, a.id DESC LIMIT 200");
    x.prepare(&sql)?
        .query_map(rusqlite::params_from_iter(binds), |r| {
            let payload: Value = serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(json!({}));
            let issue = match r.get::<_, Option<i64>>(9)? {
                Some(id) => json!({"id": id, "key": r.get::<_, Option<String>>(10)?, "title": r.get::<_, Option<String>>(11)?}),
                None => Value::Null,
            };
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "event": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?,
                "sent_at": t_json_opt(r.get(3)?), "error": r.get::<_, Option<String>>(4)?, "payload": payload,
                "created_at": t_json(&r.get::<_, String>(6)?),
                "integration": {"kind": r.get::<_, String>(7)?, "name": r.get::<_, String>(8)?},
                "issue": issue,
            }))
        })?
        .collect()
}

pub fn props(alerts: Vec<Value>, q: &str) -> Value {
    json!({"alerts": alerts, "q": q, "statuses": STATUSES})
}

async fn index(cx: Ctx, RawQuery(q): RawQuery) -> Response {
    let account = guard!(require_account(&cx));
    let q = Params::from_query(q.as_deref()).s("q");
    let q2 = q.clone();
    match cx.state.db(move |c| c.with(|x| rows(x, account, None, &q2))).await {
        Ok(alerts) => cx.render("alerts/index", props(alerts, &q), None).await,
        Err(e) => internal(e),
    }
}

async fn retry(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_account(&cx));
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let owned: bool = x.query_row(
                    "SELECT EXISTS (SELECT 1 FROM alerts a JOIN integrations i ON i.id = a.integration_id WHERE a.id = ? AND i.account_id = ?)",
                    params![id, account],
                    |r| r.get(0),
                )?;
                if !owned {
                    return Ok(None);
                }
                crate::alerts::outbox::retry(x, id).map(Some)
            })
        })
        .await;
    let back = cx.back_or("/alerts");
    match r {
        Ok(Some(true)) => cx.redirect(Redirect::to(back).notice("Retrying alert")),
        Ok(Some(false)) => cx.redirect(Redirect::to(back).alert("Only failed alerts can be retried")),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, create_account, create_environment, insert_application, insert_user};

    #[test]
    fn env_page_lists_its_issue_alerts_and_its_issueless_watchdog_alerts_only() {
        let dir = std::env::temp_dir().join(format!("rw-alerts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let user = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", "correct horse battery", true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", user)).unwrap();
        let other = c.tx(|tx| create_account(tx, "Other", user)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (prod, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let (staging, _) = c.tx(|tx| create_environment(tx, app, "staging")).unwrap();
        c.with(|x| {
            x.execute_batch(&format!(
                "INSERT INTO integrations (id, account_id, kind, name, settings, created_at, updated_at) VALUES (1, {account}, 'webhook', 'Hook', '{{}}', 't', 't'), (2, {other}, 'webhook', 'Theirs', '{{}}', 't', 't');
                 INSERT INTO issues (id, application_id, environment_id, number, title, group_hash, first_seen_at, last_seen_at, created_at, updated_at)
                   VALUES (1, {app}, {prod}, 7, 'Boom', 'g1', 't', 't', 't', 't'), (2, {app}, {staging}, 8, 'Bust', 'g2', 't', 't', 't', 't');
                 INSERT INTO alerts (id, event, integration_id, issue_id, payload, status, created_at, updated_at) VALUES
                   (1, 'new_issue', 1, 1, '{{}}', 'sent', '2026-10-14 01:00:00', 't'),
                   (2, 'new_issue', 1, 2, '{{}}', 'sent', '2026-10-14 01:01:00', 't'),
                   (3, 'silent_environment', 1, NULL, '{{\"environment_id\":{prod},\"message\":\"web-1 in production has sent nothing\"}}', 'failed', '2026-10-14 01:02:00', 't'),
                   (4, 'silent_environment', 1, NULL, '{{\"environment_id\":{staging}}}', 'sent', '2026-10-14 01:03:00', 't'),
                   (5, 'new_issue', 1, NULL, '{{\"test\":true}}', 'sent', '2026-10-14 01:04:00', 't'),
                   (6, 'quota', 2, NULL, '{{\"environment_id\":{prod}}}', 'sent', '2026-10-14 01:05:00', 't');"
            ))
        })
        .unwrap();
        let ids = |v: Vec<Value>| v.iter().map(|a| a["id"].as_i64().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            ids(c.with(|x| rows(x, account, Some(prod), "")).unwrap()),
            [3, 1],
            "prod's issue alert + its silence; not staging's, not the test, not another account's"
        );
        assert_eq!(ids(c.with(|x| rows(x, account, None, "")).unwrap()), [5, 4, 3, 2, 1], "account-wide: every alert of this account's integrations");
        assert_eq!(ids(c.with(|x| rows(x, account, None, "status:failed")).unwrap()), [3]);
        assert_eq!(ids(c.with(|x| rows(x, account, None, "event:silent_environment status:bogus")).unwrap()), [4, 3], "unknown status is ignored, as Rails");
        let one = c.with(|x| rows(x, account, Some(prod), "")).unwrap();
        assert_eq!(one[1]["issue"], json!({"id": 1, "key": "SHOP-7", "title": "Boom"}));
        assert_eq!(one[0]["issue"], Value::Null);
        assert_eq!(one[0]["integration"], json!({"kind": "webhook", "name": "Hook"}));
        // alerts/index.tsx dereferences p.alerts and p.statuses; missing keys blank the page.
        let p = props(one, "status:failed");
        assert_eq!((p["q"].as_str(), p["statuses"].as_array().map(Vec::len)), (Some("status:failed"), Some(7)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
