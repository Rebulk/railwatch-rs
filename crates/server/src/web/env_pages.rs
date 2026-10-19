//! The environment pages (`/apps/:application_id/envs/:environment_id/...`):
//! EnvironmentScoped (find the environment in the current account, share
//! `environment`, `window`, `range`, `step`, `steps`, `saved_views`) around
//! one page function per controller action.
//!
//! The page props are p3-queries' (crates/server/src/dashboard/pages): each
//! route names its component and a `PageFn`. Until that track merges, every
//! page renders with the shared props only (`props_for` below returns an
//! empty object), so the shell, sidebar and layouts load; the merge replaces
//! `props_for` with a call into `dashboard::pages` (see the TODO).
//!
//! Saved views (SavedViewsController) are control rows and live here.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::WebState;
use super::inertia::{Ctx, Redirect, t_json_opt};
use super::params::Params;
use crate::control::now_db;

type S = Arc<WebState>;

/// (path under the environment, component) for every telemetry page the
/// gem's frontend has, in railwatch-cloud's routes.rb order.
pub const PAGES: &[(&str, &str)] = &[
    ("/", "overview/show"),
    ("/requests", "requests/index"),
    ("/requests/routes/{group_hash}", "requests/route"),
    ("/jobs", "jobs/index"),
    ("/jobs/classes/{group_hash}", "jobs/klass"),
    ("/scheduled_tasks", "scheduled_tasks/index"),
    ("/commands", "commands/index"),
    ("/requests/{id}", "executions/show"),
    ("/jobs/{id}", "executions/show"),
    ("/scheduled_tasks/{id}", "executions/show"),
    ("/commands/{id}", "executions/show"),
    ("/executions/{id}", "executions/show"),
    ("/queries", "queries/index"),
    ("/queries/{id}", "queries/show"),
    ("/spans", "spans/index"),
    ("/spans/{id}", "spans/show"),
    ("/profiles", "profiles/index"),
    ("/profiles/{id}", "profiles/show"),
    ("/traces/{trace_id}", "traces/show"),
    ("/views", "view_renders/index"),
    ("/transactions", "transactions/index"),
    ("/deprecations", "deprecations/index"),
    ("/exceptions", "exceptions/index"),
    ("/cache", "cache_events/index"),
    ("/mail", "mails/index"),
    ("/notifications", "notifications/index"),
    ("/broadcasts", "broadcasts/index"),
    ("/outgoing_requests", "outgoing_requests/index"),
    ("/llm", "llm_calls/index"),
    ("/storage", "storage_ops/index"),
    ("/logs", "logs/index"),
    ("/users", "people/index"),
    ("/users/{id}", "people/show"),
    ("/deploys", "deploys/index"),
    ("/deploys/{id}", "deploys/show"),
    ("/releases", "releases/index"),
    ("/releases/{id}", "releases/show"),
    ("/visits", "visits/index"),
    ("/processes", "processes/index"),
    ("/tenants", "tenants/index"),
    ("/tenants/{id}", "tenants/show"),
    ("/thresholds", "thresholds/index"),
    ("/alerts", "alerts/index"),
    ("/usage", "usage/show"),
];

pub fn routes() -> Router<S> {
    let mut r = Router::new();
    for (path, component) in PAGES {
        let full = if *path == "/" { "/apps/{app}/envs/{env}".to_string() } else { format!("/apps/{{app}}/envs/{{env}}{path}") };
        let component: &'static str = component;
        r = r.route(&full, get(move |cx: Ctx, Path(segments): Path<Vec<(String, String)>>, RawQuery(q): RawQuery| page(cx, component, segments, q)));
    }
    r.route("/apps/{app}/envs/{env}/saved_views", post(saved_view_create))
        .route("/apps/{app}/envs/{env}/saved_views/{id}", patch(saved_view_update).put(saved_view_update).delete(saved_view_destroy))
}

/// EnvironmentScoped#set_environment + #environment_props.
pub struct Scoped {
    pub id: i64,
    pub slug: String,
    pub application_id: i64,
    pub props: Value,
}

