//! IssuesController and CommentsController (railwatch-cloud @ 464f0fd):
//! the account-level /issues pages.
//!
//! The lifecycle rules (record_status_change!, the priority/assignee
//! callbacks, merge_into!/unmerge!/split!) are the Issue model's, in
//! grouping::issue; this file is the controller: scoping to the account,
//! params, props, redirects. Every write runs in one IMMEDIATE transaction on
//! the single-writer control connection, which is what Rails' lock! gives.

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value, json};

use super::WebState;
use super::controllers::{guard, internal, not_found, require_account};
use super::inertia::{Ctx, Redirect, t_json, t_json_opt};
use super::params::Params;
use crate::control::Control;
use crate::dashboard::rb;
use crate::grouping::issue::{self, Actor, KINDS, MergeError, PRIORITIES, SPLIT_SAMPLE, STATUSES};

type S = Arc<WebState>;

/// IssuesController::BULK_LIMIT
pub const BULK_LIMIT: usize = 200;

pub fn routes() -> Router<S> {
    Router::new()
        .route("/issues", get(index))
        .route("/issues/bulk", post(bulk))
        .route("/issues/{id}", get(show).patch(update).put(update))
        .route("/issues/{id}/merge_candidates", get(merge_candidates))
        .route("/issues/{id}/comments", post(comment_create))
}

// ---------------------------------------------------------------------------
// Handlers

async fn index(cx: Ctx, p: Params) -> Response {
    let account = guard!(require_account(&cx));
    let user = cx.user_id();
    match cx.state.db(move |c| c.with(|x| index_props(x, account, user, &p))).await {
        Ok(props) => cx.render("issues/index", props, None).await,
        Err(e) => internal(e),
    }
}

async fn show(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_account(&cx));
    let found = cx.state.db(move |c| c.with(|x| find_shown(x, account, id))).await;
    let shown = match found {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    // render_detection_issue: performance/anomaly issues read only what
    // IssueDetectionPresenter reads.
    let (telemetry, detection) = if shown.kind == "exception" {
        match exception_telemetry(&cx, &shown).await {
            Ok(t) => (Some(t), Value::Null),
            Err(r) => return *r,
        }
    } else {
        match detection_props(&cx, &shown).await {
            Ok(d) => (None, d),
            Err(e) => return internal(e),
        }
    };
    let props = cx.state.db(move |c| c.with(|x| show_props(x, account, &shown, telemetry, detection))).await;
    match props {
        Ok(props) => cx.render("issues/show", props, None).await,
        Err(e) => internal(e),
    }
}

/// IssueDetectionPresenter.new(@issue).as_json over the tenant (an empty
/// schema-only one when the environment has no file yet).
async fn detection_props(cx: &Ctx, shown: &Shown) -> rusqlite::Result<Value> {
    let state = cx.state.clone();
    let (slug, kind, occurrences, sample) = (shown.environment_slug.clone(), shown.kind.clone(), shown.occurrences, shown.sample.clone());
    tokio::task::spawn_blocking(move || {
        let issue = crate::detection::presenter::Presented { kind: &kind, occurrences, sample: &sample };
        let now = crate::time::now_micros();
        if state.tenants.layout(&slug).is_some() {
            let reader = state.readers.get(&slug)?;
            crate::detection::presenter::as_json(&issue, &reader, now)
        } else {
            let mut empty = Connection::open_in_memory()?;
            crate::schema::create_tenant(&mut empty, &crate::control::now_db())?;
            crate::detection::presenter::as_json(&issue, &empty, now)
        }
    })
    .await
    .unwrap_or_else(|e| Err(rusqlite::Error::InvalidParameterName(format!("detection task failed: {e}"))))
}

/// The telemetry half of issues#show from the environment's tenant file.
/// A missing file reads as an empty tenant (a schema-only in-memory file),
/// so the props are exactly what Rails renders with no telemetry yet.
async fn exception_telemetry(cx: &Ctx, shown: &Shown) -> Result<Value, Box<Response>> {
    use crate::dashboard::PageError;
    let ctx = crate::dashboard::PageContext::new(shown.environment_id, crate::time::now_micros(), Default::default(), cx.state.cursor_key.clone());
    let control_issue = shown.control_issue();
    let state = cx.state.clone();
    let slug = shown.environment_slug.clone();
    let result = tokio::task::spawn_blocking(move || {
        if state.tenants.layout(&slug).is_some() {
            let reader = state.readers.get(&slug).map_err(PageError::Sql)?;
            crate::dashboard::pages::issue_telemetry(&reader, &ctx, &control_issue)
        } else {
            let mut empty = Connection::open_in_memory().map_err(PageError::Sql)?;
            crate::schema::create_tenant(&mut empty, &crate::control::now_db()).map_err(PageError::Sql)?;
            crate::dashboard::pages::issue_telemetry(&empty, &ctx, &control_issue)
        }
    })
    .await
    .unwrap_or_else(|e| Err(PageError::Unprocessable(format!("page task failed: {e}"))));
    match result {
        Ok(v) => Ok(v),
        Err(PageError::NotFound) => Err(Box::new(not_found())),
        Err(PageError::Unprocessable(m)) => Err(Box::new((StatusCode::UNPROCESSABLE_ENTITY, m).into_response())),
        Err(PageError::Sql(e)) => Err(Box::new(internal(e))),
    }
}

