//! The Uptime page of an environment and its monitor and status-page
//! settings (uptime/mod.rs, docs/uptime.md). Everyone in the account sees
//! the page; only admins (owner/admin) add, change or delete monitors and
//! the status page, as with integrations.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::response::Response;
use axum::routing::{get, patch, post};
use serde_json::{Value, json};

use super::WebState;
use super::controllers::{guard, internal, not_found, require_admin};
use super::env_pages::{Scoped, scope, window_props};
use super::inertia::{Ctx, Redirect};
use super::params::Params;
use crate::time;
use crate::uptime::{self, MonitorInput, probe};

type S = Arc<WebState>;

pub fn routes() -> Router<S> {
    Router::new()
        .route("/apps/{app}/envs/{env}/uptime", get(index))
        .route("/apps/{app}/envs/{env}/uptime/monitors", post(create))
        .route("/apps/{app}/envs/{env}/uptime/monitors/{id}", patch(update).put(update).delete(destroy))
        .route("/apps/{app}/envs/{env}/uptime/status_page", patch(status_page_update).put(status_page_update))
}

fn page_path(s: &Scoped) -> String {
    format!("/apps/{}/envs/{}/uptime", s.application_id, s.id)
}

/// The latency chart's bucket: a minute over a day, ten minutes over 7 days.
fn chart_range(window: &str, now: i64) -> (i64, i64) {
    match window {
        "7d" => (now - 7 * 86_400 * 1_000_000, 600),
        _ => (now - 86_400 * 1_000_000, 60),
    }
}

fn t(micros: Option<i64>) -> Value {
    micros.map_or(Value::Null, |m| json!(time::iso8601(m.div_euclid(1_000_000))))
}

/// The page's props: every monitor with its state, uptime windows, 90
/// days and chart, the recent incidents and the status page settings.
pub fn props(c: &rusqlite::Connection, env: i64, now: i64, window: &str, base_url: &str) -> rusqlite::Result<Value> {
    let (from, bucket) = chart_range(window, now);
    let mut monitors = Vec::new();
    for m in uptime::monitors(c, env)? {
        monitors.push(json!({
            "id": m.id, "name": m.name, "url": m.url, "method": m.method, "expected_status": m.expected_status, "keyword": m.keyword,
            "timeout_seconds": m.timeout_seconds, "interval_seconds": m.interval_seconds, "enabled": m.enabled,
            "public": m.public, "public_name": m.public_name, "public_url": m.public_url,
            "status": m.status, "status_since": t(uptime::status_since(c, &m)?),
            "last_checked_at": t(m.last_checked_at), "last_status_code": m.last_status_code, "last_latency_ms": m.last_latency_ms, "last_error": m.last_error,
            "uptime": uptime::uptime_windows(c, m.id, now)?,
            "days": uptime::daily(c, m.id, now, uptime::DAILY_DAYS)?,
            "latency": uptime::latency_series(c, m.id, from, bucket)?,
        }));
    }
    let incidents: Vec<Value> = uptime::incidents(c, env, 20, false)?
        .iter()
        .map(|i| {
            json!({"id": i.id, "monitor_id": i.monitor_id, "monitor": i.monitor, "started_at": t(Some(i.started_at)), "ended_at": t(i.ended_at),
                   "duration_seconds": i.duration_secs(now), "cause": i.cause, "detail": i.detail})
        })
        .collect();
    let page =
        uptime::status_page(c, env)?.map(|p| json!({"slug": p.slug, "title": p.title, "enabled": p.enabled, "url": format!("{base_url}/status/{}", p.slug)}));
    Ok(json!({
        "monitors": monitors,
        "incidents": incidents,
        "status_page": page,
        "limits": {"min_interval_seconds": uptime::MIN_INTERVAL_SECS, "max_interval_seconds": uptime::MAX_INTERVAL_SECS,
                   "max_timeout_seconds": uptime::MAX_TIMEOUT_SECS, "max_monitors": uptime::MAX_MONITORS_PER_ENVIRONMENT,
                   "failures_to_down": uptime::FAILURES_TO_DOWN},
    }))
}

async fn index(cx: Ctx, Path((app, env)): Path<(i64, i64)>, p: Params) -> Response {
    let scoped = guard!(scope(&cx, app, env).await);
    let now = time::now_micros();
    // The page's own two windows (24h, 7d) for the chart; the uptime
    // figures always show all four.
    let window = if p.s("window") == "7d" { "7d" } else { "24h" };
    let base = cx.state.config.pages_url();
    let props = match cx.state.db(move |c| c.with(|x| props(x, env, now, window, &base))).await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let mut shared = window_props(&Params::from_query(Some(&format!("window={window}"))), now);
    shared["environment"] = scoped.props.clone();
    shared["saved_views"] = json!([]);
    let user = cx.user_id();
    let account = cx.account_id();
    let admin = cx
        .state
        .db(move |c| c.with(|x| crate::control::membership_role(x, account, user)))
        .await
        .map(|r| matches!(r.as_deref(), Some("owner" | "admin")))
        .unwrap_or(false);
    let mut props = props;
    props["can_manage"] = json!(admin);
    cx.render("uptime/index", props, Some(shared)).await
}