async fn scope(cx: &Ctx, app: i64, env: i64) -> Result<Scoped, Response> {
    if cx.current.user.is_none() {
        return Err(cx.redirect(Redirect::to("/sign_in")));
    }
    let Some(account) = cx.current.account.as_ref().map(|a| a.id) else {
        return Err(cx.redirect(Redirect::to("/accounts/new").notice("Create an account to get started")));
    };
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.query_row(
                    "SELECT e.id, e.name, e.slug, ap.id, ap.name, ap.issue_prefix, e.last_seen_at, e.paused_at IS NOT NULL, e.token_prefix,
                            ap.repository_url, ap.default_branch
                     FROM environments e JOIN applications ap ON ap.id = e.application_id
                     WHERE e.id = ? AND ap.id = ? AND ap.account_id = ?",
                    params![env, app, account],
                    |r| {
                        let slug: String = r.get(2)?;
                        Ok(Scoped {
                            id: r.get(0)?,
                            slug: slug.clone(),
                            application_id: r.get(3)?,
                            props: json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "slug": slug, "application_id": r.get::<_, i64>(3)?,
                                          "application_name": r.get::<_, String>(4)?, "issue_prefix": r.get::<_, String>(5)?,
                                          "last_seen_at": t_json_opt(r.get(6)?), "paused": r.get::<_, bool>(7)?, "token_prefix": r.get::<_, String>(8)?,
                                          "repository_url": r.get::<_, Option<String>>(9)?, "default_branch": r.get::<_, String>(10)?}),
                        })
                    },
                )
                .optional()
            })
        })
        .await;
    match r {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err((StatusCode::NOT_FOUND, "Not Found").into_response()),
        Err(e) => {
            tracing::error!(error = %e, "environment lookup failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response())
        }
    }
}

/// Railwatch::Window + Telemetry::Aggregations.steps_for/default_step:
/// the window shared props. p3-queries' `dashboard::window` is the full
/// port (custom ranges, Ruby Time.zone.parse); this covers the presets and
/// is replaced by `dashboard::shared_props(&ctx)` at the merge.
pub fn window_props(p: &Params, now: i64) -> Value {
    const PRESETS: [(&str, i64); 5] = [("1h", 3600), ("6h", 21_600), ("24h", 86_400), ("7d", 604_800), ("30d", 2_592_000)];
    const STEPS: [(&str, i64); 6] = [("1m", 60), ("5m", 300), ("15m", 900), ("1h", 3600), ("6h", 21_600), ("1d", 86_400)];
    let key = p.s("window");
    let (key, span) = PRESETS.iter().find(|(k, _)| *k == key).copied().unwrap_or(("24h", 86_400));
    let from = now - span * 1_000_000;
    let offered: Vec<&str> = STEPS.iter().filter(|(_, s)| (2..=400).contains(&(span / s))).map(|(k, _)| *k).collect();
    let default = if span <= 7200 {
        "1m"
    } else if span <= 43_200 {
        "5m"
    } else if span <= 604_800 {
        "1h"
    } else {
        "6h"
    };
    let requested = p.s("step");
    let step = if offered.contains(&requested.as_str()) { requested } else { default.to_string() };
    json!({"window": key, "range": {"from": crate::time::iso8601_6(from), "to": crate::time::iso8601_6(now)}, "step": step, "steps": offered})
}

/// SavedView.props_for(environment, user).
fn saved_views(x: &rusqlite::Connection, env: i64, app: i64, user: i64, base_path: &str) -> rusqlite::Result<Vec<Value>> {
    x.prepare(
        "SELECT id, name, page, query, \"window\", params, pinned, shared, user_id FROM saved_views
         WHERE environment_id = ?1 AND (shared = 1 OR user_id = ?2) ORDER BY name",
    )?
    .query_map(params![env, user], |r| {
        let page: String = r.get(2)?;
        let query: Option<String> = r.get(3)?;
        let window: Option<String> = r.get(4)?;
        let extra: Value = serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(json!({}));
        Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "page": page, "query": query, "window": window,
                  "params": extra, "pinned": r.get::<_, bool>(6)?, "shared": r.get::<_, bool>(7)?, "mine": r.get::<_, i64>(8)? == user,
                  "url": format!("{base_path}{}", saved_view_path(app, env, &page, window.as_deref(), query.as_deref(), &extra))}))
    })?
    .collect()
}