async fn update(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    let account = guard!(require_account(&cx));
    let found = cx.state.db(move |c| c.with(|x| find_shown(x, account, id))).await;
    let shown = match found {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    // split! reads the recent occurrences from telemetry before its transaction.
    let rows = if p.s("action_name") == "split" && p.s("by") == "message" {
        match split_rows(&cx.state, &shown.environment_slug, &shown.group_hash).await {
            Ok(r) => r,
            Err(e) => return internal(e),
        }
    } else {
        vec![]
    };
    let (uid, name) = actor_of(&cx);
    let result = cx.state.db(move |c| write(c, |tx| apply_update(tx, account, Actor { user_id: uid, name: &name }, id, &p, &rows))).await;
    match result {
        Ok(notice) => cx.redirect(Redirect::to(format!("/issues/{id}")).notice(notice)),
        Err(f) => fail(&cx, f, &format!("/issues/{id}")),
    }
}

async fn bulk(cx: Ctx, p: Params) -> Response {
    let account = guard!(require_account(&cx));
    let (uid, name) = actor_of(&cx);
    let result = cx.state.db(move |c| write(c, |tx| apply_bulk(tx, account, Actor { user_id: uid, name: &name }, &p))).await;
    match result {
        Ok(n) => cx.redirect(Redirect::to(cx.back_or("/issues")).notice(format!("{n} issue(s) updated"))),
        Err(f) => fail(&cx, f, "/issues"),
    }
}

async fn merge_candidates(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    let account = guard!(require_account(&cx));
    let q = p.s("q");
    match cx.state.db(move |c| c.with(|x| merge_candidates_json(x, account, id, &q))).await {
        Ok(Some(v)) => ([(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")], v.to_string()).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// CommentsController#create.
async fn comment_create(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    let account = guard!(require_account(&cx));
    // params.require(:body): missing or blank is ActionController::ParameterMissing (400).
    let Some(body) = p.get("body").filter(|v| required_present(v)).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)) else {
        return (StatusCode::BAD_REQUEST, "param is missing or the value is empty: body").into_response();
    };
    let (uid, _) = actor_of(&cx);
    match cx.state.db(move |c| write(c, |tx| create_comment(tx, account, uid, id, &body))).await {
        Ok(()) => cx.redirect(Redirect::to(format!("/issues/{id}")).notice("Comment added")),
        Err(f) => fail(&cx, f, &format!("/issues/{id}")),
    }
}

fn actor_of(cx: &Ctx) -> (i64, String) {
    let u = cx.current.user.as_ref().expect("authenticated route");
    (u.id, u.name.clone())
}

/// `params.require` treats nil, "" / whitespace, [] and {} as missing; false is a value.
fn required_present(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => rb::present(Some(s)),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Writes

/// Why a write did not happen, mapped to Rails' responses.
#[derive(Debug)]
pub enum Fail {
    /// ActiveRecord::RecordNotFound: 404.
    NotFound,
    /// ActiveRecord::RecordInvalid: 422.
    Invalid,
    /// Issue::InvalidMerge: redirect_back with alert = message.
    Merge(String),
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        Fail::Sql(e)
    }
}

impl From<MergeError> for Fail {
    fn from(e: MergeError) -> Self {
        match e {
            MergeError::Invalid(m) => Fail::Merge(m),
            MergeError::Sql(e) => Fail::Sql(e),
        }
    }
}

fn fail(cx: &Ctx, f: Fail, fallback: &str) -> Response {
    match f {
        Fail::NotFound => not_found(),
        Fail::Invalid => (StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable Content").into_response(),
        Fail::Merge(m) => cx.redirect(Redirect::to(cx.back_or(fallback)).alert(m)),
        Fail::Sql(e) => internal(e),
    }
}

/// One IMMEDIATE transaction, committed only when `f` succeeds (an
/// InvalidMerge rolls back a half-folded bulk merge, as Issue.transaction does).
fn write<T>(c: &Control, f: impl FnOnce(&rusqlite::Transaction) -> Result<T, Fail>) -> Result<T, Fail> {
    let mut conn = c.conn();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

/// IssuesController#update's case on action_name. Returns the notice.
pub fn apply_update(c: &Connection, account: i64, actor: Actor<'_>, id: i64, p: &Params, split_rows: &[crate::grouping::ExceptionRow]) -> Result<String, Fail> {
    let Some(i) = find_in_account(c, account, id)? else { return Err(Fail::NotFound) };
    let mut notice = "Issue updated".to_string();
    match p.s("action_name").as_str() {
        "resolve" => {
            let deploy = match p.presence("deploy") {
                Some(d) => Some(d),
                None => latest_deploy(c, i.environment_id)?,
            };
            issue::resolve_by(c, &i, deploy.as_deref(), Some(actor))?;
        }
        "ignore" => {
            issue::ignore(c, &i, Some(actor))?;
        }
        "reopen" => {
            issue::reopen(c, &i, Some(actor))?;
        }
        "assign" => {
            let to = match p.get("assignee_id").filter(|v| value_present(v)) {
                None => None,
                Some(v) => Some(member(c, account, ar_int(v))?.ok_or(Fail::NotFound)?),
            };
            issue::set_assignee(c, &i, to.as_ref().map(|(id, n)| (*id, n.as_str())), Some(actor))?;
        }
        "assign_me" => issue::set_assignee(c, &i, Some((actor.user_id, actor.name)), Some(actor))?,
        "priority" => {
            if !issue::set_priority(c, &i, &p.s("priority"), Some(actor))? {
                return Err(Fail::Invalid);
            }
        }
        "merge" => {
            let target_id = ar_int(p.get("target_id").unwrap_or(&Value::Null));
            let target: Option<i64> = c
                .query_row("SELECT id FROM issues WHERE environment_id = ? AND status = 'open' AND id = ?", params![i.environment_id, target_id], |r| r.get(0))
                .optional()?;
            issue::merge_into(c, i.id, target.ok_or(Fail::NotFound)?, Some(actor))?;
        }
        "unmerge" => issue::unmerge(c, i.id, Some(actor))?,
        "split" => {
            let created = issue::split(c, &i, &p.s("by"), split_rows, Some(actor))?;
            notice = if created.is_empty() {
                "Nothing to split \u{2014} every recent occurrence is already in its own issue".into()
            } else {
                format!("Split into {} issue(s)", created.len())
            };
        }
        // update!(params.permit(:title))
        _ => {
            if let Some(v) = p.get("title").filter(|v| v.is_string() || v.is_number() || v.is_null() || v.is_boolean()) {
                let title = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                if !rb::present(Some(&title)) {
                    return Err(Fail::Invalid);
                }
                c.execute("UPDATE issues SET title = ?, updated_at = ? WHERE id = ?", params![title, issue::now_db(), i.id])?;
            }
        }
    }
    Ok(notice)
}

/// IssuesController#bulk. Returns ids.size for the notice.
pub fn apply_bulk(c: &Connection, account: i64, actor: Actor<'_>, p: &Params) -> Result<usize, Fail> {
    // Array(params[:ids]).first(BULK_LIMIT)
    let given: Vec<Value> = match p.get("ids") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Object(o)) => o.iter().map(|(k, v)| json!([k, v])).collect(),
        Some(v) => vec![v.clone()],
    };
    let given: Vec<Value> = given.into_iter().take(BULK_LIMIT).collect();
    let ids: Vec<i64> = given.iter().filter_map(ar_int).collect();
    let action = p.s("action_name");
    if action == "merge" {
        bulk_merge(c, account, &ids, actor)?;
        return Ok(given.len());
    }
    if action == "priority" && !PRIORITIES.contains(&p.s("value").as_str()) {
        // update!(priority:) raises on the first issue; nothing is saved.
        if !scope_ids(c, account, &ids, "i.id")?.is_empty() {
            return Err(Fail::Invalid);
        }
    }
    // find_each: primary-key order.
    for id in scope_ids(c, account, &ids, "i.id")? {
        let Some(i) = issue::find(c, id)? else { continue };
        match action.as_str() {
            "resolve" => {
                let deploy = latest_deploy(c, i.environment_id)?;
                issue::resolve_by(c, &i, deploy.as_deref(), Some(actor))?;
            }
            "ignore" => {
                issue::ignore(c, &i, Some(actor))?;
            }
            "priority" => {
                issue::set_priority(c, &i, &p.s("value"), Some(actor))?;
            }
            _ => {}
        }
    }
    Ok(given.len())
}

/// bulk_merge: a stale selection may hold issues merged since the list
/// rendered; skip those, fold the rest into the earliest open one, all or
/// nothing.
fn bulk_merge(c: &Connection, account: i64, ids: &[i64], actor: Actor<'_>) -> Result<(), Fail> {
    let ordered = scope_ids(c, account, ids, "i.first_seen_at, i.id")?;
    let issues: Vec<issue::Issue> = ordered.into_iter().filter_map(|id| issue::find(c, id).transpose()).collect::<rusqlite::Result<_>>()?;
    let Some(target) = issues.iter().find(|i| i.status == "open").cloned() else { return Ok(()) };
    for i in &issues {
        if i.id == target.id || i.environment_id != target.environment_id || i.status == "merged" {
            continue;
        }
        issue::merge_into(c, i.id, target.id, Some(actor))?;
    }
    Ok(())
}

/// Comment.create!(user:, body:) and its after_create :record_activity.
pub fn create_comment(c: &Connection, account: i64, user_id: i64, id: i64, body: &str) -> Result<(), Fail> {
    let Some(i) = find_in_account(c, account, id)? else { return Err(Fail::NotFound) };
    let now = issue::now_db();
    c.execute(
        "INSERT INTO comments (body, created_at, issue_id, source, updated_at, user_id) VALUES (?, ?, ?, 'railwatch', ?, ?)",
        params![body, now, i.id, now, user_id],
    )?;
    let comment_id = c.last_insert_rowid();
    issue::add_activity_by(
        c,
        i.id,
        "comment",
        Some(user_id),
        crate::rhash! { "comment_id" => comment_id, "body" => body, "source" => "railwatch", "author_name" => Option::<String>::None },
    )?;
    Ok(())
}

/// `environment.with_telemetry { Exception.where(group_hash:).recent.limit(SPLIT_SAMPLE) }`.
async fn split_rows(state: &Arc<WebState>, slug: &str, group_hash: &str) -> rusqlite::Result<Vec<crate::grouping::ExceptionRow>> {
    let state = state.clone();
    let (slug, group_hash) = (slug.to_string(), group_hash.to_string());
    tokio::task::spawn_blocking(move || {
        if state.tenants.layout(&slug).is_none() {
            return Ok(vec![]);
        }
        let reader = state.readers.get(&slug)?;
        let sql = format!(
            "SELECT {} FROM exceptions WHERE group_hash = ? ORDER BY occurred_at DESC LIMIT {SPLIT_SAMPLE}",
            crate::grouping::EXCEPTION_COLUMNS
        );
        let mut s = reader.prepare(&sql)?;
        s.query_map([group_hash], crate::grouping::exception_row)?.collect()
    })
    .await
    .expect("split task panicked")
}

// ---------------------------------------------------------------------------
// Reads

/// `Issue.joins(:application).where(applications: {account_id:}).find(id)`.
fn find_in_account(c: &Connection, account: i64, id: i64) -> rusqlite::Result<Option<issue::Issue>> {
    let ok: bool = c
        .query_row("SELECT EXISTS (SELECT 1 FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE ap.account_id = ? AND i.id = ?)", params![account, id], |r| {
            r.get(0)
        })?;
    if ok { issue::find(c, id) } else { Ok(None) }
}

/// The account's issues among `ids`, ordered by `order`.
fn scope_ids(c: &Connection, account: i64, ids: &[i64], order: &str) -> rusqlite::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let ph = vec!["?"; ids.len()].join(",");
    let sql = format!("SELECT i.id FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE ap.account_id = {account} AND i.id IN ({ph}) ORDER BY {order}");
    let mut s = c.prepare(&sql)?;
    s.query_map(params_from_iter(ids), |r| r.get(0))?.collect()
}

/// `environment.deploys.recent.first&.deploy`
fn latest_deploy(c: &Connection, environment_id: i64) -> rusqlite::Result<Option<String>> {
    c.query_row("SELECT deploy FROM deploys WHERE environment_id = ? ORDER BY deployed_at DESC, id DESC LIMIT 1", [environment_id], |r| r.get(0)).optional()
}

/// `Current.account.users.find(id)`: (id, name) of a member, or None.
fn member(c: &Connection, account: i64, id: Option<i64>) -> rusqlite::Result<Option<(i64, String)>> {
    let Some(id) = id else { return Ok(None) };
    c.query_row("SELECT u.id, u.name FROM users u JOIN memberships m ON m.user_id = u.id WHERE m.account_id = ? AND u.id = ?", params![account, id], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
}

/// `Current.account.users.map { {id:, name:} }`
fn members(c: &Connection, account: i64) -> rusqlite::Result<Vec<Value>> {
    c.prepare_cached("SELECT u.id, u.name FROM users u JOIN memberships m ON m.user_id = u.id WHERE m.account_id = ? ORDER BY m.id")?
        .query_map([account], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?})))?
        .collect()
}

/// `.present?` of a param value.
fn value_present(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => rb::present(Some(s)),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Bool(b) => *b,
        Value::Number(_) => true,
    }
}

