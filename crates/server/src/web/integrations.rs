//! Settings::IntegrationsController and Settings::AlertRulesController
//! (railwatch-cloud @ 464f0fd): where alerts go, and which events send them.
//! Props, flash texts, validation messages and redirect targets are Rails'.
//! Not here: Linear (OAuth, sync, webhook health), so `linear_oauth_enabled`
//! is always false and the `linear` kind is not offered.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use super::WebState;
use super::controllers::{guard, internal, not_found, require_account, require_admin};
use super::inertia::{Ctx, Redirect, t_json_opt};
use super::params::Params;
use crate::alerts::integration::{self, Integration, SettingsCipher};
use crate::alerts::rules;

type S = Arc<WebState>;

const INDEX: &str = "/settings/integrations";
/// Issue::KINDS, Issue::PRIORITIES
const ISSUE_KINDS: [&str; 3] = ["exception", "performance", "anomaly"];
const PRIORITIES: [&str; 4] = ["low", "normal", "high", "urgent"];

pub fn routes() -> Router<S> {
    Router::new()
        .route(INDEX, get(index).post(create))
        .route("/settings/integrations/{id}", patch(update).put(update).delete(destroy))
        .route("/settings/integrations/{id}/send_test", post(send_test))
        .route("/settings/alert_rules", post(rule_create))
        .route("/settings/alert_rules/{id}", axum::routing::delete(rule_destroy))
}

fn cipher(cx: &Ctx) -> SettingsCipher {
    cx.state.settings_cipher.clone()
}

/// `params.fetch(:settings, {}).permit(:to, :webhook_url, :url, :secret, :api_key, :team_id)`
fn permitted_settings(p: &Params) -> Map<String, Value> {
    let Some(Value::Object(given)) = p.get("settings") else { return Map::new() };
    ["to", "webhook_url", "url", "secret", "api_key", "team_id"]
        .iter()
        .filter_map(|k| match given.get(*k) {
            Some(v @ (Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null)) => Some((k.to_string(), v.clone())),
            _ => None,
        })
        .collect()
}

fn blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Bool(b) => !b,
        _ => false,
    }
}

/// IntegrationsController#integration_props: never the secrets themselves.
fn integration_props(i: &Integration) -> Value {
    let mut settings: Map<String, Value> = i
        .settings
        .iter()
        .filter(|(k, _)| !["secret", "webhook_url", "api_key", "access_token", "refresh_token"].contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if i.kind == "slack" {
        settings.insert("webhook_url".into(), json!("configured"));
    }
    if i.kind == "linear" && ["api_key", "access_token"].iter().any(|k| i.settings.get(*k).is_some_and(|v| !blank(v))) {
        settings.insert("credential".into(), json!("configured"));
    }
    json!({
        "id": i.id, "kind": i.kind, "name": i.name, "enabled": i.enabled,
        "destination": i.destination(), "last_delivered_at": t_json_opt(i.last_delivered_at.clone()),
        "last_received_at": t_json_opt(i.last_received_at.clone()), "last_synced_at": t_json_opt(i.last_synced_at.clone()),
        // A secret this server cannot decrypt is the integration's problem
        // to show, ahead of any older delivery error.
        "last_error": i.settings_error.clone().or_else(|| i.last_error.clone()),
        "settings": settings,
        "webhook_health": Value::Null,
    })
}

fn alert_rules_props(x: &Connection, account: i64) -> rusqlite::Result<Vec<Value>> {
    x.prepare(
        "SELECT r.id, r.event, ap.name, r.application_id, i.name, r.integration_id, r.filters FROM alert_rules r
         JOIN applications ap ON ap.id = r.application_id JOIN integrations i ON i.id = r.integration_id
         WHERE ap.account_id = ? ORDER BY r.id",
    )?
    .query_map([account], |r| {
        let filters: Value = serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(json!({}));
        Ok(json!({"id": r.get::<_, i64>(0)?, "event": r.get::<_, String>(1)?, "application": r.get::<_, String>(2)?,
                  "application_id": r.get::<_, i64>(3)?, "integration": r.get::<_, String>(4)?, "integration_id": r.get::<_, i64>(5)?,
                  "filters": filters}))
    })?
    .collect()
}

async fn index(cx: Ctx) -> Response {
    let account = guard!(require_account(&cx));
    let cipher = cipher(&cx);
    let data = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let integrations: Vec<Value> = integration::for_account(x, Some(&cipher), account)?.iter().map(integration_props).collect();
                let alert_rules = alert_rules_props(x, account)?;
                let apps: Vec<(i64, String)> = x
                    .prepare("SELECT id, name FROM applications WHERE account_id = ? ORDER BY name, id")?
                    .query_map([account], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut environment_options = Map::new();
                for (id, _) in &apps {
                    let envs: Vec<Value> = x
                        .prepare("SELECT id, name FROM environments WHERE application_id = ? ORDER BY name, id")?
                        .query_map([id], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?})))?
                        .collect::<rusqlite::Result<_>>()?;
                    environment_options.insert(id.to_string(), Value::Array(envs));
                }
                let application_options: Vec<Value> = apps.iter().map(|(id, name)| json!({"id": id, "name": name})).collect();
                Ok(json!({
                    "integrations": integrations, "alert_rules": alert_rules,
                    "application_options": application_options, "environment_options": environment_options,
                    "issue_kinds": ISSUE_KINDS, "priorities": PRIORITIES,
                    "events": rules::EVENTS, "kinds": integration::KINDS,
                    "linear_oauth_enabled": false,
                }))
            })
        })
        .await;
    match data {
        Ok(props) => cx.render("settings/integrations/index", props, None).await,
        Err(e) => internal(e),
    }
}