/// SavedView::PAGE_PATHS -> the index path with window, q and params.
pub fn saved_view_path(app: i64, env: i64, page: &str, window: Option<&str>, query: Option<&str>, extra: &Value) -> String {
    let segment = match page {
        "view_renders" => "views",
        "cache_events" => "cache",
        "mails" => "mail",
        "llm_calls" => "llm",
        "storage_ops" => "storage",
        "people" => "users",
        p => p,
    };
    let mut q = form_urlencoded::Serializer::new(String::new());
    if let Some(w) = window.filter(|w| !w.is_empty()) {
        q.append_pair("window", w);
    }
    if let Some(s) = query.filter(|s| !s.is_empty()) {
        q.append_pair("q", s);
    }
    for (k, v) in extra.as_object().into_iter().flatten() {
        q.append_pair(k, v.as_str().map_or_else(|| v.to_string(), str::to_string).as_str());
    }
    let q = q.finish();
    let base = format!("/apps/{app}/envs/{env}/{segment}");
    if q.is_empty() { base } else { format!("{base}?{q}") }
}

pub const SAVED_VIEW_PAGES: [&str; 16] = [
    "requests",
    "jobs",
    "scheduled_tasks",
    "commands",
    "exceptions",
    "queries",
    "spans",
    "logs",
    "visits",
    "people",
    "tenants",
    "llm_calls",
    "outgoing_requests",
    "cache_events",
    "mails",
    "notifications",
];

/// Page props. Deploys index reads only control rows, so it is here; the
/// rest are p3-queries'.
///
/// TODO(p3-queries merge): `dashboard::pages::<action>(&reader, &ctx,
/// &control_data)` for every other `component`, mapping PageError::NotFound
/// to 404 and ::Unprocessable to 422 (InertiaController's rescue_from
/// InvalidCursor). Until then they render with the shared props only.
async fn props_for(cx: &Ctx, component: &str, scoped: &Scoped, params: &Params, segments: &[(String, String)]) -> Result<Value, Response> {
    use super::dashboard_bridge::{DbControl, page_fn};
    use crate::dashboard::PageError;
    // alerts/index is control rows only (web/alerts.rs).
    if component == "alerts/index" {
        let (account, env, q) = (cx.account_id(), scoped.id, params.s("q"));
        let q2 = q.clone();
        return match cx.state.db(move |c| c.with(|x| super::alerts::rows(x, account, Some(env), &q2))).await {
            Ok(rows) => Ok(super::alerts::props(rows, &q)),
            Err(e) => {
                tracing::error!(error = %e, "alerts failed");
                Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response())
            }
        };
    }
    let Some(f) = page_fn(component) else { return Ok(json!({})) };
    let env = scoped.id;
    let control = match cx.state.db(move |c| c.with(|x| DbControl::load(x, env))).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "control rows failed");
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response());
        }
    };
    // params[:x] for the query string and the route's own segments, as Rails merges them.
    let mut p: crate::dashboard::Params = params.0.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string))).collect();
    for (k, v) in segments {
        if k != "app" && k != "env" {
            p.insert(k.clone(), v.clone());
        }
    }
    let ctx = crate::dashboard::PageContext::new(env, crate::time::now_micros(), p, cx.state.cursor_key.clone());
    let state = cx.state.clone();
    let slug = scoped.slug.clone();
    let result = tokio::task::spawn_blocking(move || {
        let reader = state.readers.get(&slug).map_err(PageError::Sql)?;
        f(&reader, &ctx, &control)
    })
    .await
    .unwrap_or_else(|e| Err(PageError::Unprocessable(format!("page task failed: {e}"))));
    match result {
        Ok(v) if component == "usage/show" => usage_control_props(cx, scoped, v).await,
        Ok(v) => Ok(v),
        Err(PageError::NotFound) => Err((StatusCode::NOT_FOUND, "Not Found").into_response()),
        Err(PageError::Unprocessable(m)) => Err((StatusCode::UNPROCESSABLE_ENTITY, m).into_response()),
        Err(PageError::Sql(e)) => {
            tracing::error!(component, error = %e, "page query failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response())
        }
    }
}