/// ActiveModel::Type::Integer's cast of a param: numbers as is, strings
/// starting (after whitespace) with an optional sign and a digit through
/// String#to_i, anything else nil.
fn ar_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => {
            let t = s.trim_start();
            let t = t.strip_prefix(['+', '-']).unwrap_or(t);
            if !t.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            match crate::ingest::batch::string_to_i(s.as_bytes()) {
                crate::rjson::Value::Int(i) => Some(i),
                _ => None,
            }
        }
        _ => None,
    }
}

const ROW_SQL: &str = "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title, i.kind, i.status, i.priority, i.culprit, i.occurrences, i.affected_users,
        i.first_seen_at, i.last_seen_at, u.id, u.name, i.sample, i.source, ap.id, ap.name, e.id, e.name
     FROM issues i JOIN applications ap ON ap.id = i.application_id JOIN environments e ON e.id = i.environment_id LEFT JOIN users u ON u.id = i.assignee_id";

fn parse_sample(s: &str) -> Value {
    serde_json::from_str::<Value>(s).ok().filter(Value::is_object).unwrap_or_else(|| Value::Object(Map::new()))
}

/// IssuesController#row.
fn row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let sample = parse_sample(&r.get::<_, String>(13)?);
    let assignee = match r.get::<_, Option<i64>>(11)? {
        Some(id) => json!({"id": id, "name": r.get::<_, String>(12)?}),
        None => Value::Null,
    };
    Ok(json!({
        "id": r.get::<_, i64>(0)?, "key": r.get::<_, String>(1)?, "title": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
        "status": r.get::<_, String>(4)?, "priority": r.get::<_, String>(5)?, "culprit": r.get::<_, Option<String>>(6)?,
        "occurrences": r.get::<_, i64>(7)?, "affected_users": r.get::<_, i64>(8)?,
        "first_seen_at": t_json(&r.get::<_, String>(9)?), "last_seen_at": t_json(&r.get::<_, String>(10)?),
        "assignee": assignee, "fingerprint_source": sample.get("fingerprint_source").cloned().unwrap_or(Value::Null),
        "source": r.get::<_, Option<String>>(14)?,
        "application": {"id": r.get::<_, i64>(15)?, "name": r.get::<_, String>(16)?},
        "environment": {"id": r.get::<_, i64>(17)?, "name": r.get::<_, String>(18)?},
    }))
}

/// `title LIKE ?` with sanitize_sql_like; the SQL carries ESCAPE '\', as
/// Rails' now does (without it SQLite reads the backslashes literally).
fn like(s: &str) -> String {
    format!("%{}%", rb::sanitize_sql_like(s))
}