/// `require_verified_user`: 403 unless the current user verified their email.
fn require_verified(cx: &Ctx) -> Result<(), Response> {
    if cx.current.user.as_ref().is_some_and(|u| u.verified) { Ok(()) } else { Err(StatusCode::FORBIDDEN.into_response()) }
}

async fn create(cx: Ctx, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(require_verified(&cx));
    let (kind, name, settings) = (p.s("kind"), p.s("name"), permitted_settings(&p));
    let errors = integration::validate(&kind, &name, &settings);
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(INDEX).errors(errors));
    }
    let cipher = cipher(&cx);
    match cx.state.db(move |c| c.with(|x| integration::insert(x, &cipher, account, &kind, &name, &settings))).await {
        Ok(_) => cx.redirect(Redirect::to(INDEX).notice("Integration added")),
        Err(e) => internal(e),
    }
}

/// Name and enabled toggle, plus rotation of the destination itself.
/// Settings are merged, never replaced; a blank value leaves the existing
/// one alone (the form never sees the current secret).
async fn update(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(require_verified(&cx));
    let cipher = cipher(&cx);
    let c2 = cipher.clone();
    let found = match cx.state.db(move |c| c.with(|x| integration::find_in_account(x, Some(&c2), account, id))).await {
        Ok(Some(i)) => i,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    if let Some(e) = &found.settings_error {
        // Merging into settings we cannot read would silently drop the
        // unreadable half; the operator re-creates the integration instead.
        return cx.redirect(Redirect::to(INDEX).alert(format!("Could not update {}: {e}", found.name)));
    }
    let name = if p.has("name") { p.s("name") } else { found.name.clone() };
    let enabled = if p.has("enabled") { p.bool("enabled").unwrap_or(false) } else { found.enabled };
    let mut settings = found.settings.clone();
    for (k, v) in permitted_settings(&p) {
        if !blank(&v) {
            settings.insert(k, v);
        }
    }
    let errors = integration::validate(&found.kind, &name, &settings);
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(INDEX).errors(errors));
    }
    match cx.state.db(move |c| c.with(|x| integration::update(x, &cipher, &found, &name, enabled, &settings))).await {
        Ok(()) => cx.redirect(Redirect::to(INDEX).notice("Integration updated")),
        Err(e) => internal(e),
    }
}