/// UsageController#show's control half: `account` (plan, quota, cap,
/// retention, the account's events this month), `environment_events`,
/// `db_bytes`. The quota is null when this server does not enforce one.
async fn usage_control_props(cx: &Ctx, scoped: &Scoped, mut props: Value) -> Result<Value, Response> {
    let Some(account) = cx.current.account.clone() else { return Ok(props) };
    let env = scoped.id;
    let month = format!("{}-01", crate::control::month_of(crate::time::now_micros()));
    let counts = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let sum = |sql: &str, id: i64| x.query_row(sql, params![id, month], |r| r.get::<_, i64>(0));
                Ok((
                    sum(
                        "SELECT COALESCE(SUM(m.events), 0) FROM ingest_usage_months m JOIN environments e ON e.id = m.environment_id
                         JOIN applications ap ON ap.id = e.application_id WHERE ap.account_id = ?1 AND m.month = ?2",
                        account.id,
                    )?,
                    sum("SELECT COALESCE(SUM(events), 0) FROM ingest_usage_months WHERE environment_id = ?1 AND month = ?2", env)?,
                ))
            })
        })
        .await;
    let (account_events, environment_events) = counts.map_err(|e| {
        tracing::error!(error = %e, "usage counts failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
    })?;
    let quota = if cx.state.config.enforce_quota { Some(account.monthly_event_quota) } else { None };
    props["account"] = json!({"plan": account.plan, "monthly_event_quota": quota, "additional_events_cap": account.additional_events_cap,
                              "retention_days": account.retention_days, "events_this_month": account_events});
    props["environment_events"] = json!(environment_events);
    props["db_bytes"] = cx.state.tenants.layout(&scoped.slug).map_or(Value::Null, |l| json!(l.bytes()));
    Ok(props)
}

async fn page(cx: Ctx, component: &'static str, segments: Vec<(String, String)>, q: Option<String>) -> Response {
    let seg = |k: &str| segments.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.parse::<i64>().ok());
    let (Some(app), Some(env)) = (seg("app"), seg("env")) else { return (StatusCode::NOT_FOUND, "Not Found").into_response() };
    let scoped = match scope(&cx, app, env).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let params = Params::from_query(q.as_deref());
    let user = cx.user_id();
    let base = cx.state.config.base_path.clone();
    let views = match cx.state.db(move |c| c.with(|x| saved_views(x, env, app, user, &base))).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "saved views failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };
    let mut shared = window_props(&params, crate::time::now_micros());
    shared["environment"] = scoped.props.clone();
    shared["saved_views"] = Value::Array(views);
    let props = match props_for(&cx, component, &scoped, &params, &segments).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    cx.render(component, props, Some(shared)).await
}

// ---------------------------------------------------------------------------
// SavedViewsController

async fn saved_view_create(cx: Ctx, Path((app, env)): Path<(i64, i64)>, p: Params) -> Response {
    let scoped = match scope(&cx, app, env).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fallback = format!("/apps/{}/envs/{}", scoped.application_id, scoped.id);
    let name = p.s("name");
    let page = p.s("page");
    let mut errors = Vec::new();
    if name.trim().is_empty() {
        errors.push(("name".to_string(), "can't be blank".to_string()));
    } else if name.chars().count() > 80 {
        errors.push(("name".to_string(), "is too long (maximum is 80 characters)".to_string()));
    }
    if !SAVED_VIEW_PAGES.contains(&page.as_str()) {
        errors.push(("page".to_string(), "is not included in the list".to_string()));
    }
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(cx.back_or(&fallback)).errors(errors));
    }
    let query = p.presence("query");
    let window = p.presence("window").map(|w| w.chars().take(8).collect::<String>());
    let extra = p.get("params").filter(|v| v.is_object()).cloned().unwrap_or(json!({}));
    let pinned = p.bool("pinned").unwrap_or(true);
    let shared = p.bool("shared").unwrap_or(true);
    let user = cx.user_id();
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.execute(
                    "INSERT INTO saved_views (environment_id, user_id, name, page, query, \"window\", params, pinned, shared, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                    params![env, user, name, page, query, window, extra.to_string(), pinned, shared, now_db()],
                )
            })
        })
        .await;
    match r {
        Ok(_) => cx.redirect(Redirect::to(cx.back_or(&fallback)).notice("View saved")),
        Err(e) => {
            tracing::error!(error = %e, "saving a view failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

/// Its owner, or an account admin, may change a view.
async fn editable(cx: &Ctx, env: i64, id: i64) -> Result<(), Response> {
    let user = cx.user_id();
    let account = cx.account_id();
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let owner: Option<i64> =
                    x.query_row("SELECT user_id FROM saved_views WHERE id = ? AND environment_id = ?", params![id, env], |r| r.get(0)).optional()?;
                let role = crate::control::membership_role(x, account, user)?;
                Ok((owner, role))
            })
        })
        .await;
    match r {
        Ok((None, _)) => Err((StatusCode::NOT_FOUND, "Not Found").into_response()),
        Ok((Some(o), role)) if o == user || matches!(role.as_deref(), Some("owner" | "admin")) => Ok(()),
        Ok(_) => Err(StatusCode::FORBIDDEN.into_response()),
        Err(e) => {
            tracing::error!(error = %e, "saved view lookup failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response())
        }
    }
}

