//! The public JSON API, /api/v1 (railwatch-cloud @ 464f0fd: Api::BaseController,
//! Api::Concerns::EnvironmentScoped and every controller under Api::V1).
//!
//! Same bearer tokens as MCP (`mcp::Token`, ApiTokenAuthenticated): the
//! token needs `api` and `read`, the write actions also `write`; every read
//! is intersected with the owner's memberships and the token's account /
//! application restriction, and a miss is 404 `{"error":"not found"}`.
//!
//! Per request, in Rails' order: route (an unrouted path is Rails'
//! exceptions app: 404 page) -> body parse (malformed JSON: 400 page) ->
//! authenticate (401) -> scope (403) -> record_use! -> rate limit (600 a
//! minute per user, 429) -> the action's own before_actions -> action.
//! Responses get Rack::ETag's weak ETag and Cache-Control, a matching
//! If-None-Match becomes 304 (Rack::ConditionalGet), HEAD drops the body.
//!
//! Times render as `as_json` (ISO 8601, milliseconds, "Z"), not MCP's
//! Time#to_s. `call` is synchronous and takes `now`, so the diff harness
//! (bin/api_dump.rs) runs it at a frozen instant.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value, json};

use super::WebState;
use super::mcp::{Env, Token, present, ruby_to_i, to_s, with_tenant};
use super::proxy_auth::Claims;
use crate::dashboard::cursor_page::{self, Order};
use crate::dashboard::db::{self, jf, js, jt, qt};
use crate::dashboard::filter_query::{self, Resource};
use crate::dashboard::window::{self, Window};
use crate::dashboard::{aggregations, models, pages, rb};
use crate::grouping::issue as issue_model;
use crate::rate_limit::FixedWindow;

/// Api::BaseController::MAX_PER_PAGE
const MAX_PER_PAGE: i64 = 200;
/// `rate_limit to: 600, within: 1.minute, by: -> { current_user.id }`
pub const RATE_LIMIT: u64 = 600;
const SERIES_TYPES: [&str; 4] = ["request", "job_attempt", "query", "visit"];

pub fn rate_limit() -> FixedWindow<(&'static str, i64)> {
    FixedWindow::new(RATE_LIMIT)
}

/// Every /api path: the handler routes them itself (Rails' routes plus
/// the `.format` suffix and the trailing slash Rails accepts).
pub fn routes() -> Router<Arc<WebState>> {
    Router::new().route("/api/{*rest}", axum::routing::any(handle))
}

async fn handle(State(state): State<Arc<WebState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let proxied = match Token::proxied_claims(&parts) {
        Ok(p) => p,
        Err(refused) => return render(&parts.method, &parts.headers, StatusCode::FORBIDDEN, &refused).into_response(),
    };
    let Ok(bytes) = axum::body::to_bytes(body, 4 << 20).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], r#"{"error":"body too large"}"#).into_response();
    };
    let s = state.clone();
    let req = ApiRequest {
        method: parts.method.clone(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().map(str::to_string),
        headers: parts.headers.clone(),
        body: bytes,
    };
    tokio::task::spawn_blocking(move || {
        let env = Env { control: &s.control, tenants: &s.tenants, readers: &s.readers, cursor_key: &s.cursor_key };
        call_as(&env, &s.api_rate_limit, crate::time::now_micros(), &req, proxied.as_ref())
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "api task failed");
        error_page(StatusCode::INTERNAL_SERVER_ERROR, &HeaderMap::new(), "", false)
    })
    .into_response()
}

/// A finished response: status, headers and the exact body bytes.
pub struct ApiResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl IntoResponse for ApiResponse {
    fn into_response(self) -> Response {
        let mut r = (self.status, self.body).into_response();
        *r.headers_mut() = self.headers;
        r
    }
}

/// One request, as `call` needs it.
pub struct ApiRequest {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

// ---------------------------------------------------------------------------
// Routing (config/routes.rb, namespace :api / :v1)

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Me,
    Accounts,
    Applications,
    Application,
    Environment,
    Summary,
    Requests,
    RequestGroup,
    Execution,
    Jobs,
    Queries,
    Exceptions,
    Logs,
    Deploys,
    CreateDeploy,
    Releases,
    Users,
    Tenants,
    Spans,
    Series,
    Visits,
    Issues,
    Issue,
    UpdateIssue,
    CreateComment,
}

impl Action {
    /// The controller_path the rate limit is scoped to.
    fn controller(self) -> &'static str {
        use Action::*;
        match self {
            Me => "api/v1/me",
            Accounts => "api/v1/accounts",
            Applications | Application => "api/v1/applications",
            Environment => "api/v1/environments",
            Summary => "api/v1/environments/summaries",
            Requests | RequestGroup => "api/v1/environments/requests",
            Execution => "api/v1/environments/executions",
            Jobs => "api/v1/environments/jobs",
            Queries => "api/v1/environments/queries",
            Exceptions => "api/v1/environments/exceptions",
            Logs => "api/v1/environments/logs",
            Deploys | CreateDeploy => "api/v1/environments/deploys",
            Releases => "api/v1/environments/releases",
            Users => "api/v1/environments/users",
            Tenants => "api/v1/environments/tenants",
            Spans => "api/v1/environments/spans",
            Series => "api/v1/environments/series",
            Visits => "api/v1/environments/visits",
            Issues | Issue | UpdateIssue => "api/v1/issues",
            CreateComment => "api/v1/issue_comments",
        }
    }

    fn environment_scoped(self) -> bool {
        use Action::*;
        matches!(
            self,
            Summary
                | Requests
                | RequestGroup
                | Execution
                | Jobs
                | Queries
                | Exceptions
                | Logs
                | Deploys
                | CreateDeploy
                | Releases
                | Users
                | Tenants
                | Spans
                | Series
                | Visits
        )
    }
}