async fn destroy(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let exists: bool =
                    tx.query_row("SELECT EXISTS (SELECT 1 FROM integrations WHERE id = ? AND account_id = ?)", params![id, account], |r| r.get(0))?;
                if exists {
                    integration::destroy(tx, id)?;
                }
                Ok(exists)
            })
        })
        .await;
    match r {
        Ok(true) => cx.redirect(Redirect::to(INDEX).notice("Integration removed")),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

/// One clearly labelled synthetic alert, delivered now; the answer
/// (including the failure) comes back in the flash. 5 per minute per user.
async fn send_test(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(require_verified(&cx));
    if cx.state.send_test_limit.exceeded(&cx.user_id()) {
        return cx.redirect(Redirect::to(INDEX).alert("Too many test deliveries. Try again in a minute."));
    }
    let name = match cx
        .state
        .db(move |c| {
            c.with(|x| x.query_row("SELECT name FROM integrations WHERE id = ? AND account_id = ?", params![id, account], |r| r.get::<_, String>(0)).optional())
        })
        .await
    {
        Ok(Some(n)) => n,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    match crate::alerts::deliver_test(&cx.state.control, id).await {
        Ok("skipped") => cx.redirect(Redirect::to(INDEX).alert(format!("{name} is disabled, so nothing was sent."))),
        Ok(_) => cx.redirect(Redirect::to(INDEX).notice(format!("Test alert delivered to {name}."))),
        Err(e) => cx.redirect(Redirect::to(INDEX).alert(format!("Test delivery to {name} failed: {e}"))),
    }
}

/// AlertRulesController#filters: unknown kinds and priorities are dropped,
/// never stored, so a bad value cannot widen or silence a rule.
fn rule_filters(p: &Params) -> Map<String, Value> {
    let given = match p.get("filters") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let strings = |k: &str| -> Vec<String> {
        match given.get(k) {
            Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).collect(),
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Number(n)) => vec![n.to_string()],
            _ => vec![],
        }
    };
    let mut out = Map::new();
    // compact_blank.map(&:to_i): "abc".to_i is 0, as Ruby's.
    let ids: Vec<i64> = strings("environment_ids").iter().filter(|s| !s.trim().is_empty()).map(|s| ruby_to_i(s)).collect();
    if !ids.is_empty() {
        out.insert("environment_ids".into(), json!(ids));
    }
    let kinds: Vec<String> = strings("kinds").into_iter().filter(|k| ISSUE_KINDS.contains(&k.as_str())).collect();
    if !kinds.is_empty() {
        out.insert("kinds".into(), json!(kinds));
    }
    let min = given.get("min_priority").and_then(Value::as_str).unwrap_or("");
    if PRIORITIES.contains(&min) {
        out.insert("min_priority".into(), json!(min));
    }
    if let Some(t) = given.get("title_pattern").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
        out.insert("title_pattern".into(), json!(t));
    }
    out
}

/// String#to_i: leading whitespace, optional sign, digits; else 0.
fn ruby_to_i(s: &str) -> i64 {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let n: i64 = digits.bytes().take_while(u8::is_ascii_digit).fold(0i64, |a, b| a.saturating_mul(10).saturating_add((b - b'0') as i64));
    if neg { -n } else { n }
}

/// AlertRule's validations: event inclusion, valid_title_pattern.
fn rule_errors(event: &str, filters: &Map<String, Value>) -> Vec<(String, String)> {
    let mut errors = Vec::new();
    if !rules::EVENTS.contains(&event) {
        errors.push(("event".into(), "is not included in the list".into()));
    }
    if let Some(pattern) = filters.get("title_pattern").and_then(Value::as_str) {
        if pattern.chars().count() > rules::TITLE_PATTERN_MAX_LENGTH {
            errors.push(("filters".into(), format!("title pattern is too long (maximum is {} characters)", rules::TITLE_PATTERN_MAX_LENGTH)));
        } else if rules::regular_expression_pattern(pattern)
            && let Err(e) = rules::title_regex(&pattern[1..pattern.len() - 1])
        {
            errors.push(("filters".into(), format!("title pattern is not a valid regular expression: {e}")));
        }
    }
    errors
}

/// Inserts an alert rule (the setup CLI uses it too).
pub fn insert_rule(x: &Connection, application_id: i64, integration_id: i64, event: &str, filters: &Map<String, Value>) -> rusqlite::Result<i64> {
    let now = crate::control::now_db();
    x.execute(
        "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
        params![application_id, now, event, Value::Object(filters.clone()).to_string(), integration_id, now],
    )?;
    Ok(x.last_insert_rowid())
}