async fn saved_view_update(cx: Ctx, Path((app, env, id)): Path<(i64, i64, i64)>, p: Params) -> Response {
    let scoped = match scope(&cx, app, env).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = editable(&cx, env, id).await {
        return r;
    }
    let fallback = format!("/apps/{}/envs/{}", scoped.application_id, scoped.id);
    let name = p.has("name").then(|| p.s("name"));
    if name.as_ref().is_some_and(|n| n.trim().is_empty()) {
        return cx.redirect(Redirect::to(cx.back_or(&fallback)).error("name", "can't be blank"));
    }
    let pinned = p.bool("pinned");
    let shared = p.bool("shared");
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.execute(
                    "UPDATE saved_views SET name = COALESCE(?, name), pinned = COALESCE(?, pinned), shared = COALESCE(?, shared), updated_at = ? WHERE id = ?",
                    params![name, pinned, shared, now_db(), id],
                )
            })
        })
        .await;
    match r {
        Ok(_) => cx.redirect(Redirect::to(cx.back_or(&fallback)).notice("View updated")),
        Err(e) => {
            tracing::error!(error = %e, "updating a view failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

async fn saved_view_destroy(cx: Ctx, Path((app, env, id)): Path<(i64, i64, i64)>) -> Response {
    let scoped = match scope(&cx, app, env).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = editable(&cx, env, id).await {
        return r;
    }
    let fallback = format!("/apps/{}/envs/{}", scoped.application_id, scoped.id);
    let _ = State::<S>;
    match cx.state.db(move |c| c.with(|x| x.execute("DELETE FROM saved_views WHERE id = ?", [id]))).await {
        Ok(_) => cx.redirect(Redirect::to(cx.back_or(&fallback)).notice("View deleted")),
        Err(e) => {
            tracing::error!(error = %e, "deleting a view failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_presets_and_steps_match_the_gem() {
        let p = Params::from_query(Some("window=7d&step=1d"));
        let w = window_props(&p, 1_791_900_000_000_000);
        assert_eq!(w["window"], "7d");
        assert_eq!(w["step"], "1d");
        assert_eq!(w["steps"], json!(["1h", "6h", "1d"]));
        let d = window_props(&Params::default(), 1_791_900_000_000_000);
        assert_eq!((d["window"].as_str(), d["step"].as_str()), (Some("24h"), Some("1h")));
        assert_eq!(d["steps"], json!(["5m", "15m", "1h", "6h"]));
    }

    #[test]
    fn saved_view_urls_use_the_index_route() {
        assert_eq!(saved_view_path(1, 2, "people", Some("7d"), Some("a b"), &json!({"sort": "p95"})), "/apps/1/envs/2/users?window=7d&q=a+b&sort=p95");
        assert_eq!(saved_view_path(1, 2, "logs", None, None, &json!({})), "/apps/1/envs/2/logs");
    }
}