/// IssuesController#index's props.
pub fn index_props(c: &Connection, account: i64, user_id: i64, p: &Params) -> rusqlite::Result<Value> {
    let mut wh = vec!["ap.account_id = ?".to_string()];
    let mut binds: Vec<rusqlite::types::Value> = vec![account.into()];
    let status = p.s("status");
    if STATUSES.contains(&status.as_str()) {
        wh.push("i.status = ?".into());
        binds.push(status.into());
    }
    if !p.has("status") {
        wh.push("i.status = 'open'".into());
    }
    for (key, col) in [("application_id", "i.application_id"), ("environment_id", "i.environment_id")] {
        if let Some(v) = p.get(key).filter(|v| value_present(v)) {
            match ar_int(v) {
                Some(id) => {
                    wh.push(format!("{col} = ?"));
                    binds.push(id.into());
                }
                None => wh.push(format!("{col} IS NULL")),
            }
        }
    }
    let kind = p.s("kind");
    if KINDS.contains(&kind.as_str()) {
        wh.push("i.kind = ?".into());
        binds.push(kind.into());
    }
    if p.s("mine") == "1" {
        wh.push("i.assignee_id = ?".into());
        binds.push(user_id.into());
    }
    let q = p.get("q").and_then(Value::as_str).map(str::to_string);
    let parsed = crate::dashboard::filter_query::parse(q.as_deref());
    if let Some(source) = parsed.field("source") {
        wh.push("i.source = ?".into());
        binds.push(source.to_string().into());
    }
    if rb::present(Some(&parsed.text)) {
        wh.push("i.title LIKE ? ESCAPE '\\'".into());
        binds.push(like(&parsed.text).into());
    }
    let sql = format!("{ROW_SQL} WHERE {} ORDER BY i.last_seen_at DESC LIMIT 200", wh.join(" AND "));
    let issues: Vec<Value> = c.prepare(&sql)?.query_map(params_from_iter(binds), row_json)?.collect::<rusqlite::Result<_>>()?;
    // params.permit(:status, :application_id, :environment_id, :kind, :mine, :q).to_h
    let permitted = ["status", "application_id", "environment_id", "kind", "mine", "q"];
    let filters: Map<String, Value> =
        p.0.iter().filter(|(k, v)| permitted.contains(&k.as_str()) && !v.is_array() && !v.is_object()).map(|(k, v)| (k.clone(), v.clone())).collect();
    let grouped = |sql: &str| -> rusqlite::Result<Map<String, Value>> {
        c.prepare_cached(sql)?.query_map([account], |r| Ok((r.get::<_, String>(0)?, Value::from(r.get::<_, i64>(1)?))))?.collect()
    };
    let counts = grouped("SELECT i.status, COUNT(*) FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE ap.account_id = ? GROUP BY i.status")?;
    let kind_counts =
        grouped("SELECT i.kind, COUNT(*) FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE ap.account_id = ? AND i.status = 'open' GROUP BY i.kind")?;
    Ok(json!({
        "issues": issues,
        "filters": filters,
        "counts": counts,
        "kindCounts": kind_counts,
        "members": members(c, account)?,
    }))
}

/// What issues#show and #update need of the issue before rendering.
pub struct Shown {
    pub id: i64,
    pub key: String,
    pub kind: String,
    pub application_id: i64,
    pub environment_id: i64,
    pub environment_slug: String,
    pub group_hash: String,
    row: Value,
    title: String,
    status: String,
    priority: String,
    occurrences: i64,
    affected_users: i64,
    culprit: Option<String>,
    first_seen_at: String,
    last_seen_at: String,
    resolved_at: Option<String>,
    resolved_in_deploy: Option<String>,
    regressed_at: Option<String>,
    merged_into_id: Option<i64>,
    sample: Value,
}

impl Shown {
    /// The dashboard's control::Issue that issue_telemetry reads.
    fn control_issue(&self) -> crate::dashboard::Issue {
        crate::dashboard::Issue {
            id: self.id,
            key: self.key.clone(),
            title: self.title.clone(),
            kind: self.kind.clone(),
            status: self.status.clone(),
            priority: self.priority.clone(),
            occurrences: self.occurrences,
            affected_users: self.affected_users,
            culprit: self.culprit.clone(),
            group_hash: self.group_hash.clone(),
            first_seen_at: crate::time::parse_db_time(&self.first_seen_at).unwrap_or(0),
            last_seen_at: crate::time::parse_db_time(&self.last_seen_at).unwrap_or(0),
            resolved_in_deploy: self.resolved_in_deploy.clone(),
            sample: self.sample.as_object().cloned().unwrap_or_default(),
        }
    }
}

/// set_issue, with what the page needs.
pub fn find_shown(c: &Connection, account: i64, id: i64) -> rusqlite::Result<Option<Shown>> {
    let row = c.query_row(&format!("{ROW_SQL} WHERE ap.account_id = ? AND i.id = ?"), params![account, id], row_json).optional()?;
    let Some(row) = row else { return Ok(None) };
    c.query_row(
        "SELECT i.kind, i.application_id, i.environment_id, e.slug, i.group_hash, i.title, i.status, i.priority, i.occurrences, i.affected_users, i.culprit,
                i.first_seen_at, i.last_seen_at, i.resolved_at, i.resolved_in_deploy, i.regressed_at, i.merged_into_id, i.sample, ap.issue_prefix || '-' || i.number
         FROM issues i JOIN environments e ON e.id = i.environment_id JOIN applications ap ON ap.id = i.application_id WHERE i.id = ?",
        [id],
        |r| {
            Ok(Shown {
                id,
                kind: r.get(0)?,
                application_id: r.get(1)?,
                environment_id: r.get(2)?,
                environment_slug: r.get(3)?,
                group_hash: r.get(4)?,
                title: r.get(5)?,
                status: r.get(6)?,
                priority: r.get(7)?,
                occurrences: r.get(8)?,
                affected_users: r.get(9)?,
                culprit: r.get(10)?,
                first_seen_at: r.get(11)?,
                last_seen_at: r.get(12)?,
                resolved_at: r.get(13)?,
                resolved_in_deploy: r.get(14)?,
                regressed_at: r.get(15)?,
                merged_into_id: r.get(16)?,
                sample: parse_sample(&r.get::<_, String>(17)?),
                key: r.get(18)?,
                row: row.clone(),
            })
        },
    )
    .optional()
}