async fn rule_create(cx: Ctx, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    let (Some(app), Some(integ)) = (p.i64("application_id"), p.i64("integration_id")) else { return not_found() };
    let event = p.s("event");
    let filters = rule_filters(&p);
    enum Out {
        NotFound,
        Invalid(Vec<(String, String)>),
        Created,
    }
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let ok_app: bool =
                    x.query_row("SELECT EXISTS (SELECT 1 FROM applications WHERE id = ? AND account_id = ?)", params![app, account], |r| r.get(0))?;
                let ok_int: bool =
                    x.query_row("SELECT EXISTS (SELECT 1 FROM integrations WHERE id = ? AND account_id = ?)", params![integ, account], |r| r.get(0))?;
                if !(ok_app && ok_int) {
                    return Ok(Out::NotFound);
                }
                let errors = rule_errors(&event, &filters);
                if !errors.is_empty() {
                    return Ok(Out::Invalid(errors));
                }
                insert_rule(x, app, integ, &event, &filters)?;
                Ok(Out::Created)
            })
        })
        .await;
    match r {
        Ok(Out::Created) => cx.redirect(Redirect::to(INDEX).notice("Alert rule added")),
        Ok(Out::Invalid(e)) => cx.redirect(Redirect::to(INDEX).errors(e)),
        Ok(Out::NotFound) => not_found(),
        Err(e) => internal(e),
    }
}

/// `destroy`: the rule and its alerts (dependent: :destroy).
async fn rule_destroy(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM alert_rules r JOIN applications ap ON ap.id = r.application_id WHERE r.id = ? AND ap.account_id = ?)",
                    params![id, account],
                    |r| r.get(0),
                )?;
                if exists {
                    tx.execute("UPDATE alerts SET summary_alert_id = NULL WHERE summary_alert_id IN (SELECT id FROM alerts WHERE alert_rule_id = ?)", [id])?;
                    tx.execute("DELETE FROM alerts WHERE alert_rule_id = ?", [id])?;
                    tx.execute("DELETE FROM alert_rules WHERE id = ?", [id])?;
                }
                Ok(exists)
            })
        })
        .await;
    match r {
        Ok(true) => cx.redirect(Redirect::to(INDEX).notice("Alert rule removed")),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_filters_drop_unknown_values_instead_of_widening_the_rule() {
        let p = Params(
            json!({"filters": {"environment_ids": ["3", "", "x7"], "kinds": ["exception", "bogus"], "min_priority": "critical", "title_pattern": "  "}})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(Value::Object(rule_filters(&p)), json!({"environment_ids": [3, 0], "kinds": ["exception"]}));
    }

    #[test]
    fn rule_validation_matches_alert_rule() {
        assert_eq!(rule_errors("nope", &Map::new()), vec![("event".to_string(), "is not included in the list".to_string())]);
        let long = Map::from_iter([("title_pattern".to_string(), json!("a".repeat(257)))]);
        assert!(rule_errors("new_issue", &long)[0].1.contains("maximum is 256"));
        let bad = Map::from_iter([("title_pattern".to_string(), json!("/(unclosed/"))]);
        assert!(rule_errors("new_issue", &bad)[0].1.starts_with("title pattern is not a valid regular expression"));
        assert!(rule_errors("silent_environment", &Map::new()).is_empty(), "the watchdog events are selectable");
    }

    #[test]
    fn integration_props_never_carry_secrets() {
        let i = Integration {
            id: 1,
            account_id: 1,
            kind: "webhook".into(),
            name: "Hook".into(),
            enabled: true,
            settings: json!({"url": "https://example.com/h", "secret": "s3cret"}).as_object().unwrap().clone(),
            settings_error: None,
            last_delivered_at: None,
            last_received_at: None,
            last_synced_at: None,
            last_error: None,
        };
        let p = integration_props(&i);
        assert!(!p.to_string().contains("s3cret"));
        assert_eq!(p["destination"], "https://example.com/h", "the destination is shown, as Rails does");
        let s =
            Integration { kind: "slack".into(), settings: json!({"webhook_url": "https://hooks.slack.com/services/T/B1/x"}).as_object().unwrap().clone(), ..i };
        assert_eq!(integration_props(&s)["settings"], json!({"webhook_url": "configured"}));
    }
}