/// The route for `method path`, with its path parameters. Rails' segments
/// stop at "/" and "." (the optional `(.:format)`, itself without a dot),
/// repeated slashes and a trailing slash are ignored (Journey's
/// normalize_path); HEAD routes as GET.
/// (action, path parameters, `.format`)
type Routed = (Action, Vec<(&'static str, String)>, Option<String>);

fn route(method: &Method, path: &str) -> Option<Routed> {
    let mut squeezed = String::with_capacity(path.len());
    for c in path.chars() {
        if !(c == '/' && squeezed.ends_with('/')) {
            squeezed.push(c);
        }
    }
    let path = squeezed.strip_suffix('/').filter(|p| !p.is_empty()).unwrap_or(&squeezed);
    let rest = path.strip_prefix("/api/v1/")?;
    let mut segs: Vec<String> = rest.split('/').map(percent_decode).collect();
    // `(.:format)` on the last segment.
    let mut format = None;
    if let Some(last) = segs.last_mut()
        && let Some((base, fmt)) = last.split_once('.')
    {
        if base.is_empty() || fmt.is_empty() || fmt.contains('.') {
            return None;
        }
        format = Some(fmt.to_string());
        *last = base.to_string();
    }
    if segs.iter().any(|s| s.is_empty() || s.contains('.')) {
        return None;
    }
    let get = matches!(*method, Method::GET | Method::HEAD);
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    use Action::*;
    let p = |pairs: &[(&'static str, &str)]| pairs.iter().map(|(k, v)| (*k, v.to_string())).collect::<Vec<_>>();
    let found = match (s.as_slice(), get, method) {
        (["me"], true, _) => (Me, vec![]),
        (["accounts"], true, _) => (Accounts, vec![]),
        (["applications"], true, _) => (Applications, vec![]),
        (["applications", id], true, _) => (Application, p(&[("id", id)])),
        (["applications", app, "environments", id], true, _) => (Environment, p(&[("application_id", app), ("id", id)])),
        (["applications", app, "environments", env, tail @ ..], _, m) => {
            let base = [("application_id", *app), ("environment_id", *env)];
            let post = *m == Method::POST;
            match (tail, get, post) {
                (["summary"], true, _) => (Summary, p(&base)),
                (["requests"], true, _) => (Requests, p(&base)),
                (["requests", g], true, _) => (RequestGroup, p(&[base[0], base[1], ("group_hash", g)])),
                (["executions", e], true, _) => (Execution, p(&[base[0], base[1], ("execution_id", e)])),
                (["jobs"], true, _) => (Jobs, p(&base)),
                (["queries"], true, _) => (Queries, p(&base)),
                (["exceptions"], true, _) => (Exceptions, p(&base)),
                (["logs"], true, _) => (Logs, p(&base)),
                (["deploys"], true, _) => (Deploys, p(&base)),
                (["deploys"], _, true) => (CreateDeploy, p(&base)),
                (["releases"], true, _) => (Releases, p(&base)),
                (["users"], true, _) => (Users, p(&base)),
                (["tenants"], true, _) => (Tenants, p(&base)),
                (["spans"], true, _) => (Spans, p(&base)),
                (["series"], true, _) => (Series, p(&base)),
                (["visits"], true, _) => (Visits, p(&base)),
                _ => return None,
            }
        }
        (["issues"], true, _) => (Issues, vec![]),
        (["issues", id], true, _) => (Issue, p(&[("id", id)])),
        (["issues", id], false, m) if *m == Method::PATCH || *m == Method::PUT => (UpdateIssue, p(&[("id", id)])),
        (["issues", id, "comments"], false, m) if *m == Method::POST => (CreateComment, p(&[("issue_id", id)])),
        _ => return None,
    };
    Some((found.0, found.1, format))
}

/// Journey's path-parameter unescaping: %XX sequences, nothing else.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Responses

/// The JSON a controller renders, before the middleware.
struct Rendered {
    status: StatusCode,
    body: Value,
}

fn ok(body: Value) -> Result<Rendered, Halt> {
    Ok(Rendered { status: StatusCode::OK, body })
}

/// What stops an action early.
enum Halt {
    /// A `render json:` from a before_action or a rescue_from.
    Json(StatusCode, Value),
    /// An exception nothing rescues: the exceptions app's page for this status.
    Page(StatusCode),
}

impl From<rusqlite::Error> for Halt {
    fn from(e: rusqlite::Error) -> Halt {
        tracing::error!(error = %e, "api request failed");
        Halt::Page(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

type R<T> = Result<T, Halt>;

/// render_not_found
fn not_found() -> Halt {
    Halt::Json(StatusCode::NOT_FOUND, json!({"error": "not found"}))
}

/// render_unprocessable: `{errors: record.errors}` (attribute => [messages]).
fn unprocessable(attribute: &str, message: &str) -> Halt {
    Halt::Json(StatusCode::UNPROCESSABLE_ENTITY, json!({"errors": {attribute: [message]}}))
}

/// The security headers ActionDispatch adds to every response a controller
/// renders (default_headers, ContentSecurityPolicyBaseline). The
/// report-only CSP with a per-request nonce is not reproduced
/// (docs/DIFFERENCES.md).
fn security_headers(h: &mut HeaderMap) {
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    h.insert("x-xss-protection", HeaderValue::from_static("0"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-permitted-cross-domain-policies", HeaderValue::from_static("none"));
    h.insert("referrer-policy", HeaderValue::from_static("strict-origin-when-cross-origin"));
}

/// `render json:` through Rack::ETag and Rack::ConditionalGet: 200/201 get
/// a weak ETag (first 32 hex of SHA-256 of the body) and "max-age=0,
/// private, must-revalidate"; anything else "no-cache". A GET/HEAD whose
/// If-None-Match equals the ETag is a bodiless 304.
fn render(method: &Method, req_headers: &HeaderMap, status: StatusCode, body: &Value) -> ApiResponse {
    use sha2::Digest;
    // escape_json_responses is false (load_defaults 8.1): no HTML escaping.
    let text = body.to_string();
    let mut h = HeaderMap::new();
    security_headers(&mut h);
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    let cacheable = status == StatusCode::OK || status == StatusCode::CREATED;
    let mut status = status;
    let mut send_body = *method != Method::HEAD;
    if cacheable {
        let digest = sha2::Sha256::digest(text.as_bytes());
        let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        let etag = format!("W/\"{hex}\"");
        h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("etag"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=0, private, must-revalidate"));
        let fresh = req_headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str());
        if fresh && matches!(*method, Method::GET | Method::HEAD) && status == StatusCode::OK {
            status = StatusCode::NOT_MODIFIED;
            send_body = false;
            h.remove(header::CONTENT_TYPE);
        }
    } else {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    if status != StatusCode::NOT_MODIFIED {
        h.insert("content-security-policy", HeaderValue::from_static("base-uri 'self'; object-src 'none'; frame-ancestors 'none'"));
    }
    // Rack's content-length; Rack::Head and Rack::ConditionalGet answer
    // without one.
    if send_body {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(text.len()));
    }
    let body = if send_body { text.into_bytes() } else { vec![] };
    ApiResponse { status, headers: h, body }
}

/// Rails' exceptions app (ActionDispatch::PublicExceptions): JSON
/// `{"status":N,"error":"..."}` when the request's format is JSON, else
/// public/<status>.html. The format is Request#formats: the `.format` path
/// parameter when params are readable, else a non-browser-like Accept
/// header's first type, else the path's extension, else HTML. No security
/// headers: the exception skipped the controller.
fn error_page(status: StatusCode, req_headers: &HeaderMap, format: &str, params_readable: bool) -> ApiResponse {
    let accept = req_headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string();
    // valid_accept_header: present and not BROWSER_LIKE_ACCEPTS (/,\s*\*\/\*|\*\/\*\s*,/).
    let browser_like = {
        let a: String = accept.chars().filter(|c| !c.is_whitespace()).collect();
        a.contains(",*/*") || a.contains("*/*,")
    };
    let json = if params_readable && !format.is_empty() {
        format == "json"
    } else if !accept.is_empty() && !browser_like {
        let first = accept.split(',').next().unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        first == "application/json"
    } else {
        format == "json"
    };
    let mut h = HeaderMap::new();
    let body = if json {
        let reason = status.canonical_reason().unwrap_or("Internal Server Error");
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=UTF-8"));
        format!("{{\"status\":{},\"error\":\"{reason}\"}}", status.as_u16()).into_bytes()
    } else {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=UTF-8"));
        let page: &'static str = match status.as_u16() {
            400 => include_str!("api_pages/400.html"),
            404 => include_str!("api_pages/404.html"),
            _ => include_str!("api_pages/500.html"),
        };
        page.as_bytes().to_vec()
    };
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    ApiResponse { status, headers: h, body }
}

// ---------------------------------------------------------------------------
// The request, end to end

/// Routes, authenticates and runs one request at `now`.
pub fn call(env: &Env, limit: &FixedWindow<(&'static str, i64)>, now: i64, req: &ApiRequest) -> ApiResponse {
    call_as(env, limit, now, req, None)
}

/// `call`, for a request that may carry the trusted proxy's verified claims
/// instead of a token (Token::from_request).
pub fn call_as(env: &Env, limit: &FixedWindow<(&'static str, i64)>, now: i64, req: &ApiRequest, proxied: Option<&Claims>) -> ApiResponse {
    let Some((action, path_params, format)) = route(&req.method, &req.path) else {
        return error_page(StatusCode::NOT_FOUND, &req.headers, route_format(&req.path), false);
    };
    let format = format.unwrap_or_default();
    // Rails parses the body on the first `params` access, after the
    // before_actions that do not read params (authentication, scope, rate
    // limit, the write check): an unparsable body is a 400 page from there,
    // and MeController never reads params at all.
    let params = Params::parse(req, path_params);
    let readable = params.is_ok();
    let authorization = req.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    match run(env, limit, now, action, authorization, proxied, params) {
        Ok(r) => render(&req.method, &req.headers, r.status, &r.body),
        Err(Halt::Json(status, body)) => render(&req.method, &req.headers, status, &body),
        Err(Halt::Page(status)) => error_page(status, &req.headers, &format, readable),
    }
}

/// The `.format` of an unrouted path (format_from_path_extension).
fn route_format(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(_, f)| f).trim_end_matches('/')
}

fn run(
    env: &Env,
    limit: &FixedWindow<(&'static str, i64)>,
    now: i64,
    action: Action,
    authorization: Option<&str>,
    proxied: Option<&Claims>,
    params: Result<Params, ()>,
) -> R<Rendered> {
    // ApiTokenAuthenticated#authenticate_user!, or the trusted proxy's user.
    let token = Token::from_request(env.control, authorization, proxied, now)?.ok_or(Halt::Json(StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"})))?;
    if !(token.allows("api") && token.allows("read")) {
        return Err(Halt::Json(StatusCode::FORBIDDEN, json!({"error": "insufficient_scope"})));
    }
    token.record_use(env.control, now);
    // rate_limit, scoped to the controller_path.
    if limit.exceeded(&(action.controller(), token.user_id)) {
        return Err(Halt::Json(StatusCode::TOO_MANY_REQUESTS, json!({"error": "rate limit exceeded"})));
    }
    // The write before_actions (DeploysController's runs before
    // EnvironmentScoped's, which it is declared ahead of).
    if matches!(action, Action::CreateDeploy | Action::UpdateIssue | Action::CreateComment) && !token.allows("write") {
        return Err(Halt::Json(StatusCode::FORBIDDEN, json!({"error": "insufficient_scope"})));
    }
    // Deploys are recorded by a deploy hook with its own token, never from
    // the dashboard, so the proxy's user does not get them with `write`.
    if action == Action::CreateDeploy && token.is_proxied() {
        return Err(Halt::Json(StatusCode::FORBIDDEN, json!({"error": "insufficient_scope"})));
    }
    let params = match params {
        Ok(p) => p,
        Err(()) if action == Action::Me => Params { map: Map::new() },
        Err(()) => return Err(Halt::Page(StatusCode::BAD_REQUEST)),
    };
    let api = Api { env, now, token, params: &params };
    let scoped = if action.environment_scoped() { Some(api.environment_scoped()?) } else { None };
    let e = || scoped.as_ref().expect("environment-scoped action");
    use Action::*;
    match action {
        Me => api.me(),
        Accounts => api.accounts(),
        Applications => api.applications(),
        Application => api.application(),
        Environment => api.environment(),
        Summary => api.summary(e()),
        Requests => api.requests(e()),
        RequestGroup => api.request_group(e()),
        Execution => api.execution(e()),
        Jobs => api.jobs(e()),
        Queries => api.queries(e()),
        Exceptions => api.exceptions(e()),
        Logs => api.logs(e()),
        Deploys => api.deploys(e()),
        CreateDeploy => api.create_deploy(e()),
        Releases => api.releases(e()),
        Users => api.users(e()),
        Tenants => api.tenants(e()),
        Spans => api.spans(e()),
        Series => api.series(e()),
        Visits => api.visits(e()),
        Issues => api.issues(),
        Issue => api.issue(),
        UpdateIssue => api.update_issue(),
        CreateComment => api.create_comment(),
    }
}

// ---------------------------------------------------------------------------
// Params: query string merged with a JSON or form body, then path params

pub struct Params {
    map: Map<String, Value>,
}

impl Params {
    /// `request_parameters.merge(query_parameters).merge!(path_parameters)`.
    /// Err: a body that does not parse (ParseError, 400).
    fn parse(req: &ApiRequest, path: Vec<(&'static str, String)>) -> Result<Params, ()> {
        let mut map = Map::new();
        let ctype = req.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let ctype = ctype.split(';').next().unwrap_or("").trim().to_string();
        if !req.body.is_empty() {
            // Mime[:json] and its synonyms.
            if matches!(ctype.as_str(), "application/json" | "text/x-json" | "application/jsonrequest") {
                // A JSON body that is not an object lands under "_json".
                match serde_json::from_slice::<Value>(&req.body) {
                    Ok(Value::Object(m)) => map = m,
                    Ok(other) => {
                        map.insert("_json".into(), other);
                    }
                    Err(_) => return Err(()),
                }
            } else if ctype == "application/x-www-form-urlencoded" {
                map = super::params::Params::from_query(Some(&String::from_utf8_lossy(&req.body))).0;
            }
        }
        for (k, v) in super::params::Params::from_query(req.query.as_deref()).0 {
            map.insert(k, v);
        }
        for (k, v) in path {
            map.insert(k.to_string(), Value::String(v));
        }
        Ok(Params { map })
    }

    fn get(&self, k: &str) -> &Value {
        self.map.get(k).unwrap_or(&Value::Null)
    }

    fn has(&self, k: &str) -> bool {
        self.map.contains_key(k)
    }

    /// `params[k].present?`
    fn present(&self, k: &str) -> bool {
        present(self.get(k))
    }

    /// `params[k].to_s`
    fn s(&self, k: &str) -> String {
        to_s(self.get(k))
    }

    /// `params[k]` as an optional string (nil stays nil).
    fn opt(&self, k: &str) -> Option<String> {
        (!self.get(k).is_null()).then(|| self.s(k))
    }

    /// `params[k].presence`
    fn presence(&self, k: &str) -> Option<String> {
        self.present(k).then(|| self.s(k))
    }

    /// A pagination param (page, per_page, limit) as `to_i`, as
    /// Api::BaseController#number_param: a String or a JSON number; anything
    /// else (`page[]=1`, a hash, null) reads as absent.
    fn page_i(&self, k: &str) -> Option<i64> {
        match self.get(k) {
            v @ (Value::String(_) | Value::Number(_)) => ruby_to_i(v),
            _ => None,
        }
    }
}

/// Active Model's integer cast of a param for a `where`/`find` on an
/// integer column: None is `= NULL` / RecordNotFound (no row can match).
fn cast_id(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(a) => a.iter().flat_map(cast_id).collect(),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)).into_iter().collect(),
        Value::Bool(b) => vec![*b as i64],
        Value::String(s) => cast_id_str(s).into_iter().collect(),
        _ => vec![],
    }
}

/// ActiveModel::Type::Integer#cast_value for a String: leading whitespace,
/// a sign, digits and underscores (`"1abc"` is 1, `"1.9"` 1, `"abc"` nil);
/// out of the 64-bit range is nil too (Rails answers `1=0`).
fn cast_id_str(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if !rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '_').filter(|c| *c != '_').collect();
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

// ---------------------------------------------------------------------------
// Controllers

struct Api<'a> {
    env: &'a Env<'a>,
    now: i64,
    token: Token,
    params: &'a Params,
}

/// The application and environment an EnvironmentScoped action is for.
struct Scoped {
    id: i64,
    slug: String,
    application_id: i64,
    last_seen_at: Option<String>,
}

impl Api<'_> {
    fn control<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> R<T> {
        Ok(self.env.control.with(f)?)
    }

    /// `accessible_applications.find(id)`: the application's id, or 404.
    fn find_application(&self, id: &Value) -> R<i64> {
        let ids = cast_id(id);
        let Some(&id) = ids.first().filter(|_| !id.is_array()) else { return Err(not_found()) };
        let (from, mut binds) = self.token.applications_from(self.env.control);
        binds.push(id.into());
        let found: Option<i64> = self.control(|c| {
            c.query_row(&format!("SELECT applications.id {from} AND applications.id = ? LIMIT 1"), params_from_iter(binds), |r| r.get(0)).optional()
        })?;
        found.ok_or_else(not_found)
    }

    /// `application.environments.find(id)`.
    fn find_environment(&self, application_id: i64, id: &Value) -> R<Scoped> {
        let ids = cast_id(id);
        let Some(&id) = ids.first().filter(|_| !id.is_array()) else { return Err(not_found()) };
        let found = self.control(|c| {
            c.query_row("SELECT id, slug, last_seen_at FROM environments WHERE application_id = ? AND id = ? LIMIT 1", params![application_id, id], |r| {
                Ok(Scoped { id: r.get(0)?, slug: r.get(1)?, application_id, last_seen_at: r.get(2)? })
            })
            .optional()
        })?;
        found.ok_or_else(not_found)
    }

    /// EnvironmentScoped's before_actions.
    fn environment_scoped(&self) -> R<Scoped> {
        let app = self.find_application(self.params.get("application_id"))?;
        self.find_environment(app, self.params.get("environment_id"))
    }

    fn telemetry<T>(&self, e: &Scoped, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
        with_tenant(self.env, &e.slug, f)
    }

    /// `Window.preset(params[:window])`
    fn window(&self) -> Window {
        Window::preset(Some(&self.params.s("window")), self.now)
    }

    /// pagination_params
    fn pagination(&self) -> R<(i64, i64)> {
        let page = self.params.page_i("page").unwrap_or(0).max(1);
        let per_page = self.params.page_i("per_page").unwrap_or(0);
        let per_page = if per_page > 0 { per_page } else { 25 }.min(MAX_PER_PAGE);
        Ok((page, per_page))
    }

    /// `paginate(array)`: [items, {page, per_page, total}].
    fn paginate(&self, items: Vec<Value>) -> R<(Vec<Value>, Value)> {
        let (page, per_page) = self.pagination()?;
        let total = items.len();
        let offset = ((page - 1).saturating_mul(per_page)).min(total as i64) as usize;
        let items: Vec<Value> = items.into_iter().skip(offset).take(per_page as usize).collect();
        Ok((items, meta(page, per_page, total as i64)))
    }

    /// `paginate(relation)` for a control-DB query: COUNT(*) and LIMIT/OFFSET.
    fn paginate_sql(&self, select: &str, from: &str, order: &str, binds: Vec<rusqlite::types::Value>) -> R<(Vec<Vec<rusqlite::types::Value>>, Value)> {
        let (page, per_page) = self.pagination()?;
        let offset = (page - 1).saturating_mul(per_page);
        let (total, rows) = self.control(|c| {
            let total: i64 = c.query_row(&format!("SELECT COUNT(*) {from}"), params_from_iter(binds.iter()), |r| r.get(0))?;
            let mut stmt = c.prepare(&format!("SELECT {select} {from} ORDER BY {order} LIMIT {per_page} OFFSET {offset}"))?;
            let n = stmt.column_count();
            let rows = stmt
                .query_map(params_from_iter(binds.iter()), |r| (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect::<rusqlite::Result<Vec<_>>>())?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((total, rows))
        })?;
        Ok((rows, meta(page, per_page, total)))
    }

    fn accounts_json(&self, page: bool) -> R<(Vec<Value>, Option<Value>)> {
        let (from, binds) = self.token.accounts_from(self.env.control);
        let account = |r: &[rusqlite::types::Value]| crate::obj! { "id" => sv(&r[0]), "name" => sv(&r[1]), "slug" => sv(&r[2]), "plan" => sv(&r[3]) };
        let select = "accounts.id, accounts.name, accounts.slug, accounts.plan";
        let order = "accounts.name ASC";
        if page {
            let (rows, meta) = self.paginate_sql(select, &from, order, binds)?;
            return Ok((rows.iter().map(|r| account(r)).collect(), Some(meta)));
        }
        let rows = self.control(|c| {
            let mut stmt = c.prepare(&format!("SELECT {select} {from} ORDER BY {order}"))?;
            stmt.query_map(params_from_iter(binds), |r| (0..4).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect::<rusqlite::Result<Vec<_>>>())?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok((rows.iter().map(|r| account(r)).collect(), None))
    }

    // -- me, accounts, applications, environments ------------------------------

    fn me(&self) -> R<Rendered> {
        let (user_id, name, email): (i64, String, String) =
            self.control(|c| c.query_row("SELECT id, name, email FROM users WHERE id = ?", [self.token.user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
        let (accounts, _) = self.accounts_json(false)?;
        let (acc, mut binds) = self.token.accounts_sql(self.env.control);
        binds.insert(0, self.token.user_id.into());
        let memberships: Vec<Value> = self.control(|c| {
            c.prepare(&format!(
                "SELECT memberships.account_id, memberships.role FROM memberships WHERE memberships.user_id = ? AND memberships.account_id IN ({acc})"
            ))?
            .query_map(params_from_iter(binds), |r| Ok(crate::obj! { "account_id" => r.get::<_, i64>(0)?, "role" => r.get::<_, String>(1)? }))?
            .collect()
        })?;
        let t = &self.token;
        ok(crate::obj! {
            "user" => crate::obj! { "id" => user_id, "name" => name, "email" => email },
            "accounts" => accounts,
            "memberships" => memberships,
            "token" => crate::obj! {
                "name" => t.name.clone(), "scopes" => t.scopes.clone(), "account_id" => t.account_id, "application_id" => t.application_id,
                "expires_at" => db_time(t.expires_at.as_deref()),
            },
        })
    }

    fn accounts(&self) -> R<Rendered> {
        let (data, meta) = self.accounts_json(true)?;
        ok(crate::obj! { "data" => data, "meta" => meta.unwrap() })
    }

    fn applications(&self) -> R<Rendered> {
        let (from, binds) = self.token.applications_from(self.env.control);
        let (rows, meta) = self.paginate_sql(
            "applications.id, applications.name, applications.slug, applications.issue_prefix, applications.account_id",
            &from,
            "applications.name ASC",
            binds,
        )?;
        ok(crate::obj! { "data" => rows.iter().map(|r| application_json(r)).collect::<Vec<_>>(), "meta" => meta })
    }

    fn application(&self) -> R<Rendered> {
        let id = self.find_application(self.params.get("id"))?;
        let row: Vec<rusqlite::types::Value> = self.control(|c| {
            c.query_row("SELECT id, name, slug, issue_prefix, account_id FROM applications WHERE id = ?", [id], |r| {
                (0..5).map(|i| r.get(i)).collect::<rusqlite::Result<Vec<_>>>()
            })
        })?;
        let mut json = application_json(&row);
        let envs: Vec<Value> = self.control(|c| {
            c.prepare("SELECT id, name, slug, last_seen_at, paused_at FROM environments WHERE environments.application_id = ? ORDER BY environments.name ASC")?
                .query_map([id], |r| {
                    Ok(crate::obj! {
                        "id" => r.get::<_, i64>(0)?, "name" => r.get::<_, String>(1)?, "slug" => r.get::<_, String>(2)?,
                        "last_seen_at" => db_time(r.get::<_, Option<String>>(3)?.as_deref()),
                        "paused" => paused(r.get::<_, Option<String>>(4)?.as_deref()),
                    })
                })?
                .collect()
        })?;
        db::merge_into(&mut json, "environments", Value::Array(envs));
        ok(json)
    }

    fn environment(&self) -> R<Rendered> {
        let app = self.find_application(self.params.get("application_id"))?;
        let e = self.find_environment(app, self.params.get("id"))?;
        let (name, paused_at): (String, Option<String>) =
            self.control(|c| c.query_row("SELECT name, paused_at FROM environments WHERE id = ?", [e.id], |r| Ok((r.get(0)?, r.get(1)?))))?;
        ok(crate::obj! {
            "id" => e.id, "name" => name, "slug" => e.slug.clone(), "application_id" => app,
            "last_seen_at" => db_time(e.last_seen_at.as_deref()), "paused" => paused(paused_at.as_deref()),
        })
    }

    // -- telemetry ---------------------------------------------------------------

    fn summary(&self, e: &Scoped) -> R<Rendered> {
        let w = self.window();
        let (requests, jobs) = self.telemetry(e, |conn| {
            Ok((
                summary_json(aggregations::summary_with_delta(conn, "request", w.range(), w.previous().range(), None)?),
                summary_json(aggregations::summary_with_delta(conn, "job_attempt", w.range(), w.previous().range(), None)?),
            ))
        })?;
        let open: i64 = self.control(|c| c.query_row("SELECT COUNT(*) FROM issues WHERE environment_id = ? AND status = 'open'", [e.id], |r| r.get(0)))?;
        ok(crate::obj! { "requests" => requests, "jobs" => jobs, "open_issues" => open, "last_seen_at" => db_time(e.last_seen_at.as_deref()) })
    }

    fn requests(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (sort, dir) = (self.params.opt("sort"), self.params.opt("dir"));
        let parsed = filter_query::parse(self.params.opt("q").as_deref());
        self.check_request_keys(&parsed)?;
        if filter_query::has_request_keys(&parsed) {
            // Only the raw requests hold ip, path, ...: the table from them.
            let (scanned, routes) = self.telemetry(e, |conn| {
                let cond = filter_query::requests(conn, &parsed, from, to)?;
                Ok(crate::dashboard::request_scan::grouped(conn, &cond, from, to, 200, sort.as_deref(), dir.as_deref())?)
            })?;
            let (items, meta) = self.paginate(routes)?;
            return ok(crate::obj! { "data" => items, "meta" => meta, "scan" => scanned.to_json() });
        }
        let routes = self.telemetry(e, |conn| Ok(aggregations::grouped(conn, "request", from, to, 200, sort.as_deref(), dir.as_deref())?))?;
        let routes = filter_query::filter_routes(routes, self.params.opt("q").as_deref());
        let (items, meta) = self.paginate(routes)?;
        ok(crate::obj! { "data" => items, "meta" => meta })
    }

    /// A query's request keys: a bad value is 422 with what is wrong; ip:
    /// and ua: (personal data) need the token's security scope (403).
    fn check_request_keys(&self, parsed: &filter_query::Parsed) -> R<()> {
        if let Some(bad) = filter_query::check_request_keys(parsed) {
            return Err(Halt::Json(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": bad})));
        }
        let personal = filter_query::personal_keys(parsed);
        if !personal.is_empty() && !self.token.allows("security") {
            return Err(Halt::Json(
                StatusCode::FORBIDDEN,
                json!({"error": "insufficient_scope", "detail": format!("{} filters need a token with the security scope", personal.join(" and "))}),
            ));
        }
        Ok(())
    }

    fn request_group(&self, e: &Scoped) -> R<Rendered> {
        let group_hash = self.params.s("group_hash");
        let w = self.window();
        let (from, to) = w.range();
        let (page, per_page) = self.pagination()?;
        let (name, total, rows, summary) = self.telemetry(e, |conn| {
            let cond = "\"executions\".\"kind\" = ?1 AND \"executions\".\"group_hash\" = ?2 AND \"executions\".\"occurred_at\" BETWEEN ?3 AND ?4";
            let b = params!["request", group_hash, qt(from), qt(to)];
            let first = db::query_one(
                conn,
                &format!("SELECT \"executions\".* FROM \"executions\" WHERE {cond} ORDER BY \"executions\".\"occurred_at\" DESC LIMIT 1"),
                b,
            )?;
            let name = match first.as_ref().and_then(|r| r.s("name")) {
                Some(n) => Value::String(n.to_string()),
                None => pages::rollup_name(conn, "request", &group_hash)?,
            };
            let total = db::count(conn, &format!("SELECT COUNT(*) FROM \"executions\" WHERE {cond}"), b)?;
            let offset = (page - 1).saturating_mul(per_page);
            let rows = db::query(
                conn,
                &format!(
                    "SELECT \"executions\".* FROM \"executions\" WHERE {cond} ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {per_page} OFFSET {offset}"
                ),
                b,
            )?;
            let summary = aggregations::summary_with_delta(conn, "request", (from, to), w.previous().range(), Some(&group_hash))?;
            Ok((name, total, rows, summary))
        })?;
        ok(crate::obj! {
            "group_hash" => group_hash.clone(), "route" => name, "summary" => summary_json(summary),
            "data" => rows.iter().map(models::execution_row).collect::<Vec<_>>(), "meta" => meta(page, per_page, total),
        })
    }

    fn execution(&self, e: &Scoped) -> R<Rendered> {
        let id = self.params.s("execution_id");
        let control = self.control(|c| super::dashboard_bridge::DbControl::load(c, e.id))?;
        let mut p = crate::dashboard::Params::new();
        p.insert("_environment_path".into(), format!("/apps/{}/envs/{}", e.application_id, e.id));
        let ctx = crate::dashboard::PageContext::new(e.id, self.now, p, self.env.cursor_key.clone());
        let props = self.telemetry(e, |conn| {
            let exe = db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [&id])?
                .ok_or_else(not_found)?;
            Ok(crate::dashboard::execution_presenter::props(conn, &exe, &ctx, &control)?)
        })?;
        ok(props)
    }

    /// FilterQuery.cursor_context(environment_id:, resource:, query:, window: window.key)
    fn cursor_context(&self, e: &Scoped, resource: &str) -> String {
        use sha2::Digest;
        let q = self.params.opt("q").unwrap_or_default();
        let j = serde_json::to_string(&json!([e.id, resource, q, self.window().key])).unwrap();
        let j = crate::rjson::escape_html(&j);
        sha2::Sha256::digest(j.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// CursorPage.call; InvalidCursor is 422 with its message.
    #[allow(clippy::too_many_arguments)]
    fn cursor_page(
        &self,
        conn: &Connection,
        e: &Scoped,
        resource: &str,
        table: &str,
        model: &str,
        select: &str,
        cond: filter_query::Cond,
        order: Order,
    ) -> R<cursor_page::Page> {
        let (cursor, limit) = (self.params.opt("cursor"), self.cursor_limit()?);
        match cursor_page::call(
            conn,
            self.env.cursor_key,
            table,
            model,
            select,
            cond,
            cursor.as_deref(),
            limit.as_deref(),
            order,
            &self.cursor_context(e, resource),
        ) {
            Ok(p) => Ok(p?),
            Err(err) => Err(Halt::Json(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": err.to_string()}))),
        }
    }

    /// `limit.to_i` for CursorPage; a non-String limit is absent (50).
    fn cursor_limit(&self) -> R<Option<String>> {
        Ok(self.params.page_i("limit").map(|l| l.to_string()))
    }

    fn jobs(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (sort, dir) = (self.params.opt("sort"), self.params.opt("dir"));
        let classes = self.telemetry(e, |conn| Ok(aggregations::grouped(conn, "job_attempt", from, to, 200, sort.as_deref(), dir.as_deref())?))?;
        let (items, meta) = self.paginate(classes)?;
        let q = self.params.opt("q");
        self.check_request_keys(&filter_query::parse(q.as_deref()))?;
        let (page, queues) = self.telemetry(e, |conn| {
            let cond = filter_query::apply(conn, Resource::Jobs, q.as_deref(), from, to, &[])?;
            let page = self.cursor_page(conn, e, "jobs", "executions", "Railwatch::Telemetry::Execution", "\"executions\".*", cond, Order::Recent)?;
            Ok((page, pages::queue_stats(conn, from, to)?))
        })?;
        ok(crate::obj! {
            "data" => items, "meta" => meta, "results" => page.rows.iter().map(models::execution_row).collect::<Vec<_>>(),
            "results_meta" => page.meta, "queues" => queues,
        })
    }

    /// The grouped table, the slowest-queries page, which groups have an
    /// EXPLAIN and the n+1s, each on its own pooled reader (as
    /// queries_index_parallel): over a per-day tenant each one reads every
    /// day file, and one after another they added up. The explained lookup
    /// needs the grouped page, so it runs after it.
    fn queries(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (sort, dir) = (self.params.opt("sort"), self.params.opt("dir"));
        let q = self.params.opt("q");
        self.check_request_keys(&filter_query::parse(q.as_deref()))?;
        let (grouped, page, n_plus_ones) = std::thread::scope(|s| {
            let grouped = s.spawn(|| -> R<(Vec<Value>, Value, Vec<String>)> {
                let groups = self.telemetry(e, |conn| Ok(aggregations::grouped(conn, "query", from, to, 200, sort.as_deref(), dir.as_deref())?))?;
                let (items, meta) = self.paginate(groups)?;
                let hashes: Vec<String> = items.iter().filter_map(|i| i["group_hash"].as_str().map(str::to_string)).collect();
                let explained = if hashes.is_empty() { vec![] } else { self.telemetry(e, |conn| Ok(explained_groups(conn, from, to, &hashes)?))? };
                Ok((items, meta, explained))
            });
            let page = s.spawn(|| {
                self.telemetry(e, |conn| {
                    let cond = filter_query::apply(conn, Resource::Queries, q.as_deref(), from, to, &[])?;
                    self.cursor_page(conn, e, "queries", "queries", "Railwatch::Telemetry::Query", models::QUERY_WITH_SQL, cond, Order::Slowest)
                })
            });
            let n_plus_ones = s.spawn(|| self.telemetry(e, |conn| Ok(pages::n_plus_ones(conn, from, to)?)));
            (
                grouped.join().expect("queries section panicked"),
                page.join().expect("queries section panicked"),
                n_plus_ones.join().expect("queries section panicked"),
            )
        });
        let (items, meta, explained) = grouped?;
        let (page, n_plus_ones) = (page?, n_plus_ones?);
        let items: Vec<Value> = items
            .into_iter()
            .map(|mut i| {
                let has = i["group_hash"].as_str().is_some_and(|g| explained.iter().any(|x| x == g));
                db::merge_into(&mut i, "explain", Value::Bool(has));
                i
            })
            .collect();
        ok(crate::obj! {
            "data" => items, "meta" => meta, "results" => page.rows.iter().map(models::query_row).collect::<Vec<_>>(),
            "results_meta" => page.meta, "n_plus_ones" => n_plus_ones,
        })
    }

    /// The exceptions and logs actions: a page/per_page slice and a cursor
    /// page, each run only when asked for. Only `cursor`/`limit` present:
    /// the cursor page alone; only `page`/`per_page`: the slice alone;
    /// both families or neither: both (presence is the key, whatever its value).
    fn raw_rows(&self, e: &Scoped, resource: Resource, name: &str, table: &str, model: &str, row: fn(&db::Rec) -> Value) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (page, per_page) = self.pagination()?;
        let cursor_asked = self.params.has("cursor") || self.params.has("limit");
        let offset_asked = self.params.has("page") || self.params.has("per_page");
        let (want_offset, want_cursor) = (offset_asked || !cursor_asked, cursor_asked || !offset_asked);
        let q = self.params.opt("q");
        self.check_request_keys(&filter_query::parse(q.as_deref()))?;
        let (offset_page, cursor) = self.telemetry(e, |conn| {
            let cond = filter_query::apply(conn, resource, q.as_deref(), from, to, &[])?;
            let offset_page = if want_offset {
                let from_sql = cond.from.clone().unwrap_or_else(|| format!("\"{table}\""));
                let total = db::count(conn, &format!("SELECT COUNT(*) FROM {from_sql} WHERE {}", cond.sql()), params_from_iter(cond.binds.iter()))?;
                let offset = (page - 1).saturating_mul(per_page);
                let rows = db::query(
                    conn,
                    &format!(
                        "SELECT \"{table}\".* FROM {from_sql} WHERE {} ORDER BY \"{table}\".\"occurred_at\" DESC, \"{table}\".\"id\" DESC LIMIT {per_page} OFFSET {offset}",
                        cond.sql()
                    ),
                    params_from_iter(cond.binds.iter()),
                )?;
                Some((total, rows))
            } else {
                None
            };
            let cursor = if want_cursor { Some(self.cursor_page(conn, e, name, table, model, &format!("\"{table}\".*"), cond, Order::Recent)?) } else { None };
            Ok((offset_page, cursor))
        })?;
        let mut body = Map::new();
        if let Some((total, rows)) = offset_page {
            body.insert("data".into(), rows.iter().map(row).collect::<Vec<_>>().into());
            body.insert("meta".into(), meta(page, per_page, total));
        }
        if let Some(cursor) = cursor {
            body.insert("results".into(), cursor.rows.iter().map(row).collect::<Vec<_>>().into());
            body.insert("results_meta".into(), cursor.meta);
        }
        ok(Value::Object(body))
    }

    fn exceptions(&self, e: &Scoped) -> R<Rendered> {
        self.raw_rows(e, Resource::Exceptions, "exceptions", "exceptions", "Railwatch::Telemetry::Exception", models::exception_row)
    }

    fn logs(&self, e: &Scoped) -> R<Rendered> {
        self.raw_rows(e, Resource::Logs, "logs", "logs", "Railwatch::Telemetry::Log", models::log_row)
    }

    fn deploys(&self, e: &Scoped) -> R<Rendered> {
        let (rows, meta) = self.paginate_sql(
            "id, deploy, ref, name, url, deployed_at",
            "FROM deploys WHERE deploys.environment_id = ?",
            "deploys.deployed_at DESC",
            vec![e.id.into()],
        )?;
        ok(crate::obj! { "data" => rows.iter().map(|r| deploy_json(r)).collect::<Vec<_>>(), "meta" => meta })
    }

    /// DeploysController#create: find_or_initialize_by(deploy:), assign
    /// ref/name/url (each `.to_s.first(n).presence`), deployed_at now for a
    /// new deploy only, save! (Deploy's before_validation hooks and url validation).
    fn create_deploy(&self, e: &Scoped) -> R<Rendered> {
        // params.require(:deploy): missing or blank is ParameterMissing (400 page).
        if !self.params.present("deploy") {
            return Err(Halt::Page(StatusCode::BAD_REQUEST));
        }
        let deploy = rb::first_chars(&self.params.s("deploy"), 128).to_string();
        let opt = |k: &str, n: usize| Some(rb::first_chars(&self.params.s(k), n).to_string()).filter(|s| rb::present(Some(s)));
        let (ref_, name, url) = (opt("ref", 128), opt("name", 255), opt("url", 1024));
        if let Some(u) = &url
            && !crate::http::web_url(u)
        {
            return Err(unprocessable("url", "must be a single http:// or https:// URL"));
        }
        let now = qt(self.now);
        let row = self.write(|c| {
            let existing: Option<(i64, Option<String>, String, String)> = c
                .query_row(
                    "SELECT id, previous_ref, commits, deployed_at FROM deploys WHERE environment_id = ? AND deploy = ? LIMIT 1",
                    params![e.id, deploy],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let deployed_at = existing.as_ref().map_or_else(|| now.clone(), |x| x.3.clone());
            // assign_previous_ref: only when blank, from the deploy before
            // deployed_at (the record itself is excluded by `<`).
            let mut previous_ref = existing.as_ref().and_then(|x| x.1.clone());
            if !rb::present(previous_ref.as_deref()) {
                previous_ref = c
                    .query_row(
                        "SELECT ref FROM deploys WHERE deploys.environment_id = ? AND deploys.deployed_at < ? ORDER BY deploys.deployed_at DESC LIMIT 1",
                        params![e.id, deployed_at],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten();
            }
            // trim_commits
            let mut commits: Vec<Value> = existing.as_ref().and_then(|x| serde_json::from_str(&x.2).ok()).unwrap_or_default();
            if let Some(prev) = previous_ref.as_deref().filter(|p| rb::present(Some(p)))
                && let Some(i) = commits.iter().position(|c| to_s(c.get("sha").unwrap_or(&Value::Null)).starts_with(prev))
            {
                commits.truncate(i);
            }
            let commits = crate::rjson::escape_html(&Value::Array(commits).to_string());
            let id = match existing {
                // A re-post keeps its deployed_at (a retried hook must not
                // move an old release to the top of the timeline).
                Some((id, ..)) => {
                    c.execute(
                        "UPDATE deploys SET ref = ?, name = ?, url = ?, previous_ref = ?, commits = ?, updated_at = ? WHERE id = ?",
                        params![ref_, name, url, previous_ref, commits, now, id],
                    )?;
                    id
                }
                None => {
                    c.execute(
                        "INSERT INTO deploys (environment_id, deploy, ref, name, url, deployed_at, previous_ref, commits, detail, created_at, updated_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, '{}', ?, ?)",
                        params![e.id, deploy, ref_, name, url, now, previous_ref, commits, now, now],
                    )?;
                    c.last_insert_rowid()
                }
            };
            c.query_row("SELECT id, deploy, ref, name, url, deployed_at FROM deploys WHERE id = ?", [id], |r| {
                (0..6).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect::<rusqlite::Result<Vec<_>>>()
            })
        })?;
        Ok(Rendered { status: StatusCode::CREATED, body: deploy_json(&row) })
    }

    fn releases(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let deploys: Vec<(String, Option<String>, Option<String>, String)> = self.control(|c| {
            c.prepare(
                "SELECT deploy, ref, name, deployed_at FROM deploys WHERE deploys.environment_id = ? AND deploys.deployed_at BETWEEN ? AND ? \
                 ORDER BY deploys.deployed_at DESC LIMIT 50",
            )?
            .query_map(params![e.id, qt(from), qt(to)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect()
        })?;
        let rows = self.telemetry(e, |conn| {
            let adoption = pages::adoption(conn, from, to)?;
            let mut out = vec![];
            for (deploy, ref_, name, at) in &deploys {
                let mut v = pages::release_health_for(conn, Some(deploy), from, to)?;
                let share = adoption.iter().find(|(d, _)| d.as_deref() == Some(deploy.as_str())).map_or(0.0, |x| x.1);
                db::merge_into(&mut v, "deploy", Value::from(deploy.clone()));
                db::merge_into(&mut v, "ref", js(ref_.as_deref()));
                db::merge_into(&mut v, "name", js(name.as_deref()));
                db::merge_into(&mut v, "deployed_at", db_time(Some(at)));
                db::merge_into(&mut v, "adoption", jf(share));
                out.push(v);
            }
            Ok(out)
        })?;
        let (items, meta) = self.paginate(rows)?;
        ok(crate::obj! { "data" => items, "meta" => meta })
    }

    fn users(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let active = self.telemetry(e, |conn| {
            let rows = crate::dashboard::pages::active_people(conn, from, to)?;
            let active: Vec<(i64, Value)> = rows
                .iter()
                .map(|r| {
                    let c = r.i("COUNT(*)").unwrap_or(0);
                    (
                        c,
                        crate::obj! {
                            "ref" => db::raw_json(r.at(0)), "requests" => c, "errors" => db::raw_json(r.at(2)), "last_seen_at" => db::raw_json(r.at(3)),
                        },
                    )
                })
                .collect();
            Ok(rb::sort_by_num(active, |a| rb::Num::I(-a.0)).into_iter().map(|a| a.1).collect::<Vec<_>>())
        })?;
        let (items, meta) = self.paginate(active)?;
        let refs: Vec<String> = items.iter().filter_map(|a| a["ref"].as_str().map(str::to_string)).collect();
        let people = self.telemetry(e, |conn| Ok(models::people_by_ref(conn, &refs)?))?;
        let data: Vec<Value> = items
            .into_iter()
            .map(|mut a| {
                let p = a["ref"].as_str().and_then(|r| people.get(r));
                db::merge_into(&mut a, "name", p.map_or(Value::Null, |p| Value::String(p.display_name())));
                db::merge_into(&mut a, "email", p.map_or(Value::Null, |p| js(p.email.as_deref())));
                db::merge_into(&mut a, "tenant", p.map_or(Value::Null, |p| js(p.app_tenant.as_deref())));
                a
            })
            .collect();
        ok(crate::obj! { "data" => data, "meta" => meta })
    }

    fn tenants(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (q, sort, dir) = (self.params.opt("q"), self.params.opt("sort"), self.params.opt("dir"));
        let rows = self.telemetry(e, |conn| Ok(crate::dashboard::tenant::index(conn, from, to, q.as_deref(), sort.as_deref(), dir.as_deref())?))?;
        let (items, meta) = self.paginate(rows)?;
        ok(crate::obj! { "data" => items, "meta" => meta })
    }

    fn spans(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (sort, dir) = (self.params.opt("sort"), self.params.opt("dir"));
        let mut spans = self.telemetry(e, |conn| Ok(aggregations::grouped(conn, "span", from, to, 200, sort.as_deref(), dir.as_deref())?))?;
        let parsed = filter_query::parse(self.params.opt("q").as_deref());
        if let Some(n) = parsed.field("name") {
            spans.retain(|s| s["name"].as_str().unwrap_or("").contains(n));
        }
        if parsed.fields.get("status").map(String::as_str) == Some("failed") {
            spans.retain(|s| s["errors"].as_i64().unwrap_or(0) > 0);
        }
        let (items, meta) = self.paginate(spans)?;
        ok(crate::obj! { "data" => items, "meta" => meta })
    }

    fn series(&self, e: &Scoped) -> R<Rendered> {
        let kind = self.params.s("type");
        if !SERIES_TYPES.contains(&kind.as_str()) {
            return Err(Halt::Json(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": format!("type must be one of {}", SERIES_TYPES.join(", "))})));
        }
        let (from, to) = self.window().range();
        let steps = window::steps_for(from, to);
        let step = self.params.presence("step");
        if let Some(s) = &step
            && !steps.contains(&s.as_str())
        {
            return Err(Halt::Json(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": format!("step must be one of {} for this window", steps.join(", "))})));
        }
        let step: &str = match &step {
            Some(s) => steps.iter().find(|k| **k == s.as_str()).copied().expect("validated"),
            None => window::default_step(from, to),
        };
        let group = self.params.presence("group_hash");
        let points = self.telemetry(e, |conn| Ok(aggregations::points(conn, &kind, from, to, group.as_deref(), step, None)?))?;
        ok(crate::obj! { "data" => points })
    }

    /// The visits page's components and web vitals (p75 per metric, rated),
    /// plus the vitals of each deploy in the window.
    fn visits(&self, e: &Scoped) -> R<Rendered> {
        let (from, to) = self.window().range();
        let (sort, dir, component) = (self.params.opt("sort"), self.params.opt("dir"), self.params.presence("component"));
        let deploys: Vec<(String, Option<String>, Option<String>, String)> = self.control(|c| {
            c.prepare(
                "SELECT deploy, ref, name, deployed_at FROM deploys WHERE deploys.environment_id = ? AND deploys.deployed_at BETWEEN ? AND ? \
                 ORDER BY deploys.deployed_at DESC LIMIT 50",
            )?
            .query_map(params![e.id, qt(from), qt(to)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect()
        })?;
        let (components, vitals, ttfb_phases, releases) = self.telemetry(e, |conn| {
            let samples = pages::vital_samples(conn, from, to)?;
            let phases = pages::ttfb_phases(conn, from, to)?;
            let components = pages::visit_components(conn, from, to, &samples, &phases, sort.as_deref(), dir.as_deref(), component.as_deref())?;
            let (samples, phases) = match &component {
                Some(c) => (samples.of_component(c), phases.of_component(c)),
                None => (samples, phases),
            };
            let names: Vec<String> = deploys.iter().map(|d| d.0.clone()).collect();
            let releases: Vec<Value> = deploys
                .iter()
                .zip(pages::vitals_by_deploy(&samples, &names))
                .zip(pages::ttfb_phases_by_deploy(&phases, &names))
                .map(|(((deploy, ref_, name, at), vitals), ttfb_phases)| {
                    crate::obj! {
                        "deploy" => deploy.clone(), "ref" => js(ref_.as_deref()), "name" => js(name.as_deref()), "deployed_at" => db_time(Some(at)),
                        "vitals" => Value::Object(vitals), "ttfb_phases" => Value::Object(ttfb_phases),
                    }
                })
                .collect();
            Ok((components, samples.summary(), phases.summary(), releases))
        })?;
        let (items, meta) = self.paginate(components)?;
        ok(crate::obj! {
            "vitals" => Value::Object(vitals), "thresholds" => pages::vital_thresholds(), "ttfb_phases" => Value::Object(ttfb_phases),
            "data" => items, "meta" => meta, "releases" => releases,
        })
    }

    // -- issues ------------------------------------------------------------------

    /// `Issue.where(application_id: accessible_applications.select(:id))`
    fn issues_scope(&self) -> (String, Vec<rusqlite::types::Value>) {
        let (apps, binds) = self.token.applications_sql(self.env.control);
        (format!("FROM issues WHERE issues.application_id IN ({apps})"), binds)
    }

    fn issues(&self) -> R<Rendered> {
        let (mut from, mut binds) = self.issues_scope();
        let p = self.params;
        let id_filter = |from: &mut String, binds: &mut Vec<rusqlite::types::Value>, col: &str, key: &str| {
            if p.present(key) {
                let ids = cast_id(p.get(key));
                if p.get(key).is_array() {
                    let ids: Vec<String> = ids.iter().map(i64::to_string).collect();
                    if ids.is_empty() {
                        from.push_str(" AND 1=0");
                    } else {
                        from.push_str(&format!(" AND issues.{col} IN ({})", ids.join(", ")));
                    }
                } else {
                    match ids.first() {
                        Some(id) => {
                            from.push_str(&format!(" AND issues.{col} = ?"));
                            binds.push((*id).into());
                        }
                        // `= NULL`, or 1=0 out of range: nothing matches.
                        None => from.push_str(" AND 1=0"),
                    }
                }
            }
        };
        id_filter(&mut from, &mut binds, "application_id", "application_id");
        id_filter(&mut from, &mut binds, "environment_id", "environment_id");
        for (key, allowed) in [("status", issue_model::STATUSES), ("kind", issue_model::KINDS), ("priority", issue_model::PRIORITIES)] {
            let v = p.s(key);
            if allowed.contains(&v.as_str()) {
                from.push_str(&format!(" AND issues.{key} = ?"));
                binds.push(v.into());
            }
        }
        id_filter(&mut from, &mut binds, "assignee_id", "assignee_id");
        if p.present("q") {
            // ESCAPE '\': without it SQLite reads sanitize_sql_like's
            // backslashes literally (a Rails bug, fixed on both sides).
            from.push_str(" AND (title LIKE ? ESCAPE '\\')");
            binds.push(format!("%{}%", rb::sanitize_sql_like(&p.s("q"))).into());
        }
        // "issues".* as Rails selects it (the plan, so the tie order, follows
        // the select list); id is the first column.
        let (rows, meta) = self.paginate_sql("\"issues\".*", &from, "\"issues\".\"last_seen_at\" DESC", binds)?;
        let ids: Vec<i64> = rows.iter().filter_map(|r| if let rusqlite::types::Value::Integer(i) = r[0] { Some(i) } else { None }).collect();
        let data = ids.iter().map(|id| self.issue_row(*id).map(|i| i.json())).collect::<R<Vec<_>>>()?;
        ok(crate::obj! { "data" => data, "meta" => meta })
    }

    /// set_issue: the issue among the accessible applications', or 404.
    fn find_issue(&self, key: &str) -> R<i64> {
        let ids = cast_id(self.params.get(key));
        let Some(&id) = ids.first().filter(|_| !self.params.get(key).is_array()) else { return Err(not_found()) };
        let (from, mut binds) = self.issues_scope();
        binds.push(id.into());
        let found: Option<i64> =
            self.control(|c| c.query_row(&format!("SELECT issues.id {from} AND issues.id = ? LIMIT 1"), params_from_iter(binds), |r| r.get(0)).optional())?;
        found.ok_or_else(not_found)
    }

    fn issue_row(&self, id: i64) -> R<IssueRow> {
        self.control(|c| {
            c.query_row(
                "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title, i.kind, i.status, i.priority, i.culprit, i.occurrences, i.affected_users,
                        i.first_seen_at, i.last_seen_at, i.assignee_id, i.application_id, i.environment_id, i.sample, i.resolved_at,
                        i.resolved_in_deploy, i.regressed_at, i.group_hash, e.slug
                 FROM issues i JOIN applications ap ON ap.id = i.application_id JOIN environments e ON e.id = i.environment_id WHERE i.id = ?",
                [id],
                |r| {
                    Ok(IssueRow {
                        id: r.get(0)?,
                        key: r.get(1)?,
                        title: r.get(2)?,
                        kind: r.get(3)?,
                        status: r.get(4)?,
                        priority: r.get(5)?,
                        culprit: r.get(6)?,
                        occurrences: r.get(7)?,
                        affected_users: r.get(8)?,
                        first_seen_at: r.get(9)?,
                        last_seen_at: r.get(10)?,
                        assignee_id: r.get(11)?,
                        application_id: r.get(12)?,
                        environment_id: r.get(13)?,
                        sample: serde_json::from_str(&r.get::<_, String>(14)?).unwrap_or(Value::Null),
                        resolved_at: r.get(15)?,
                        resolved_in_deploy: r.get(16)?,
                        regressed_at: r.get(17)?,
                        group_hash: r.get(18)?,
                        environment_slug: r.get(19)?,
                    })
                },
            )
        })
    }

    /// issue_json(i, full: true)
    fn issue_full(&self, id: i64) -> R<Value> {
        let i = self.issue_row(id)?;
        let mut v = i.json();
        let extra = crate::obj! {
            "sample" => i.sample.clone(), "resolved_at" => db_time(i.resolved_at.as_deref()),
            "resolved_in_deploy" => i.resolved_in_deploy.clone(), "regressed_at" => db_time(i.regressed_at.as_deref()),
        };
        v = db::merge(v, extra);
        // IssueDetectionPresenter#as_json (nil for exception issues)
        let detection = if i.kind == "performance" || i.kind == "anomaly" {
            let presented = crate::detection::presenter::Presented { kind: &i.kind, occurrences: i.occurrences, sample: &i.sample };
            with_tenant(self.env, &i.environment_slug, |t| crate::detection::presenter::as_json(&presented, t, self.now))?
        } else {
            Value::Null
        };
        db::merge_into(&mut v, "detection", detection);
        // **IssueExceptionPresenter#as_json
        let (sample, last) = if i.kind == "exception" {
            with_tenant(self.env, &i.environment_slug, |conn| super::mcp::issue_exception_occurrences(conn, &i.group_hash, &i.sample))?
        } else {
            (None, None)
        };
        let detail = |r: Option<db::Rec>| r.as_ref().map_or(Value::Null, super::mcp::exception_occurrence);
        db::merge_into(&mut v, "sample_occurrence", detail(sample));
        db::merge_into(&mut v, "last_occurrence", detail(last));
        let comments: Vec<Value> = self.control(|c| {
            c.prepare("SELECT id, body, user_id, author_name, source, created_at FROM comments WHERE comments.issue_id = ? ORDER BY comments.created_at ASC")?
                .query_map([id], |r| {
                    Ok(crate::obj! {
                        "id" => r.get::<_, i64>(0)?, "body" => r.get::<_, String>(1)?, "user_id" => r.get::<_, Option<i64>>(2)?,
                        "author_name" => r.get::<_, Option<String>>(3)?, "source" => r.get::<_, String>(4)?,
                        "created_at" => db_time(r.get::<_, Option<String>>(5)?.as_deref()),
                    })
                })?
                .collect()
        })?;
        db::merge_into(&mut v, "comments", Value::Array(comments));
        Ok(v)
    }

    fn issue(&self) -> R<Rendered> {
        let id = self.find_issue("id")?;
        ok(self.issue_full(id)?)
    }

    /// One IMMEDIATE transaction on the control DB, rolled back when `f`
    /// fails (each update! is its own transaction in Rails; an action that
    /// fails part way leaves the earlier ones, so callers commit per step).
    fn write<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> rusqlite::Result<T>) -> R<T> {
        let mut conn = self.env.control.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// IssuesController#update. Each update! commits on its own, as in
    /// Rails: a later failure (bad priority, unknown assignee) keeps the
    /// earlier changes. Every write is the token's user's (Current.api_user,
    /// as MCP): the activities carry its user_id, the alerts its name.
    fn update_issue(&self) -> R<Rendered> {
        let id = self.find_issue("id")?;
        let p = self.params;
        let user: String = self.control(|c| c.query_row("SELECT name FROM users WHERE id = ?", [self.token.user_id], |r| r.get(0)))?;
        let actor = Some(issue_model::Actor { user_id: self.token.user_id, name: &user });
        if p.present("status") {
            let status = p.s("status");
            if !matches!(status.as_str(), "resolved" | "ignored" | "open") {
                return Err(unprocessable("status", "must be one of open, resolved, ignored"));
            }
            let merged = self.write(|c| {
                let i = issue_model::find(c, id)?.expect("issue");
                match status.as_str() {
                    "resolved" => {
                        issue_model::resolve_by(c, &i, None, actor)?;
                    }
                    "ignored" => {
                        issue_model::ignore(c, &i, actor)?;
                    }
                    _ if i.status == "merged" => return Ok(Some(())),
                    _ => {
                        issue_model::reopen(c, &i, actor)?;
                    }
                }
                Ok(None)
            })?;
            if merged.is_some() {
                // unmerge! raises InvalidMerge on a broken chain: a 422 with
                // its message on status.
                match self.write(|c| Ok(issue_model::unmerge(c, id, actor)))? {
                    Ok(()) => {}
                    Err(issue_model::MergeError::Invalid(m)) => return Err(unprocessable("status", &m)),
                    Err(issue_model::MergeError::Sql(e)) => return Err(e.into()),
                }
            }
        }
        if p.present("priority") {
            let priority = p.s("priority");
            let valid = self.write(|c| {
                let i = issue_model::find(c, id)?.expect("issue");
                issue_model::set_priority(c, &i, &priority, actor)
            })?;
            if !valid {
                return Err(unprocessable("priority", "is not included in the list"));
            }
        }
        if p.has("assignee_id") {
            // assignee_from: a member of the issue's account (find raises 404).
            let to = if p.present("assignee_id") {
                let ids = cast_id(p.get("assignee_id"));
                let wanted = ids.first().copied().filter(|_| !p.get("assignee_id").is_array());
                let member: Option<(i64, String)> = match wanted {
                    Some(uid) => self.control(|c| {
                        c.query_row(
                            "SELECT u.id, u.name FROM users u INNER JOIN memberships m ON u.id = m.user_id
                             WHERE m.account_id = (SELECT ap.account_id FROM issues i JOIN applications ap ON ap.id = i.application_id WHERE i.id = ?)
                               AND u.id = ? LIMIT 1",
                            params![id, uid],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()
                    })?,
                    None => None,
                };
                Some(member.ok_or_else(not_found)?)
            } else {
                None
            };
            self.write(|c| {
                let i = issue_model::find(c, id)?.expect("issue");
                issue_model::set_assignee(c, &i, to.as_ref().map(|(u, n)| (*u, n.as_str())), actor)
            })?;
        }
        ok(self.issue_full(id)?)
    }

    /// IssueCommentsController#create: `issue.comments.create!(user:, body:)`,
    /// whose after_create leaves a "comment" activity by that user.
    fn create_comment(&self) -> R<Rendered> {
        let id = self.find_issue("issue_id")?;
        let body = self.params.opt("body");
        let Some(body) = body.filter(|b| rb::present(Some(b))) else { return Err(unprocessable("body", "can't be blank")) };
        let uid = self.token.user_id;
        let now = qt(self.now);
        let (cid, created_at) = self.write(|c| {
            c.execute(
                "INSERT INTO comments (body, created_at, issue_id, source, updated_at, user_id) VALUES (?, ?, ?, 'railwatch', ?, ?)",
                params![body, now, id, now, uid],
            )?;
            let cid = c.last_insert_rowid();
            issue_model::add_activity_by(
                c,
                id,
                "comment",
                Some(uid),
                crate::rhash! { "comment_id" => cid, "body" => body.as_str(), "source" => "railwatch", "author_name" => Option::<String>::None },
            )?;
            Ok((cid, now.clone()))
        })?;
        Ok(Rendered {
            status: StatusCode::CREATED,
            body: crate::obj! { "id" => cid, "body" => body.clone(), "user_id" => uid, "created_at" => db_time(Some(&created_at)) },
        })
    }
}

struct IssueRow {
    id: i64,
    key: String,
    title: String,
    kind: String,
    status: String,
    priority: String,
    culprit: Option<String>,
    occurrences: i64,
    affected_users: i64,
    first_seen_at: Option<String>,
    last_seen_at: Option<String>,
    assignee_id: Option<i64>,
    application_id: i64,
    environment_id: i64,
    sample: Value,
    resolved_at: Option<String>,
    resolved_in_deploy: Option<String>,
    regressed_at: Option<String>,
    group_hash: String,
    environment_slug: String,
}

impl IssueRow {
    /// issue_json(i) (the list shape).
    fn json(&self) -> Value {
        crate::obj! {
            "id" => self.id, "key" => self.key.clone(), "title" => self.title.clone(), "kind" => self.kind.clone(),
            "status" => self.status.clone(), "priority" => self.priority.clone(), "culprit" => self.culprit.clone(),
            "occurrences" => self.occurrences, "affected_users" => self.affected_users,
            "first_seen_at" => db_time(self.first_seen_at.as_deref()), "last_seen_at" => db_time(self.last_seen_at.as_deref()),
            "assignee_id" => self.assignee_id, "application_id" => self.application_id, "environment_id" => self.environment_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Shapes

/// `Query.between(from, to).where(group_hash: hashes).where.not(explain: nil)
/// .distinct.pluck(:group_hash)`, on the partial index of EXPLAINed rows:
/// without stats the planner took the group_hash index and read every row
/// of those groups in the window to test `explain` (queries@7d: 248 of
/// 306 ms of SQL).
fn explained_groups(conn: &Connection, from: i64, to: i64, hashes: &[String]) -> rusqlite::Result<Vec<String>> {
    let ph = vec!["?"; hashes.len()].join(", ");
    let mut binds: Vec<rusqlite::types::Value> = vec![qt(from).into(), qt(to).into()];
    binds.extend(hashes.iter().map(|h| h.clone().into()));
    Ok(db::query(
        conn,
        &format!(
            "SELECT DISTINCT \"queries\".\"group_hash\" FROM \"queries\" INDEXED BY idx_queries_explained WHERE \"queries\".\"occurred_at\" BETWEEN ?1 AND ?2 \
             AND \"queries\".\"group_hash\" IN ({ph}) AND \"queries\".\"explain\" IS NOT NULL"
        ),
        params_from_iter(binds),
    )?
    .iter()
    .filter_map(|r| r.s("group_hash").map(str::to_string))
    .collect())
}

fn meta(page: i64, per_page: i64, total: i64) -> Value {
    crate::obj! { "page" => page, "per_page" => per_page, "total" => total }
}

/// A control-DB datetime column as `as_json` renders it.
fn db_time(s: Option<&str>) -> Value {
    jt(s.and_then(crate::time::parse_db_time))
}

/// `paused_at.present?`
fn paused(s: Option<&str>) -> bool {
    s.is_some_and(|s| !s.is_empty())
}

fn sv(v: &rusqlite::types::Value) -> Value {
    db::raw_json(v)
}

/// application_json: id, name, slug, issue_prefix, account_id.
fn application_json(r: &[rusqlite::types::Value]) -> Value {
    crate::obj! { "id" => sv(&r[0]), "name" => sv(&r[1]), "slug" => sv(&r[2]), "issue_prefix" => sv(&r[3]), "account_id" => sv(&r[4]) }
}

/// deploy_json: id, deploy, ref, name, url, deployed_at.
fn deploy_json(r: &[rusqlite::types::Value]) -> Value {
    let at = match &r[5] {
        rusqlite::types::Value::Text(t) => db_time(Some(t)),
        _ => Value::Null,
    };
    crate::obj! { "id" => sv(&r[0]), "deploy" => sv(&r[1]), "ref" => sv(&r[2]), "name" => sv(&r[3]), "url" => sv(&r[4]), "deployed_at" => at }
}

/// summary_json: a summary_with_delta's durations (avg, p50, p95, p99,
/// max) from microseconds to milliseconds, rounded to 2 places.
fn summary_json(v: Value) -> Value {
    let Value::Object(m) = v else { return v };
    let out: Map<String, Value> = m
        .into_iter()
        .map(|(k, mut s)| {
            if let Value::Object(sm) = &mut s {
                for key in ["avg", "p50", "p95", "p99", "max"] {
                    if let Some(x) = sm.get_mut(key) {
                        let us = x.as_f64().unwrap_or(0.0);
                        *x = jf(rb::round(us / 1000.0, 2));
                    }
                }
            }
            (k, s)
        })
        .collect();
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, create_account, create_environment, insert_application, insert_user, issue_api_token, test_password};
    use crate::dashboard::CursorKey;
    use crate::dashboard::pool::ReadPools;
    use crate::tenants::Tenants;

    const NOW: i64 = 1_792_000_000_000_000;

    struct Fixture {
        dir: std::path::PathBuf,
        c: Control,
        tenants: Tenants,
        readers: ReadPools,
        key: CursorKey,
        limit: FixedWindow<(&'static str, i64)>,
        ada: i64,
        app: i64,
        env: i64,
        other_app: i64,
        other_env: i64,
        issue: i64,
        theirs: i64,
        tokens: std::collections::HashMap<&'static str, String>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("rw-api-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let eve = c.tx(|tx| insert_user(tx, "Eve", "eve@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let other = c.tx(|tx| create_account(tx, "Other", eve)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let second = c.tx(|tx| insert_application(tx, account, "Admin", "admin", "ADM", None, "main")).unwrap();
        let other_app = c.tx(|tx| insert_application(tx, other, "Else", "else", "ELSE", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let (other_env, _) = c.tx(|tx| create_environment(tx, other_app, "production")).unwrap();
        let issue_in = |app: i64, env: i64, n: i64| {
            c.with(|x| {
                x.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                       first_seen_at, last_seen_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'NoMethodError: 100% broken', 'exception', 'open', printf('%032d', ?2 * 1000 + ?3), 3, 1,
                       '2026-10-14 03:00:00', '2026-10-14 03:0' || ?3 || ':00', '2026-10-14 03:00:00', '2026-10-14 03:00:00')",
                    params![app, env, n],
                )?;
                Ok(x.last_insert_rowid())
            })
            .unwrap()
        };
        let issue = issue_in(app, env, 1);
        issue_in(app, env, 2);
        let theirs = issue_in(other_app, other_env, 1);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut tokens = std::collections::HashMap::new();
        let all = s(&["api", "mcp", "read", "write"]);
        for (name, scopes, acc, application, expires) in [
            ("full", all.clone(), None, None, None),
            ("read_only", s(&["api", "read"]), None, None, None),
            ("mcp_only", s(&["mcp", "read", "write"]), None, None, None),
            ("no_read", s(&["api", "write"]), None, None, None),
            ("second_app", all.clone(), None, Some(second), None),
            ("revoked", all.clone(), None, None, None),
            ("expired", all.clone(), Some(account), None, Some("2026-01-01 00:00:00".to_string())),
            ("security", s(&["api", "read", "security"]), None, None, None),
        ] {
            let (id, raw) = c.tx(|tx| issue_api_token(tx, ada, name, &scopes, acc, application, expires.clone())).unwrap();
            if name == "revoked" {
                c.with(|x| x.execute("UPDATE api_tokens SET revoked_at = '2026-01-01 00:00:00' WHERE id = ?", [id])).unwrap();
            }
            tokens.insert(name, raw);
        }
        let (followups, _rx) = crate::followups::channel();
        let tenants = Tenants::new(dir.join("telemetry"), std::time::Duration::from_secs(60), followups);
        let readers = ReadPools::new(dir.join("telemetry"), 1);
        Fixture { dir, c, tenants, readers, key: CursorKey(b"k".to_vec()), limit: rate_limit(), ada, app, env, other_app, other_env, issue, theirs, tokens }
    }

    fn req(f: &Fixture, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> ApiResponse {
        req_at(f, NOW, method, path, token, body)
    }

    fn req_at(f: &Fixture, now: i64, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> ApiResponse {
        let mut headers = HeaderMap::new();
        if let Some(t) = token {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", f.tokens[t])).unwrap());
        }
        if body.is_some() {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (path.to_string(), None),
        };
        let r = ApiRequest { method, path, query, headers, body: Bytes::from(body.map_or_else(String::new, |b| b.to_string())) };
        let env = Env { control: &f.c, tenants: &f.tenants, readers: &f.readers, cursor_key: &f.key };
        call(&env, &f.limit, now, &r)
    }

    fn get(f: &Fixture, path: &str, token: &str) -> (u16, Value) {
        let r = req(f, Method::GET, path, Some(token), None);
        (r.status.as_u16(), serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    }

    fn activities(f: &Fixture, issue: i64) -> Vec<(String, Option<i64>, Value)> {
        f.c.with(|x| {
            x.prepare("SELECT kind, user_id, data FROM issue_activities WHERE issue_id = ? ORDER BY id")?
                .query_map([issue], |r| Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&r.get::<_, String>(2)?).unwrap())))?
                .collect()
        })
        .unwrap()
    }

    #[test]
    fn a_token_must_be_active_and_carry_api_and_read() {
        let f = fixture("auth");
        let r = req(&f, Method::GET, "/api/v1/me", None, None);
        assert_eq!((r.status.as_u16(), r.body.as_slice()), (401, br#"{"error":"unauthorized"}"#.as_slice()));
        assert_eq!(r.headers[header::CACHE_CONTROL], "no-cache", "only 200/201 get an ETag and must-revalidate");
        assert_eq!(get(&f, "/api/v1/me", "revoked").0, 401, "a revoked token is not a token");
        assert_eq!(get(&f, "/api/v1/me", "expired").0, 401, "nor is an expired one");
        assert_eq!(get(&f, "/api/v1/me", "mcp_only"), (403, json!({"error": "insufficient_scope"})), "an MCP-only token cannot use the API");
        assert_eq!(get(&f, "/api/v1/me", "no_read").0, 403, "read is needed even to write");
        let (s, me) = get(&f, "/api/v1/me", "full");
        assert_eq!(s, 200);
        assert_eq!(me["user"], json!({"id": f.ada, "name": "Ada", "email": "ada@example.com"}));
        assert_eq!(me["token"]["scopes"], json!(["api", "mcp", "read", "write"]));
        let used: Option<String> = f.c.with(|x| x.query_row("SELECT last_used_at FROM api_tokens WHERE name = 'full'", [], |r| r.get(0))).unwrap();
        assert_eq!(used.as_deref(), Some(qt(NOW).as_str()), "record_use! stamps the token");
    }

    #[test]
    fn reads_are_scoped_to_memberships_and_the_token_restriction() {
        let f = fixture("scope");
        let (_, apps) = get(&f, "/api/v1/applications", "full");
        assert_eq!(apps["data"].as_array().unwrap().iter().map(|a| a["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["admin", "shop"], "by name");
        assert_eq!(apps["meta"], json!({"page": 1, "per_page": 25, "total": 2}));
        assert_eq!(get(&f, &format!("/api/v1/applications/{}", f.other_app), "full"), (404, json!({"error": "not found"})), "another account's app");
        assert_eq!(get(&f, &format!("/api/v1/applications/{}/environments/{}", f.app, f.other_env), "full").0, 404, "another app's environment");
        assert_eq!(get(&f, &format!("/api/v1/issues/{}", f.theirs), "full").0, 404, "another account's issue");
        // A token restricted to the second application sees only that one.
        let (_, apps) = get(&f, "/api/v1/applications", "second_app");
        assert_eq!(apps["meta"]["total"], 1);
        assert_eq!(get(&f, &format!("/api/v1/applications/{}", f.app), "second_app").0, 404);
        assert_eq!(get(&f, &format!("/api/v1/issues/{}", f.issue), "second_app").0, 404);
        assert_eq!(get(&f, &format!("/api/v1/applications/{}/environments/{}/summary", f.app, f.env), "second_app").0, 404);
        // Active Model's integer cast on ids: "1abc" finds 1, "abc" nothing.
        assert_eq!(get(&f, &format!("/api/v1/applications/{}abc", f.app), "full").0, 200);
        assert_eq!(get(&f, "/api/v1/applications/abc", "full").0, 404);
    }

    #[test]
    fn pagination_clamps_like_pagination_params() {
        let f = fixture("pages");
        assert_eq!(get(&f, "/api/v1/issues?page=-3&per_page=0", "full").1["meta"], json!({"page": 1, "per_page": 25, "total": 2}));
        assert_eq!(get(&f, "/api/v1/issues?per_page=500", "full").1["meta"]["per_page"], 200, "MAX_PER_PAGE");
        let (_, p2) = get(&f, "/api/v1/issues?per_page=1&page=2", "full");
        assert_eq!(p2["data"].as_array().unwrap().len(), 1);
        assert_eq!(p2["data"][0]["id"], f.issue, "recent: last_seen_at desc, so the first issue is on page 2");
        assert!(get(&f, "/api/v1/issues?page=99", "full").1["data"].as_array().unwrap().is_empty());
        // Filters: unknown status ignored.
        assert_eq!(get(&f, "/api/v1/issues?status=bogus", "full").1["meta"]["total"], 2);
        assert_eq!(get(&f, "/api/v1/issues?q=broken", "full").1["meta"]["total"], 2);
        assert_eq!(get(&f, "/api/v1/issues?assignee_id=abc", "full").1["meta"]["total"], 0, "assignee_id = NULL");
    }

    #[test]
    fn writes_need_write_scope_and_leave_rails_activities() {
        let f = fixture("writes");
        let path = format!("/api/v1/issues/{}", f.issue);
        let r = req(&f, Method::PATCH, &path, Some("read_only"), Some(json!({"status": "resolved"})));
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = req(&f, Method::PATCH, &path, Some("full"), Some(json!({"status": "resolved", "priority": "high", "assignee_id": f.ada})));
        assert_eq!(r.status, StatusCode::OK);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!((v["status"].as_str(), v["priority"].as_str(), v["assignee_id"].as_i64()), (Some("resolved"), Some("high"), Some(f.ada)));
        assert!(v["resolved_at"].is_string() && v["comments"] == json!([]));
        // Current.api_user: every activity is the token's user's.
        let acts = activities(&f, f.issue);
        assert_eq!(
            acts.iter().map(|a| (a.0.as_str(), a.1)).collect::<Vec<_>>(),
            [("status", Some(f.ada)), ("priority", Some(f.ada)), ("assignee", Some(f.ada))]
        );
        assert_eq!(acts[0].2, json!({"from": "open", "to": "resolved"}));
        let bad = req(&f, Method::PATCH, &path, Some("full"), Some(json!({"status": "merged"})));
        assert_eq!((bad.status.as_u16(), bad.body.as_slice()), (422, br#"{"errors":{"status":["must be one of open, resolved, ignored"]}}"#.as_slice()));
        let bad = req(&f, Method::PATCH, &path, Some("full"), Some(json!({"priority": "whenever"})));
        assert_eq!(bad.body.as_slice(), br#"{"errors":{"priority":["is not included in the list"]}}"#);
        assert_eq!(req(&f, Method::PATCH, &path, Some("full"), Some(json!({"assignee_id": 999}))).status, 404, "not a member");

        // Comments: the commenter's "comment" activity.
        let c = req(&f, Method::POST, &format!("{path}/comments"), Some("full"), Some(json!({"body": "On it <b>now</b>"})));
        assert_eq!(c.status, StatusCode::CREATED);
        let cv: Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(cv["user_id"], f.ada);
        let last = activities(&f, f.issue).pop().unwrap();
        assert_eq!((last.0.as_str(), last.1), ("comment", Some(f.ada)));
        assert_eq!(last.2["body"], "On it <b>now</b>");
        let blank = req(&f, Method::POST, &format!("{path}/comments"), Some("full"), Some(json!({"body": " "})));
        assert_eq!(blank.body.as_slice(), br#"{"errors":{"body":["can't be blank"]}}"#);
        assert_eq!(req(&f, Method::POST, &format!("{path}/comments"), Some("read_only"), Some(json!({"body": "x"}))).status, 403);
    }

    #[test]
    fn deploys_upsert_by_name_and_validate_the_url() {
        let f = fixture("deploys");
        let path = format!("/api/v1/applications/{}/environments/{}/deploys", f.app, f.env);
        assert_eq!(req(&f, Method::POST, &path, Some("read_only"), Some(json!({"deploy": "v1"}))).status, 403);
        assert_eq!(req(&f, Method::POST, &path, Some("full"), Some(json!({}))).status, 400, "params.require(:deploy)");
        let bad = req(&f, Method::POST, &path, Some("full"), Some(json!({"deploy": "v1", "url": "javascript:alert(1)"})));
        assert_eq!(bad.body.as_slice(), br#"{"errors":{"url":["must be a single http:// or https:// URL"]}}"#);
        let one = req(&f, Method::POST, &path, Some("full"), Some(json!({"deploy": "v1", "ref": "abc", "url": "https://ci.example.com/1"})));
        assert_eq!(one.status, StatusCode::CREATED);
        let one: Value = serde_json::from_slice(&one.body).unwrap();
        assert_eq!(one["deployed_at"], rb::t_json(NOW));
        let two: Value =
            serde_json::from_slice(&req_at(&f, NOW + 3_600_000_000, Method::POST, &path, Some("full"), Some(json!({"deploy": "v1", "ref": "def"}))).body)
                .unwrap();
        assert_eq!((two["id"].clone(), two["ref"].clone(), two["url"].clone()), (one["id"].clone(), json!("def"), Value::Null), "same row, fields reassigned");
        assert_eq!(two["deployed_at"], rb::t_json(NOW), "a re-post keeps deployed_at");
        let (_, list) = get(&f, &path, "full");
        assert_eq!(list["meta"]["total"], 1);
    }

    #[test]
    fn responses_carry_rack_etag_and_answer_if_none_match_with_304() {
        let f = fixture("etag");
        let r = req(&f, Method::GET, "/api/v1/accounts", Some("full"), None);
        let etag = r.headers[header::ETAG].to_str().unwrap().to_string();
        assert!(etag.starts_with("W/\"") && etag.len() == 36);
        assert_eq!(r.headers[header::CACHE_CONTROL], "max-age=0, private, must-revalidate");
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", f.tokens["full"])).unwrap());
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(&etag).unwrap());
        let env = Env { control: &f.c, tenants: &f.tenants, readers: &f.readers, cursor_key: &f.key };
        let again = call(&env, &f.limit, NOW, &ApiRequest { method: Method::GET, path: "/api/v1/accounts".into(), query: None, headers, body: Bytes::new() });
        assert_eq!((again.status.as_u16(), again.body.len()), (304, 0));
        let head = req(&f, Method::HEAD, "/api/v1/accounts", Some("full"), None);
        assert_eq!((head.status.as_u16(), head.body.len()), (200, 0));
    }

    #[test]
    fn unrouted_paths_and_bad_bodies_are_rails_error_pages() {
        let f = fixture("routes");
        let r = req(&f, Method::GET, "/api/v1/nope", Some("full"), None);
        assert_eq!((r.status.as_u16(), r.headers[header::CONTENT_TYPE].to_str().unwrap()), (404, "text/html; charset=UTF-8"));
        let r = req(&f, Method::GET, "/api/v1/nope.json", Some("full"), None);
        assert_eq!(r.body.as_slice(), br#"{"status":404,"error":"Not Found"}"#);
        assert_eq!(req(&f, Method::DELETE, &format!("/api/v1/issues/{}", f.issue), Some("full"), None).status, 404);
        assert_eq!(get(&f, "/api/v1/me.json", "full").0, 200, "(.:format)");
        assert_eq!(get(&f, "/api/v1/me/", "full").0, 200, "trailing slash");
        // A malformed JSON body: 400 page, but only once params are read.
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", f.tokens["full"])).unwrap());
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let env = Env { control: &f.c, tenants: &f.tenants, readers: &f.readers, cursor_key: &f.key };
        let bad = |path: &str, h: HeaderMap| {
            call(&env, &f.limit, NOW, &ApiRequest { method: Method::GET, path: path.into(), query: None, headers: h, body: Bytes::from("{bad") })
                .status
                .as_u16()
        };
        assert_eq!(bad("/api/v1/accounts", headers.clone()), 400);
        assert_eq!(bad("/api/v1/me", headers.clone()), 200, "MeController never reads params");
        let mut anon = headers.clone();
        anon.remove(header::AUTHORIZATION);
        assert_eq!(bad("/api/v1/accounts", anon), 401, "authentication runs before params are parsed");
        let _ = (f.other_env, f.env);
    }

    #[test]
    fn series_validates_type_and_step_and_the_rate_limit_is_per_user_and_controller() {
        let f = fixture("series");
        let base = format!("/api/v1/applications/{}/environments/{}/series", f.app, f.env);
        assert_eq!(get(&f, &base, "full"), (422, json!({"error": "type must be one of request, job_attempt, query, visit"})));
        assert_eq!(
            get(&f, &format!("{base}?type=request&step=1d"), "full"),
            (422, json!({"error": "step must be one of 5m, 15m, 1h, 6h for this window"})),
            "24h: 1m is 1,440 points, 1d one"
        );
        let (s, v) = get(&f, &format!("{base}?type=request&window=1h&step=5m"), "full");
        assert_eq!((s, v["data"].as_array().unwrap().len()), (200, 13), "an empty tenant still fills every bucket");
        let small = FixedWindow::new(2);
        let env = Env { control: &f.c, tenants: &f.tenants, readers: &f.readers, cursor_key: &f.key };
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", f.tokens["full"])).unwrap());
        let hit =
            |p: &str| call(&env, &small, NOW, &ApiRequest { method: Method::GET, path: p.into(), query: None, headers: headers.clone(), body: Bytes::new() });
        assert_eq!(hit("/api/v1/me").status, 200);
        assert_eq!(hit("/api/v1/me").status, 200);
        let third = hit("/api/v1/me");
        assert_eq!((third.status.as_u16(), third.body.as_slice()), (429, br#"{"error":"rate limit exceeded"}"#.as_slice()));
        assert_eq!(hit("/api/v1/accounts").status, 200, "each controller has its own counter");
    }

    /// Seeds initial-load visits with vitals (and navigations without) into
    /// the environment's tenant, rolls them up, and inserts deploys d1, d2.
    fn seed_visits(f: &Fixture) {
        let slug: String = f.c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [f.env], |r| r.get(0))).unwrap();
        let conn = crate::tenants::open_tenant(&f.dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        let at = qt(NOW - 2 * 3600 * 1_000_000);
        // (component, lcp, inp, cls, ttfb, deploy, duration ms)
        let loads: [(&str, i64, i64, f64, i64, &str, i64); 6] = [
            ("orders/index", 1000, 100, 0.05, 200, "d1", 900),
            ("orders/index", 2000, 150, 0.05, 300, "d2", 1100),
            ("orders/index", 3000, 300, 0.3, 400, "d2", 1500),
            ("orders/index", 5000, 600, 0.3, 900, "d2", 2600),
            ("home/show", 1200, 80, 0.01, 100, "d1", 700),
            ("home/show", 1500, 90, 0.02, 150, "d1", 800),
        ];
        for (c, lcp, inp, cls, ttfb, deploy, d) in loads {
            conn.execute(
                "INSERT INTO visits (component, group_hash, url, method, duration, status, occurred_at, deploy, lcp, inp, cls, ttfb)
                 VALUES (?1, substr(hex(?1) || '00000000000000000000000000000000', 1, 32), '/x', 'GET', ?2, 'success', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![c, d * 1000, at, deploy, lcp, inp, cls, ttfb],
            )
            .unwrap();
        }
        for d in [120, 340] {
            conn.execute(
                "INSERT INTO visits (component, group_hash, url, method, duration, status, occurred_at, deploy)
                 VALUES ('orders/index', substr(hex('orders/index') || '00000000000000000000000000000000', 1, 32), '/x', 'GET', ?1, 'success', ?2, 'd2')",
                params![d * 1000, at],
            )
            .unwrap();
        }
        for h in crate::rollup::rollup_job::hours_with_rows(&conn).unwrap() {
            crate::rollup::rollup_job::perform(&conn, &conn, h, &mut crate::rollup::rollup_job::Rng::PerGroup).unwrap();
        }
        for (deploy, hours_ago) in [("d1", 5), ("d2", 3)] {
            f.c.with(|x| {
                x.execute(
                    "INSERT INTO deploys (environment_id, deploy, ref, deployed_at, created_at, updated_at) VALUES (?1, ?2, ?2, ?3, ?3, ?3)",
                    params![f.env, deploy, qt(NOW - hours_ago * 3600 * 1_000_000)],
                )
            })
            .unwrap();
        }
    }

    /// The visits page's props for the fixture's environment at NOW.
    fn visits_page(f: &Fixture, params: &[(&str, &str)]) -> Value {
        let slug: String = f.c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [f.env], |r| r.get(0))).unwrap();
        let control = f.c.with(|c| super::super::dashboard_bridge::DbControl::load(c, f.env)).unwrap();
        let p: crate::dashboard::Params = params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let ctx = crate::dashboard::PageContext::new(f.env, NOW, p, f.key.clone());
        let env = Env { control: &f.c, tenants: &f.tenants, readers: &f.readers, cursor_key: &f.key };
        with_tenant(&env, &slug, |conn| pages::visits_index(conn, &ctx, &control).map_err(|_| rusqlite::Error::InvalidQuery)).unwrap()
    }

    fn mcp_tool(f: &Fixture, name: &str, args: Value) -> Value {
        let env = Env { control: &f.c, tenants: &f.tenants, readers: &f.readers, cursor_key: &f.key };
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
        let (status, v) = super::super::mcp::call(&env, NOW, Some(&format!("Bearer {}", f.tokens["full"])), &Bytes::from(body.to_string()));
        assert_eq!(status, StatusCode::OK);
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{v}"))).unwrap()
    }

    /// More than 100 components: a rarely visited one ranks past the
    /// 100-row cap by visit count, yet a component filter finds it and a
    /// worst-LCP sort puts it first.
    #[test]
    fn web_vitals_filter_and_sort_see_past_the_hundredth_component() {
        let f = fixture("vitals-many");
        let slug: String = f.c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [f.env], |r| r.get(0))).unwrap();
        let conn = crate::tenants::open_tenant(&f.dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        let at = qt(NOW - 2 * 3600 * 1_000_000);
        let insert = |c: &str, lcp: i64| {
            conn.execute(
                "INSERT INTO visits (component, group_hash, url, method, duration, status, occurred_at, lcp, inp, cls, ttfb)
                 VALUES (?1, substr(hex(?1) || '00000000000000000000000000000000', 1, 32), '/x', 'GET', 500000, 'success', ?2, ?3, 100, 0.01, 100)",
                params![c, at, lcp],
            )
            .unwrap();
        };
        // 120 busy components (3 visits each, good LCP) and one rare page
        // (1 visit, terrible LCP) that ranks last by visit count.
        for i in 0..120 {
            for _ in 0..3 {
                insert(&format!("busy/{i:03}"), 1000);
            }
        }
        insert("rare/slow", 9000);
        for h in crate::rollup::rollup_job::hours_with_rows(&conn).unwrap() {
            crate::rollup::rollup_job::perform(&conn, &conn, h, &mut crate::rollup::rollup_job::Rng::PerGroup).unwrap();
        }

        let mcp = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "window": "24h", "sort": "lcp"}));
        let names: Vec<&str> = mcp["components"].as_array().unwrap().iter().map(|c| c["component"].as_str().unwrap()).collect();
        assert_eq!(names.first(), Some(&"rare/slow"), "worst LCP first, though 121st by visits");
        assert_eq!(names.len(), 20, "the tool's default limit");

        let one = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "window": "24h", "component": "rare/slow"}));
        let rows = one["components"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{one}");
        assert_eq!(rows[0]["vitals"]["lcp"]["value"], 9000);

        let (s, api) = get(&f, &format!("/api/v1/applications/{}/environments/{}/visits?window=24h&component=rare%2Fslow", f.app, f.env), "full");
        assert_eq!(s, 200, "{api}");
        assert_eq!(api["data"].as_array().map(Vec::len), Some(1), "{api}");
    }

    #[test]
    fn web_vitals_match_the_visits_page_in_the_api_and_mcp() {
        let f = fixture("vitals");
        seed_visits(&f);
        let page = visits_page(&f, &[("window", "24h")]);
        let (s, api) = get(&f, &format!("/api/v1/applications/{}/environments/{}/visits?window=24h", f.app, f.env), "full");
        assert_eq!(s, 200, "{api}");
        let mcp = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "window": "24h"}));

        // The p75 over the six initial loads (index ceil(6 * 0.75) - 1 = 4 of
        // the sorted values), rated with web.dev's thresholds.
        let expected = json!({
            "lcp": {"value": 3000, "rating": "needs-improvement", "samples": 6},
            "cls": {"value": 0.3, "rating": "poor", "samples": 6},
            "inp": {"value": 300, "rating": "needs-improvement", "samples": 6},
            "ttfb": {"value": 400, "rating": "good", "samples": 6},
        });
        assert_eq!(page["vitals"].to_string(), expected.to_string(), "the page itself");
        assert_eq!(api["vitals"].to_string(), page["vitals"].to_string(), "the API reports the page's vitals");
        assert_eq!(mcp["vitals"].to_string(), page["vitals"].to_string(), "MCP reports the page's vitals");
        assert_eq!(api["thresholds"]["lcp"].to_string(), r#"{"good":2500.0,"poor":4000.0,"unit":"ms"}"#);

        // Components: the page's visit counts, duration p95 and per-component LCP/INP p75.
        let row = |name: &str, count: &Value, p95: &Value, lcp: &Value, inp: &Value| json!([name, count, p95, lcp, inp]).to_string();
        let page_rows: Vec<String> =
            page["components"].as_array().unwrap().iter().map(|c| row(c["name"].as_str().unwrap(), &c["count"], &c["p95"], &c["lcp"], &c["inp"])).collect();
        let api_rows: Vec<String> = api["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| row(c["name"].as_str().unwrap(), &c["count"], &c["p95"], &c["vitals"]["lcp"]["value"], &c["vitals"]["inp"]["value"]))
            .collect();
        let mcp_rows: Vec<String> = mcp["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| row(c["component"].as_str().unwrap(), &c["visits"], &c["p95_ms"], &c["vitals"]["lcp"]["value"], &c["vitals"]["inp"]["value"]))
            .collect();
        assert_eq!(page_rows.len(), 2);
        assert!(page_rows[0].starts_with(r#"["orders/index",6,"#) && page_rows[0].ends_with(",3000,300]"), "{page_rows:?}");
        assert!(page_rows[1].starts_with(r#"["home/show",2,"#) && page_rows[1].ends_with(",1500,90]"), "{page_rows:?}");
        assert_eq!(api_rows, page_rows, "API components are the page's");
        assert_eq!(mcp_rows, page_rows, "MCP components are the page's");
        assert_eq!(api["meta"], json!({"page": 1, "per_page": 25, "total": 2}));

        // Per release: d2 shipped the slow orders loads.
        let releases: Vec<(String, Value)> =
            mcp["releases"].as_array().unwrap().iter().map(|r| (r["deploy"].as_str().unwrap().to_string(), r["vitals"]["lcp"].clone())).collect();
        assert_eq!(
            json!(releases).to_string(),
            json!([["d2", {"value": 5000, "rating": "poor", "samples": 3}], ["d1", {"value": 1500, "rating": "good", "samples": 3}]]).to_string()
        );
        let api_deploys: Vec<&str> = api["releases"].as_array().unwrap().iter().map(|r| r["deploy"].as_str().unwrap()).collect();
        assert_eq!(api_deploys, ["d2", "d1"]);
        assert_eq!(api["releases"][0]["vitals"].to_string(), mcp["releases"][0]["vitals"].to_string());
        assert_eq!(api["releases"][1]["vitals"].to_string(), mcp["releases"][1]["vitals"].to_string());

        // The worst pages first: sort by a vital's p75.
        let by_lcp = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "sort": "lcp", "dir": "asc"}));
        let names: Vec<&str> = by_lcp["components"].as_array().unwrap().iter().map(|c| c["component"].as_str().unwrap()).collect();
        assert_eq!(names, ["home/show", "orders/index"]);
        let (_, api_by_inp) = get(&f, &format!("/api/v1/applications/{}/environments/{}/visits?sort=inp", f.app, f.env), "full");
        assert_eq!(api_by_inp["data"][0]["name"], "orders/index");

        // One component: its vitals are the page's component row, its releases only its loads.
        let orders = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "component": "orders/index"}));
        assert_eq!(orders["components"].as_array().unwrap().len(), 1);
        assert_eq!(orders["vitals"]["lcp"]["value"], page["components"][0]["lcp"]);
        assert_eq!(orders["vitals"]["lcp"]["samples"], 4);
        assert_eq!(orders["releases"][1]["vitals"]["lcp"].to_string(), r#"{"value":1000,"rating":"good","samples":1}"#);

        // The visit series is the page's chart.
        let (s, series) = get(&f, &format!("/api/v1/applications/{}/environments/{}/series?type=visit&window=24h", f.app, f.env), "full");
        assert_eq!(s, 200);
        assert_eq!(series["data"].to_string(), page["series"].to_string(), "the page's visit chart");

        // No load in this fixture split its TTFB (a gem before 1.2).
        assert_eq!(page["ttfb_phases"], json!({}));
        assert_eq!(mcp["ttfb_phases"], json!({}));
    }

    /// Initial loads that split their TTFB (gem 1.2+): a slow first byte that
    /// is all redirects shows as redirect time, the same on the page, the API
    /// and MCP, overall, per component and per release. A load whose browser
    /// hid a cross-origin chain (no count) still counts its redirect time,
    /// and only the counted ones make up `redirected_at_least`.
    #[test]
    fn ttfb_phases_show_redirect_time_on_the_page_the_api_and_mcp() {
        let f = fixture("phases");
        let slug: String = f.c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [f.env], |r| r.get(0))).unwrap();
        let conn = crate::tenants::open_tenant(&f.dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        let at = qt(NOW - 2 * 3600 * 1_000_000);
        // (component, deploy, ttfb, redirect_ms, redirect_count, connect_ms, server_ms)
        type Load<'a> = (&'a str, &'a str, i64, i64, Option<i64>, i64, i64);
        let loads: [Load; 5] = [
            ("auth/sign_in", "d2", 1300, 1180, Some(2), 20, 100), // / -> /sign_in, same origin
            ("auth/sign_in", "d2", 1250, 1130, None, 20, 100),    // the old host first: cross-origin, hidden
            ("auth/sign_in", "d2", 1400, 1200, None, 60, 140),    // hidden too
            ("auth/sign_in", "d1", 140, 3, None, 30, 107),        // typed /sign_in: no redirect
            ("orders/index", "d1", 160, 2, None, 40, 118),
        ];
        for (c, deploy, ttfb, redirect_ms, redirect_count, connect_ms, server_ms) in loads {
            conn.execute(
                "INSERT INTO visits (component, group_hash, url, method, duration, status, occurred_at, deploy, ttfb, redirect_ms, redirect_count, connect_ms, server_ms)
                 VALUES (?1, substr(hex(?1) || '00000000000000000000000000000000', 1, 32), '/x', 'GET', 900000, 'success', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![c, at, deploy, ttfb, redirect_ms, redirect_count, connect_ms, server_ms],
            )
            .unwrap();
        }
        // A load from a gem without phases: no sample.
        conn.execute(
            "INSERT INTO visits (component, group_hash, url, method, duration, status, occurred_at, deploy, ttfb)
             VALUES ('orders/index', substr(hex('orders/index') || '00000000000000000000000000000000', 1, 32), '/x', 'GET', 900000, 'success', ?1, 'd1', 2000)",
            [&at],
        )
        .unwrap();
        for h in crate::rollup::rollup_job::hours_with_rows(&conn).unwrap() {
            crate::rollup::rollup_job::perform(&conn, &conn, h, &mut crate::rollup::rollup_job::Rng::PerGroup).unwrap();
        }
        for (deploy, hours_ago) in [("d1", 5), ("d2", 3)] {
            f.c.with(|x| {
                x.execute(
                    "INSERT INTO deploys (environment_id, deploy, ref, deployed_at, created_at, updated_at) VALUES (?1, ?2, ?2, ?3, ?3, ?3)",
                    params![f.env, deploy, qt(NOW - hours_ago * 3600 * 1_000_000)],
                )
            })
            .unwrap();
        }

        let page = visits_page(&f, &[("window", "24h")]);
        let (s, api) = get(&f, &format!("/api/v1/applications/{}/environments/{}/visits?window=24h", f.app, f.env), "full");
        assert_eq!(s, 200, "{api}");
        let mcp = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "window": "24h"}));

        // p75 of 5 samples is the 4th smallest (index ceil(5 * 0.75) - 1 = 3).
        let overall = json!({"samples": 5, "redirect_ms": 1180, "connect_ms": 40, "server_ms": 118, "redirected_at_least": 0.2});
        assert_eq!(page["ttfb_phases"], overall, "the page");
        assert_eq!(api["ttfb_phases"], overall, "the API");
        assert_eq!(mcp["ttfb_phases"], overall, "MCP");

        // Per component: the sign-in page's first byte is redirects.
        let sign_in = mcp["components"].as_array().unwrap().iter().find(|c| c["component"] == "auth/sign_in").unwrap();
        // 4 samples: the 3rd smallest. redirect_ms [3, 1130, 1180, 1200].
        assert_eq!(sign_in["ttfb_phases"], json!({"samples": 4, "redirect_ms": 1180, "connect_ms": 30, "server_ms": 107, "redirected_at_least": 0.25}));
        let page_row = page["components"].as_array().unwrap().iter().find(|c| c["name"] == "auth/sign_in").unwrap();
        assert_eq!(page_row["redirect_ms"], 1180, "the page's component table");
        let api_row = api["data"].as_array().unwrap().iter().find(|c| c["name"] == "auth/sign_in").unwrap();
        assert_eq!(api_row["ttfb_phases"], sign_in["ttfb_phases"]);

        // Per release: d2 is where the redirects came in.
        let by_deploy: Vec<(String, Value)> =
            mcp["releases"].as_array().unwrap().iter().map(|r| (r["deploy"].as_str().unwrap().to_string(), r["ttfb_phases"]["redirect_ms"].clone())).collect();
        assert_eq!(json!(by_deploy), json!([["d2", 1200], ["d1", 3]]));
        assert_eq!(api["releases"][0]["ttfb_phases"], mcp["releases"][0]["ttfb_phases"]);

        // One component: its phases only.
        let orders = mcp_tool(&f, "web_vitals", json!({"application_slug": "shop", "environment": "production", "component": "orders/index"}));
        assert_eq!(orders["ttfb_phases"]["samples"], 1);

        // Each recent visit carries its own split; NULL count stays null.
        let hidden = page["recent"].as_array().unwrap().iter().find(|v| v["redirect_ms"] == 1130).unwrap();
        assert_eq!((hidden["redirect_count"].clone(), hidden["server_ms"].clone()), (Value::Null, json!(100)));
    }

    #[test]
    fn issues_q_matches_percent_and_underscore_literally() {
        let f = fixture("like");
        f.c.with(|x| {
            x.execute("UPDATE issues SET title = 'digest_daily missed' WHERE id = ?", [f.issue])?;
            x.execute("UPDATE issues SET title = 'digestXdaily missed' WHERE id != ? AND application_id = ?", params![f.issue, f.app])
        })
        .unwrap();
        let titles = |q: &str| -> Vec<String> {
            get(&f, &format!("/api/v1/issues?q={q}"), "full").1["data"].as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(titles("_"), ["digest_daily missed"], "_ is a literal underscore");
        assert_eq!(titles("digest_daily"), ["digest_daily missed"], "not digestXdaily");
        f.c.with(|x| x.execute("UPDATE issues SET title = 'NoMethodError: 100% broken' WHERE id = ?", [f.issue])).unwrap();
        f.c.with(|x| x.execute("UPDATE issues SET title = 'NoMethodError: 1000 broken' WHERE id != ? AND application_id = ?", params![f.issue, f.app]))
            .unwrap();
        assert_eq!(titles("100%25"), ["NoMethodError: 100% broken"], "% is a literal percent");
    }

    #[test]
    fn non_scalar_pagination_params_read_as_absent() {
        let f = fixture("arrays");
        for q in ["page[]=1", "per_page[]=1", "page[a]=2&per_page[]=1"] {
            let (s, v) = get(&f, &format!("/api/v1/issues?{q}"), "full");
            assert_eq!((s, v["meta"].clone()), (200, json!({"page": 1, "per_page": 25, "total": 2})), "{q}");
        }
        assert_eq!(get(&f, "/api/v1/accounts?page[]=1", "full").0, 200);
        let base = format!("/api/v1/applications/{}/environments/{}", f.app, f.env);
        let (s, v) = get(&f, &format!("{base}/logs?limit[]=1"), "full");
        assert_eq!((s, v["results_meta"]["limit"].clone()), (200, json!(50)), "limit[] is the default 50");
        // A JSON number is a value (number_param keeps Numeric), not absent.
        let r = req(&f, Method::GET, "/api/v1/issues", Some("full"), Some(json!({"page": 1, "per_page": 1})));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!((r.status.as_u16(), v["meta"].clone()), (200, json!({"page": 1, "per_page": 1, "total": 2})));
    }

    #[test]
    fn exceptions_and_logs_run_only_the_pagination_asked_for() {
        let f = fixture("raw");
        let base = format!("/api/v1/applications/{}/environments/{}", f.app, f.env);
        let keys = |q: &str, resource: &str| -> Vec<String> {
            let (s, v) = get(&f, &format!("{base}/{resource}{q}"), "full");
            assert_eq!(s, 200, "{resource}{q}");
            v.as_object().unwrap().keys().cloned().collect()
        };
        for resource in ["exceptions", "logs"] {
            assert_eq!(keys("", resource), ["data", "meta", "results", "results_meta"], "neither: both");
            assert_eq!(keys("?limit=5", resource), ["results", "results_meta"]);
            assert_eq!(keys("?cursor=", resource), ["results", "results_meta"], "presence, not value");
            assert_eq!(keys("?page=2", resource), ["data", "meta"]);
            assert_eq!(keys("?per_page[]=1", resource), ["data", "meta"], "a non-scalar still counts as present");
            assert_eq!(keys("?page=1&limit=5", resource), ["data", "meta", "results", "results_meta"], "both families: both");
        }
    }

    #[test]
    fn api_writes_are_attributed_to_the_token_user_in_alerts() {
        let f = fixture("actor");
        f.c.with(|x| {
            x.execute(
                "INSERT INTO integrations (account_id, created_at, enabled, kind, name, settings, updated_at)
                 SELECT account_id, '2026-01-01', 1, 'webhook', 'hook', '{\"url\":\"https://example.com/h\"}', '2026-01-01' FROM applications WHERE id = ?",
                [f.app],
            )?;
            let integ = x.last_insert_rowid();
            for event in ["resolved_issue", "assigned_issue"] {
                x.execute(
                    "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?, '2026-01-01', ?, '{}', ?, '2026-01-01')",
                    params![f.app, event, integ],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let path = format!("/api/v1/issues/{}", f.issue);
        assert_eq!(req(&f, Method::PATCH, &path, Some("full"), Some(json!({"status": "resolved", "assignee_id": f.ada}))).status, 200);
        let actors: Vec<(String, Option<String>)> =
            f.c.with(|x| {
                x.prepare("SELECT event, json_extract(payload, '$.actor') FROM alerts ORDER BY id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
            })
            .unwrap();
        assert_eq!(actors, [("resolved_issue".into(), Some("Ada".into())), ("assigned_issue".into(), Some("Ada".into()))]);
    }

    #[test]
    fn reopening_a_merged_issue_with_a_broken_chain_is_a_422() {
        let f = fixture("unmerge");
        let others: Vec<i64> =
            f.c.with(|x| {
                x.prepare("SELECT id FROM issues WHERE application_id = ? AND id != ? ORDER BY id")?.query_map(params![f.app, f.issue], |r| r.get(0))?.collect()
            })
            .unwrap();
        let target = others[0];
        // Merged into an issue that is itself merged: unmerge top-down first.
        f.c
            .with(|x| {
                x.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                       first_seen_at, last_seen_at, created_at, updated_at)
                     VALUES (?1, ?2, 3, 'Top', 'exception', 'open', 'top', 1, 1, '2026-10-14 03:00:00', '2026-10-14 03:00:00', '2026-10-14 03:00:00', '2026-10-14 03:00:00')",
                    params![f.app, f.env],
                )?;
                let third = x.last_insert_rowid();
                x.execute("UPDATE issues SET status = 'merged', merged_into_id = ? WHERE id = ?", params![target, f.issue])?;
                x.execute("UPDATE issues SET status = 'merged', merged_into_id = ? WHERE id = ?", params![third, target])
            })
            .unwrap();
        let r = req(&f, Method::PATCH, &format!("/api/v1/issues/{}", f.issue), Some("full"), Some(json!({"status": "open"})));
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            serde_json::from_slice::<Value>(&r.body).unwrap(),
            json!({"errors": {"status": ["cannot unmerge this issue yet; unmerge SHOP-2 first (top-down order)"]}})
        );
        // The chain changed under the client (the target was itself unmerged
        // and our row's status no longer says merged): "issue merge changed".
        f.c.with(|x| x.execute("UPDATE issues SET status = 'merged', merged_into_id = NULL WHERE id = ?", [target])).unwrap();
        f.c.with(|x| x.execute("UPDATE issues SET status = 'open', merged_into_id = NULL WHERE id = ?", [target])).unwrap();
        f.c.with(|x| x.execute("UPDATE issues SET status = 'merged' WHERE id = ?", [f.issue])).unwrap();
        let ok = req(&f, Method::PATCH, &format!("/api/v1/issues/{}", f.issue), Some("full"), Some(json!({"status": "open"})));
        assert_eq!(ok.status, StatusCode::OK, "an intact chain unmerges");
        f.c.with(|x| x.execute("UPDATE issues SET status = 'merged', merged_into_id = NULL WHERE id = ?", [f.issue])).unwrap();
        let r = req(&f, Method::PATCH, &format!("/api/v1/issues/{}", f.issue), Some("full"), Some(json!({"status": "open"})));
        assert_eq!(r.body.as_slice(), br#"{"errors":{"status":["cannot unmerge an issue that is not merged"]}}"#);
    }

    /// ip: and ua: need the security scope (403 without it, the same call
    /// 200 with it); a bad request-key value is 422; the requests table
    /// under a request key comes from the raw requests and says how many
    /// were read; jobs, queries, exceptions and logs take the keys too.
    #[test]
    fn request_keys_on_the_api_need_the_security_scope_for_personal_data() {
        let f = fixture("request-keys");
        let slug: String = f.c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [f.env], |r| r.get(0))).unwrap();
        let conn = crate::tenants::open_tenant(&f.dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        for (i, (ip, path, status)) in [("198.51.100.7", "/.env", 404), ("198.51.100.7", "/wp-login.php", 404), ("10.0.0.1", "/orders", 200)].iter().enumerate()
        {
            let detail = json!({"ip": ip, "path": path, "user_agent": "curl/8"}).to_string();
            conn.execute(
                "INSERT INTO executions (execution_id, kind, name, method, group_hash, status, occurred_at, duration, detail) VALUES (?1, 'request', ?2, 'GET', ?3, ?4, ?5, 1000, ?6)",
                params![format!("e{i}"), format!("GET {path}"), format!("g{i}"), status, qt(NOW - 60_000_000), detail],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO exceptions (execution_id, execution_source, class_name, message, handled, occurred_at) VALUES ('e0', 'request', 'Trap', 'x', 1, ?1)",
            [qt(NOW - 60_000_000)],
        )
        .unwrap();
        let base = format!("/api/v1/applications/{}/environments/{}", f.app, f.env);
        for path in ["requests", "jobs", "queries", "exceptions", "logs"] {
            for q in ["ip:198.51.100.7", "ua:curl"] {
                let (status, body) = get(&f, &format!("{base}/{path}?q={q}"), "read_only");
                assert_eq!((status, body["error"].as_str()), (403, Some("insufficient_scope")), "{path} {q}");
                assert!(body["detail"].as_str().unwrap().contains("security scope"));
                assert_eq!(get(&f, &format!("{base}/{path}?q={q}"), "security").0, 200, "{path} {q}");
            }
            assert_eq!(get(&f, &format!("{base}/{path}?q=probe:maybe"), "read_only").0, 422, "{path}");
        }
        let (_, probes) = get(&f, &format!("{base}/requests?q=probe:true"), "read_only");
        let names: Vec<&str> = probes["data"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 2, "{probes}");
        assert!(names.contains(&"GET /.env") && names.contains(&"GET /wp-login.php"));
        assert_eq!(probes["scan"], json!({"scanned": 2, "capped": false}));
        let (_, ex) = get(&f, &format!("{base}/exceptions?q=ip:198.51.100.7"), "security");
        assert_eq!(ex["data"][0]["class_name"], "Trap");
        let (_, none) = get(&f, &format!("{base}/exceptions?q=ip:10.0.0.1"), "security");
        assert!(none["data"].as_array().unwrap().is_empty());
        // Without a request key the table is the rollups' as before (no scan).
        assert!(get(&f, &format!("{base}/requests"), "read_only").1.get("scan").is_none());
    }
}