/// issues#show's props. `telemetry` is issue_telemetry's object for an
/// exception issue, None for render_detection_issue, which renders
/// `detection` (IssueDetectionPresenter#as_json).
pub fn show_props(c: &Connection, account: i64, s: &Shown, telemetry: Option<Value>, detection: Value) -> rusqlite::Result<Value> {
    let mut issue = s.row.as_object().cloned().unwrap_or_default();
    issue.insert("first_seen_at".into(), Value::String(t_json(&s.first_seen_at)));
    issue.insert("resolved_at".into(), t_json_opt(s.resolved_at.clone()));
    issue.insert("resolved_in_deploy".into(), json!(s.resolved_in_deploy));
    issue.insert("regressed_at".into(), t_json_opt(s.regressed_at.clone()));
    issue.insert("sample".into(), s.sample.clone());
    let mut props = Map::new();
    props.insert("issue".into(), Value::Object(issue));
    let exception = telemetry.is_some();
    match telemetry {
        Some(Value::Object(t)) => {
            props.insert("detection".into(), Value::Null);
            props.extend(t);
        }
        _ => {
            props.insert("detection".into(), detection);
            for (k, v) in [
                ("latest", Value::Null),
                ("breadcrumbs", json!([])),
                ("browser_breadcrumbs", json!([])),
                ("fingerprint", json!([])),
                ("fingerprint_source", Value::Null),
                ("distinct_messages", json!(0)),
                ("top_messages", json!([])),
                ("occurrences", json!([])),
                ("daily", json!([])),
                ("by_deploy", json!({})),
                ("by_tenant", json!({})),
                ("attachments", json!([])),
            ] {
                props.insert(k.into(), v);
            }
        }
    }
    let comments: Vec<Value> = c
        .prepare_cached(
            "SELECT c.id, c.body, u.name, c.author_name, c.source, c.created_at FROM comments c LEFT JOIN users u ON u.id = c.user_id WHERE c.issue_id = ? ORDER BY c.created_at, c.id",
        )?
        .query_map([s.id], |r| {
            let user = r.get::<_, Option<String>>(2)?.or(r.get::<_, Option<String>>(3)?).unwrap_or_else(|| "Linear".into());
            let (id, body, created) = (r.get::<_, i64>(0)?, r.get::<_, String>(1)?, t_json(&r.get::<_, String>(5)?));
            Ok(if exception {
                json!({"id": id, "body": body, "user": user, "source": r.get::<_, String>(4)?, "created_at": created})
            } else {
                json!({"id": id, "body": body, "user": user, "created_at": created})
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let activities: Vec<Value> = c
        .prepare_cached(
            "SELECT a.id, a.kind, a.data, u.id, u.name, a.created_at FROM issue_activities a LEFT JOIN users u ON u.id = a.user_id WHERE a.issue_id = ? ORDER BY a.created_at, a.id",
        )?
        .query_map([s.id], |r| {
            let user = match r.get::<_, Option<i64>>(3)? {
                Some(id) => json!({"id": id, "name": r.get::<_, String>(4)?}),
                None => Value::Null,
            };
            Ok(json!({"id": r.get::<_, i64>(0)?, "kind": r.get::<_, String>(1)?, "data": parse_sample(&r.get::<_, String>(2)?),
                      "user": user, "created_at": t_json(&r.get::<_, String>(5)?)}))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let deploys: Vec<Value> = c
        .prepare_cached("SELECT id, deploy, ref, deployed_at FROM deploys WHERE environment_id = ? ORDER BY deployed_at DESC, id DESC LIMIT 20")?
        .query_map([s.environment_id], |r| {
            let at: String = r.get(3)?;
            let d = crate::dashboard::Deploy {
                id: r.get(0)?,
                deploy: r.get(1)?,
                ref_: r.get(2)?,
                name: None,
                url: None,
                server: None,
                deployed_at: 0,
                previous_ref: None,
                commits: vec![],
                detail: Default::default(),
            };
            Ok(json!({"deploy": d.deploy, "ref": d.short_ref(), "at": t_json(&at)}))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let brief = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
        Ok(json!({"id": r.get::<_, i64>(0)?, "key": r.get::<_, String>(1)?, "title": r.get::<_, String>(2)?}))
    };
    let merged_into = match s.merged_into_id {
        None => Value::Null,
        Some(t) => c
            .query_row("SELECT i.id, ap.issue_prefix || '-' || i.number, i.title FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE i.id = ?", [t], brief)
            .optional()?
            .unwrap_or(Value::Null),
    };
    let merged_issues: Vec<Value> = c
        .prepare_cached("SELECT i.id, ap.issue_prefix || '-' || i.number, i.title FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE i.merged_into_id = ? ORDER BY i.id")?
        .query_map([s.id], brief)?
        .collect::<rusqlite::Result<_>>()?;
    let alerts: Vec<Value> = c
        .prepare_cached(
            "SELECT a.id, a.event, a.status, a.sent_at, a.error, n.kind, n.name FROM alerts a JOIN integrations n ON n.id = a.integration_id WHERE a.issue_id = ? ORDER BY a.created_at DESC, a.id ASC",
        )?
        .query_map([s.id], |r| {
            Ok(json!({"id": r.get::<_, i64>(0)?, "event": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?, "sent_at": t_json_opt(r.get(3)?),
                      "error": r.get::<_, Option<String>>(4)?, "integration": {"kind": r.get::<_, String>(5)?, "name": r.get::<_, String>(6)?}}))
        })?
        .collect::<rusqlite::Result<_>>()?;
    props.insert("comments".into(), Value::Array(comments));
    props.insert("activities".into(), Value::Array(activities));
    props.insert("members".into(), Value::Array(members(c, account)?));
    props.insert("deploys".into(), Value::Array(deploys));
    props.insert("related_issues".into(), Value::Array(related_issues(c, s)?));
    props.insert("merged_into".into(), merged_into);
    props.insert("merged_issues".into(), Value::Array(merged_issues));
    props.insert("alerts".into(), Value::Array(alerts));
    Ok(Value::Object(props))
}

/// related_issues: the same "ClassName:" title prefix in the application's
/// other issues (any environment), 20 of them.
fn related_issues(c: &Connection, s: &Shown) -> rusqlite::Result<Vec<Value>> {
    let class_name = s.title.split(':').next().unwrap_or("");
    if !rb::present(Some(class_name)) {
        return Ok(vec![]);
    }
    c.prepare_cached(
        "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title, i.status, e.id, e.name FROM issues i
         JOIN applications ap ON ap.id = i.application_id JOIN environments e ON e.id = i.environment_id
         WHERE ap.id = ? AND i.id != ? AND i.title LIKE ? ESCAPE '\\' ORDER BY i.id LIMIT 20",
    )?
    .query_map(params![s.application_id, s.id, format!("{}:%", rb::sanitize_sql_like(class_name))], |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "key": r.get::<_, String>(1)?, "title": r.get::<_, String>(2)?, "status": r.get::<_, String>(3)?,
                  "environment": {"id": r.get::<_, i64>(4)?, "name": r.get::<_, String>(5)?}}))
    })?
    .collect()
}

/// IssuesController#merge_candidates: the environment's other open issues,
/// by title or by key ("SHOP-12" matches number 12). None when the issue is
/// not the account's.
pub fn merge_candidates_json(c: &Connection, account: i64, id: i64, q: &str) -> rusqlite::Result<Option<Value>> {
    let Some(i) = find_in_account(c, account, id)? else { return Ok(None) };
    let prefix: String = c.query_row("SELECT issue_prefix FROM applications WHERE id = ?", [i.application_id], |r| r.get(0))?;
    // params[:q].to_s.strip.first(200)
    let query = rb::first_chars(q.trim_matches(|ch: char| ch.is_whitespace() || ch == '\0'), 200).to_string();
    let mut sql = "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title FROM issues i JOIN applications ap ON ap.id = i.application_id
                   WHERE i.environment_id = ? AND i.status = 'open' AND i.id != ?"
        .to_string();
    let mut binds: Vec<rusqlite::types::Value> = vec![i.environment_id.into(), i.id.into()];
    if rb::present(Some(&query)) {
        let number = query
            .to_uppercase()
            .strip_prefix(&format!("{prefix}-"))
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<i64>().ok());
        match number {
            Some(n) => {
                sql.push_str(" AND (i.title LIKE ? ESCAPE '\\' OR i.number = ?)");
                binds.push(like(&query).into());
                binds.push(n.into());
            }
            None => {
                sql.push_str(" AND i.title LIKE ? ESCAPE '\\'");
                binds.push(like(&query).into());
            }
        }
    }
    sql.push_str(" ORDER BY i.last_seen_at DESC, i.id DESC LIMIT 20");
    let issues: Vec<Value> = c
        .prepare(&sql)?
        .query_map(params_from_iter(binds), |r| Ok(json!({"id": r.get::<_, i64>(0)?, "key": r.get::<_, String>(1)?, "title": r.get::<_, String>(2)?})))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(json!({"issues": issues})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable(s: &Shown) -> Value {
        crate::detection::presenter::unavailable_json(&crate::detection::presenter::Presented { kind: &s.kind, occurrences: s.occurrences, sample: &s.sample })
    }
    use crate::control::{create_account, create_environment, insert_application, insert_user};

    struct Fixture {
        dir: std::path::PathBuf,
        c: Control,
        ada: i64,
        account: i64,
        app: i64,
        env: i64,
        other_app: i64,
        other_env: i64,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("rw-issues-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", "correct horse battery", true)).unwrap();
        let eve = c.tx(|tx| insert_user(tx, "Eve", "eve@example.com", "correct horse battery", true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let other = c.tx(|tx| create_account(tx, "Other", eve)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let other_app = c.tx(|tx| insert_application(tx, other, "Else", "else", "ELSE", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let (other_env, _) = c.tx(|tx| create_environment(tx, other_app, "production")).unwrap();
        Fixture { dir, c, ada, account, app, env, other_app, other_env }
    }

    /// An issue row as grouping leaves it; returns its id.
    #[allow(clippy::too_many_arguments)]
    fn issue_row(c: &Connection, app: i64, env: i64, n: i64, kind: &str, status: &str, title: &str, occurrences: i64, seen: &str) -> i64 {
        c.execute(
            "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users, first_seen_at, last_seen_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, printf('%032d', ?2 * 1000 + ?3), ?7, ?7, '2026-10-14 03:00:00', '2026-10-14 ' || ?8, '2026-10-14 03:00:00', '2026-10-14 03:00:00')",
            params![app, env, n, title, kind, status, occurrences, seen],
        )
        .unwrap();
        let id = c.last_insert_rowid();
        // Application#next_issue_number! counts from issues_count, as grouping's inserts keep it.
        c.execute("UPDATE applications SET issues_count = MAX(issues_count, ?) WHERE id = ?", params![n, app]).unwrap();
        id
    }

    fn p(v: Value) -> Params {
        Params(v.as_object().cloned().unwrap())
    }

    fn ada(f: &Fixture) -> Actor<'_> {
        Actor { user_id: f.ada, name: "Ada" }
    }

    fn update(f: &Fixture, id: i64, params: Value) -> Result<String, Fail> {
        write(&f.c, |tx| apply_update(tx, f.account, ada(f), id, &p(params), &[]))
    }

    fn keys(v: &Value) -> Vec<String> {
        v["issues"].as_array().unwrap().iter().map(|i| i["key"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn index_lists_only_the_accounts_issues_open_by_default_with_status_and_kind_counts() {
        let f = fixture("index");
        let x = f.c.conn();
        issue_row(&x, f.app, f.env, 1, "exception", "open", "NoMethodError: a", 3, "03:01:00");
        issue_row(&x, f.app, f.env, 2, "performance", "open", "Slow GET /", 1, "03:05:00");
        issue_row(&x, f.app, f.env, 3, "exception", "resolved", "KeyError: b", 1, "03:09:00");
        issue_row(&x, f.other_app, f.other_env, 1, "exception", "open", "NoMethodError: theirs", 9, "03:10:00");
        let props = index_props(&x, f.account, f.ada, &p(json!({}))).unwrap();
        assert_eq!(keys(&props), ["SHOP-2", "SHOP-1"], "open only, never another account's, most recently seen first");
        assert_eq!(props["counts"], json!({"open": 2, "resolved": 1}), "counts cover every status of this account only");
        assert_eq!(props["kindCounts"], json!({"exception": 1, "performance": 1}), "kindCounts count open issues only");
        assert_eq!(props["filters"], json!({}), "no params, no filters echoed");
        assert_eq!(props["members"], json!([{"id": f.ada, "name": "Ada"}]));
        let row = &props["issues"][1];
        assert_eq!((row["application"]["name"].as_str(), row["environment"]["name"].as_str()), (Some("Shop"), Some("production")));
        assert_eq!(row["last_seen_at"], "2026-10-14T03:01:00.000Z", "times render like as_json");

        // An explicitly empty status shows every status; a known one filters.
        assert_eq!(keys(&index_props(&x, f.account, f.ada, &p(json!({"status": ""}))).unwrap()), ["SHOP-3", "SHOP-2", "SHOP-1"]);
        assert_eq!(keys(&index_props(&x, f.account, f.ada, &p(json!({"status": "resolved"}))).unwrap()), ["SHOP-3"]);
        // kind, free text, and the permitted-keys echo.
        let q = index_props(&x, f.account, f.ada, &p(json!({"kind": "exception", "q": "nomethod", "page": "2"}))).unwrap();
        assert_eq!(keys(&q), ["SHOP-1"]);
        assert_eq!(q["filters"], json!({"kind": "exception", "q": "nomethod"}), "only permitted keys are echoed");
        // LIKE wildcards in the text are literal.
        assert!(keys(&index_props(&x, f.account, f.ada, &p(json!({"q": "%"}))).unwrap()).is_empty());
        // mine=1: assigned to the current user.
        x.execute("UPDATE issues SET assignee_id = ? WHERE number = 2 AND application_id = ?", params![f.ada, f.app]).unwrap();
        assert_eq!(keys(&index_props(&x, f.account, f.ada, &p(json!({"mine": "1"}))).unwrap()), ["SHOP-2"]);
    }

    #[test]
    fn show_is_scoped_to_the_account_and_detection_issues_render_the_unavailable_contract() {
        let f = fixture("show");
        let x = f.c.conn();
        let perf = issue_row(&x, f.app, f.env, 1, "performance", "open", "Slow GET /", 4, "03:01:00");
        let theirs = issue_row(&x, f.other_app, f.other_env, 1, "exception", "open", "Boom: x", 1, "03:01:00");
        assert!(find_shown(&x, f.account, theirs).unwrap().is_none(), "another account's issue is a 404");
        let shown = find_shown(&x, f.account, perf).unwrap().unwrap();
        let props = show_props(&x, f.account, &shown, None, unavailable(&shown)).unwrap();
        assert_eq!(
            props["detection"],
            json!({"kind": "performance", "available": false, "count_label": "Breached evaluation windows", "breached_windows": 4,
                   "message": "Typed detector context was not captured for this historical issue."})
        );
        let order: Vec<&str> = props.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            order,
            [
                "issue", "detection", "latest", "breadcrumbs", "browser_breadcrumbs", "fingerprint", "fingerprint_source", "distinct_messages", "top_messages",
                "occurrences", "daily", "by_deploy", "by_tenant", "attachments", "comments", "activities", "members", "deploys", "related_issues",
                "merged_into", "merged_issues", "alerts"
            ],
            "render_detection_issue's key order"
        );
        let issue_keys: Vec<&str> = props["issue"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(issue_keys.last(), Some(&"sample"), "row merged with first_seen_at/resolved_at/resolved_in_deploy/regressed_at/sample");
    }

    #[test]
    fn merge_folds_counts_into_the_target_and_unmerge_gives_them_back() {
        let f = fixture("merge");
        let x = f.c.conn();
        let a = issue_row(&x, f.app, f.env, 1, "exception", "open", "Boom: a", 3, "03:01:00");
        let b = issue_row(&x, f.app, f.env, 2, "exception", "open", "Boom: b", 5, "03:05:00");
        let c3 = issue_row(&x, f.app, f.env, 3, "exception", "open", "Boom: c", 2, "03:06:00");
        drop(x);
        create_comment(&f.c.conn(), f.account, f.ada, a, "moved along").unwrap();
        assert_eq!(update(&f, a, json!({"action_name": "merge", "target_id": b.to_string()})).unwrap(), "Issue updated");
        let x = f.c.conn();
        let occ = |id: i64| -> i64 { x.query_row("SELECT occurrences FROM issues WHERE id = ?", [id], |r| r.get(0)).unwrap() };
        assert_eq!(occ(b), 8, "target absorbs the source's occurrences");
        let (st, into): (String, Option<i64>) = x.query_row("SELECT status, merged_into_id FROM issues WHERE id = ?", [a], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((st.as_str(), into), ("merged", Some(b)));
        let moved: i64 = x.query_row("SELECT COUNT(*) FROM comments WHERE issue_id = ?", [b], |r| r.get(0)).unwrap();
        assert_eq!(moved, 1, "comments move to the target");
        let absorbed: String = x.query_row("SELECT data FROM issue_activities WHERE issue_id = ? AND kind = 'absorbed'", [b], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&absorbed).unwrap(), json!({"source_id": a, "source_key": "SHOP-1", "occurrences": 3}));
        drop(x);

        // Refusals carry Issue::InvalidMerge's messages and change nothing.
        let msg = |r: Result<String, Fail>| match r {
            Err(Fail::Merge(m)) => m,
            other => panic!("expected InvalidMerge, got {other:?}"),
        };
        assert_eq!(msg(update(&f, c3, json!({"action_name": "merge", "target_id": c3}))), "cannot merge an issue into itself");
        assert_eq!(msg(update(&f, a, json!({"action_name": "merge", "target_id": c3}))), "cannot merge an issue that is already merged");
        assert!(matches!(update(&f, c3, json!({"action_name": "merge", "target_id": a})), Err(Fail::NotFound)), "a merged target is not among the open issues: 404");
        assert_eq!(msg(update(&f, c3, json!({"action_name": "unmerge"}))), "cannot unmerge an issue that is not merged");

        // Chain: b -> c3, then a's unmerge must wait for b's.
        update(&f, b, json!({"action_name": "merge", "target_id": c3})).unwrap();
        assert_eq!(f.c.conn().query_row("SELECT occurrences FROM issues WHERE id = ?", [c3], |r| r.get::<_, i64>(0)).unwrap(), 10);
        assert_eq!(msg(update(&f, a, json!({"action_name": "unmerge"}))), "cannot unmerge this issue yet; unmerge SHOP-2 first (top-down order)");
        update(&f, b, json!({"action_name": "unmerge"})).unwrap();
        update(&f, a, json!({"action_name": "unmerge"})).unwrap();
        let x = f.c.conn();
        let occ = |id: i64| -> i64 { x.query_row("SELECT occurrences FROM issues WHERE id = ?", [id], |r| r.get(0)).unwrap() };
        assert_eq!((occ(a), occ(b), occ(c3)), (3, 5, 2), "unmerging top-down folds every count back");
        let open: i64 = x.query_row("SELECT COUNT(*) FROM issues WHERE status = 'open' AND merged_into_id IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(open, 3);
    }

    #[test]
    fn resolve_via_update_records_the_status_change_and_fires_resolved_issue_with_the_actor() {
        let f = fixture("resolve");
        let x = f.c.conn();
        let id = issue_row(&x, f.app, f.env, 1, "exception", "open", "Boom: a", 1, "03:01:00");
        x.execute(
            "INSERT INTO integrations (account_id, created_at, enabled, kind, name, settings, updated_at) VALUES (?, '2026-01-01', 1, 'webhook', 'hook', '{}', '2026-01-01')",
            [f.account],
        )
        .unwrap();
        let integ = x.last_insert_rowid();
        for (event, filters) in [("resolved_issue", "{}"), ("resolved_issue", r#"{"kinds":["performance"]}"#), ("ignored_issue", "{}")] {
            x.execute(
                "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?, '2026-01-01', ?, ?, ?, '2026-01-01')",
                params![f.app, event, filters, integ],
            )
            .unwrap();
        }
        x.execute(
            "INSERT INTO deploys (created_at, deploy, deployed_at, environment_id, ref, updated_at) VALUES ('2026-10-14', 'v41', '2026-10-14 01:00:00', ?1, 'aaa', '2026-10-14'),
                                                                                                         ('2026-10-14', 'v42', '2026-10-14 02:00:00', ?1, 'bbb', '2026-10-14')",
            [f.env],
        )
        .unwrap();
        drop(x);
        update(&f, id, json!({"action_name": "resolve"})).unwrap();
        let x = f.c.conn();
        let (status, deploy): (String, Option<String>) = x.query_row("SELECT status, resolved_in_deploy FROM issues WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((status.as_str(), deploy.as_deref()), ("resolved", Some("v42")), "no deploy param: the environment's most recent deploy");
        let (data, user): (String, Option<i64>) =
            x.query_row("SELECT data, user_id FROM issue_activities WHERE issue_id = ? AND kind = 'status'", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&data).unwrap(), json!({"from": "open", "to": "resolved"}));
        assert_eq!(user, Some(f.ada), "the status activity is the current user's");
        let alerts: Vec<(String, String)> =
            x.prepare("SELECT event, payload FROM alerts").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(alerts.len(), 1, "only the matching resolved_issue rule fires (the kinds filter and the ignored rule do not)");
        assert_eq!(alerts[0].0, "resolved_issue");
        let payload: Value = serde_json::from_str(&alerts[0].1).unwrap();
        assert_eq!(payload, json!({"issue_key": "SHOP-1", "title": "Boom: a", "environment": "production", "actor": "Ada"}));
        drop(x);
        // Resolving again changes no status: no second activity, no alert.
        update(&f, id, json!({"action_name": "resolve", "deploy": "v43"})).unwrap();
        let x = f.c.conn();
        let n: i64 = x.query_row("SELECT COUNT(*) FROM issue_activities WHERE issue_id = ? AND kind = 'status'", [id], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert_eq!(x.query_row("SELECT COUNT(*) FROM alerts", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn priority_and_assignee_changes_are_recorded_and_only_members_can_be_assigned() {
        let f = fixture("assign");
        let x = f.c.conn();
        let id = issue_row(&x, f.app, f.env, 1, "exception", "open", "Boom: a", 1, "03:01:00");
        let eve: i64 = x.query_row("SELECT id FROM users WHERE name = 'Eve'", [], |r| r.get(0)).unwrap();
        drop(x);
        assert!(matches!(update(&f, id, json!({"action_name": "assign", "assignee_id": eve})), Err(Fail::NotFound)), "Eve is not in Acme");
        update(&f, id, json!({"action_name": "assign_me"})).unwrap();
        update(&f, id, json!({"action_name": "assign", "assignee_id": ""})).unwrap();
        update(&f, id, json!({"action_name": "priority", "priority": "high"})).unwrap();
        assert!(matches!(update(&f, id, json!({"action_name": "priority", "priority": "extreme"})), Err(Fail::Invalid)));
        let x = f.c.conn();
        let acts: Vec<(String, String)> = x
            .prepare("SELECT kind, data FROM issue_activities WHERE issue_id = ? AND kind IN ('assignee', 'priority') ORDER BY id")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let acts: Vec<(String, Value)> = acts.into_iter().map(|(k, d)| (k, serde_json::from_str(&d).unwrap())).collect();
        assert_eq!(
            acts,
            [
                ("assignee".to_string(), json!({"from_id": null, "to_id": f.ada})),
                ("assignee".to_string(), json!({"from_id": f.ada, "to_id": null})),
                ("priority".to_string(), json!({"from": "normal", "to": "high"})),
            ]
        );
    }

    #[test]
    fn a_comment_is_stored_with_its_comment_activity() {
        let f = fixture("comment");
        let id = issue_row(&f.c.conn(), f.app, f.env, 1, "exception", "open", "Boom: a", 1, "03:01:00");
        let theirs = issue_row(&f.c.conn(), f.other_app, f.other_env, 1, "exception", "open", "Boom: b", 1, "03:01:00");
        assert!(matches!(write(&f.c, |tx| create_comment(tx, f.account, f.ada, theirs, "hi")), Err(Fail::NotFound)));
        write(&f.c, |tx| create_comment(tx, f.account, f.ada, id, "Looking into it")).unwrap();
        let x = f.c.conn();
        let (cid, source, user): (i64, String, i64) =
            x.query_row("SELECT id, source, user_id FROM comments WHERE issue_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((source.as_str(), user), ("railwatch", f.ada));
        let (data, auser): (String, i64) =
            x.query_row("SELECT data, user_id FROM issue_activities WHERE issue_id = ? AND kind = 'comment'", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&data).unwrap(), json!({"comment_id": cid, "body": "Looking into it", "source": "railwatch", "author_name": null}));
        assert_eq!(auser, f.ada);
        let shown = find_shown(&x, f.account, id).unwrap().unwrap();
        let props = show_props(&x, f.account, &shown, Some(json!({})), Value::Null).unwrap();
        assert_eq!(props["comments"][0]["user"], "Ada");
        assert_eq!(props["comments"][0]["source"], "railwatch", "exception issues' comments carry source");
    }

    #[test]
    fn bulk_acts_on_at_most_200_of_the_accounts_issues() {
        let f = fixture("bulk");
        let x = f.c.conn();
        let ids: Vec<i64> = (1..=250).map(|n| issue_row(&x, f.app, f.env, n, "exception", "open", "Boom: a", 1, "03:01:00")).collect();
        let theirs = issue_row(&x, f.other_app, f.other_env, 1, "exception", "open", "Boom: b", 1, "03:01:00");
        drop(x);
        let mut sent: Vec<Value> = vec![json!(theirs)];
        sent.extend(ids.iter().map(|i| json!(i.to_string())));
        let n = write(&f.c, |tx| apply_bulk(tx, f.account, ada(&f), &p(json!({"ids": sent, "action_name": "ignore"})))).unwrap();
        assert_eq!(n, BULK_LIMIT, "the notice counts the first 200 ids sent");
        let x = f.c.conn();
        let ignored: i64 = x.query_row("SELECT COUNT(*) FROM issues WHERE status = 'ignored'", [], |r| r.get(0)).unwrap();
        assert_eq!(ignored, 199, "199 of ours in the first 200 ids; the rest untouched");
        let their_status: String = x.query_row("SELECT status FROM issues WHERE id = ?", [theirs], |r| r.get(0)).unwrap();
        assert_eq!(their_status, "open", "another account's id in the list is ignored");
    }

    #[test]
    fn bulk_merge_folds_into_the_earliest_open_issue_and_skips_stale_merged_ones() {
        let f = fixture("bulkmerge");
        let x = f.c.conn();
        let a = issue_row(&x, f.app, f.env, 1, "exception", "open", "Boom: a", 2, "03:01:00");
        let b = issue_row(&x, f.app, f.env, 2, "exception", "open", "Boom: b", 3, "03:02:00");
        let c3 = issue_row(&x, f.app, f.env, 3, "exception", "open", "Boom: c", 4, "03:03:00");
        x.execute("UPDATE issues SET first_seen_at = '2026-10-13 00:00:00' WHERE id = ?", [b]).unwrap();
        drop(x);
        update(&f, c3, json!({"action_name": "merge", "target_id": a})).unwrap();
        write(&f.c, |tx| apply_bulk(tx, f.account, ada(&f), &p(json!({"ids": [a, b, c3], "action_name": "merge"})))).unwrap();
        let x = f.c.conn();
        let row = |id: i64| -> (String, i64) { x.query_row("SELECT status, occurrences FROM issues WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap() };
        assert_eq!(row(b), ("open".into(), 9), "b was first seen earliest, so it is the target: 3 + a's 2 + c's 4 already in a");
        assert_eq!(row(a).0, "merged");
        assert_eq!(x.query_row("SELECT merged_into_id FROM issues WHERE id = ?", [c3], |r| r.get::<_, i64>(0)).unwrap(), a, "the stale merged one is skipped");
    }

    #[test]
    fn split_by_message_opens_one_issue_per_new_message_and_moves_the_counts() {
        let f = fixture("split");
        let id = issue_row(&f.c.conn(), f.app, f.env, 1, "exception", "open", "Boom: x", 10, "03:01:00");
        let i = issue::find(&f.c.conn(), id).unwrap().unwrap();
        let ex = |eid: i64, msg: &str, at: i64| crate::grouping::ExceptionRow {
            id: eid,
            group_hash: Some(i.group_hash.clone()),
            class_name: "Boom".into(),
            message: msg.as_bytes().to_vec(),
            file: None,
            line: None,
            occurred_at: at,
            deploy: Some("v1".into()),
            user_ref: None,
            source: None,
            handled: false,
            execution_id: None,
            execution_preview: None,
            fingerprint: crate::rjson::Value::Nil,
            fingerprint_source: None,
        };
        let rows = vec![ex(3, "a", 3_000_000), ex(2, "b", 2_000_000), ex(1, "a", 1_000_000)];
        let notice = write(&f.c, |tx| apply_update(tx, f.account, ada(&f), id, &p(json!({"action_name": "split", "by": "message"})), &rows)).unwrap();
        assert_eq!(notice, "Split into 2 issue(s)");
        let x = f.c.conn();
        let made: Vec<(String, String, i64)> = x
            .prepare("SELECT group_hash, title, occurrences FROM issues WHERE id != ? ORDER BY id")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(made.len(), 2);
        assert_eq!(made[0], (issue::split_group_hash(&i.group_hash, b"a"), "Boom: a".to_string(), 2));
        assert_eq!(made[1].2, 1);
        assert_eq!(x.query_row("SELECT occurrences FROM issues WHERE id = ?", [id], |r| r.get::<_, i64>(0)).unwrap(), 7, "the source gives up what moved");
        drop(x);
        let again = write(&f.c, |tx| apply_update(tx, f.account, ada(&f), id, &p(json!({"action_name": "split", "by": "message"})), &rows)).unwrap();
        assert_eq!(again, "Nothing to split \u{2014} every recent occurrence is already in its own issue");
    }

    #[test]
    fn split_group_hash_is_md5_of_hash_unit_separator_message() {
        // Digest::MD5.hexdigest(["abc", "msg"].join("\x1f"))
        assert_eq!(issue::split_group_hash("abc", b"msg"), crate::dashboard::filter_query::md5_hex("abc\u{1f}msg"));
    }

    #[test]
    fn merge_candidates_match_title_or_key_among_the_environments_other_open_issues() {
        let f = fixture("candidates");
        let x = f.c.conn();
        let a = issue_row(&x, f.app, f.env, 1, "exception", "open", "Boom: a", 1, "03:01:00");
        issue_row(&x, f.app, f.env, 12, "exception", "open", "Other: z", 1, "03:02:00");
        issue_row(&x, f.app, f.env, 13, "exception", "resolved", "Boom: closed", 1, "03:03:00");
        issue_row(&x, f.app, f.env, 14, "exception", "open", "Boom: b", 1, "03:04:00");
        assert_eq!(keys(&merge_candidates_json(&x, f.account, a, "").unwrap().unwrap()), ["SHOP-14", "SHOP-12"]);
        assert_eq!(keys(&merge_candidates_json(&x, f.account, a, " boom ").unwrap().unwrap()), ["SHOP-14"]);
        assert_eq!(keys(&merge_candidates_json(&x, f.account, a, "shop-12").unwrap().unwrap()), ["SHOP-12"]);
        let theirs = issue_row(&x, f.other_app, f.other_env, 1, "exception", "open", "Boom", 1, "03:01:00");
        assert!(merge_candidates_json(&x, f.account, theirs, "").unwrap().is_none());
    }

    #[test]
    fn comment_body_is_required() {
        assert!(!required_present(&json!(null)) && !required_present(&json!("  ")) && !required_present(&json!([])));
        assert!(required_present(&json!("hi")) && required_present(&json!(false)));
    }
}