/// The form's fields. A blank expected status is "any 2xx/3xx", a blank
/// keyword none.
fn input(p: &Params) -> MonitorInput {
    let d = MonitorInput::default();
    let int = |k: &str, default: i64| match p.presence(k) {
        None => Some(default),
        Some(s) => s.trim().parse().ok(),
    };
    MonitorInput {
        name: p.s("name"),
        url: p.s("url").trim().to_string(),
        method: p.presence("method").unwrap_or(d.method).to_ascii_uppercase(),
        expected_status: p.presence("expected_status").map(|s| s.trim().parse().unwrap_or(-1)),
        keyword: p.presence("keyword"),
        // An unparsable number fails validation rather than taking the default.
        timeout_seconds: int("timeout_seconds", d.timeout_seconds).unwrap_or(-1),
        interval_seconds: int("interval_seconds", d.interval_seconds).unwrap_or(-1),
        enabled: p.bool("enabled").unwrap_or(true),
        public: p.bool("public").unwrap_or(false),
        public_name: p.presence("public_name"),
        public_url: p.bool("public_url").unwrap_or(false),
    }
}

async fn admin_scope(cx: &Ctx, app: i64, env: i64) -> Result<Scoped, Response> {
    let scoped = scope(cx, app, env).await?;
    require_admin(cx).await?;
    Ok(scoped)
}

async fn create(cx: Ctx, Path((app, env)): Path<(i64, i64)>, p: Params) -> Response {
    let scoped = guard!(admin_scope(&cx, app, env).await);
    let back = page_path(&scoped);
    let m = input(&p);
    let errors = uptime::validate(&m, probe::Policy::from_env());
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(back).errors(errors));
    }
    match cx.state.db(move |c| c.with(|x| uptime::insert(x, env, &m))).await {
        Ok(Ok(_)) => cx.redirect(Redirect::to(back).notice("Monitor added. The first check runs within a few seconds.")),
        Ok(Err(msg)) => cx.redirect(Redirect::to(back).alert(msg)),
        Err(e) => internal(e),
    }
}

async fn update(cx: Ctx, Path((app, env, id)): Path<(i64, i64, i64)>, p: Params) -> Response {
    let scoped = guard!(admin_scope(&cx, app, env).await);
    let back = page_path(&scoped);
    let existing = match cx.state.db(move |c| c.with(|x| uptime::find(x, env, id))).await {
        Ok(Some(m)) => m,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    // A partial update (the enable toggle) keeps every field it does not send.
    let mut m = MonitorInput {
        name: existing.name,
        url: existing.url,
        method: existing.method,
        expected_status: existing.expected_status,
        keyword: existing.keyword,
        timeout_seconds: existing.timeout_seconds,
        interval_seconds: existing.interval_seconds,
        enabled: existing.enabled,
        public: existing.public,
        public_name: existing.public_name,
        public_url: existing.public_url,
    };
    let given = input(&p);
    macro_rules! take {
        ($($f:ident),*) => { $( if p.has(stringify!($f)) { m.$f = given.$f.clone(); } )* };
    }
    take!(name, url, method, expected_status, keyword, timeout_seconds, interval_seconds, enabled, public, public_name, public_url);
    let errors = uptime::validate(&m, probe::Policy::from_env());
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(back).errors(errors));
    }
    let now = time::now_micros();
    match cx.state.db(move |c| c.with(|x| uptime::update(x, id, &m, now))).await {
        Ok(()) => cx.redirect(Redirect::to(back).notice("Monitor updated")),
        Err(e) => internal(e),
    }
}

async fn destroy(cx: Ctx, Path((app, env, id)): Path<(i64, i64, i64)>) -> Response {
    let scoped = guard!(admin_scope(&cx, app, env).await);
    let back = page_path(&scoped);
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| match uptime::find(x, env, id)? {
                Some(_) => uptime::delete(x, id).map(|_| true),
                None => Ok(false),
            })
        })
        .await;
    match r {
        Ok(true) => cx.redirect(Redirect::to(back).notice("Monitor deleted")),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

/// Creates or changes the environment's status page: title, enabled, and
/// a slug (blank keeps the current one, or makes an unguessable one).
async fn status_page_update(cx: Ctx, Path((app, env)): Path<(i64, i64)>, p: Params) -> Response {
    let scoped = guard!(admin_scope(&cx, app, env).await);
    let back = page_path(&scoped);
    let current = match cx.state.db(move |c| c.with(|x| uptime::status_page(x, env))).await {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let slug = match p.presence("slug").map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) => s,
        None => current.as_ref().map_or_else(uptime::new_slug, |c| c.slug.clone()),
    };
    if let Some(msg) = uptime::validate_slug(&slug) {
        return cx.redirect(Redirect::to(back).error("slug", &msg));
    }
    let title = p
        .presence("title")
        .or_else(|| current.as_ref().map(|c| c.title.clone()))
        .unwrap_or_else(|| format!("{} status", scoped.props["application_name"].as_str().unwrap_or("")));
    if title.chars().count() > uptime::MAX_NAME_CHARS {
        return cx.redirect(Redirect::to(back).error("title", "is too long (maximum is 80 characters)"));
    }
    let enabled = p.bool("enabled").unwrap_or_else(|| current.as_ref().is_some_and(|c| c.enabled));
    match cx.state.db(move |c| c.with(|x| uptime::save_status_page(x, env, &slug, &title, enabled))).await {
        Ok(Ok(())) => cx.redirect(Redirect::to(back).notice(if enabled { "Status page saved and public" } else { "Status page saved (not public)" })),
        Ok(Err((field, msg))) => cx.redirect(Redirect::to(back).error(&field, &msg)),
        Err(e) => internal(e),
    }
}
