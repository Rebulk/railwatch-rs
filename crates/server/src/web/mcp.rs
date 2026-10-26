//! McpController (railwatch-cloud @ 464f0fd) plus ApiTokenAuthenticated:
//! the JSON-RPC 2.0 MCP server over POST /mcp (Streamable HTTP, no SSE),
//! and the public discovery document at GET /mcp and
//! GET /.well-known/mcp.json.
//!
//! Tool, prompt and argument definitions are Rails' own constants, dumped
//! from the running app into `mcp_definitions.json` (TOOLS, PROMPTS,
//! WRITE_TOOLS, and the three prompt texts with \u{1}name\u{1} markers
//! where Rails interpolates). The gem docs served as resources are the
//! cloud's `docs/gem` copy, in `mcp_docs/`.
//!
//! Every result is the text `JSON.generate` writes, so times inside tool
//! results are `Time#to_s` ("2026-10-15 02:20:00 UTC"), not `as_json`.
//! The results are shaped for an agent, not Rails' (mcp_summary.rs):
//! get_execution and last_execution lead with a summary and page their
//! timeline, get_issue collapses framework frames, records carry a
//! dashboard `url`, and every result is capped.
//! `call` is synchronous and takes `now`: the server runs it on the
//! blocking pool, the diff harness (bin/mcp_dump.rs) at a frozen instant.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value, json};

use super::WebState;
use super::mcp_summary;

/// The answer-shaped tools (investigate_issue, explain_route, ...): each one
/// call that answers a question the low-level tools take several to.
#[path = "mcp_answers.rs"]
mod answers;
use crate::control::Control;
use crate::dashboard::cursor_page::{self, CursorKey, Order};
use crate::dashboard::db::{self, Rec, jf, qt};
use crate::dashboard::filter_query::{self, Resource};
use crate::dashboard::pool::ReadPools;
use crate::dashboard::window::Window;
use crate::dashboard::{models, rb, rollup};
use crate::rate_limit::FixedWindow;
use crate::tenants::Tenants;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "railwatch-cloud";
pub const SERVER_VERSION: &str = "1.1.0";
const DOCS_RESOURCE: &str = "railwatch://docs/ai-and-mcp";
/// PREAUTH_IP_LIMIT per PREAUTH_IP_WINDOW.
pub const PREAUTH_IP_LIMIT: u64 = 300;
const PREAUTH_IP_WINDOW_SECS: i64 = 60;
/// ApiToken::LAST_USED_WRITE_INTERVAL.
const LAST_USED_WRITE_INTERVAL: i64 = 5 * 60 * 1_000_000;
/// Token::id of a trusted-proxy identity, which has no api_tokens row.
const PROXIED_TOKEN_ID: i64 = 0;
const TELEMETRY_RESOURCES: [&str; 4] = ["jobs", "exceptions", "logs", "queries"];
/// Alert::STATUSES, which list_alerts filters by (wider than the schema's enum).
const ALERT_STATUSES: [&str; 7] = ["pending", "delivering", "sent", "failed", "skipped", "collapsed", "summarized"];

struct Definitions {
    tools: Vec<Value>,
    prompts: Vec<Value>,
    write_tools: Vec<String>,
    prompt_texts: Map<String, Value>,
}

fn definitions() -> &'static Definitions {
    static D: OnceLock<Definitions> = OnceLock::new();
    D.get_or_init(|| {
        let v: Value = serde_json::from_str(include_str!("mcp_definitions.json")).expect("mcp_definitions.json");
        Definitions {
            tools: v["tools"].as_array().cloned().unwrap_or_default(),
            prompts: v["prompts"].as_array().cloned().unwrap_or_default(),
            write_tools: v["write_tools"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)).collect(),
            prompt_texts: v["prompt_texts"].as_object().cloned().unwrap_or_default(),
        }
    })
}

/// `docs/gem/*.md` as {name => text}, sorted by path (Dir[] + sort), names
/// matching DOC_NAME.
fn gem_docs() -> &'static Vec<(String, &'static str)> {
    static D: OnceLock<Vec<(String, &'static str)>> = OnceLock::new();
    D.get_or_init(|| {
        static DIR: include_dir::Dir<'static> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/web/mcp_docs");
        let mut files: Vec<(&str, &'static str)> = DIR
            .files()
            .filter_map(|f| {
                let name = f.path().file_name()?.to_str()?;
                Some((name, f.contents_utf8()?))
            })
            .filter(|(n, _)| n.ends_with(".md"))
            .collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        let mut out: Vec<(String, &'static str)> = vec![];
        for (file, text) in files {
            let name = file.trim_end_matches(".md").to_lowercase();
            // DOC_NAME = /\A[a-z0-9][a-z0-9._-]*\z/; a later file with the same
            // downcased name replaces the earlier (Hash#[]=).
            let ok = name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
            if ok {
                match out.iter_mut().find(|(n, _)| *n == name) {
                    Some(slot) => slot.1 = text,
                    None => out.push((name, text)),
                }
            }
        }
        out
    })
}

// ---------------------------------------------------------------------------
// HTTP

pub fn routes() -> Router<Arc<WebState>> {
    Router::new().route("/mcp", get(show).post(create)).route("/.well-known/mcp.json", get(show))
}

fn json_body(status: StatusCode, v: &Value) -> Response {
    // render json: escapes <, >, & (Active Support's encoder).
    (status, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], crate::rjson::escape_html(&v.to_string())).into_response()
}

/// GET /mcp, GET /.well-known/mcp.json: no token needed.
async fn show(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    let base = base_url(&state, &headers);
    json_body(StatusCode::OK, &discovery(&base))
}

/// The request's own scheme and host, as `mcp_url`/`settings_profile_url`
/// would build them: the configured url's scheme with the Host header. Behind
/// a trusted proxy, X-Forwarded-Proto names the scheme: the railwatch-server
/// gem runs the server on plain http and serves its app over https.
fn base_url(state: &WebState, headers: &HeaderMap) -> String {
    let configured = state.config.url.trim_end_matches('/');
    let forwarded = headers.get("x-forwarded-proto").and_then(|h| h.to_str().ok()).filter(|p| state.config.trust_proxy && matches!(*p, "http" | "https"));
    let scheme = forwarded.unwrap_or_else(|| configured.split_once("://").map_or("http", |(s, _)| s));
    let base_path = &state.config.base_path;
    match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(host) => format!("{scheme}://{host}{base_path}"),
        None => format!("{configured}{base_path}"),
    }
}

pub fn discovery(base: &str) -> Value {
    let d = definitions();
    json!({
        "name": SERVER_NAME, "version": SERVER_VERSION, "protocolVersion": PROTOCOL_VERSION,
        "description": "Railwatch Cloud monitoring for Rails: applications, issues, requests, queries, profiles, logs, deploys, release health, and browser web vitals.",
        "transport": "streamable-http", "endpoint": format!("{base}/mcp"),
        "authentication": {"type": "http-bearer", "header": "Authorization",
                           "description": "Per-user API & MCP token, generated at Settings -> Profile (\"API & MCP tokens\"). Needs the mcp and read scopes; the write tools also need write.",
                           "tokenUrl": format!("{base}/settings/profile")},
        "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
        "tools": d.tools.iter().map(|t| t["name"].clone()).collect::<Vec<_>>(),
        "prompts": d.prompts.iter().map(|p| p["name"].clone()).collect::<Vec<_>>(),
        "resources": ["railwatch://applications", "railwatch://applications/<slug>/environments/<name>/summary", "railwatch://docs/<name>"],
        "documentation": DOCS_RESOURCE,
    })
}

/// What `call` needs besides the request.
pub struct Env<'a> {
    pub control: &'a Control,
    pub tenants: &'a Tenants,
    pub readers: &'a ReadPools,
    pub cursor_key: &'a CursorKey,
}

/// POST /mcp.
async fn create(State(state): State<Arc<WebState>>, req: Request) -> Response {
    // rate_limit to: 300, within: 1.minute, before the token lookup.
    let ip = state.client_ip(&req);
    if state.mcp_preauth_limit.exceeded(&ip) {
        return json_body(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({"jsonrpc": "2.0", "id": null,
            "error": {"code": -32029, "message": "Rate limit exceeded", "data": {"retry_after": PREAUTH_IP_WINDOW_SECS}}}),
        );
    }
    let (parts, body) = req.into_parts();
    let auth = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_string);
    let proxied = match Token::proxied_claims(&parts) {
        Ok(p) => p,
        Err(refused) => return json_body(StatusCode::FORBIDDEN, &refused),
    };
    let base = base_url(&state, &parts.headers);
    let Ok(bytes) = axum::body::to_bytes(body, 4 << 20).await else {
        return json_body(StatusCode::PAYLOAD_TOO_LARGE, &json!({"error": "body too large"}));
    };
    let s = state.clone();
    let (status, v) = tokio::task::spawn_blocking(move || {
        let env = Env { control: &s.control, tenants: &s.tenants, readers: &s.readers, cursor_key: &s.cursor_key };
        call_as(&env, &base, crate::time::now_micros(), auth.as_deref(), proxied.as_ref(), &bytes)
    })
    .await
    .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("mcp task failed: {e}")})));
    match v {
        Value::Null => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response(),
        v => json_body(status, &v),
    }
}

pub fn preauth_limit() -> FixedWindow<std::net::IpAddr> {
    FixedWindow::with_window(PREAUTH_IP_LIMIT, std::time::Duration::from_secs(PREAUTH_IP_WINDOW_SECS as u64))
}

// ---------------------------------------------------------------------------
// The request, end to end

/// The authenticated token: its user and its restrictions. Shared with the
/// JSON API (web/api.rs): ApiTokenAuthenticated is one concern in Rails.
pub struct Token {
    pub id: i64,
    pub user_id: i64,
    pub scopes: Vec<String>,
    pub account_id: Option<i64>,
    pub application_id: Option<i64>,
    pub name: String,
    pub expires_at: Option<String>,
    last_used_at: Option<i64>,
}

impl Token {
    /// ApiToken#allows?
    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// `authenticate_user!`'s lookup: the Bearer token of an Authorization
    /// header (/\ABearer ([^\s]+)\z/), active, by SHA-256 digest. Err is a
    /// database failure.
    pub fn from_authorization(control: &Control, authorization: Option<&str>, now: i64) -> rusqlite::Result<Option<Token>> {
        let raw = authorization.and_then(|a| a.strip_prefix("Bearer ")).map(|t| t.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']));
        let raw = raw.filter(|t| !t.is_empty() && !t.chars().any(char::is_whitespace));
        match raw {
            Some(t) if t.len() <= 256 => control.with(|c| authenticate(c, t, now)),
            _ => Ok(None),
        }
    }

    /// The request's verified trusted-proxy claims (proxy_auth::layer put
    /// them there and removed the header), for /mcp and /api/v1. A proxied
    /// request the browser marks cross-origin is refused (the JSON body is
    /// Err): the proxy signs whatever passes the host app's gate, and with
    /// no CSRF token on these routes the Origin check is what keeps another
    /// site's page from calling them through a signed-in browser (the MCP
    /// transport's "servers MUST validate the Origin header"). Agents send
    /// no Origin.
    pub fn proxied_claims(parts: &axum::http::request::Parts) -> Result<Option<super::proxy_auth::Claims>, Value> {
        let Some(super::proxy_auth::Proxied(claims)) = parts.extensions.get::<super::proxy_auth::Proxied>() else { return Ok(None) };
        if !super::session::same_origin(&parts.headers) {
            tracing::warn!(path = parts.uri.path(), "trusted proxy: cross-origin MCP/API request refused");
            return Err(json!({"error": "cross_origin"}));
        }
        // Any Authorization header, readable or not, is the credential: one
        // that is not ASCII is a bad token (401), not a fall back to the proxy.
        if parts.headers.contains_key(header::AUTHORIZATION) {
            return Ok(None);
        }
        Ok(Some(claims.clone()))
    }

    /// Whether this is a trusted-proxy identity rather than a token row.
    pub fn is_proxied(&self) -> bool {
        self.id == PROXIED_TOKEN_ID
    }

    /// The request's identity on /mcp and /api/v1: an Authorization header,
    /// when there is one, is the only credential looked at (a bad token is
    /// 401 whatever else the request carries); without one, the trusted
    /// proxy's verified claims (`from_proxied`). Err is a database failure.
    pub fn from_request(
        control: &Control,
        authorization: Option<&str>,
        proxied: Option<&super::proxy_auth::Claims>,
        now: i64,
    ) -> rusqlite::Result<Option<Token>> {
        match (authorization, proxied) {
            (None, Some(claims)) => Self::from_proxied(control, claims),
            _ => Self::from_authorization(control, authorization, now),
        }
    }

    /// A token for a user the trusted proxy names: the user and session
    /// proxy_auth::resolve gives the dashboard, with what that user can do
    /// in the dashboard (read, and write issues and comments; not deploys,
    /// which api::run refuses it), limited to the account the proxy fronts.
    /// Not a row: nothing to revoke or record.
    pub fn from_proxied(control: &Control, claims: &super::proxy_auth::Claims) -> rusqlite::Result<Option<Token>> {
        let resolved = control.tx(|tx| {
            let (_, user) = super::proxy_auth::resolve(tx, claims)?;
            let account: i64 = tx.query_row("SELECT id FROM accounts ORDER BY id LIMIT 1", [], |r| r.get(0))?;
            Ok((user.id, account))
        });
        match resolved {
            Ok((user_id, account)) => Ok(Some(Token {
                id: PROXIED_TOKEN_ID,
                user_id,
                scopes: ["mcp", "api", "read", "write"].map(String::from).to_vec(),
                account_id: Some(account),
                application_id: None,
                name: super::proxy_auth::SESSION_AGENT.to_string(),
                expires_at: None,
                last_used_at: None,
            })),
            Err(crate::control::ModelError::Sql(e)) => Err(e),
            // No account yet: setup has not run. Nobody to be.
            Err(crate::control::ModelError::Invalid(m)) => {
                tracing::warn!(?m, "trusted proxy: no identity for MCP/API");
                Ok(None)
            }
        }
    }

    /// ApiToken#record_use!: last_used_at at most every five minutes.
    pub fn record_use(&self, control: &Control, now: i64) {
        if self.is_proxied() {
            return;
        }
        if self.last_used_at.is_none_or(|t| t <= now - LAST_USED_WRITE_INTERVAL) {
            let at = qt(now);
            let _ = control.with(|c| c.execute("UPDATE api_tokens SET last_used_at = ?1, updated_at = ?1 WHERE id = ?2", params![at, self.id]));
        }
    }

    /// SQL for the ids of accessible accounts (accessible_accounts), with
    /// its binds: the owner's memberships, narrowed to the token's
    /// application's account, else its account.
    pub fn accounts_sql(&self, control: &Control) -> (String, Vec<rusqlite::types::Value>) {
        let (from, binds) = self.accounts_from(control);
        (format!("SELECT accounts.id {from}"), binds)
    }

    /// accessible_accounts without its select list: "FROM ... WHERE ...".
    pub fn accounts_from(&self, control: &Control) -> (String, Vec<rusqlite::types::Value>) {
        let restricted: Option<i64> = match self.application_id {
            Some(app) => control.with(|c| c.query_row("SELECT account_id FROM applications WHERE id = ?", [app], |r| r.get(0)).optional()).ok().flatten(),
            None => None,
        }
        .or(self.account_id);
        let mut sql = String::from("FROM accounts INNER JOIN memberships ON accounts.id = memberships.account_id WHERE memberships.user_id = ?");
        let mut binds: Vec<rusqlite::types::Value> = vec![self.user_id.into()];
        if let Some(a) = restricted {
            sql.push_str(" AND accounts.id = ?");
            binds.push(a.into());
        }
        (sql, binds)
    }

    /// SQL for the ids of accessible applications, with its binds.
    pub fn applications_sql(&self, control: &Control) -> (String, Vec<rusqlite::types::Value>) {
        let (from, binds) = self.applications_from(control);
        (format!("SELECT applications.id {from}"), binds)
    }

    /// accessible_applications without its select list.
    pub fn applications_from(&self, control: &Control) -> (String, Vec<rusqlite::types::Value>) {
        let (acc, mut binds) = self.accounts_sql(control);
        let mut sql = format!("FROM applications WHERE applications.account_id IN ({acc})");
        if let Some(app) = self.application_id {
            sql.push_str(" AND applications.id = ?");
            binds.push(app.into());
        }
        (sql, binds)
    }
}

/// Authenticates, parses and dispatches one POST. Returns the status and
/// body; a `Value::Null` body is Rails' HTML 500 page (an exception the
/// controller does not rescue). Dashboard links in the results are paths.
pub fn call(env: &Env, now: i64, authorization: Option<&str>, body: &Bytes) -> (StatusCode, Value) {
    call_at(env, "", now, authorization, body)
}

/// `call`, with every record's dashboard `url` starting at `base`: the
/// public scheme, host and base path ("https://railwatch.example.com").
pub fn call_at(env: &Env, base: &str, now: i64, authorization: Option<&str>, body: &Bytes) -> (StatusCode, Value) {
    call_as(env, base, now, authorization, None, body)
}

/// `call_at`, for a request that may carry the trusted proxy's verified
/// claims instead of a token (Token::from_request).
pub fn call_as(env: &Env, base: &str, now: i64, authorization: Option<&str>, proxied: Option<&super::proxy_auth::Claims>, body: &Bytes) -> (StatusCode, Value) {
    let token = match Token::from_request(env.control, authorization, proxied, now) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "mcp token lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, Value::Null);
        }
    };
    let Some(token) = token else { return (StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"})) };
    if !(token.allows("mcp") && token.allows("read")) {
        return (StatusCode::FORBIDDEN, json!({"error": "insufficient_scope"}));
    }
    token.record_use(env.control, now);

    // The raw body is JSON whatever the Content-Type (a form-encoded POST of
    // a JSON-RPC body works); valid JSON that is not an object is -32600.
    let Ok(payload) = serde_json::from_slice::<Value>(body) else {
        return (StatusCode::BAD_REQUEST, json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}));
    };
    if !payload.is_object() {
        return (StatusCode::BAD_REQUEST, json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Invalid Request"}}));
    }
    let mcp = Mcp { env, now, token, base: base.trim_end_matches('/').to_string() };
    match mcp.dispatch_rpc(&payload) {
        Some(v) => (StatusCode::OK, v),
        None => (StatusCode::INTERNAL_SERVER_ERROR, Value::Null),
    }
}

/// ApiToken.authenticate: active, by SHA-256 digest.
fn authenticate(c: &Connection, raw: &str, now: i64) -> rusqlite::Result<Option<Token>> {
    c.query_row(
        "SELECT id, user_id, scopes, account_id, application_id, last_used_at, name, expires_at FROM api_tokens
         WHERE revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?1) AND token_digest = ?2 LIMIT 1",
        params![qt(now), crate::control::digest(raw)],
        |r| {
            let scopes: String = r.get(2)?;
            Ok(Token {
                id: r.get(0)?,
                user_id: r.get(1)?,
                scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                account_id: r.get(3)?,
                application_id: r.get(4)?,
                last_used_at: r.get::<_, Option<String>>(5)?.and_then(|t| crate::time::parse_db_time(&t)),
                name: r.get(6)?,
                expires_at: r.get(7)?,
            })
        },
    )
    .optional()
}

/// A tool's failure, mapped to its JSON-RPC error.
enum Fail {
    /// AmbiguousIdentifier / InvalidCursor: -32602 with the message.
    Invalid(String),
    /// Anything else: -32603 "Internal error".
    Internal(String),
    /// An exception McpController does not rescue (raised outside call_tool):
    /// Rails' HTML 500 page.
    Unrescued,
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Fail {
        Fail::Internal(e.to_string())
    }
}

type R<T> = Result<T, Fail>;

struct Mcp<'a> {
    env: &'a Env<'a>,
    now: i64,
    token: Token,
    /// What dashboard links start with ("" for paths).
    base: String,
}

fn success(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Ruby's `hash["k"]` on whatever JSON.parse returned: a String or an Array
/// receiver behaves differently (String#[] is a substring search, Array#[]
/// raises TypeError), which `payload["id"]` on a non-Hash body exposes.
fn index(v: &Value, key: &str) -> Result<Value, ()> {
    match v {
        Value::Object(m) => Ok(m.get(key).cloned().unwrap_or(Value::Null)),
        Value::String(s) => Ok(if s.contains(key) { Value::String(key.to_string()) } else { Value::Null }),
        _ => Err(()),
    }
}

impl Mcp<'_> {
    fn dispatch_rpc(&self, payload: &Value) -> Option<Value> {
        let id = index(payload, "id").ok()?;
        // `params` absent or null is {}; anything else but an object is -32602.
        let params = match index(payload, "params").ok()? {
            Value::Null => Value::Object(Map::new()),
            p @ Value::Object(_) => p,
            _ => return Some(error(&id, -32602, "Invalid params")),
        };
        let method = index(payload, "method").ok()?;
        let r = match method.as_str() {
            Some("initialize") => Ok(success(
                &id,
                json!({"protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "capabilities": {"tools": {}, "prompts": {}, "resources": {}}}),
            )),
            Some("tools/list") => Ok(success(&id, json!({"tools": self.visible_tools()}))),
            Some("tools/call") => Ok(self.call_tool(&id, &params)),
            Some("prompts/list") => Ok(success(&id, json!({"prompts": definitions().prompts}))),
            Some("prompts/get") => self.get_prompt(&id, &params),
            Some("resources/list") => self.resources().map(|r| success(&id, json!({"resources": r}))),
            Some("resources/read") => self.read_resource(&id, &params),
            _ => Ok(error(&id, -32601, "Method not found")),
        };
        match r {
            Ok(v) => Some(v),
            Err(Fail::Invalid(m)) => Some(error(&id, -32602, &m)),
            Err(Fail::Internal(e)) => {
                tracing::error!(error = %e, "mcp request failed");
                None
            }
            Err(Fail::Unrescued) => None,
        }
    }

    fn write_tool(name: &str) -> bool {
        definitions().write_tools.iter().any(|t| t == name)
    }

    fn visible_tools(&self) -> Vec<Value> {
        let all = &definitions().tools;
        if self.token.allows("write") { all.clone() } else { all.iter().filter(|t| !Self::write_tool(t["name"].as_str().unwrap_or(""))).cloned().collect() }
    }

    fn call_tool(&self, id: &Value, params: &Value) -> Value {
        let name = match index(params, "name") {
            Ok(n) => n,
            Err(()) => return error(id, -32603, "Internal error"),
        };
        let name_s = name.as_str().unwrap_or("");
        let known = definitions().tools.iter().any(|t| t["name"].as_str() == Some(name_s)) && name.is_string();
        if !known {
            return error(id, -32601, &format!("Unknown tool: {}", ruby_interpolate(&name)));
        }
        if Self::write_tool(name_s) && !self.token.allows("write") {
            return error(id, -32003, "Token does not have write scope");
        }
        let args = match index(params, "arguments") {
            Ok(Value::Null) | Ok(Value::Bool(false)) => Map::new(),
            Ok(Value::Object(m)) => m,
            // send(method, [..]) -> args["x"] on an Array or String: TypeError
            // (Array) or a substring (String); both end as a rescued error.
            _ => return error(id, -32603, "Internal error"),
        };
        let a = Args(&args);
        let result = match name_s {
            "list_applications" => self.list_applications(),
            "list_issues" => self.list_issues(&a),
            "get_issue" => self.get_issue(&a),
            "list_slow_routes" => self.list_slow_routes(&a),
            "get_route" => self.get_route(&a),
            "get_profile" => self.get_profile(&a),
            "search_requests" => self.search_requests(&a),
            "explain_query" => self.explain_query(&a),
            "list_tenants" => self.list_tenants(&a),
            "get_execution" => self.get_execution(&a),
            "last_execution" => self.last_execution(&a),
            "search_logs" => self.search_logs(&a),
            "search_telemetry" => self.search_telemetry(&a).map(|v| v.to_string()),
            "release_health" => self.release_health(&a),
            "web_vitals" => self.web_vitals(&a),
            "recent_deploys" => self.recent_deploys(&a),
            "list_alerts" => self.list_alerts(&a),
            "investigate_issue" => self.investigate_issue(&a),
            "explain_route" => self.explain_route(&a),
            "update_issue" => self.update_issue(&a),
            "add_comment" => self.add_comment(&a),
            _ => unreachable!("known tool"),
        };
        match result {
            Ok(text) => success(id, json!({"content": [{"type": "text", "text": mcp_summary::cap(text, mcp_summary::MAX_RESULT_BYTES, narrow_hint(name_s))}]})),
            Err(Fail::Invalid(m)) => error(id, -32602, &m),
            Err(Fail::Internal(e)) => {
                tracing::error!(tool = name_s, error = %e, "mcp tool failed");
                error(id, -32603, "Internal error")
            }
            Err(Fail::Unrescued) => error(id, -32603, "Internal error"),
        }
    }
}

/// `"#{value}"` for a JSON value (nil is "", an Array/Hash its inspect).
fn ruby_interpolate(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => {
            let rv = crate::rjson::parse(other.to_string().as_bytes()).unwrap_or(crate::rjson::Value::Nil);
            String::from_utf8_lossy(&crate::rjson::ruby::to_s(&rv)).into_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Arguments: `args["x"]` with Ruby's present?/to_s/to_i

struct Args<'a>(&'a Map<String, Value>);

impl Args<'_> {
    fn get(&self, k: &str) -> &Value {
        self.0.get(k).unwrap_or(&Value::Null)
    }
    fn has(&self, k: &str) -> bool {
        self.0.contains_key(k)
    }
    /// `args[k].present?`
    fn present(&self, k: &str) -> bool {
        present(self.get(k))
    }
    /// `args[k].to_s`
    fn s(&self, k: &str) -> String {
        to_s(self.get(k))
    }
    /// `args[k].presence` as a string.
    fn presence(&self, k: &str) -> Option<String> {
        self.present(k).then(|| self.s(k))
    }
}

pub(crate) fn present(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !crate::rjson::ruby::blank(s.as_bytes()),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

/// `value.to_s` for a JSON value.
pub(crate) fn to_s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            let t = n.to_string();
            match n.as_f64() {
                Some(f) if !n.is_i64() && !n.is_u64() => crate::rjson::ruby::float_to_s(f),
                _ => t,
            }
        }
        other => ruby_interpolate(other),
    }
}

/// `value.to_i` (Integer, Float or String receivers; anything else raises).
fn to_i(v: &Value) -> Result<i64, Fail> {
    ruby_to_i(v).ok_or_else(|| Fail::Internal("undefined method 'to_i'".into()))
}

/// `value.to_i` on a JSON value; None where Ruby raises NoMethodError
/// (true/false, an Array, a Hash).
pub(crate) fn ruby_to_i(v: &Value) -> Option<i64> {
    match v {
        Value::Null => Some(0),
        Value::Number(n) => Some(n.as_i64().unwrap_or_else(|| n.as_f64().map_or(0, |f| f.trunc() as i64))),
        Value::String(s) => Some(cursor_page::ruby_to_i(s).unwrap_or(0)),
        _ => None,
    }
}

/// `[value.to_i.nonzero?&.abs || default, max].min`
fn clamp(v: &Value, default: i64, max: i64) -> Result<i64, Fail> {
    let i = to_i(v)?;
    Ok(if i == 0 { default } else { i.abs() }.min(max))
}

/// `/\A[1-9]\d*\z/.match?(value.to_s)`
fn positive_integer(v: &Value) -> bool {
    let s = to_s(v);
    let b = s.as_bytes();
    !b.is_empty() && (b'1'..=b'9').contains(&b[0]) && b.iter().all(u8::is_ascii_digit)
}

/// `(microseconds / 1000.0).round(2)`, nil stays nil.
fn ms(us: Option<i64>) -> Value {
    us.map_or(Value::Null, |u| jf(rb::round(u as f64 / 1000.0, 2)))
}

/// `Time#to_s` of a UTC time: "2026-10-15 02:20:00 UTC".
fn time_s(micros: Option<i64>) -> Value {
    micros.map_or(Value::Null, |m| Value::String(format!("{} UTC", crate::time::utc_seconds_string(m.div_euclid(1_000_000)))))
}

/// A datetime column read off the control DB, as Time#to_s.
fn db_time_s(s: Option<String>) -> Value {
    time_s(s.as_deref().and_then(crate::time::parse_db_time))
}

/// Rewrites every `as_json` time string ("2026-10-15T02:20:00.123Z") in a
/// row built by the dashboard's `as_row` ports into Time#to_s, the form
/// JSON.generate writes for a Time.
fn to_s_times(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Some(t) = as_json_time(s) {
                *s = format!("{} UTC", crate::time::utc_seconds_string(t.div_euclid(1_000_000)));
            }
        }
        Value::Array(a) => a.iter_mut().for_each(to_s_times),
        Value::Object(o) => o.values_mut().for_each(to_s_times),
        _ => {}
    }
}

/// Parses exactly "YYYY-MM-DDTHH:MM:SS.mmmZ" (rb::t_json's output).
fn as_json_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 24 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'.' || b[23] != b'Z' {
        return None;
    }
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22];
    if !digits.iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    crate::time::parse_db_time(&s[..23])
}

/// Times in a row: the named keys only, so a string column that happens to
/// look like a time is left alone.
fn row_times(mut v: Value, keys: &[&str]) -> Value {
    if let Value::Object(m) = &mut v {
        for k in keys {
            if let Some(x) = m.get_mut(*k) {
                to_s_times(x);
            }
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Scope: ApiTokenAuthenticated#accessible_accounts / _applications

struct App {
    id: i64,
    slug: String,
    name: String,
    account_slug: String,
}

struct EnvRow {
    id: i64,
    name: String,
    slug: String,
    application_id: i64,
    app_slug: String,
    account_slug: String,
    last_seen_at: Option<String>,
}

impl Mcp<'_> {
    fn accounts_sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        self.token.accounts_sql(self.env.control)
    }

    fn applications_sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        self.token.applications_sql(self.env.control)
    }

    fn accessible_applications(&self) -> R<Vec<App>> {
        let (ids, binds) = self.applications_sql();
        let sql = format!(
            "SELECT ap.id, ap.slug, ap.name, ac.slug FROM applications ap JOIN accounts ac ON ac.id = ap.account_id
             WHERE ap.id IN ({ids}) ORDER BY ap.name ASC"
        );
        Ok(self.env.control.with(|c| {
            c.prepare(&sql)?
                .query_map(params_from_iter(binds), |r| Ok(App { id: r.get(0)?, slug: r.get(1)?, name: r.get(2)?, account_slug: r.get(3)? }))?
                .collect()
        })?)
    }

    fn environments_of(&self, app: i64) -> R<Vec<(String, String, i64)>> {
        Ok(self.env.control.with(|c| {
            c.prepare("SELECT name, slug, id FROM environments WHERE application_id = ? ORDER BY id")?
                .query_map([app], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })?)
    }

    fn application_identity_given(a: &Args) -> bool {
        ["application_slug", "account_slug", "application_id"].iter().any(|k| a.present(k))
    }

    /// resolve_application: slug, narrowed by account_slug / application_id;
    /// two matches is AmbiguousIdentifier. With neither slug nor id, the one
    /// application the token can see (in account_slug, when given): an
    /// embedded server has exactly one, and an agent should not have to look
    /// it up first.
    fn resolve_application(&self, a: &Args) -> R<Option<App>> {
        let slug = a.s("application_slug");
        let app_id = a.get("application_id");
        let slug_blank = crate::rjson::ruby::blank(slug.as_bytes());
        let defaulting = slug_blank && !present(app_id);
        if present(app_id) && !positive_integer(app_id) {
            return Ok(None);
        }
        let (ids, mut binds) = self.applications_sql();
        let mut sql = format!("SELECT ap.id, ap.slug, ap.name, ac.slug FROM applications ap JOIN accounts ac ON ac.id = ap.account_id WHERE ap.id IN ({ids})");
        if !slug_blank {
            sql.push_str(" AND ap.slug = ?");
            binds.push(slug.clone().into());
        }
        if present(app_id) {
            sql.push_str(" AND ap.id = ?");
            binds.push(to_s(app_id).into());
        }
        if a.present("account_slug") {
            let (acc, b) = self.accounts_sql();
            sql.push_str(&format!(" AND ap.account_id IN ({acc} AND accounts.slug = ?)"));
            binds.extend(b);
            binds.push(a.s("account_slug").into());
        }
        sql.push_str(" LIMIT 2");
        let mut matches: Vec<App> = self.env.control.with(|c| {
            c.prepare(&sql)?
                .query_map(params_from_iter(binds), |r| Ok(App { id: r.get(0)?, slug: r.get(1)?, name: r.get(2)?, account_slug: r.get(3)? }))?
                .collect()
        })?;
        if matches.len() > 1 {
            if defaulting {
                return Ok(None);
            }
            return Err(Fail::Invalid(format!(
                "Application slug {} exists in more than one of your accounts; pass account_slug alongside it, or pass application_id",
                ruby_inspect(&slug)
            )));
        }
        Ok(matches.pop())
    }

    /// resolve_environment: by name, then by slug, within the application;
    /// with no environment given, the application's only one.
    fn resolve_environment(&self, a: &Args) -> R<Option<EnvRow>> {
        let Some(app) = self.resolve_application(a)? else { return Ok(None) };
        let wanted = a.s("environment");
        let row = |r: &rusqlite::Row<'_>| {
            Ok(EnvRow {
                id: r.get(0)?,
                name: r.get(1)?,
                slug: r.get(2)?,
                application_id: app.id,
                app_slug: app.slug.clone(),
                account_slug: app.account_slug.clone(),
                last_seen_at: r.get(3)?,
            })
        };
        Ok(self.env.control.with(|c| {
            if !a.present("environment") {
                let mut only: Vec<EnvRow> = c
                    .prepare("SELECT id, name, slug, last_seen_at FROM environments WHERE application_id = ? LIMIT 2")?
                    .query_map([app.id], row)?
                    .collect::<rusqlite::Result<_>>()?;
                return Ok(if only.len() == 1 { only.pop() } else { None });
            }
            let find = |col: &str| {
                c.query_row(
                    &format!("SELECT id, name, slug, last_seen_at FROM environments WHERE application_id = ? AND {col} = ? LIMIT 1"),
                    params![app.id, wanted],
                    row,
                )
                .optional()
            };
            match find("name")? {
                Some(e) => Ok(Some(e)),
                None => find("slug"),
            }
        })?)
    }

    /// The "environment not found" result, saying what to pass instead and
    /// listing what this token can see.
    fn no_environment(&self, a: &Args) -> R<String> {
        Ok(self.no_environment_value(a)?.to_string())
    }

    fn no_environment_value(&self, a: &Args) -> R<Value> {
        let apps = self.accessible_applications()?;
        let mut available = vec![];
        for app in apps.iter().take(20) {
            let envs: Vec<String> = self.environments_of(app.id)?.into_iter().map(|(n, _, _)| n).collect();
            available.push(json!({"application_slug": app.slug, "account_slug": app.account_slug, "application_id": app.id, "environments": envs}));
        }
        let hint = match self.resolve_application(a)? {
            None if apps.is_empty() => "This token can see no application. Create one in the dashboard, or use a token of a member of its account.".to_string(),
            None if !Self::application_identity_given(a) => {
                "This token can see more than one application: pass application_slug (and environment) from available.".to_string()
            }
            None => "No application by that name for this token: pass an application_slug from available.".to_string(),
            Some(app) => {
                let envs: Vec<String> = self.environments_of(app.id)?.into_iter().map(|(n, _, _)| n).collect();
                if a.present("environment") {
                    format!("{} has no environment {}: pass one of {}.", app.slug, ruby_inspect(&a.s("environment")), envs.join(", "))
                } else {
                    format!("{} has more than one environment: pass environment, one of {}.", app.slug, envs.join(", "))
                }
            }
        };
        Ok(json!({"error": "environment not found", "hint": hint, "available": available}))
    }

    /// A dashboard link: the public base URL (or nothing, for paths) and a
    /// path as the dashboard's router has it.
    fn url(&self, path: &str) -> Value {
        Value::String(format!("{}{path}", self.base))
    }

    /// models::execution_row with Time#to_s times and the dashboard link.
    fn execution_row(&self, env: &EnvRow, r: &Rec) -> Value {
        let mut v = row_times(models::execution_row(r), &["occurred_at"]);
        db::merge_into(&mut v, "url", self.execution_url(env, r.s("kind"), r.s("execution_id")));
        v
    }

    fn execution_url(&self, env: &EnvRow, kind: Option<&str>, execution_id: Option<&str>) -> Value {
        match execution_id {
            Some(eid) if !eid.is_empty() && kind != Some("browser") => self.url(&execution_path(env.application_id, env.id, kind, eid)),
            _ => Value::Null,
        }
    }

    /// `environment.with_telemetry { ... }` over the tenant's read connection
    /// (an empty schema-only database when the environment has none yet,
    /// as create_telemetry_database would leave it).
    fn with_telemetry<T>(&self, env: &EnvRow, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
        with_tenant(self.env, &env.slug, f)
    }

    fn window(&self, a: &Args) -> (i64, i64) {
        // Window.preset(key): a preset key (to_s), else the default.
        let key = a.s("window");
        Window::preset(Some(&key), self.now).range()
    }
}

/// `environment.with_telemetry { ... }` over the tenant's read connection
/// (an empty schema-only database when the environment has none yet, as
/// create_telemetry_database would leave it). Shared with web/api.rs.
pub(crate) fn with_tenant<T, E: From<rusqlite::Error>>(env: &Env, slug: &str, f: impl FnOnce(&Connection) -> Result<T, E>) -> Result<T, E> {
    if env.tenants.layout(slug).is_some() {
        let reader = env.readers.get(slug)?;
        f(&reader)
    } else {
        let mut empty = Connection::open_in_memory()?;
        crate::schema::create_tenant(&mut empty, &crate::control::now_db())?;
        f(&empty)
    }
}

/// IssueExceptionPresenter#as_json's two lookups: the sample occurrence
/// (`sample["exception_id"]`, when present) and the latest one.
pub(crate) fn issue_exception_occurrences(conn: &Connection, group_hash: &str, sample: &Value) -> rusqlite::Result<(Option<Rec>, Option<Rec>)> {
    let sample_id = sample.get("exception_id").cloned().unwrap_or(Value::Null);
    let first = if present(&sample_id) {
        db::query_one(
            conn,
            "SELECT \"exceptions\".* FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 AND \"exceptions\".\"id\" = ?2 LIMIT 1",
            params![group_hash, to_s(&sample_id)],
        )?
    } else {
        None
    };
    let last = db::query_one(
        conn,
        "SELECT \"exceptions\".* FROM \"exceptions\" WHERE \"exceptions\".\"group_hash\" = ?1 ORDER BY \"exceptions\".\"occurred_at\" DESC LIMIT 1",
        [group_hash],
    )?;
    Ok((first, last))
}

/// What get_profile and the execution summary say when there is no profile.
pub(super) const PROFILE_SETUP: &str = "To capture profiles, the app needs the vernier (Ruby 3.2+) or stackprof gem and RAILWATCH_PROFILE_SAMPLE_RATE (a fraction of executions to profile), or RAILWATCH_PROFILE_SLOW_MS together with RAILWATCH_TAIL_SAMPLE_SLOW_MS (profile the slow ones).";

/// list_issues' sort: impact, with the last bind as now.
const IMPACT_SQL: &str = "(i.occurrences * 1.0 / (1 + MAX(0, julianday(?) - julianday(i.last_seen_at))))";

const ISSUE_NOT_FOUND_HINT: &str = "Pass key as list_issues returns it (e.g. SHOP-12, or its canonical_key account_slug/application_id/SHOP-12), or issue_id. list_issues with status open, resolved or ignored finds it.";

fn not_found(what: &str) -> R<String> {
    Ok(json!({"error": format!("{what} not found")}).to_string())
}

/// `String#inspect` for the plain strings these messages quote.
fn ruby_inspect(s: &str) -> String {
    String::from_utf8_lossy(&crate::rjson::ruby::inspect(&crate::rjson::Value::from(s))).into_owned()
}

// ---------------------------------------------------------------------------
// Issues

/// One issue row with what issue_json needs.
struct IssueRow {
    id: i64,
    key: String,
    title: String,
    kind: String,
    status: String,
    priority: String,
    occurrences: i64,
    affected_users: i64,
    culprit: Option<String>,
    first_seen_at: Option<String>,
    last_seen_at: Option<String>,
    account_slug: String,
    application_id: i64,
    application_slug: String,
    account_id: i64,
    environment_id: i64,
    environment_name: String,
    environment_slug: String,
    group_hash: String,
    sample: String,
}

const ISSUE_SELECT: &str = "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title, i.kind, i.status, i.priority, i.occurrences, i.affected_users, i.culprit,
        i.first_seen_at, i.last_seen_at, ac.slug, i.application_id, ap.slug, ap.account_id, i.environment_id, e.name, e.slug, i.group_hash, i.sample
     FROM issues i JOIN applications ap ON ap.id = i.application_id JOIN accounts ac ON ac.id = ap.account_id JOIN environments e ON e.id = i.environment_id";

fn issue_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<IssueRow> {
    Ok(IssueRow {
        id: r.get(0)?,
        key: r.get(1)?,
        title: r.get(2)?,
        kind: r.get(3)?,
        status: r.get(4)?,
        priority: r.get(5)?,
        occurrences: r.get(6)?,
        affected_users: r.get(7)?,
        culprit: r.get(8)?,
        first_seen_at: r.get(9)?,
        last_seen_at: r.get(10)?,
        account_slug: r.get(11)?,
        application_id: r.get(12)?,
        application_slug: r.get(13)?,
        account_id: r.get(14)?,
        environment_id: r.get(15)?,
        environment_name: r.get(16)?,
        environment_slug: r.get(17)?,
        group_hash: r.get(18)?,
        sample: r.get(19)?,
    })
}

impl IssueRow {
    fn canonical_key(&self) -> String {
        format!("{}/{}/{}", self.account_slug, self.application_id, self.key)
    }

    /// issue_json
    fn json(&self) -> Value {
        crate::obj! {
            "key" => self.key.clone(), "canonical_key" => self.canonical_key(), "issue_id" => self.id,
            "title" => self.title.clone(), "kind" => self.kind.clone(), "status" => self.status.clone(), "priority" => self.priority.clone(),
            "occurrences" => self.occurrences, "affected_users" => self.affected_users, "culprit" => self.culprit.clone(),
            "first_seen_at" => db_time_s(self.first_seen_at.clone()), "last_seen_at" => db_time_s(self.last_seen_at.clone()),
            "account_slug" => self.account_slug.clone(), "application_id" => self.application_id,
            "application" => self.application_slug.clone(), "environment" => self.environment_name.clone(),
        }
    }

    fn sample_json(&self) -> Value {
        serde_json::from_str(&self.sample).unwrap_or(Value::Null)
    }

    fn env_row(&self) -> EnvRow {
        EnvRow {
            id: self.environment_id,
            name: self.environment_name.clone(),
            slug: self.environment_slug.clone(),
            application_id: self.application_id,
            app_slug: self.application_slug.clone(),
            account_slug: self.account_slug.clone(),
            last_seen_at: None,
        }
    }
}

/// `key.split("/", 3)` matched against [a, b, c] / [a, b] / else. With a
/// positive limit Ruby keeps trailing empty fields ("a/" is ["a", ""]); an
/// empty string splits to [].
fn split_issue_key(key: &str) -> (Option<String>, Option<String>, String) {
    let parts: Vec<&str> = if key.is_empty() { vec![] } else { key.splitn(3, '/').collect() };
    match parts.as_slice() {
        [a, b, c] => (Some(a.to_string()), Some(b.to_string()), c.to_string()),
        [a, b] => (Some(a.to_string()), None, b.to_string()),
        _ => (None, None, key.to_string()),
    }
}

impl Mcp<'_> {
    fn issues_scope_sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        let (apps, binds) = self.applications_sql();
        (format!("{ISSUE_SELECT} WHERE i.application_id IN ({apps})"), binds)
    }

    fn query_issues(&self, sql: &str, binds: Vec<rusqlite::types::Value>) -> R<Vec<IssueRow>> {
        Ok(self.env.control.with(|c| c.prepare(sql)?.query_map(params_from_iter(binds), issue_row)?.collect())?)
    }

    /// resolve_issue: issue_id first, then the key.
    fn resolve_issue(&self, a: &Args) -> R<Option<IssueRow>> {
        if !a.present("issue_id") {
            return self.find_issue_by_key(&a.s("key"));
        }
        if !positive_integer(a.get("issue_id")) {
            return Ok(None);
        }
        let (sql, mut binds) = self.issues_scope_sql();
        binds.push(to_s(a.get("issue_id")).into());
        let issue = self.query_issues(&format!("{sql} AND i.id = ? LIMIT 1"), binds)?.pop();
        if let Some(i) = &issue
            && a.present("key")
            && !issue_matches_key(i, &a.s("key"))
        {
            return Ok(None);
        }
        Ok(issue)
    }

    fn find_issue_by_key(&self, key: &str) -> R<Option<IssueRow>> {
        let (account_slug, application_id, bare_key) = split_issue_key(key);
        let (prefix, number) = match bare_key.rfind('-') {
            Some(i) => (&bare_key[..i], &bare_key[i + 1..]),
            None => ("", ""),
        };
        if crate::rjson::ruby::blank(prefix.as_bytes()) || !positive_integer(&Value::String(number.into())) {
            return Ok(None);
        }
        if let Some(app) = &application_id
            && !positive_integer(&Value::String(app.clone()))
        {
            return Ok(None);
        }
        let (sql, mut binds) = self.issues_scope_sql();
        let mut sql = format!("{sql} AND ap.issue_prefix = ? AND i.number = ?");
        binds.push(prefix.to_string().into());
        binds.push(number.parse::<i64>().unwrap_or(0).into());
        if let Some(s) = account_slug {
            sql.push_str(" AND ac.slug = ?");
            binds.push(s.into());
        }
        if let Some(app) = application_id {
            sql.push_str(" AND ap.id = ?");
            binds.push(app.into());
        }
        sql.push_str(" LIMIT 2");
        let mut matches = self.query_issues(&sql, binds)?;
        if matches.len() > 1 {
            return Err(Fail::Invalid(format!(
                "Issue key {} exists in more than one application; use the canonical_key form account_slug/application_id/{bare_key} or pass issue_id",
                ruby_inspect(&bare_key)
            )));
        }
        Ok(matches.pop())
    }
}

fn issue_matches_key(issue: &IssueRow, key: &str) -> bool {
    let (account_slug, application_id, bare_key) = split_issue_key(key);
    if bare_key != issue.key {
        return false;
    }
    if account_slug.is_some_and(|s| s != issue.account_slug) {
        return false;
    }
    if application_id.is_some_and(|a| a != issue.application_id.to_string()) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Tools

impl Mcp<'_> {
    fn list_applications(&self) -> R<String> {
        let mut out = vec![];
        for app in self.accessible_applications()? {
            let mut envs = self.environments_of(app.id)?;
            envs.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(crate::obj! {
                "slug" => app.slug.clone(), "name" => app.name.clone(), "account" => app.account_slug.clone(),
                "application_id" => app.id, "application_slug" => app.slug.clone(), "account_slug" => app.account_slug.clone(),
                "environments" => envs.into_iter().map(|(n, s, id)| json!({"name": n, "slug": s, "url": self.url(&format!("/apps/{}/envs/{id}", app.id))})).collect::<Vec<_>>(),
            });
        }
        Ok(Value::Array(out).to_string())
    }

    fn list_issues(&self, a: &Args) -> R<String> {
        let (sql, mut binds) = self.issues_scope_sql();
        let mut sql = sql;
        if Self::application_identity_given(a) {
            let Some(app) = self.resolve_application(a)? else { return Ok("[]".into()) };
            sql.push_str(" AND i.application_id = ?");
            binds.push(app.id.into());
        }
        if a.present("environment") {
            sql.push_str(" AND e.name = ?");
            binds.push(a.s("environment").into());
        }
        sql.push_str(" AND i.status = ?");
        binds.push(a.presence("status").unwrap_or_else(|| "open".into()).into());
        let impact = match a.presence("sort").as_deref() {
            None | Some("recent") => false,
            Some("impact") => true,
            Some(_) => return Ok(json!({"error": "sort must be recent or impact"}).to_string()),
        };
        if impact {
            // Occurrences, discounted by the days since the issue was last
            // seen: 30 today outranks 30 a week ago and 3 an hour ago.
            sql.push_str(&format!(" ORDER BY {IMPACT_SQL} DESC, i.affected_users DESC, i.last_seen_at DESC"));
            binds.push(qt(self.now).into());
        } else {
            sql.push_str(" ORDER BY i.last_seen_at DESC");
        }
        sql.push_str(&format!(" LIMIT {}", clamp(a.get("limit"), 20, 100)?));
        let rows = self.query_issues(&sql, binds)?;
        let now_days = self.now as f64 / 86_400_000_000.0;
        Ok(Value::Array(
            rows.iter()
                .map(|i| {
                    let mut v = self.issue_json(i);
                    if impact {
                        let last = i.last_seen_at.as_deref().and_then(crate::time::parse_db_time).map_or(now_days, |t| t as f64 / 86_400_000_000.0);
                        db::merge_into(&mut v, "impact", jf(rb::round(i.occurrences as f64 / (1.0 + (now_days - last).max(0.0)), 2)));
                    }
                    v
                })
                .collect(),
        )
        .to_string())
    }

    fn get_issue(&self, a: &Args) -> R<String> {
        let Some(issue) = self.resolve_issue(a)? else { return Ok(json!({"error": "issue not found", "hint": ISSUE_NOT_FOUND_HINT}).to_string()) };
        let mut v = self.issue_json(&issue);
        db::merge_into(&mut v, "sample", issue.sample_json());
        db::merge_into(&mut v, "detection", self.detection(&issue)?);
        let (sample, last) = self.agent_occurrences(&issue, a.s("frames") == "all")?;
        if issue.kind == "exception" && sample.is_null() && last.is_null() {
            db::merge_into(
                &mut v,
                "note",
                Value::from(
                    "This issue's occurrences are past the environment's retention: frames and context are gone. Its title, culprit and counts are all that is left.",
                ),
            );
        }
        db::merge_into(&mut v, "sample_occurrence", sample);
        db::merge_into(&mut v, "last_occurrence", last);
        Ok(v.to_string())
    }

    /// The sample and latest occurrences as get_issue prints them: framework
    /// frames collapsed (unless `all_frames`), each with its execution_url,
    /// and the latest one reduced to {id, same_as} when it is the sample.
    fn agent_occurrences(&self, issue: &IssueRow, all_frames: bool) -> R<(Value, Value)> {
        let (mut sample, mut last) = self.exception_occurrences(issue)?;
        let env = issue.env_row();
        for o in [&mut sample, &mut last] {
            if let Value::Object(m) = o {
                if !all_frames && let Some(frames) = m.get("frames") {
                    let collapsed = mcp_summary::collapse_frames(frames);
                    m.insert("frames".into(), collapsed);
                }
                let url = self.execution_url(&env, m.get("execution_source").and_then(Value::as_str), m.get("execution_id").and_then(Value::as_str));
                m.insert("execution_url".into(), url);
            }
        }
        // Often the sample is the latest occurrence: print it once.
        if sample.is_object() && sample.get("id") == last.get("id") {
            last = json!({"id": sample["id"], "same_as": "sample_occurrence"});
        }
        Ok((sample, last))
    }

    /// issue_json with the issue's dashboard link.
    fn issue_json(&self, issue: &IssueRow) -> Value {
        let mut v = issue.json();
        db::merge_into(&mut v, "url", self.url(&format!("/issues/{}", issue.id)));
        v
    }

    /// IssueDetectionPresenter#as_json, its Times (representative records'
    /// occurred_at) as JSON.generate writes them.
    fn detection(&self, issue: &IssueRow) -> R<Value> {
        if issue.kind != "performance" && issue.kind != "anomaly" {
            return Ok(Value::Null);
        }
        let sample = issue.sample_json();
        let presented = crate::detection::presenter::Presented { kind: &issue.kind, occurrences: issue.occurrences, sample: &sample };
        let mut v = self.with_telemetry(&issue.env_row(), |t| Ok(crate::detection::presenter::as_json(&presented, t, self.now)?))?;
        if let Some(Value::Array(records)) = v.get_mut("representative_records") {
            for r in records {
                if let Some(at) = r.get_mut("occurred_at") {
                    to_s_times(at);
                }
            }
        }
        Ok(v)
    }

    /// IssueExceptionPresenter#as_json
    fn exception_occurrences(&self, issue: &IssueRow) -> R<(Value, Value)> {
        if issue.kind != "exception" {
            return Ok((Value::Null, Value::Null));
        }
        let sample = issue.sample_json();
        self.with_telemetry(&issue.env_row(), |conn| {
            let (sample, last) = issue_exception_occurrences(conn, &issue.group_hash, &sample)?;
            Ok((sample.as_ref().map_or(Value::Null, exception_detail), last.as_ref().map_or(Value::Null, exception_detail)))
        })
    }

    fn list_slow_routes(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let sort = a.presence("sort").unwrap_or_else(|| "total".into());
        if !["total", "avg", "p95"].contains(&sort.as_str()) {
            return Ok(json!({"error": "sort must be total, avg, or p95"}).to_string());
        }
        let (from, to) = self.window(a);
        let rows = self.with_telemetry(&env, |conn| {
            Ok(db::query(
                conn,
                "SELECT \"rollups\".\"group_hash\", \"rollups\".\"name\", SUM(count), SUM(error_count), MAX(p95), SUM(duration_sum) FROM \"rollups\" \
                 WHERE \"rollups\".\"record_type\" = 'request' AND \"rollups\".\"bucket\" BETWEEN ?1 AND ?2 GROUP BY \"rollups\".\"group_hash\", \"rollups\".\"name\"",
                params![qt(rb::beginning_of_hour(from)), qt(to)],
            )?)
        })?;
        let num = |v: &rusqlite::types::Value| match v {
            rusqlite::types::Value::Integer(i) => *i as f64,
            rusqlite::types::Value::Real(f) => *f,
            _ => 0.0,
        };
        let window = a.presence("window").filter(|w| Window::preset(Some(w), self.now).key == *w);
        let rows: Vec<(f64, Value)> = rows
            .iter()
            .map(|r| {
                let count = num(r.at(2));
                let p95_ms = rb::round(num(r.at(4)) / 1000.0, 2);
                let total_ms = rb::round(num(r.at(5)) / 1000.0, 1);
                let avg_ms = if count > 0.0 { rb::round(num(r.at(5)) / count / 1000.0, 2) } else { 0.0 };
                let key = match sort.as_str() {
                    "avg" => avg_ms,
                    "p95" => p95_ms,
                    _ => total_ms,
                };
                let url = match (r.at(0), &window) {
                    (rusqlite::types::Value::Text(g), Some(w)) => {
                        self.url(&format!("/apps/{}/envs/{}/requests/routes/{}?window={w}", env.application_id, env.id, segment(g)))
                    }
                    (rusqlite::types::Value::Text(g), None) => {
                        self.url(&format!("/apps/{}/envs/{}/requests/routes/{}", env.application_id, env.id, segment(g)))
                    }
                    _ => Value::Null,
                };
                (
                    key,
                    crate::obj! {
                        "route" => db::raw_json(r.at(1)), "group_hash" => db::raw_json(r.at(0)), "count" => db::raw_json(r.at(2)),
                        "errors" => db::raw_json(r.at(3)), "total_ms" => jf(total_ms), "avg_ms" => jf(avg_ms), "p95_ms" => jf(p95_ms),
                        "url" => url,
                    },
                )
            })
            .collect();
        let sorted = rb::sort_by_num(rows, |(k, _)| rb::Num::F(-k));
        Ok(Value::Array(sorted.into_iter().take(20).map(|(_, v)| v).collect()).to_string())
    }

    fn get_route(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let route = a.s("route");
        let given = a.presence("group_hash");
        self.with_telemetry(&env, |conn| {
            let hour = qt(rb::beginning_of_hour(from));
            let Some(group_hash) = find_group(conn, "request", &route, given, from, to)? else { return not_found("route") };
            let rollups = rollup::load(
                conn,
                "SELECT \"rollups\".* FROM \"rollups\" WHERE \"rollups\".\"record_type\" = 'request' AND \"rollups\".\"group_hash\" = ?1 \
                 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3",
                params![group_hash, hour, qt(to)],
            )?;
            let requests = db::query(
                conn,
                "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"kind\" = 'request' AND \"executions\".\"group_hash\" = ?1 \
                 AND \"executions\".\"occurred_at\" BETWEEN ?2 AND ?3 ORDER BY \"executions\".\"occurred_at\" DESC LIMIT 200",
                params![group_hash, qt(from), qt(to)],
            )?;
            if requests.is_empty() && rollups.is_empty() {
                return not_found("route");
            }
            let summary = rollup::summarize_owned(&rollups);
            let last_name: Option<Value> = db::query_one(
                conn,
                "SELECT \"rollups\".* FROM \"rollups\" WHERE \"rollups\".\"record_type\" = 'request' AND \"rollups\".\"group_hash\" = ?1 \
                 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3 ORDER BY \"rollups\".\"bucket\" DESC LIMIT 1",
                params![group_hash, hour, qt(to)],
            )?
            .map(|r| crate::dashboard::db::js(r.s("name")));
            let name = match last_name {
                Some(Value::Null) | None => requests.first().map_or(Value::Null, |r| crate::dashboard::db::js(r.s("name"))),
                Some(n) => n,
            };
            let ids: Vec<String> = requests.iter().filter_map(|r| r.s("execution_id").map(str::to_string)).collect();
            let slowest: Vec<Value> = max_by_n(&requests, 10, |r| r.i("duration").unwrap_or(0)).into_iter().map(|r| self.execution_row(&env, r)).collect();
            let v = crate::obj! {
                "route" => name, "group_hash" => group_hash.clone(),
                "url" => self.url(&format!("/apps/{}/envs/{}/requests/routes/{}", env.application_id, env.id, segment(&group_hash))),
                "window" => json!({"from": time_s(Some(from)), "to": time_s(Some(to))}),
                "count" => summary.count, "errors" => summary.errors, "client_errors" => summary.client_errors,
                "avg_ms" => ms(Some(summary.avg)), "p50_ms" => ms(Some(summary.p50)), "p95_ms" => ms(Some(summary.p95)),
                "p99_ms" => ms(Some(summary.p99)), "max_ms" => ms(Some(summary.max)),
                "slowest_requests" => slowest,
                "slow_queries" => slow_queries_for(conn, &ids)?,
                "n_plus_ones" => n_plus_ones_for(conn, &ids)?,
            };
            Ok(v.to_string())
        })
    }

    fn get_profile(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let limit = clamp(a.get("limit"), 25, 100)?;
        self.with_telemetry(&env, |conn| {
            let profile = if a.present("profile_id") {
                // find_by(id:): an Integer cast of the argument.
                match profile_id(a.get("profile_id")) {
                    Some(id) => db::query_one(conn, "SELECT \"profiles\".* FROM \"profiles\" WHERE \"profiles\".\"id\" = ?1 LIMIT 1", [id])?,
                    None => None,
                }
            } else if a.present("execution_id") {
                db::query_one(
                    conn,
                    "SELECT \"profiles\".* FROM \"profiles\" WHERE \"profiles\".\"execution_id\" = ?1 ORDER BY \"profiles\".\"occurred_at\" DESC LIMIT 1",
                    [a.s("execution_id")],
                )?
            } else {
                db::query_one(conn, "SELECT \"profiles\".* FROM \"profiles\" ORDER BY \"profiles\".\"occurred_at\" DESC LIMIT 1", [])?
            };
            let Some(p) = profile else {
                let hint = if a.present("execution_id") {
                    "No profile was captured for this execution."
                } else if a.present("profile_id") {
                    "No profile with that id; profile ids come from get_execution's timeline (type profile)."
                } else {
                    "This environment has no profiles."
                };
                return Ok(json!({"error": "profile not found", "hint": format!("{hint} {PROFILE_SETUP}")}).to_string());
            };
            let (collapsed, truncated) = match crate::dashboard::execution_presenter::collapsed_capped(
                p.blob("stacks").unwrap_or(&[]),
                crate::dashboard::execution_presenter::MAX_COLLAPSED_BYTES,
            ) {
                Ok(x) => x,
                Err(_) => return Ok(json!({"error": "profile data is invalid"}).to_string()),
            };
            let v = crate::obj! {
                "id" => p.i("id"), "profiler" => p.s("profiler"), "mode" => p.s("mode"),
                "interval_us" => p.i("interval"), "duration_ms" => p.i("duration").map_or(Value::Null, |d| jf(rb::round(d as f64 / 1000.0, 2))),
                "samples" => p.i("samples"), "execution_id" => p.s("execution_id"), "execution" => p.s("execution_preview"),
                "occurred_at" => time_s(p.t("occurred_at")), "deploy" => p.s("deploy"), "server" => p.s("server"), "truncated" => truncated,
                "frames" => hot_frames(&collapsed, limit as usize),
                "url" => self.url(&format!("/apps/{}/envs/{}/profiles/{}", env.application_id, env.id, p.i("id").unwrap_or(0))),
                "execution_url" => self.execution_url(&env, p.s("execution_source"), p.s("execution_id")),
            };
            Ok(v.to_string())
        })
    }

    fn search_requests(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let q = a.get("q");
        let q = if q.is_null() { None } else { Some(to_s(q)) };
        let parsed = filter_query::parse(q.as_deref());
        let limit = clamp(a.get("limit"), 50, 200)?;
        self.with_telemetry(&env, |conn| {
            use filter_query::{Cond, s};
            let mut c = Cond::new();
            let f = |k: &str| parsed.field(k).map(str::to_string);
            // The where(...) calls in Active Record's order: method first (it
            // precedes the merged scopes), then requests.between, then the
            // merges, then the rest.
            let mut head = Cond::new();
            if let Some(m) = f("method") {
                head.eq("executions", "method", s(&m.to_uppercase()));
            }
            c.eq("executions", "kind", s("request"));
            c.and("\"executions\".\"occurred_at\" BETWEEN ? AND ?", vec![s(&qt(from)), s(&qt(to))]);
            let range = (from, to, false);
            if let Some(route) = f("route") {
                filter_query::named_like(conn, &mut c, "request", &route, range, false)?;
            }
            if rb::present(Some(&parsed.text)) {
                filter_query::named_like(conn, &mut c, "request", &parsed.text, range, false)?;
            }
            if let Some(d) = f("deploy") {
                c.eq("executions", "deploy", s(&d));
            }
            if let Some(t) = f("tenant") {
                c.eq("executions", "app_tenant", s(&t));
            }
            if let Some(u) = f("user") {
                c.eq("executions", "user_ref", s(&u));
            }
            if let Some((lo, hi)) = filter_query::status_range(parsed.fields.get("status").map(String::as_str)) {
                c.and("\"executions\".\"status\" BETWEEN ? AND ?", vec![lo.into(), hi.into()]);
            }
            if let Some(m) = f("min_ms") {
                let us = rb::round0(ruby_string_to_f(&m) * 1000.0);
                c.and("\"executions\".\"duration\" >= ?", vec![us.into()]);
            }
            let mut all = head;
            all.parts.extend(c.parts);
            all.binds.extend(c.binds);
            all.from = c.from;
            let from_sql = all.from.clone().unwrap_or_else(|| "\"executions\"".into());
            let rows = db::query(
                conn,
                &format!("SELECT \"executions\".* FROM {from_sql} WHERE {} ORDER BY \"executions\".\"occurred_at\" DESC LIMIT {limit}", all.sql()),
                params_from_iter(all.binds.iter()),
            )?;
            Ok(Value::Array(rows.iter().map(|r| self.execution_row(&env, r)).collect()).to_string())
        })
    }

    fn explain_query(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let group_hash = a.s("group_hash");
        self.with_telemetry(&env, |conn| {
            let base = format!(
                "SELECT {} FROM \"queries\" WHERE \"queries\".\"group_hash\" = ?1 AND \"queries\".\"occurred_at\" BETWEEN ?2 AND ?3",
                models::QUERY_WITH_SQL.replace("\"queries\".*", "queries.*")
            );
            let explained = db::query_one(
                conn,
                &format!("{base} AND \"queries\".\"explain\" IS NOT NULL ORDER BY \"queries\".\"occurred_at\" DESC LIMIT 1"),
                params![group_hash, qt(from), qt(to)],
            )?;
            let newest = match &explained {
                Some(e) => Some(e.clone()),
                None => db::query_one(conn, &format!("{base} ORDER BY \"queries\".\"occurred_at\" DESC LIMIT 1"), params![group_hash, qt(from), qt(to)])?,
            };
            let Some(n) = newest else {
                return Ok(json!({"error": "query group not found",
                    "hint": "No query with this group_hash in the window. Take group_hash from get_route's slow_queries or n_plus_ones, or search_telemetry resource queries; pass a longer window (7d, 30d) for older ones."}).to_string());
            };
            let mut v = crate::obj! {
                "group_hash" => group_hash.clone(), "sql" => n.s("resolved_sql").unwrap_or(""), "source" => n.s("source"),
                "connection" => n.s("connection"), "duration_ms" => n.i("duration").map_or(Value::Null, |d| jf(rb::round(d as f64 / 1000.0, 3))),
                "occurred_at" => time_s(n.t("occurred_at")), "execution_id" => n.s("execution_id"),
                "execution" => n.s("execution_preview"),
                "explain" => explained.as_ref().map_or(Value::Null, |e| crate::dashboard::db::js(e.s("explain"))),
                "url" => self.url(&format!("/apps/{}/envs/{}/queries/{}", env.application_id, env.id, segment(&group_hash))),
                "execution_url" => self.execution_url(&env, n.s("execution_source"), n.s("execution_id")),
            };
            if explained.is_none() {
                db::merge_into(
                    &mut v,
                    "hint",
                    Value::from("No stored plan for this query in the window. Plans are captured only when the app sets RAILWATCH_CAPTURE_QUERY_EXPLAIN=true, and only for SELECTs slower than RAILWATCH_EXPLAIN_THRESHOLD_MS (100 by default). Or run EXPLAIN on sql yourself."),
                );
            }
            Ok(v.to_string())
        })
    }

    fn list_tenants(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let q = a.get("q");
        let q = if q.is_null() { None } else { Some(to_s(q)) };
        self.with_telemetry(&env, |conn| {
            let rows = crate::dashboard::tenant::index(conn, from, to, q.as_deref(), None, None)?;
            Ok(Value::Array(
                rows.into_iter()
                    .map(|r| {
                        let mut r = row_times(r, &["last_seen_at"]);
                        if let Some(t) = r.get("tenant").and_then(Value::as_str) {
                            let url = self.url(&format!("/apps/{}/envs/{}/tenants/{}", env.application_id, env.id, segment(t)));
                            db::merge_into(&mut r, "url", url);
                        }
                        r
                    })
                    .collect(),
            )
            .to_string())
        })
    }

    fn get_execution(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let eid = a.s("execution_id");
        if !a.present("execution_id") {
            return Ok(json!({"error": "execution_id is required", "hint": "Take one from search_requests, get_route's slowest_requests, an issue's sample_occurrence, or call last_execution for the newest request or job."}).to_string());
        }
        self.with_telemetry(&env, |conn| {
            let Some(exe) = db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [&eid])?
            else {
                return Ok(json!({"error": "execution not found", "hint": "Executions are kept for the environment's retention period, and only sampled-in ones are stored. Search recent ones with search_requests, or call last_execution."}).to_string());
            };
            Ok(self.execution_detail(conn, &env, &exe, a)?.to_string())
        })
    }

    /// The newest execution, optionally of one kind and matching a path or
    /// route, in get_execution's shape.
    fn last_execution(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let kind = a.presence("kind").unwrap_or_else(|| "any".into());
        let kinds: &[&str] = match kind.as_str() {
            "request" => &["request"],
            "job" => &["job_attempt"],
            "any" => &["request", "job_attempt", "scheduled_task", "command", "channel_action"],
            _ => return Ok(json!({"error": "kind must be request, job, or any"}).to_string()),
        };
        let path = a.presence("path");
        self.with_telemetry(&env, |conn| {
            // A path that matches nothing would walk every execution: look
            // back a day at most (the dev loop wants the request just made;
            // search_requests reaches further).
            let since = path.as_ref().map(|_| qt(self.now - 24 * 3_600_000_000));
            let Some(exe) = newest_execution(conn, kinds, path.as_deref(), since.as_deref())? else {
                let what = match &path {
                    Some(p) => format!("no {} matching {} in the last 24 hours", if kind == "job" { "job" } else { "request or job" }, ruby_inspect(p)),
                    None => "no executions".into(),
                };
                return Ok(json!({"error": format!("{what} in {}/{}", env.app_slug, env.name),
                    "hint": "Make the request (or run the job) again and retry: the app ships records every RAILWATCH_FLUSH_INTERVAL seconds (2 by default). Nothing arrives when RAILWATCH_TOKEN/RAILWATCH_INGEST_URL point elsewhere, when RAILWATCH_REQUEST_SAMPLE_RATE / RAILWATCH_JOB_SAMPLE_RATE sample it out, or when its path is in ignored_request_paths."}).to_string());
            };
            Ok(self.execution_detail(conn, &env, &exe, a)?.to_string())
        })
    }

    /// get_execution's result: what ran, the summary of where its time went,
    /// and one page of the timeline.
    fn execution_detail(&self, conn: &Connection, env: &EnvRow, exe: &Rec, a: &Args) -> R<Value> {
        let eid = exe.s("execution_id").unwrap_or("").to_string();
        let start = exe.t("occurred_at").unwrap_or(0);
        let duration_ms = exe.i("duration").unwrap_or(0) as f64 / 1000.0;
        let ExecutionFacts { entries, summary, detail, dropped } = execution_facts(conn, exe)?;

        let kind = exe.s("kind");
        let mut v = crate::obj! {
            "execution_id" => eid.clone(), "kind" => kind, "name" => exe.s("name"),
            "duration_ms" => jf(duration_ms),
            "status" => exe.i("status"), "outcome" => exe.s("outcome"), "occurred_at" => time_s(Some(start)),
            "url" => self.execution_url(env, kind, Some(&eid)),
        };
        if let Some(path) = detail.get("path").filter(|p| p.is_string()) {
            db::merge_into(&mut v, "path", path.clone());
        }
        // The gem's per-execution buffer kept the earliest child records
        // and dropped the rest: say so, or the timeline reads as complete.
        if dropped.is_some() {
            db::merge_into(&mut v, "dropped_records", detail["dropped_records"].clone());
            db::merge_into(&mut v, "dropped_bytes", detail.get("dropped_bytes").cloned().unwrap_or(Value::Null));
        }
        db::merge_into(&mut v, "summary", summary);
        let (timeline, page) = timeline_page(&entries, a)?;
        db::merge_into(&mut v, "timeline", timeline);
        db::merge_into(&mut v, "timeline_page", page);
        Ok(v)
    }

    fn search_logs(&self, a: &Args) -> R<String> {
        // [args["q"], ("level:#{args["level"]}" if present)].compact.join(" ")
        let mut parts: Vec<String> = vec![];
        if !a.get("q").is_null() {
            parts.push(a.s("q"));
        }
        if a.present("level") {
            parts.push(format!("level:{}", a.s("level")));
        }
        let mut merged = a.0.clone();
        merged.insert("resource".into(), Value::from("logs"));
        merged.insert("q".into(), Value::String(parts.join(" ")));
        let result = self.search_telemetry(&Args(&merged))?;
        if result.get("error").is_some() {
            return Ok(result.to_string());
        }
        Ok(result["data"].to_string())
    }

    fn search_telemetry(&self, a: &Args) -> R<Value> {
        let resource = a.s("resource");
        if !TELEMETRY_RESOURCES.contains(&resource.as_str()) {
            return Ok(json!({"error": "resource must be jobs, exceptions, logs, or queries"}));
        }
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment_value(a) };
        let (from, to) = self.window(a);
        let q = a.get("q");
        let q_s = if q.is_null() { None } else { Some(to_s(q)) };
        let window = a.presence("window").unwrap_or_else(|| "24h".into());
        // FilterQuery.cursor_context(environment_id:, resource:, query:, window:)
        let context = {
            use sha2::Digest;
            let j = serde_json::to_string(&json!([env.id, resource, q_s.clone().unwrap_or_default(), window])).unwrap();
            let j = crate::rjson::escape_html(&j);
            sha2::Sha256::digest(j.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let cursor = a.get("cursor");
        let cursor = if cursor.is_null() { None } else { Some(to_s(cursor)) };
        let limit = a.get("limit");
        let limit_s = match limit {
            Value::Null => None,
            v => Some(to_i(v)?.to_string()),
        };
        self.with_telemetry(&env, |conn| {
            let (res, table, model, select, order) = match resource.as_str() {
                "jobs" => (Resource::Jobs, "executions", "Railwatch::Telemetry::Execution", "\"executions\".*", Order::Recent),
                "exceptions" => (Resource::Exceptions, "exceptions", "Railwatch::Telemetry::Exception", "\"exceptions\".*", Order::Recent),
                "logs" => (Resource::Logs, "logs", "Railwatch::Telemetry::Log", "\"logs\".*", Order::Recent),
                _ => (Resource::Queries, "queries", "Railwatch::Telemetry::Query", models::QUERY_WITH_SQL, Order::Slowest),
            };
            let cond = filter_query::apply(conn, res, q_s.as_deref(), from, to, &[])?;
            let page = match cursor_page::call(conn, self.env.cursor_key, table, model, select, cond, cursor.as_deref(), limit_s.as_deref(), order, &context) {
                Ok(p) => p?,
                Err(e) => return Err(Fail::Invalid(e.to_string())),
            };
            let row = |r: &Rec| match resource.as_str() {
                "jobs" => models::execution_row(r),
                "exceptions" => models::exception_row(r),
                "logs" => models::log_row(r),
                _ => models::query_row(r),
            };
            let data: Vec<Value> = page
                .rows
                .iter()
                .map(|r| {
                    let mut v = row_times(row(r), &["occurred_at"]);
                    let url = match resource.as_str() {
                        "jobs" => self.execution_url(&env, r.s("kind"), r.s("execution_id")),
                        "queries" => {
                            r.s("group_hash").map_or(Value::Null, |g| self.url(&format!("/apps/{}/envs/{}/queries/{}", env.application_id, env.id, segment(g))))
                        }
                        _ => self.execution_url(&env, r.s("execution_source"), r.s("execution_id")),
                    };
                    db::merge_into(&mut v, "url", url);
                    v
                })
                .collect();
            Ok(json!({"data": data, "meta": page.meta}))
        })
    }

    fn release_health(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let deploys: Vec<(String, Option<String>, String)> = self.env.control.with(|c| {
            c.prepare(
                "SELECT deploy, ref, deployed_at FROM deploys WHERE environment_id = ?1 AND deployed_at BETWEEN ?2 AND ?3 ORDER BY deployed_at DESC LIMIT 50",
            )?
            .query_map(params![env.id, qt(from), qt(to)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
        })?;
        let rows = self.with_telemetry(&env, |conn| {
            let adoption = crate::dashboard::pages::adoption(conn, from, to)?;
            let mut out = vec![];
            for (deploy, ref_, at) in &deploys {
                let mut v = crate::dashboard::pages::release_health_for(conn, Some(deploy), from, to)?;
                let share = adoption.iter().find(|(d, _)| d.as_deref() == Some(deploy.as_str())).map_or(0.0, |x| x.1);
                db::merge_into(&mut v, "deploy", Value::from(deploy.clone()));
                db::merge_into(&mut v, "ref", ref_.clone().map_or(Value::Null, Value::from));
                db::merge_into(&mut v, "deployed_at", db_time_s(Some(at.clone())));
                db::merge_into(&mut v, "adoption", jf(share));
                db::merge_into(&mut v, "url", self.url(&format!("/apps/{}/envs/{}/releases/{}", env.application_id, env.id, segment(deploy))));
                out.push(v);
            }
            Ok(out)
        })?;
        Ok(Value::Array(rows).to_string())
    }

    /// The visits page's numbers: p75 web vitals for the window (or one
    /// component), the components with their visit timings and vitals, and
    /// the vitals of each release deployed in the window.
    fn web_vitals(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let (from, to) = self.window(a);
        let limit = clamp(a.get("limit"), 20, 100)?;
        let sort = a.presence("sort").unwrap_or_else(|| "count".into());
        let component = a.presence("component");
        let deploys: Vec<(String, Option<String>, String)> = self.env.control.with(|c| {
            c.prepare(
                "SELECT deploy, ref, deployed_at FROM deploys WHERE environment_id = ?1 AND deployed_at BETWEEN ?2 AND ?3 ORDER BY deployed_at DESC LIMIT 50",
            )?
            .query_map(params![env.id, qt(from), qt(to)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
        })?;
        self.with_telemetry(&env, |conn| {
            let samples = crate::dashboard::pages::vital_samples(conn, from, to)?;
            let phases = crate::dashboard::pages::ttfb_phases(conn, from, to)?;
            let components =
                crate::dashboard::pages::visit_components(conn, from, to, &samples, &phases, Some(&sort), a.presence("dir").as_deref(), component.as_deref())?;
            let (samples, phases) = match &component {
                Some(c) => (samples.of_component(c), phases.of_component(c)),
                None => (samples, phases),
            };
            let names: Vec<String> = deploys.iter().map(|d| d.0.clone()).collect();
            let releases: Vec<Value> = deploys
                .iter()
                .zip(crate::dashboard::pages::vitals_by_deploy(&samples, &names))
                .zip(crate::dashboard::pages::ttfb_phases_by_deploy(&phases, &names))
                .map(|(((deploy, ref_, at), vitals), ttfb_phases)| {
                    crate::obj! {
                        "deploy" => deploy.clone(), "ref" => ref_.clone().map_or(Value::Null, Value::from), "deployed_at" => db_time_s(Some(at.clone())),
                        "vitals" => Value::Object(vitals), "ttfb_phases" => Value::Object(ttfb_phases),
                    }
                })
                .collect();
            let v = crate::obj! {
                "window" => json!({"from": time_s(Some(from)), "to": time_s(Some(to))}),
                "component" => component.clone().map_or(Value::Null, Value::from),
                "vitals" => Value::Object(samples.summary()), "thresholds" => crate::dashboard::pages::vital_thresholds(),
                "ttfb_phases" => Value::Object(phases.summary()),
                "components" => components.into_iter().take(limit as usize).map(|r| crate::obj! {
                    "component" => r["name"].clone(), "group_hash" => r["group_hash"].clone(), "visits" => r["count"].clone(), "errors" => r["errors"].clone(),
                    "avg_ms" => r["avg"].clone(), "p50_ms" => r["p50"].clone(), "p95_ms" => r["p95"].clone(), "p99_ms" => r["p99"].clone(),
                    "max_ms" => r["max"].clone(), "vitals" => r["vitals"].clone(), "ttfb_phases" => r["ttfb_phases"].clone(),
                }).collect::<Vec<_>>(),
                "releases" => releases,
            };
            Ok(v.to_string())
        })
    }

    fn recent_deploys(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let rows: Vec<Value> = self.env.control.with(|c| {
            c.prepare("SELECT deploy, ref, name, deployed_at, url, id FROM deploys WHERE environment_id = ?1 ORDER BY deployed_at DESC LIMIT 20")?
                .query_map([env.id], |r| {
                    Ok(crate::obj! {
                        "deploy" => r.get::<_, String>(0)?, "ref" => r.get::<_, Option<String>>(1)?, "name" => r.get::<_, Option<String>>(2)?,
                        "deployed_at" => db_time_s(r.get(3)?), "url" => r.get::<_, Option<String>>(4)?,
                        "dashboard_url" => self.url(&format!("/apps/{}/envs/{}/deploys/{}", env.application_id, env.id, r.get::<_, i64>(5)?)),
                    })
                })?
                .collect()
        })?;
        Ok(Value::Array(rows).to_string())
    }

    fn list_alerts(&self, a: &Args) -> R<String> {
        let (acc, mut binds) = self.accounts_sql();
        let mut sql = String::from(
            "SELECT a.id, a.event, a.status, a.sent_at, a.error, a.created_at, n.kind, a.alert_rule_id, a.issue_id
             FROM alerts a INNER JOIN integrations n ON n.id = a.integration_id",
        );
        let mut wheres = format!(" WHERE n.account_id IN ({acc})");
        if Self::application_identity_given(a) {
            let Some(app) = self.resolve_application(a)? else { return Ok("[]".into()) };
            sql.push_str(" INNER JOIN alert_rules r ON r.id = a.alert_rule_id");
            wheres.push_str(" AND r.application_id = ?");
            binds.push(app.id.into());
        }
        let event = a.s("event");
        if crate::alerts::rules::EVENTS.contains(&event.as_str()) {
            wheres.push_str(" AND a.event = ?");
            binds.push(event.into());
        }
        let status = a.s("status");
        if ALERT_STATUSES.contains(&status.as_str()) {
            wheres.push_str(" AND a.status = ?");
            binds.push(status.into());
        }
        sql.push_str(&wheres);
        sql.push_str(&format!(" ORDER BY a.created_at DESC LIMIT {}", clamp(a.get("limit"), 20, 100)?));
        type AlertRow = (i64, String, String, Option<String>, Option<String>, String, String, Option<i64>, Option<i64>);
        let rows: Vec<AlertRow> = self.env.control.with(|c| {
            c.prepare(&sql)?
                .query_map(params_from_iter(binds), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
                })?
                .collect()
        })?;
        let mut out = vec![];
        for (id, event, status, sent_at, err, created_at, kind, rule_id, issue_id) in rows {
            let application: Value = match rule_id {
                Some(rid) => self
                    .env
                    .control
                    .with(|c| {
                        c.query_row("SELECT ap.slug FROM alert_rules r JOIN applications ap ON ap.id = r.application_id WHERE r.id = ?", [rid], |r| {
                            r.get::<_, String>(0)
                        })
                        .optional()
                    })?
                    .map_or(Value::Null, Value::from),
                None => Value::Null,
            };
            let issue = match issue_id {
                Some(iid) => self.query_issues(&format!("{ISSUE_SELECT} WHERE i.id = ?"), vec![iid.into()])?.pop().map_or(Value::Null, |i| {
                    crate::obj! {
                        "key" => i.key.clone(), "canonical_key" => i.canonical_key(), "issue_id" => i.id,
                        "title" => i.title.clone(), "environment" => i.environment_name.clone(), "url" => self.url(&format!("/issues/{}", i.id)),
                    }
                }),
                None => Value::Null,
            };
            out.push(crate::obj! {
                "id" => id, "event" => event, "status" => status, "sent_at" => db_time_s(sent_at), "error" => err,
                "created_at" => db_time_s(Some(created_at)), "application" => application, "integration" => kind, "issue" => issue,
            });
        }
        Ok(Value::Array(out).to_string())
    }

    /// update_issue: every write attributed to the token's user, with an
    /// "agent" activity listing what the client asked to change.
    fn update_issue(&self, a: &Args) -> R<String> {
        let Some(found) = self.resolve_issue(a)? else { return Ok(json!({"error": "issue not found", "hint": ISSUE_NOT_FOUND_HINT}).to_string()) };
        let user = self.user_name()?;
        let actor = crate::grouping::issue::Actor { user_id: self.token.user_id, name: &user };
        let id = found.id;
        let result: Result<(), Fail> = self.write(|c| {
            use crate::grouping::issue;
            let i = issue::find(c, id)?.ok_or(Fail::Unrescued)?;
            match a.s("status").as_str() {
                "resolved" => {
                    issue::resolve_by(c, &i, None, Some(actor))?;
                }
                "ignored" => {
                    issue::ignore(c, &i, Some(actor))?;
                }
                "open" => {
                    if i.status == "merged" {
                        match issue::unmerge(c, id, Some(actor)) {
                            Ok(()) => {}
                            Err(issue::MergeError::Sql(e)) => return Err(e.into()),
                            Err(issue::MergeError::Invalid(m)) => return Err(Fail::Internal(m)),
                        }
                    } else {
                        issue::reopen(c, &i, Some(actor))?;
                    }
                }
                _ => {}
            }
            if a.present("priority") {
                let i = issue::find(c, id)?.ok_or(Fail::Unrescued)?;
                // update!(priority:) raises RecordInvalid on an unknown value.
                if !issue::set_priority(c, &i, &a.s("priority"), Some(actor))? {
                    return Err(Fail::Internal("Validation failed: Priority is not included in the list".into()));
                }
            }
            if a.has("assignee_email") {
                let to = if a.present("assignee_email") {
                    let email = a.s("assignee_email").to_lowercase();
                    let member: Option<(i64, String)> = c
                        .query_row(
                            "SELECT u.id, u.name FROM users u JOIN memberships m ON m.user_id = u.id WHERE m.account_id = ?1 AND u.email = ?2 LIMIT 1",
                            params![found.account_id, email],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    Some(member.ok_or_else(|| Fail::Internal("Couldn't find User".into()))?)
                } else {
                    None
                };
                let i = issue::find(c, id)?.ok_or(Fail::Unrescued)?;
                issue::set_assignee(c, &i, to.as_ref().map(|(id, n)| (*id, n.as_str())), Some(actor))?;
            }
            // data: {agent:, changes: args.except("key", "issue_id", "agent")}
            let changes: Map<String, Value> =
                a.0.iter().filter(|(k, _)| !matches!(k.as_str(), "key" | "issue_id" | "agent")).map(|(k, v)| (k.clone(), v.clone())).collect();
            let data = json!({"agent": a.presence("agent").map_or_else(|| Value::from("mcp"), |_| a.get("agent").clone()), "changes": changes});
            add_activity_json(c, id, "agent", self.token.user_id, &data)?;
            Ok(())
        });
        result?;
        let issue = self.query_issues(&format!("{ISSUE_SELECT} WHERE i.id = ?"), vec![id.into()])?.pop().ok_or(Fail::Unrescued)?;
        Ok(self.issue_json(&issue).to_string())
    }

    fn add_comment(&self, a: &Args) -> R<String> {
        let Some(found) = self.resolve_issue(a)? else { return Ok(json!({"error": "issue not found", "hint": ISSUE_NOT_FOUND_HINT}).to_string()) };
        let body = a.s("body");
        if crate::rjson::ruby::blank(body.as_bytes()) {
            // validates :body, presence: true -> RecordInvalid
            return Err(Fail::Internal("Validation failed: Body can't be blank".into()));
        }
        let id = found.id;
        let (comment_id, created_at) = self.write(|c| {
            crate::web::issues::create_comment(c, found.account_id, self.token.user_id, id, &body).map_err(|_| Fail::Internal("comment".into()))?;
            let (cid, at): (i64, String) =
                c.query_row("SELECT id, created_at FROM comments WHERE issue_id = ? ORDER BY id DESC LIMIT 1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let data = json!({"agent": a.presence("agent").map_or_else(|| Value::from("mcp"), |_| a.get("agent").clone()), "changes": {"comment_id": cid}});
            add_activity_json(c, id, "agent", self.token.user_id, &data)?;
            Ok((cid, at))
        })?;
        Ok(json!({"id": comment_id, "body": body, "created_at": db_time_s(Some(created_at))}).to_string())
    }

    fn user_name(&self) -> R<String> {
        Ok(self.env.control.with(|c| c.query_row("SELECT name FROM users WHERE id = ?", [self.token.user_id], |r| r.get(0)))?)
    }

    /// One IMMEDIATE transaction, rolled back when `f` fails.
    fn write<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> R<T>) -> R<T> {
        let mut conn = self.env.control.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}

/// `activities.create!(kind:, user:, data:)` with a JSON value as Rails
/// serializes it (to_json of the Hash).
fn add_activity_json(c: &Connection, issue_id: i64, kind: &str, user_id: i64, data: &Value) -> rusqlite::Result<()> {
    c.prepare_cached("INSERT INTO issue_activities (issue_id, kind, data, user_id, created_at) VALUES (?, ?, ?, ?, ?)")?.execute(params![
        issue_id,
        kind,
        crate::rjson::escape_html(&data.to_string()),
        user_id,
        crate::control::now_db()
    ])?;
    Ok(())
}

/// `Profile.find_by(id: value)`: Active Model's integer cast.
fn profile_id(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => {
            let t = s.trim_start();
            let t2 = t.strip_prefix(['+', '-']).unwrap_or(t);
            t2.starts_with(|c: char| c.is_ascii_digit()).then(|| cursor_page::ruby_to_i(s).unwrap_or(0))
        }
        Value::Bool(true) => Some(1),
        Value::Bool(false) => Some(0),
        _ => None,
    }
}

/// `String#to_f`: the leading float, else 0.0.
fn ruby_string_to_f(s: &str) -> f64 {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let st = *i;
        while *i < b.len() && (b[*i].is_ascii_digit() || (b[*i] == b'_' && *i > st && *i + 1 < b.len() && b[*i + 1].is_ascii_digit())) {
            *i += 1;
        }
        *i > st
    };
    if !digits(&mut i) {
        return 0.0;
    }
    if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
        i += 1;
        digits(&mut i);
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            digits(&mut i);
        }
    }
    t[..i].replace('_', "").parse().unwrap_or(0.0)
}

/// IssueExceptionPresenter#detail as JSON.generate writes it (MCP).
fn exception_detail(e: &Rec) -> Value {
    row_times(exception_occurrence(e), &["occurred_at"])
}

/// IssueExceptionPresenter#detail, times as `as_json` writes them (the
/// JSON API renders it with `render json:`).
pub(crate) fn exception_occurrence(e: &Rec) -> Value {
    let context: Value = e.s("context").and_then(|c| serde_json::from_str::<Value>(c).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let breadcrumbs: Vec<Value> = match context.get("browser").and_then(|b| b.get("breadcrumbs")) {
        Some(Value::Array(trail)) if context["browser"].is_object() => trail
            .iter()
            .filter_map(|c| {
                let m = c.as_object()?;
                let mut out = Map::new();
                for k in ["at", "kind", "text"] {
                    if let Some(v) = m.get(k) {
                        out.insert(k.into(), v.clone());
                    }
                }
                Some(Value::Object(out))
            })
            .collect(),
        _ => vec![],
    };
    let mut row = models::exception_row(e);
    for (k, v) in [
        ("message", crate::dashboard::db::js(e.s("message"))),
        ("frames", e.json("frames")),
        ("cause", e.json("cause")),
        ("locals", e.json("locals")),
        ("context", context),
        ("browser_breadcrumbs", Value::Array(breadcrumbs)),
        ("ruby_version", crate::dashboard::db::js(e.s("ruby_version"))),
        ("rails_version", crate::dashboard::db::js(e.s("rails_version"))),
    ] {
        db::merge_into(&mut row, k, v);
    }
    row
}

/// `Enumerable#max_by(n) { key }`: MRI's rb_nmin_run (by, rev) over Integer
/// keys: a 4n buffer partitioned by nmin_filter, then qsort_r (glibc's
/// stable mergesort) of the survivors, reversed.
fn max_by_n<T>(items: &[T], n: usize, key: impl Fn(&T) -> i64) -> Vec<&T> {
    if n == 0 {
        return vec![];
    }
    let bufmax = n * 4;
    let mut buf: Vec<(i64, usize)> = Vec::with_capacity(bufmax);
    let mut limit: Option<i64> = None;
    let filter = |buf: &mut Vec<(i64, usize)>, limit: &mut Option<i64>| {
        if buf.len() <= n {
            return;
        }
        let (mut left, mut right) = (0usize, buf.len() - 1);
        // cmp reversed (rev): c = -(a <=> b)
        let cmp = |a: i64, b: i64| -(a.cmp(&b) as i32);
        let store_index;
        loop {
            let pivot_index = left + (right - left) / 2;
            let mut num_pivots = 1usize;
            buf.swap(pivot_index, right);
            let pivot = right;
            let mut store = left;
            let mut i = left;
            while i + num_pivots <= right {
                let c = cmp(buf[i].0, buf[pivot].0);
                if c == 0 {
                    buf.swap(i, right - num_pivots);
                    num_pivots += 1;
                    continue;
                }
                if c < 0 {
                    buf.swap(i, store);
                    store += 1;
                }
                i += 1;
            }
            let mut j = store;
            let mut k = right;
            while right - num_pivots < k {
                if k <= j {
                    break;
                }
                buf.swap(j, k);
                j += 1;
                k -= 1;
            }
            if store <= n && n <= store + num_pivots {
                store_index = store;
                break;
            }
            if n < store {
                right = store - 1;
            } else {
                left = store + num_pivots;
            }
        }
        *limit = Some(buf[store_index].0);
        buf.truncate(n);
    };
    for (idx, item) in items.iter().enumerate() {
        let k = key(item);
        if let Some(l) = limit
            && -(k.cmp(&l) as i32) >= 0
        {
            continue;
        }
        buf.push((k, idx));
        if buf.len() == bufmax {
            filter(&mut buf, &mut limit);
        }
    }
    filter(&mut buf, &mut limit);
    // ruby_qsort = glibc qsort_r: a stable merge sort (ascending by key).
    buf.sort_by_key(|a| a.0);
    buf.reverse();
    buf.into_iter().map(|(_, i)| &items[i]).collect()
}

/// slow_queries_for
fn slow_queries_for(conn: &Connection, ids: &[String]) -> rusqlite::Result<Value> {
    if ids.is_empty() {
        return Ok(json!([]));
    }
    let ph = vec!["?"; ids.len()].join(", ");
    let rows = db::query(
        conn,
        &format!(
            "SELECT queries.*, COALESCE(NULLIF(queries.sql, ''), (SELECT query_shapes.sql FROM query_shapes WHERE query_shapes.group_hash = queries.group_hash), '') AS resolved_sql \
             FROM \"queries\" WHERE \"queries\".\"execution_id\" IN ({ph}) ORDER BY \"queries\".\"duration\" DESC LIMIT 500"
        ),
        params_from_iter(ids.iter()),
    )?;
    let groups = group_by(&rows, |r| r.s("group_hash").map(str::to_string));
    let out: Vec<(f64, Value)> = groups
        .into_iter()
        .map(|(g, rs)| {
            let first = rs[0];
            let total: i64 = rs.iter().map(|q| q.i("duration").unwrap_or(0)).sum();
            let total_ms = rb::round(total as f64 / 1000.0, 3);
            (
                total_ms,
                crate::obj! {
                    "group_hash" => g, "sql" => rb::first_chars(first.s("resolved_sql").unwrap_or(""), 400), "source" => first.s("source"),
                    "count" => rs.len(), "max_ms" => jf(rb::round(first.i("duration").unwrap_or(0) as f64 / 1000.0, 3)), "total_ms" => jf(total_ms),
                },
            )
        })
        .collect();
    let sorted = rb::sort_by_num(out, |(t, _)| rb::Num::F(-t));
    Ok(Value::Array(sorted.into_iter().take(10).map(|(_, v)| v).collect()))
}

/// n_plus_ones_for
fn n_plus_ones_for(conn: &Connection, ids: &[String]) -> rusqlite::Result<Value> {
    if ids.is_empty() {
        return Ok(json!([]));
    }
    let ph = vec!["?"; ids.len()].join(", ");
    let rows = db::query(
        conn,
        &format!("SELECT \"n_plus_ones\".* FROM \"n_plus_ones\" WHERE \"n_plus_ones\".\"execution_id\" IN ({ph})"),
        params_from_iter(ids.iter()),
    )?;
    let groups = group_by(&rows, |r| r.s("group_hash").map(str::to_string));
    let out: Vec<(i64, Value)> = groups
        .into_iter()
        .map(|(g, rs)| {
            let first = rs[0];
            let max_repeats = rs.iter().filter_map(|r| r.i("count")).max();
            (
                rs.len() as i64,
                crate::obj! {
                    "group_hash" => g, "sql" => rb::first_chars(first.s("sql").unwrap_or(""), 400), "source" => first.s("source"),
                    "occurrences" => rs.len(), "max_repeats" => max_repeats,
                    "suggestion" => crate::dashboard::pages::n_plus_one::suggestion(first.s("sql").unwrap_or(""), first.s("source")),
                },
            )
        })
        .collect();
    let sorted = rb::sort_by_num(out, |(n, _)| rb::Num::I(-n));
    Ok(Value::Array(sorted.into_iter().take(10).map(|(_, v)| v).collect()))
}

/// The group_hash of a route or job named `name` in [from, to] (a rollup
/// with that name), or `given` when the caller passed one. A job class's
/// group is the MD5 of its name (the gem's Record.group_hash), so a job
/// with no rollup in the window still resolves.
fn find_group(conn: &Connection, record_type: &str, name: &str, given: Option<String>, from: i64, to: i64) -> rusqlite::Result<Option<String>> {
    if let Some(g) = given.filter(|g| rb::present(Some(g))) {
        return Ok(Some(g));
    }
    let found = db::query_one(
        conn,
        "SELECT \"rollups\".\"group_hash\" FROM \"rollups\" WHERE \"rollups\".\"record_type\" = ?1 AND \"rollups\".\"bucket\" BETWEEN ?2 AND ?3 \
         AND \"rollups\".\"name\" = ?4 LIMIT 1",
        params![record_type, qt(rb::beginning_of_hour(from)), qt(to), name],
    )?
    .and_then(|r| r.s("group_hash").map(str::to_string))
    .filter(|g| rb::present(Some(g)));
    Ok(match found {
        None if record_type == "job_attempt" && rb::present(Some(name)) => Some(filter_query::md5_hex(name)),
        other => other,
    })
}

/// `group_by`: groups in first-appearance order.
fn group_by<K: PartialEq + Clone>(rows: &[Rec], key: impl Fn(&Rec) -> K) -> Vec<(K, Vec<&Rec>)> {
    let mut out: Vec<(K, Vec<&Rec>)> = vec![];
    for r in rows {
        let k = key(r);
        match out.iter_mut().find(|(g, _)| *g == k) {
            Some((_, v)) => v.push(r),
            None => out.push((k, vec![r])),
        }
    }
    out
}

/// hot_frames: folded stacks "outer;inner;leaf <count>" per line.
fn hot_frames(collapsed: &str, limit: usize) -> Value {
    let mut own: Vec<(String, i64)> = vec![];
    let mut own_ix: HashMap<String, usize> = HashMap::new();
    let mut total: HashMap<String, i64> = HashMap::new();
    for line in collapsed.split_inclusive('\n') {
        let line = line.trim_end();
        let (stack, count) = match line.rfind(' ') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => ("", line),
        };
        let count = cursor_page::ruby_to_i(count).unwrap_or(0);
        if stack.is_empty() || count == 0 {
            continue;
        }
        let frames: Vec<&str> = split_semicolons(stack);
        let leaf = frames.last().copied().unwrap_or("").to_string();
        match own_ix.get(&leaf) {
            Some(&i) => own[i].1 += count,
            None => {
                own_ix.insert(leaf.clone(), own.len());
                own.push((leaf, count));
            }
        }
        let mut seen: Vec<&str> = vec![];
        for f in frames {
            if !seen.contains(&f) {
                seen.push(f);
                *total.entry(f.to_string()).or_insert(0) += count;
            }
        }
    }
    let samples: i64 = own.iter().map(|o| o.1).sum();
    let pct = |part: i64| if samples == 0 { 0.0 } else { rb::round(part as f64 * 100.0 / samples as f64, 1) };
    let sorted = rb::sort_by_num(own, |(_, c)| rb::Num::I(-c));
    Value::Array(
        sorted
            .into_iter()
            .take(limit)
            .map(|(frame, count)| {
                let t = total.get(&frame).copied().unwrap_or(0);
                crate::obj! {
                    "frame" => frame, "self_samples" => count, "self_pct" => jf(pct(count)),
                    "total_samples" => t, "total_pct" => jf(pct(t)),
                }
            })
            .collect(),
    )
}

/// `String#split(";")`: trailing empty fields dropped.
fn split_semicolons(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split(';').collect();
    while v.last() == Some(&"") {
        v.pop();
    }
    v
}

/// Execution#timeline: every child's timeline_entry, sorted by offset, with
/// what the summary reads besides each row's JSON.
fn execution_timeline(conn: &Connection, eid: &str, start: i64) -> rusqlite::Result<Vec<mcp_summary::Entry>> {
    // CHILDREN in declaration order; label per model's timeline_label.
    const CHILDREN: [(&str, &str); 16] = [
        ("queries", "query"),
        ("exceptions", "exception"),
        ("cache_events", "cache_event"),
        ("mails", "mail"),
        ("broadcasts", "broadcast"),
        ("outgoing_requests", "outgoing_request"),
        ("storage_ops", "storage_op"),
        ("view_renders", "view_render"),
        ("logs", "log"),
        ("enqueued_jobs", "enqueued_job"),
        ("transactions", "transaction"),
        ("notifications", "notification"),
        ("spans", "span"),
        ("profiles", "profile"),
        ("attachments", "attachment"),
        ("llm_calls", "llm_call"),
    ];
    let mut entries: Vec<(f64, mcp_summary::Entry)> = vec![];
    for (table, singular) in CHILDREN {
        let select = if table == "queries" {
            "queries.*, COALESCE(NULLIF(queries.sql, ''), (SELECT query_shapes.sql FROM query_shapes WHERE query_shapes.group_hash = queries.group_hash), '') AS resolved_sql".to_string()
        } else {
            format!("\"{table}\".*")
        };
        let rows = db::query(conn, &format!("SELECT {select} FROM \"{table}\" WHERE \"{table}\".\"execution_id\" = ?1"), [eid])?;
        let has_duration = has_column(table, "duration");
        let has_source = has_column(table, "source");
        for r in &rows {
            let first = |s: Option<&str>| rb::first_chars(s.unwrap_or(""), 120).to_string();
            let label = match table {
                "queries" => first(r.s("resolved_sql")),
                "exceptions" => format!("{}: {}", r.s("class_name").unwrap_or(""), rb::first_chars(r.s("message").unwrap_or(""), 100)),
                "cache_events" => first(Some(&format!("{} {}", r.s("type").unwrap_or(""), r.s("key").unwrap_or("")))),
                "mails" => first(r.s("mailer")),
                "broadcasts" => first(r.s("stream")),
                "outgoing_requests" => first(r.s("url")),
                "storage_ops" => first(r.s("key")),
                "view_renders" => first(r.s("identifier")),
                "logs" => first(r.s("message")),
                "enqueued_jobs" => first(r.s("name")),
                "transactions" => first(r.s("outcome")),
                "notifications" => first(r.s("notifier")),
                "spans" => first(r.s("name")),
                "profiles" => format!("{} profile ({} samples)", r.s("profiler").unwrap_or(""), r.i("samples").map_or(String::new(), |s| s.to_string())),
                "attachments" => first(r.s("name")),
                _ => {
                    if r.s("operation") == Some("tool") {
                        r.s("tool_name").unwrap_or("").to_string()
                    } else {
                        format!("{}/{}", r.s("provider").unwrap_or(""), r.s("model").unwrap_or(""))
                    }
                }
            };
            let offset = rb::round(rb::time_sub(r.t("occurred_at").unwrap_or(0), start) * 1000.0, 3);
            let duration = if has_duration { r.i("duration").map(|d| d as f64 / 1000.0) } else { None };
            let source = if has_source { r.s("source").map(str::to_string) } else { None };
            let row = crate::obj! {
                "type" => singular, "id" => r.i("id"), "offset" => jf(offset), "duration" => duration.map_or(Value::Null, jf),
                "label" => label, "stage" => r.s("execution_stage"), "source" => source.clone(),
            };
            entries.push((
                offset,
                mcp_summary::Entry {
                    kind: singular,
                    offset,
                    duration,
                    group_hash: r.s("group_hash").map(str::to_string),
                    sql: (table == "queries").then(|| r.s("resolved_sql").unwrap_or("").to_string()),
                    source,
                    row,
                },
            ));
        }
    }
    let sorted = rb::sort_by_num(entries, |(o, _)| rb::Num::F(*o));
    Ok(sorted.into_iter().map(|(_, e)| e).collect())
}

/// One execution's timeline and the summary get_execution leads with.
struct ExecutionFacts {
    entries: Vec<mcp_summary::Entry>,
    summary: Value,
    detail: Value,
    dropped: Option<i64>,
}

fn execution_facts(conn: &Connection, exe: &Rec) -> rusqlite::Result<ExecutionFacts> {
    let eid = exe.s("execution_id").unwrap_or("");
    let start = exe.t("occurred_at").unwrap_or(0);
    let duration_ms = exe.i("duration").unwrap_or(0) as f64 / 1000.0;
    let entries = execution_timeline(conn, eid, start)?;
    let n_plus_ones: Vec<mcp_summary::NPlusOne> =
        db::query(conn, "SELECT \"n_plus_ones\".* FROM \"n_plus_ones\" WHERE \"n_plus_ones\".\"execution_id\" = ?1 ORDER BY \"n_plus_ones\".\"id\"", [eid])?
            .iter()
            .map(|r| mcp_summary::NPlusOne {
                group_hash: r.s("group_hash").map(str::to_string),
                sql: r.s("sql").unwrap_or("").to_string(),
                source: r.s("source").map(str::to_string),
                count: r.i("count").unwrap_or(0),
            })
            .collect();
    let warnings: Vec<Value> = db::query(
        conn,
        "SELECT \"logs\".* FROM \"logs\" WHERE \"logs\".\"execution_id\" = ?1 AND \"logs\".\"level\" IN ('warn', 'error', 'fatal') ORDER BY \"logs\".\"occurred_at\" LIMIT 5",
        [eid],
    )?
    .iter()
    .map(|r| crate::obj! { "level" => r.s("level"), "message" => rb::first_chars(r.s("message").unwrap_or(""), 300), "source" => r.s("source") })
    .collect();
    let detail = exe.json("detail");
    let dropped = detail.get("dropped_records").and_then(Value::as_i64).filter(|n| *n > 0);
    let has_profile = entries.iter().any(|e| e.kind == "profile") || exe.i("profile_id").is_some();
    let summary = mcp_summary::execution_summary(&mcp_summary::Facts {
        duration_ms,
        entries: &entries,
        n_plus_ones: &n_plus_ones,
        warnings,
        dropped_records: dropped,
        has_profile,
    });
    Ok(ExecutionFacts { entries, summary, detail, dropped })
}

/// get_execution's `summary` of one execution (where the time went, the
/// repeated queries, the N+1s with their fix, warnings and notes); None when
/// no execution has that id. For anything that hands an agent an execution
/// without a tool call (the agent webhook).
pub fn execution_summary_of(conn: &Connection, execution_id: &str) -> rusqlite::Result<Option<Value>> {
    let Some(exe) = db::query_one(conn, "SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"execution_id\" = ?1 LIMIT 1", [execution_id])?
    else {
        return Ok(None);
    };
    Ok(Some(execution_facts(conn, &exe)?.summary))
}

/// Timeline rows per page by default, and at most.
const TIMELINE_PAGE: i64 = 25;
const TIMELINE_PAGE_MAX: i64 = 200;

/// One page of the timeline (`timeline_offset`, `timeline_limit`, and
/// `timeline_type` to keep one record type), and where the next one starts.
fn timeline_page(entries: &[mcp_summary::Entry], a: &Args) -> R<(Value, Value)> {
    let offset = to_i(a.get("timeline_offset"))?.max(0) as usize;
    let limit = clamp(a.get("timeline_limit"), TIMELINE_PAGE, TIMELINE_PAGE_MAX)? as usize;
    let wanted = a.presence("timeline_type");
    let matching: Vec<&mcp_summary::Entry> = entries.iter().filter(|e| wanted.as_deref().is_none_or(|t| e.kind == t)).collect();
    let rows: Vec<Value> = matching.iter().skip(offset).take(limit).map(|e| e.row.clone()).collect();
    let next = offset + rows.len();
    let more = next < matching.len();
    let mut page = crate::obj! {
        "offset" => offset, "returned" => rows.len(), "total" => matching.len(),
        "next_offset" => if more { Value::from(next) } else { Value::Null },
    };
    if let Some(t) = &wanted {
        db::merge_into(&mut page, "type", Value::from(t.clone()));
    }
    if more {
        db::merge_into(
            &mut page,
            "hint",
            Value::from(format!(
                "{} more rows. Pass timeline_offset: {next} for the next page (timeline_limit up to {TIMELINE_PAGE_MAX}), or timeline_type (query, transaction, view_render, log, ...) for one kind of record. summary already lists the slowest rows and the repeated queries.",
                matching.len() - next
            )),
        );
    }
    Ok((Value::Array(rows), page))
}

/// The newest execution of `kinds` whose path, route or name contains
/// `path`, at or after `since` when given. One lookup per kind, so each walks
/// the (kind, occurred_at) index backwards and stops at its first match.
fn newest_execution(conn: &Connection, kinds: &[&str], path: Option<&str>, since: Option<&str>) -> rusqlite::Result<Option<Rec>> {
    let mut best: Option<Rec> = None;
    for kind in kinds {
        let mut sql = String::from("SELECT \"executions\".* FROM \"executions\" WHERE \"executions\".\"kind\" = ?");
        let mut binds: Vec<rusqlite::types::Value> = vec![kind.to_string().into()];
        if let Some(since) = since {
            sql.push_str(" AND \"executions\".\"occurred_at\" >= ?");
            binds.push(since.to_string().into());
        }
        if let Some(p) = path {
            let like = format!("%{}%", p.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            sql.push_str(
                " AND (\"executions\".\"name\" LIKE ? ESCAPE '\\' OR \"executions\".\"route\" LIKE ? ESCAPE '\\' \
                 OR json_extract(\"executions\".\"detail\", '$.path') LIKE ? ESCAPE '\\')",
            );
            binds.extend([like.clone().into(), like.clone().into(), like.into()]);
        }
        sql.push_str(" ORDER BY \"executions\".\"occurred_at\" DESC, \"executions\".\"id\" DESC LIMIT 1");
        if let Some(r) = db::query_one(conn, &sql, params_from_iter(binds))?
            && best.as_ref().is_none_or(|b| r.t("occurred_at") > b.t("occurred_at"))
        {
            best = Some(r);
        }
    }
    Ok(best)
}

/// The dashboard page of one execution, as the frontend's executionPath
/// builds it.
fn execution_path(app: i64, env: i64, kind: Option<&str>, execution_id: &str) -> String {
    let section = match kind {
        Some("request") => "requests",
        Some("job" | "job_attempt") => "jobs",
        Some("scheduled_task") => "scheduled_tasks",
        Some("command") => "commands",
        _ => "executions",
    };
    format!("/apps/{app}/envs/{env}/{section}/{}", segment(execution_id))
}

/// A path segment, percent-encoded (tenant names and deploy versions are
/// whatever the app sent).
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What to do when a tool's result is over the cap.
fn narrow_hint(tool: &str) -> &'static str {
    match tool {
        "get_execution" | "last_execution" => {
            "Read summary first. Page the timeline with timeline_offset/timeline_limit, or keep one record type with timeline_type."
        }
        "get_issue" => {
            "Frames are collapsed unless frames: \"all\" was passed; omit it. Read the in-app frames, then get_execution on the occurrence's execution_id."
        }
        "search_telemetry" | "search_logs" => {
            "Pass a smaller limit, a narrower window, or more filters in q (e.g. level:error, source:app/models/order.rb). meta.next_cursor pages on."
        }
        "search_requests" => "Pass a smaller limit or a narrower q (route:, status:, min_ms:) or window.",
        "web_vitals" => "Pass a smaller limit, or component to read one component.",
        "release_health" => "Pass a narrower window.",
        "get_route" => "Pass a narrower window.",
        "explain_route" => "Pass a narrower window; get_execution on one of slowest_requests has a single request's detail.",
        "investigate_issue" => "Read occurrence and lead_up first; get_execution on occurrence.execution_id pages the full timeline.",
        "get_profile" => "Pass a smaller limit.",
        _ => "Pass a smaller limit or a narrower window.",
    }
}

fn has_column(table: &str, col: &str) -> bool {
    crate::schema::schema().tables.get(table).is_some_and(|cs| cs.iter().any(|c| c.name == col))
}

// ---------------------------------------------------------------------------
// Prompts and resources

impl Mcp<'_> {
    fn get_prompt(&self, id: &Value, params: &Value) -> R<Value> {
        let name = index(params, "name").map_err(|_| Fail::Unrescued)?;
        let d = definitions();
        let Some(prompt) = d.prompts.iter().find(|p| p["name"] == name && name.is_string()) else {
            return Ok(error(id, -32602, &format!("Unknown prompt: {}", ruby_interpolate(&name))));
        };
        let args = match index(params, "arguments").map_err(|_| Fail::Unrescued)? {
            Value::Null | Value::Bool(false) => Value::Object(Map::new()),
            v => v,
        };
        let text = prompt_text(name.as_str().unwrap_or(""), &args).map_err(|_| Fail::Unrescued)?;
        Ok(success(
            id,
            json!({"description": prompt["description"],
            "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}),
        ))
    }

    fn resources(&self) -> R<Vec<Value>> {
        let mut list = vec![json!({"uri": "railwatch://applications", "name": "Applications",
            "description": "Every application and environment this token can see.", "mimeType": "application/json"})];
        for app in self.accessible_applications()? {
            let mut envs = self.environments_of(app.id)?;
            envs.sort_by(|a, b| a.0.cmp(&b.0));
            for (env, _, _) in envs {
                list.push(json!({"uri": format!("railwatch://applications/{}/{}/environments/{env}/summary", app.account_slug, app.slug),
                    "name": format!("{} / {env} summary", app.name),
                    "description": "Request and job volume, p95, error rate, open issues, and last-seen time over the last 24 hours.",
                    "mimeType": "application/json"}));
            }
        }
        for (name, text) in gem_docs() {
            let description = text.lines().find(|l| l.starts_with("# ")).map(|l| l.trim_start_matches('#').trim().to_string());
            list.push(json!({"uri": format!("railwatch://docs/{name}"), "name": format!("Railwatch docs: {name}"),
                "description": description.unwrap_or_else(|| "Railwatch documentation.".into()), "mimeType": "text/markdown"}));
        }
        Ok(list)
    }

    fn read_resource(&self, id: &Value, params: &Value) -> R<Value> {
        let uri = to_s(&index(params, "uri").map_err(|_| Fail::Unrescued)?);
        let content = |mime: &str, text: String| success(id, json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}));
        if uri == "railwatch://applications" {
            return Ok(content("application/json", self.list_applications()?));
        }
        if let Some(rest) = uri.strip_prefix("railwatch://applications/")
            && let Some(path) = rest.strip_suffix("/summary")
        {
            let seg: Vec<&str> = path.split('/').collect();
            let args = match seg.as_slice() {
                [acc, app, "environments", env] if [acc, app, env].iter().all(|s| !s.is_empty()) => {
                    Some(json!({"account_slug": acc, "application_slug": app, "environment": env}))
                }
                [app, "environments", env] if !app.is_empty() && !env.is_empty() => Some(json!({"application_slug": app, "environment": env})),
                _ => None,
            };
            if let Some(Value::Object(m)) = args {
                let Some(env) = self.resolve_environment(&Args(&m))? else {
                    return Ok(error(id, -32602, &format!("Unknown environment: {uri}")));
                };
                return Ok(content("application/json", self.environment_summary(&env)?.to_string()));
            }
        }
        if let Some(name) = uri.strip_prefix("railwatch://docs/").filter(|n| !n.is_empty() && !n.contains('\n')) {
            return match gem_docs().iter().find(|(n, _)| *n == name.to_lowercase()) {
                Some((_, text)) => Ok(content("text/markdown", text.to_string())),
                None => Ok(error(id, -32602, &format!("Unknown document: {uri}"))),
            };
        }
        Ok(error(id, -32602, &format!("Unknown resource: {uri}")))
    }

    fn environment_summary(&self, env: &EnvRow) -> R<Value> {
        let to = self.now;
        let from = to - 24 * 3_600_000_000;
        let previous_from = from - 24 * 3_600_000_000;
        let (requests, jobs) = self.with_telemetry(env, |conn| {
            let row = |t: &str| -> rusqlite::Result<Value> {
                let cur = rollup::summarize_owned(&rollup::between(conn, t, from, to)?);
                let prev = rollup::summarize_owned(&rollup::between(conn, t, previous_from, from)?);
                Ok(crate::obj! {
                    "count" => cur.count, "errors" => cur.errors, "p95_ms" => ms(Some(cur.p95)), "max_ms" => ms(Some(cur.max)),
                    "previous_count" => prev.count, "previous_p95_ms" => ms(Some(prev.p95)),
                })
            };
            Ok((row("request")?, row("job_attempt")?))
        })?;
        let (open, deploys): (i64, Vec<Value>) = self.env.control.with(|c| {
            let open = c.query_row("SELECT COUNT(*) FROM issues WHERE environment_id = ? AND status = 'open'", [env.id], |r| r.get(0))?;
            let deploys = c
                .prepare(
                    "SELECT deploy, deployed_at FROM deploys WHERE environment_id = ?1 AND deployed_at BETWEEN ?2 AND ?3 ORDER BY deployed_at DESC LIMIT 10",
                )?
                .query_map(params![env.id, qt(from), qt(to)], |r| Ok(json!({"deploy": r.get::<_, String>(0)?, "deployed_at": db_time_s(r.get(1)?)})))?
                .collect::<rusqlite::Result<_>>()?;
            Ok((open, deploys))
        })?;
        Ok(crate::obj! {
            "application" => env.app_slug.clone(), "environment" => env.name.clone(), "window" => "24h",
            "account_slug" => env.account_slug.clone(), "application_id" => env.application_id,
            "requests" => requests, "jobs" => jobs, "open_issues" => open,
            "last_seen_at" => db_time_s(env.last_seen_at.clone()), "deploys" => deploys,
        })
    }
}

/// The prompt's text: Rails' heredoc with each `#{args["x"]}` filled with
/// `"#{value}"`, and window_clause.
fn prompt_text(name: &str, args: &Value) -> Result<String, ()> {
    let template = definitions().prompt_texts.get(name).and_then(Value::as_str).ok_or(())?;
    let get = |k: &str| index(args, k);
    let mut out = String::with_capacity(template.len() + 64);
    let mut parts = template.split('\u{1}');
    out.push_str(parts.next().unwrap_or(""));
    while let (Some(key), Some(rest)) = (parts.next(), parts.next()) {
        if key == "window_clause" {
            let w = get("window")?;
            if present(&w) {
                out.push_str(&format!(" and window \"{}\"", ruby_interpolate(&w)));
            }
        } else {
            out.push_str(&ruby_interpolate(&get(key)?));
        }
        out.push_str(rest);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected orders printed by Ruby 4.0.0 in rw-baseline:
    /// `a.each_with_index.map { |(x, y), i| [i, y] }.max_by(10) { |i, y| y }.map(&:first)`.
    #[test]
    fn max_by_n_keeps_mris_tie_order() {
        let a: Vec<(usize, i64)> = [5, 3, 5, 5, 1].iter().cycle().take(100).copied().enumerate().collect();
        let got: Vec<usize> = max_by_n(&a, 10, |(_, y)| *y).into_iter().map(|(i, _)| *i).collect();
        assert_eq!(got, [23, 33, 25, 2, 27, 28, 32, 30, 0, 15]);
        let b: Vec<(usize, i64)> = (0..57).map(|i| (i, ((i * 7919) % 13) as i64)).collect();
        let got: Vec<usize> = max_by_n(&b, 10, |(_, v)| *v).into_iter().map(|(i, _)| *i).collect();
        assert_eq!(got, [45, 32, 6, 19, 51, 25, 12, 38, 5, 18]);
        assert!(max_by_n(&b, 0, |(_, v)| *v).is_empty());
    }

    #[test]
    fn ruby_string_conversions() {
        assert_eq!(ruby_string_to_f("1.5e3"), 1500.0);
        assert_eq!(ruby_string_to_f(" 12abc"), 12.0);
        assert_eq!(ruby_string_to_f("1_000"), 1000.0);
        assert_eq!(ruby_string_to_f("abc"), 0.0);
        assert_eq!(split_semicolons("x;y;;"), ["x", "y"]);
        // key.split("/", 3)
        assert_eq!(split_issue_key("a/b"), (Some("a".into()), None, "b".into()));
        assert_eq!(split_issue_key("a/"), (Some("a".into()), None, "".into()));
        assert_eq!(split_issue_key("a//b"), (Some("a".into()), Some("".into()), "b".into()));
        assert_eq!(split_issue_key("APP-1"), (None, None, "APP-1".into()));
        assert!(positive_integer(&json!("42")) && positive_integer(&json!(7)));
        assert!(!positive_integer(&json!("042")) && !positive_integer(&json!("4a")) && !positive_integer(&json!(-1)));
        assert_eq!(clamp(&json!(-3), 20, 100).ok(), Some(3));
        assert_eq!(clamp(&json!("7"), 20, 100).ok(), Some(7));
        assert_eq!(clamp(&json!(0), 20, 100).ok(), Some(20));
        assert_eq!(clamp(&json!(500), 50, 200).ok(), Some(200));
    }

    #[test]
    fn hot_frames_counts_self_and_total_once_per_line() {
        let v = hot_frames("main;a;b 3\nmain;a 1\nmain;b;b 2\nbad\n 4\n", 10);
        let f: Vec<(&str, i64, i64)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["frame"].as_str().unwrap(), r["self_samples"].as_i64().unwrap(), r["total_samples"].as_i64().unwrap()))
            .collect();
        assert_eq!(f, [("b", 5, 5), ("a", 1, 4)]);
        assert_eq!(v[0]["self_pct"].to_string(), "83.3");
    }

    #[test]
    fn times_render_as_ruby_time_to_s() {
        assert_eq!(time_s(Some(1_792_030_354_123_456)), json!("2026-10-15 02:12:34 UTC"));
        let mut v = json!({"occurred_at": "2026-10-15T02:12:34.123Z", "name": "2026-10-15T02:12:34.123Z!"});
        to_s_times(&mut v);
        assert_eq!(v["occurred_at"], "2026-10-15 02:12:34 UTC");
        assert_eq!(v["name"], "2026-10-15T02:12:34.123Z!");
    }

    #[test]
    fn the_body_is_json_whatever_its_type_and_must_be_an_object_with_object_params() {
        use crate::control::{Control, insert_user, issue_api_token, test_password};
        let dir = std::env::temp_dir().join(format!("rw-mcp-{}-body", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let scopes: Vec<String> = ["mcp", "read"].iter().map(|s| s.to_string()).collect();
        let (_, raw) = c.tx(|tx| issue_api_token(tx, ada, "t", &scopes, None, None, None)).unwrap();
        let (followups, _rx) = crate::followups::channel();
        let tenants = crate::tenants::Tenants::new(dir.join("telemetry"), std::time::Duration::from_secs(60), followups);
        let readers = crate::dashboard::pool::ReadPools::new(dir.join("telemetry"), 1);
        let key = CursorKey(b"k".to_vec());
        let env = Env { control: &c, tenants: &tenants, readers: &readers, cursor_key: &key };
        let auth = format!("Bearer {raw}");
        let post = |body: &str| call(&env, 1_792_000_000_000_000, Some(&auth), &Bytes::from(body.to_string()));
        // Malformed: -32700.
        assert_eq!(post("{bad").0, StatusCode::BAD_REQUEST);
        assert_eq!(post("{bad").1["error"]["code"], -32700);
        // Valid JSON, not an object: 400 -32600 with a null id.
        for body in ["[1]", "null", "42", "\"tools/list\""] {
            assert_eq!(
                post(body),
                (StatusCode::BAD_REQUEST, json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Invalid Request"}})),
                "{body}"
            );
        }
        // params present and not an object, for any method: 200 -32602.
        for p in ["[]", "\"x\"", "1", "true", "false"] {
            for m in ["initialize", "prompts/get", "nope"] {
                assert_eq!(
                    post(&format!(r#"{{"jsonrpc":"2.0","id":7,"method":"{m}","params":{p}}}"#)),
                    (StatusCode::OK, json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32602, "message": "Invalid params"}})),
                    "{m} {p}"
                );
            }
        }
        // params absent or null is {}.
        assert_eq!(post(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":null}"#).1["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(post(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).0, StatusCode::OK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Agent-facing behaviour, over a seeded environment -------------------

    const NOW: i64 = 1_792_000_000_000_000;

    struct Seeded {
        dir: std::path::PathBuf,
        c: Control,
        tenants: crate::tenants::Tenants,
        readers: crate::dashboard::pool::ReadPools,
        key: CursorKey,
        token: String,
        app: i64,
        env: i64,
        slug: String,
    }

    impl Drop for Seeded {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    use crate::control::{Control, create_account, create_environment, insert_application, insert_user, issue_api_token, test_password};

    /// One account, one application ("shop"), one environment, and a token
    /// that sees only them: what an embedded server looks like.
    fn seeded(name: &str) -> Seeded {
        let dir = std::env::temp_dir().join(format!("rw-mcp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "development")).unwrap();
        let slug: String = c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0))).unwrap();
        let scopes: Vec<String> = ["mcp", "read", "write"].iter().map(|s| s.to_string()).collect();
        let (_, token) = c.tx(|tx| issue_api_token(tx, ada, "t", &scopes, None, None, None)).unwrap();
        let (followups, _rx) = crate::followups::channel();
        let tenants = crate::tenants::Tenants::new(dir.join("telemetry"), std::time::Duration::from_secs(60), followups);
        let readers = crate::dashboard::pool::ReadPools::new(dir.join("telemetry"), 1);
        Seeded { dir, c, tenants, readers, key: CursorKey(b"k".to_vec()), token, app, env, slug }
    }

    impl Seeded {
        fn tenant(&self) -> Connection {
            crate::tenants::open_tenant(&self.dir.join("telemetry").join(format!("{}.sqlite3", self.slug))).unwrap()
        }

        fn rpc(&self, base: &str, body: Value) -> Value {
            let env = Env { control: &self.c, tenants: &self.tenants, readers: &self.readers, cursor_key: &self.key };
            let (status, v) = call_at(&env, base, NOW, Some(&format!("Bearer {}", self.token)), &Bytes::from(body.to_string()));
            assert_eq!(status, StatusCode::OK, "{v}");
            v
        }

        /// A tool's result text, as the client gets it.
        fn text(&self, name: &str, args: Value) -> String {
            let v = self.rpc(
                "https://rw.example.com/railwatch",
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}),
            );
            v["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {v}")).to_string()
        }

        fn tool(&self, name: &str, args: Value) -> Value {
            serde_json::from_str(&self.text(name, args)).unwrap()
        }
    }

    /// An execution shaped like the production invoicing request: 3.67 s,
    /// twelve commits holding most of it, 826 queries in 77 shapes (one run
    /// 60 times from one line), an N+1 on it, and a warning.
    fn seed_invoicing(conn: &Connection, eid: &str, at: i64) {
        let t = |ms: f64| qt(at + (ms * 1000.0) as i64);
        conn.execute(
            "INSERT INTO executions (execution_id, kind, name, route, group_hash, status, occurred_at, duration, detail)
             VALUES (?1, 'request', 'GET /:org_slug/invoicing(.:format)', '/:org_slug/invoicing(.:format)', 'route_invoicing', 200, ?2, 3669559, ?3)",
            params![eid, qt(at), json!({"path": "/acme/invoicing"}).to_string()],
        )
        .unwrap();
        let mut offset = 200.0;
        for i in 0..12 {
            let d = if i < 5 { 400.0 } else { 120.0 };
            conn.execute(
                "INSERT INTO transactions (execution_id, occurred_at, duration, outcome) VALUES (?1, ?2, ?3, 'commit')",
                params![eid, t(offset), (d * 1000.0) as i64],
            )
            .unwrap();
            offset += d + 5.0;
        }
        let q = |n: usize, sql: &str, group: &str, source: &str, start: f64| {
            for k in 0..n {
                conn.execute(
                    "INSERT INTO queries (execution_id, occurred_at, duration, sql, group_hash, source) VALUES (?1, ?2, 100, ?3, ?4, ?5)",
                    params![eid, t(start + k as f64 * 2.0), sql, group, source],
                )
                .unwrap();
            }
        };
        q(
            60,
            "SELECT \"car_invoices\".* FROM \"car_invoices\" WHERE \"car_invoices\".\"customer_id\" = ?",
            "g_invoices",
            "app/services/billing/customer_documents.rb:511",
            210.0,
        );
        for shape in 0..76 {
            let n = if shape < 70 { 10 } else { 11 };
            q(
                n,
                &format!("SELECT \"t{shape}\".* FROM \"t{shape}\" WHERE \"t{shape}\".\"id\" = ?"),
                &format!("g{shape:02}"),
                &format!("app/models/m{shape}.rb:{shape}"),
                300.0 + shape as f64 * 40.0,
            );
        }
        conn.execute(
            "INSERT INTO n_plus_ones (execution_id, occurred_at, count, sql, group_hash, source) VALUES (?1, ?2, 5, ?3, 'g_invoices', 'app/services/billing/customer_documents.rb:511')",
            params![eid, t(220.0), "SELECT \"car_invoices\".* FROM \"car_invoices\" WHERE \"car_invoices\".\"customer_id\" = ?"],
        )
        .unwrap();
        conn.execute("INSERT INTO logs (execution_id, occurred_at, level, message) VALUES (?1, ?2, 'warn', 'slow invoice sync')", params![eid, t(3000.0)])
            .unwrap();
        conn.execute("INSERT INTO logs (execution_id, occurred_at, level, message) VALUES (?1, ?2, 'info', 'Completed 200 OK')", params![eid, t(3660.0)])
            .unwrap();
    }

    #[test]
    fn get_execution_leads_with_where_the_time_went() {
        let f = seeded("summary");
        seed_invoicing(&f.tenant(), "e-invoicing", NOW - 60_000_000);
        // No application or environment: the token sees one of each.
        let v = f.tool("get_execution", json!({"execution_id": "e-invoicing"}));
        let s = &v["summary"];
        assert_eq!(s["counts"]["query"], 826);
        assert_eq!(s["counts"]["transaction"], 12);
        assert_eq!(s["distinct_query_shapes"], 77);
        assert_eq!(s["time_ms"]["sql"].as_f64(), Some(82.6));
        assert_eq!(s["time_ms"]["transactions"].as_f64(), Some(2840.0));
        assert!(s["time_ms"]["untracked"].as_f64().unwrap() > 700.0, "{}", s["time_ms"]);

        let top = &s["repeated_queries"][0];
        assert_eq!(top["count"], 60);
        assert_eq!(top["source"], "app/services/billing/customer_documents.rb:511");
        assert_eq!(top["total_ms"].as_f64(), Some(6.0));
        assert_eq!(s["n_plus_ones"][0]["repeats"], 60);
        assert_eq!(s["n_plus_ones"][0]["fix"], "Customer.includes(:car_invoices)");
        assert_eq!(s["slowest"][0]["type"], "transaction");
        assert_eq!(s["slowest"][0]["duration"].as_f64(), Some(400.0));
        assert_eq!(s["warnings"], json!([{"level": "warn", "message": "slow invoice sync", "source": null}]));
        let notes = s["notes"].to_string();
        assert!(notes.contains("12 database transaction(s)") && notes.contains("RAILWATCH_PROFILE_SLOW_MS"), "{notes}");

        // The timeline is one page, with the way to the rest.
        assert_eq!(v["timeline"].as_array().unwrap().len(), 25);
        assert_eq!(v["timeline_page"]["total"], 840);
        assert_eq!(v["timeline_page"]["next_offset"], 25);
        assert!(v["timeline_page"]["hint"].as_str().unwrap().contains("timeline_offset: 25"));
        assert_eq!(v["path"], "/acme/invoicing");
        assert_eq!(v["url"], format!("https://rw.example.com/railwatch/apps/{}/envs/{}/requests/e-invoicing", f.app, f.env));
        // The production response for this request was 224 KB.
        let bytes = f.text("get_execution", json!({"execution_id": "e-invoicing"})).len();
        assert!(bytes < 20_000, "{bytes} bytes");

        // Paging reaches every row, in offset order, and filters by type.
        let mut seen = 0;
        let mut offset = 0;
        let mut last_offset = f64::MIN;
        while let Some(o) = Some(offset) {
            let p = f.tool("get_execution", json!({"execution_id": "e-invoicing", "timeline_offset": o, "timeline_limit": 200}));
            for r in p["timeline"].as_array().unwrap() {
                let at = r["offset"].as_f64().unwrap();
                assert!(at >= last_offset);
                last_offset = at;
                seen += 1;
            }
            match p["timeline_page"]["next_offset"].as_u64() {
                Some(n) => offset = n,
                None => break,
            }
        }
        assert_eq!(seen, 840);
        let tx = f.tool("get_execution", json!({"execution_id": "e-invoicing", "timeline_type": "transaction"}));
        assert_eq!((tx["timeline"].as_array().unwrap().len(), tx["timeline_page"]["total"].as_u64()), (12, Some(12)));
        assert!(tx["timeline_page"]["next_offset"].is_null() && tx["timeline_page"].get("hint").is_none());
    }

    #[test]
    fn last_execution_finds_the_newest_by_path_and_kind() {
        let f = seeded("last");
        let conn = f.tenant();
        let empty = f.tool("last_execution", json!({}));
        assert!(empty["error"].as_str().unwrap().starts_with("no executions in shop/development"), "{empty}");
        assert!(empty["hint"].as_str().unwrap().contains("RAILWATCH_FLUSH_INTERVAL"));

        seed_invoicing(&conn, "e-old-invoicing", NOW - 3_600_000_000);
        let insert = |eid: &str, kind: &str, name: &str, path: Option<&str>, ago_s: i64| {
            let detail = path.map_or_else(|| "{}".to_string(), |p| json!({"path": p}).to_string());
            conn.execute(
                "INSERT INTO executions (execution_id, kind, name, occurred_at, duration, detail) VALUES (?1, ?2, ?3, ?4, 5000, ?5)",
                params![eid, kind, name, qt(NOW - ago_s * 1_000_000), detail],
            )
            .unwrap();
        };
        insert("e-checkout", "request", "POST /checkout", Some("/checkout"), 30);
        insert("e-job", "job_attempt", "InvoiceSyncJob", None, 20);
        insert("e-home", "request", "GET /", Some("/"), 10);

        assert_eq!(f.tool("last_execution", json!({}))["execution_id"], "e-home");
        assert_eq!(f.tool("last_execution", json!({"kind": "job"}))["execution_id"], "e-job");
        assert_eq!(f.tool("last_execution", json!({"kind": "request", "path": "/checkout"}))["execution_id"], "e-checkout");
        // A path matches the request path as well as the route pattern.
        let inv = f.tool("last_execution", json!({"path": "/acme/invoicing"}));
        assert_eq!(inv["execution_id"], "e-old-invoicing");
        assert_eq!(inv["summary"]["n_plus_ones"][0]["source"], "app/services/billing/customer_documents.rb:511", "the dev loop's answer in one call");
        assert_eq!(
            f.tool("last_execution", json!({"path": "InvoiceSync"}))["url"],
            format!("https://rw.example.com/railwatch/apps/{}/envs/{}/jobs/e-job", f.app, f.env)
        );
        // LIKE wildcards in the path are literal.
        assert!(
            f.tool("last_execution", json!({"path": "/c%t"}))["error"].as_str().unwrap().contains("no request or job matching \"/c%t\" in the last 24 hours")
        );
        assert_eq!(f.tool("last_execution", json!({"kind": "mail"}))["error"], "kind must be request, job, or any");
    }

    #[test]
    fn application_and_environment_default_only_when_unambiguous() {
        let f = seeded("defaults");
        f.tenant()
            .execute(
                "INSERT INTO executions (execution_id, kind, name, occurred_at, duration) VALUES ('e1', 'request', 'GET /', ?1, 1000)",
                [qt(NOW - 1_000_000)],
            )
            .unwrap();
        // Passing them still works, by name or by slug.
        assert_eq!(f.tool("get_execution", json!({"application_slug": "shop", "environment": "development", "execution_id": "e1"}))["name"], "GET /");
        assert_eq!(f.tool("get_execution", json!({"application_slug": "shop", "environment": f.slug, "execution_id": "e1"}))["name"], "GET /");
        let wrong = f.tool("get_execution", json!({"environment": "staging", "execution_id": "e1"}));
        assert_eq!(wrong["error"], "environment not found");
        assert_eq!(wrong["hint"], "shop has no environment \"staging\": pass one of development.");
        assert_eq!(wrong["available"][0]["environments"], json!(["development"]));

        // A second environment: environment is required again; the app is not.
        f.c.tx(|tx| create_environment(tx, f.app, "production")).unwrap();
        let two = f.tool("search_requests", json!({}));
        assert_eq!(two["hint"], "shop has more than one environment: pass environment, one of development, production.");
        assert_eq!(f.tool("search_requests", json!({"environment": "development"}))[0]["execution_id"], "e1");

        // A second application: the slug is required.
        let account: i64 = f.c.with(|x| x.query_row("SELECT account_id FROM applications WHERE id = ?", [f.app], |r| r.get(0))).unwrap();
        f.c.tx(|tx| insert_application(tx, account, "Admin", "admin", "ADM", None, "main")).unwrap();
        let apps = f.tool("search_requests", json!({"environment": "development"}));
        assert!(apps["hint"].as_str().unwrap().starts_with("This token can see more than one application"), "{apps}");
        assert_eq!(apps["available"].as_array().unwrap().len(), 2);
        assert_eq!(f.tool("search_requests", json!({"application_slug": "shop", "environment": "development"}))[0]["execution_id"], "e1");
    }

    /// An exception issue whose sample is its latest occurrence, raised
    /// through 45 framework frames.
    fn seed_exception_issue(f: &Seeded) -> i64 {
        let mut frames = vec![
            json!({"file": "/usr/local/bundle/ruby/3.4.0/gems/cloudflare-email-0.4.2/lib/cloudflare/email/client.rb", "line": 163, "function": "Cloudflare::Email::Client#request", "in_app": false}),
            json!({"file": "app/services/outbound_email/cloudflare_feedback_consumer.rb", "line": 41, "function": "poll", "in_app": true, "code": {"41": "count = @client.poll(batch_size: 1) do |event|"}}),
            json!({"file": "app/jobs/outbound_email_feedback_job.rb", "line": 9, "function": "perform", "in_app": true}),
        ];
        for i in 0..45 {
            let gem = ["activejob-8.1.4", "activesupport-8.1.4", "solid_queue-1.7.0"][i % 3];
            frames
                .push(json!({"file": format!("/usr/local/bundle/ruby/3.4.0/gems/{gem}/lib/x{i}.rb"), "line": i, "function": format!("f{i}"), "in_app": false}));
        }
        let conn = f.tenant();
        conn.execute(
            "INSERT INTO executions (execution_id, kind, name, occurred_at, duration) VALUES ('e-job', 'job_attempt', 'OutboundEmailFeedbackJob', ?1, 1000)",
            [qt(NOW - 3_600_000_000)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO exceptions (id, class_name, message, occurred_at, group_hash, frames, execution_id, execution_source)
             VALUES (55, 'Cloudflare::Email::NetworkError', 'provider response could not be read', ?1, 'gx', ?2, 'e-job', 'job')",
            params![qt(NOW - 3_600_000_000), Value::Array(frames).to_string()],
        )
        .unwrap();
        f.c.with(|x| {
            x.execute(
                "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                   first_seen_at, last_seen_at, created_at, updated_at, sample)
                 VALUES (?1, ?2, 79, 'Cloudflare::Email::NetworkError', 'exception', 'open', 'gx', 3, 0, ?3, ?3, ?3, ?3, ?4)",
                params![f.app, f.env, qt(NOW - 3_600_000_000), json!({"exception_id": 55, "execution_id": "e-job"}).to_string()],
            )?;
            Ok(x.last_insert_rowid())
        })
        .unwrap()
    }

    #[test]
    fn get_issue_prints_one_occurrence_and_collapses_framework_frames() {
        let f = seeded("issue");
        let id = seed_exception_issue(&f);
        let v = f.tool("get_issue", json!({"key": "SHOP-79"}));
        assert_eq!(v["url"], format!("https://rw.example.com/railwatch/issues/{id}"));
        let o = &v["sample_occurrence"];
        assert_eq!(o["id"], 55);
        assert_eq!(o["execution_url"], format!("https://rw.example.com/railwatch/apps/{}/envs/{}/jobs/e-job", f.app, f.env));
        assert_eq!(v["last_occurrence"], json!({"id": 55, "same_as": "sample_occurrence"}));
        let frames = o["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 4, "{frames:?}");
        assert_eq!(frames[0]["function"], "Cloudflare::Email::Client#request", "the raising frame stays");
        assert_eq!(frames[1]["code"]["41"], "count = @client.poll(batch_size: 1) do |event|", "in-app frames keep their code");
        assert_eq!(frames[3], json!({"omitted_frames": 45, "gems": ["activejob", "activesupport", "solid_queue"]}));
        // frames: all is the stored backtrace.
        let all = f.tool("get_issue", json!({"key": "SHOP-79", "frames": "all"}));
        assert_eq!(all["sample_occurrence"]["frames"].as_array().unwrap().len(), 48);
        // A newer occurrence is printed too.
        f.tenant()
            .execute(
                "INSERT INTO exceptions (id, class_name, message, occurred_at, group_hash, frames) VALUES (56, 'X', 'y', ?1, 'gx', '[]')",
                [qt(NOW - 60_000_000)],
            )
            .unwrap();
        let two = f.tool("get_issue", json!({"key": "SHOP-79"}));
        assert_eq!((two["sample_occurrence"]["id"].as_i64(), two["last_occurrence"]["id"].as_i64()), (Some(55), Some(56)));
        assert!(two["last_occurrence"].get("same_as").is_none());
        let missing = f.tool("get_issue", json!({"key": "SHOP-999"}));
        assert!(missing["hint"].as_str().unwrap().contains("list_issues"));
    }

    #[test]
    fn list_slow_routes_ranks_by_total_time_unless_asked() {
        let f = seeded("routes");
        let conn = f.tenant();
        let bucket = qt(rb::beginning_of_hour(NOW - 3_600_000_000));
        // Hit 4 times at 3 s; hit 28,000 times at 40 ms.
        for (g, name, count, sum, p95) in [("rare", "GET /reports", 4, 12_000_000i64, 3_500_000), ("hot", "POST /ingest", 28_000, 1_120_000_000, 90_000)] {
            conn.execute(
                "INSERT INTO rollups (record_type, group_hash, name, bucket, count, duration_sum, p95) VALUES ('request', ?1, ?2, ?3, ?4, ?5, ?6)",
                params![g, name, bucket, count, sum, p95],
            )
            .unwrap();
        }
        let by_total = f.tool("list_slow_routes", json!({}));
        assert_eq!(by_total[0]["route"], "POST /ingest");
        assert_eq!((by_total[0]["total_ms"].as_f64(), by_total[0]["avg_ms"].as_f64()), (Some(1_120_000.0), Some(40.0)));
        assert_eq!(by_total[0]["url"], format!("https://rw.example.com/railwatch/apps/{}/envs/{}/requests/routes/hot", f.app, f.env));
        assert_eq!(f.tool("list_slow_routes", json!({"sort": "avg"}))[0]["route"], "GET /reports");
        assert_eq!(
            f.tool("list_slow_routes", json!({"sort": "p95", "window": "7d"}))[0]["url"].as_str().map(|u| u.ends_with("/routes/rare?window=7d")),
            Some(true)
        );
        assert_eq!(f.tool("list_slow_routes", json!({"sort": "count"}))["error"], "sort must be total, avg, or p95");
    }

    #[test]
    fn list_issues_sort_impact_weighs_occurrences_by_recency() {
        let f = seeded("impact");
        let issue = |n: i64, occurrences: i64, days_ago: i64| {
            let at = qt(NOW - days_ago * 86_400_000_000);
            f.c.with(|x| {
                x.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                       first_seen_at, last_seen_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'E' || ?3, 'exception', 'open', 'g' || ?3, ?4, 0, ?5, ?5, ?5, ?5)",
                    params![f.app, f.env, n, occurrences, at],
                )
            })
            .unwrap();
        };
        issue(1, 1, 0); // a one-off, just now
        issue(2, 300, 0); // frequent and current
        issue(3, 400, 30); // frequent a month ago
        issue(4, 20, 1); // steady
        let keys = |v: Value| v.as_array().unwrap().iter().map(|i| i["key"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        // Default: newest first, as before.
        assert_eq!(keys(f.tool("list_issues", json!({})))[..2], ["SHOP-1", "SHOP-2"]);
        let impact = f.tool("list_issues", json!({"sort": "impact"}));
        assert_eq!(keys(impact.clone()), ["SHOP-2", "SHOP-3", "SHOP-4", "SHOP-1"]);
        assert_eq!(impact[0]["impact"].as_f64(), Some(300.0));
        assert_eq!(impact[1]["impact"].as_f64(), Some(12.9));
        assert_eq!(f.tool("list_issues", json!({"sort": "loud"}))["error"], "sort must be recent or impact");
    }

    #[test]
    fn missing_profiles_and_plans_name_the_setting() {
        let f = seeded("hints");
        let p = f.tool("get_profile", json!({"execution_id": "nope"}));
        assert_eq!(p["error"], "profile not found");
        let hint = p["hint"].as_str().unwrap();
        assert!(hint.contains("RAILWATCH_PROFILE_SAMPLE_RATE") && hint.contains("RAILWATCH_PROFILE_SLOW_MS") && hint.contains("vernier"), "{hint}");
        f.tenant()
            .execute(
                "INSERT INTO queries (occurred_at, duration, sql, group_hash, source) VALUES (?1, 250000, 'SELECT 1', 'gq', 'app/x.rb:1')",
                [qt(NOW - 60_000_000)],
            )
            .unwrap();
        let q = f.tool("explain_query", json!({"group_hash": "gq"}));
        assert!(q["explain"].is_null() && q["hint"].as_str().unwrap().contains("RAILWATCH_CAPTURE_QUERY_EXPLAIN"), "{q}");
        assert_eq!(q["url"], format!("https://rw.example.com/railwatch/apps/{}/envs/{}/queries/gq", f.app, f.env));
    }

    #[test]
    fn urls_are_paths_without_a_base_and_encode_what_the_app_sent() {
        let f = seeded("paths");
        f.tenant()
            .execute(
                "INSERT INTO executions (execution_id, kind, name, occurred_at, duration) VALUES ('a b/c', 'command', 'rake x', ?1, 1)",
                [qt(NOW - 1_000_000)],
            )
            .unwrap();
        let v = f.rpc("", json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "last_execution", "arguments": {}}}));
        let v: Value = serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["url"], format!("/apps/{}/envs/{}/commands/a%20b%2Fc", f.app, f.env));
        let apps = f.tool("list_applications", json!({}));
        assert_eq!(apps[0]["environments"][0]["url"], format!("https://rw.example.com/railwatch/apps/{}/envs/{}", f.app, f.env));
    }

    /// Deploys "d1" (two days ago) and "d2" (four hours ago, with commits),
    /// the issue's occurrences in d2's hour, and a resolved sibling issue of
    /// the same class with a comment saying how it was fixed.
    fn seed_issue_history(f: &Seeded, issue: i64) {
        f.c.with(|x| {
            x.execute(
                "INSERT INTO deploys (environment_id, deploy, ref, name, deployed_at, commits, created_at, updated_at) VALUES
                 (?1, 'd1', 'aaaaaaaaaaaa1111', 'shop@1', ?2, '[]', ?2, ?2),
                 (?1, 'd2', 'bbbbbbbbbbbb2222', 'shop@2', ?3, ?4, ?3, ?3)",
                params![
                    f.env,
                    qt(NOW - 48 * 3_600_000_000),
                    qt(NOW - 4 * 3_600_000_000),
                    json!([{"sha": "bbbbbbbbbbbb2222", "message": "Poll Cloudflare one lease at a time\n\nDetails", "author": "ada"}, {"sha": "cccc", "message": "Bump cloudflare-email to 0.4.2", "author": "bob"}]).to_string()
                ],
            )?;
            x.execute("UPDATE issues SET first_seen_at = ?1, culprit = 'app/services/outbound_email/cloudflare_feedback_consumer.rb:41' WHERE id = ?2", params![qt(NOW - 3 * 3_600_000_000 - 1_800_000_000), issue])?;
            x.execute(
                "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                   first_seen_at, last_seen_at, created_at, updated_at, resolved_in_deploy)
                 VALUES (?1, ?2, 12, 'Cloudflare::Email::NetworkError: timeout', 'exception', 'resolved', 'gy', 9, 0, ?3, ?3, ?3, ?3, 'd1')",
                params![f.app, f.env, qt(NOW - 10 * 86_400_000_000)],
            )?;
            let sibling = x.last_insert_rowid();
            x.execute("INSERT INTO comments (issue_id, body, created_at, updated_at) VALUES (?1, 'Fixed by retrying the ack with a fresh lease', ?2, ?2)", params![sibling, qt(NOW - 9 * 86_400_000_000)])?;
            // Unrelated: neither class nor file.
            x.execute(
                "INSERT INTO issues (application_id, environment_id, number, title, kind, status, group_hash, occurrences, affected_users,
                   first_seen_at, last_seen_at, created_at, updated_at, culprit)
                 VALUES (?1, ?2, 13, 'NoMethodError: undefined method', 'exception', 'open', 'gz', 1, 0, ?3, ?3, ?3, ?3, 'app/models/order.rb:3')",
                params![f.app, f.env, qt(NOW - 86_400_000_000)],
            )?;
            Ok(())
        })
        .unwrap();
        let conn = f.tenant();
        // The job ran a few queries and logged before raising.
        conn.execute(
            "UPDATE exceptions SET deploy = 'd2', occurred_at = ?1, user_ref = 'u1', app_tenant = 'acme' WHERE id = 55",
            [qt(NOW - 3 * 3_600_000_000)],
        )
        .unwrap();
        conn.execute(
            "UPDATE executions SET occurred_at = ?1, duration = 900000, outcome = 'failed' WHERE execution_id = 'e-job'",
            [qt(NOW - 3 * 3_600_000_000 - 800_000)],
        )
        .unwrap();
        for (i, sql) in ["SELECT \"leases\".* FROM \"leases\" WHERE \"leases\".\"id\" = ?", "UPDATE \"feedback\" SET \"acked\" = ?"].iter().enumerate() {
            conn.execute(
                "INSERT INTO queries (execution_id, occurred_at, duration, sql, group_hash, source) VALUES ('e-job', ?1, 2000, ?2, ?3, 'app/services/outbound_email/cloudflare_feedback_consumer.rb:38')",
                params![qt(NOW - 3 * 3_600_000_000 - 700_000 + i as i64 * 1000), sql, format!("gq{i}")],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO logs (execution_id, occurred_at, level, message) VALUES ('e-job', ?1, 'warn', 'cloudflare ack timed out, retry disabled')",
            [qt(NOW - 3 * 3_600_000_000 - 100_000)],
        )
        .unwrap();
        // A log line after the raise is not lead-up.
        conn.execute(
            "INSERT INTO logs (execution_id, occurred_at, level, message) VALUES ('e-job', ?1, 'info', 'job finished')",
            [qt(NOW - 3 * 3_600_000_000 + 5_000_000)],
        )
        .unwrap();
        for k in 0..2 {
            conn.execute(
                "INSERT INTO exceptions (class_name, message, occurred_at, group_hash, frames, deploy, user_ref) VALUES ('Cloudflare::Email::NetworkError', 'x', ?1, 'gx', '[]', 'd2', 'u2')",
                [qt(NOW - 3_600_000_000 - k)],
            )
            .unwrap();
        }
    }

    #[test]
    fn investigate_issue_attributes_the_deploy_and_brings_the_lead_up() {
        let f = seeded("investigate");
        let id = seed_exception_issue(&f);
        seed_issue_history(&f, id);
        let v = f.tool("investigate_issue", json!({"key": "SHOP-79"}));
        assert_eq!(v["url"], format!("https://rw.example.com/railwatch/issues/{id}"));

        // Where: get_issue's collapsed frames, code included.
        let frames = v["occurrence"]["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[1]["code"]["41"], "count = @client.poll(batch_size: 1) do |event|");

        // When: the deploy its first occurrence ran, the one before it, the
        // range between them and the deploy's commits.
        let s = &v["started_in"];
        assert_eq!(s["deploy"], "d2");
        assert_eq!(s["how"], "the deploy its first retained occurrence ran");
        assert_eq!(s["previous_deploy"]["deploy"], "d1");
        assert_eq!(s["commit_range"], "aaaaaaaaaaaa..bbbbbbbbbbbb");
        assert_eq!(s["commits"][0], json!({"sha": "bbbbbbbbbbbb", "message": "Poll Cloudflare one lease at a time", "author": "ada"}));
        assert_eq!(s["first_seen_after_deploy"], "30 minutes");

        // Lead-up: the job's summary and what ran before the raise, not after.
        let l = &v["lead_up"];
        assert_eq!((l["name"].as_str(), l["outcome"].as_str()), (Some("OutboundEmailFeedbackJob"), Some("failed")));
        let before: Vec<&str> = l["before_raise"].as_array().unwrap().iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(before, ["query", "query", "log"], "{l}");
        assert_eq!(l["before_raise"][0]["source"], "app/services/outbound_email/cloudflare_feedback_consumer.rb:38");
        assert_eq!(l["summary"]["warnings"][0]["message"], "cloudflare ack timed out, retry disabled");
        assert!(l["summary"].get("slowest").is_none());

        // How often and who: 3 occurrences in 30 days, all in the last week.
        assert_eq!((v["trend"]["last_7d"].as_i64(), v["trend"]["previous_7d"].as_i64()), (Some(3), Some(0)));
        assert_eq!(v["trend"]["by_deploy"][0]["deploy"], "d2");
        assert_eq!((v["who"]["users_30d"].as_i64(), v["who"]["tenants_30d"].as_i64()), (Some(2), Some(1)));

        // Similar: the resolved issue of the same class, with how it was fixed.
        let sim = v["similar_issues"].as_array().unwrap();
        assert_eq!(sim.len(), 1, "{sim:?}");
        assert_eq!((sim[0]["key"].as_str(), sim[0]["status"].as_str(), sim[0]["resolved_in_deploy"].as_str()), (Some("SHOP-12"), Some("resolved"), Some("d1")));
        assert_eq!(sim[0]["last_comment"], "Fixed by retrying the ack with a fresh lease");

        // The brief says the same things.
        let md = v["markdown"].as_str().unwrap();
        for want in [
            "# SHOP-79 Cloudflare::Email::NetworkError",
            "`app/services/outbound_email/cloudflare_feedback_consumer.rb:41`",
            "> 41: count = @client.poll(batch_size: 1) do |event|",
            "Deploy `d2` (shop@2)",
            "Commit range: `aaaaaaaaaaaa..bbbbbbbbbbbb`",
            "Poll Cloudflare one lease at a time",
            "cloudflare ack timed out",
            "SHOP-12 [resolved]",
        ] {
            assert!(md.contains(want), "markdown lacks {want:?}:\n{md}");
        }
        assert!(!md.contains("job finished"));
        let missing = f.tool("investigate_issue", json!({"key": "SHOP-404"}));
        assert!(missing["hint"].as_str().unwrap().contains("list_issues"));
    }

    #[test]
    fn execution_summary_of_is_get_executions_summary() {
        let f = seeded("summary-of");
        seed_invoicing(&f.tenant(), "e-invoicing", NOW - 60_000_000);
        let conn = f.tenant();
        let s = execution_summary_of(&conn, "e-invoicing").unwrap().unwrap();
        assert_eq!(s, f.tool("get_execution", json!({"execution_id": "e-invoicing"}))["summary"]);
        assert!(execution_summary_of(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn explain_route_aggregates_its_requests_and_names_the_causes() {
        let f = seeded("explain");
        let conn = f.tenant();
        // Three invoicing requests shaped like production's, and a fast one.
        for (i, eid) in ["e1", "e2", "e3"].iter().enumerate() {
            seed_invoicing(&conn, eid, NOW - 600_000_000 - i as i64 * 60_000_000);
        }
        conn.execute(
            "INSERT INTO executions (execution_id, kind, name, route, group_hash, status, occurred_at, duration, detail) \
             VALUES ('e-fast', 'request', 'GET /:org_slug/invoicing(.:format)', '/:org_slug/invoicing(.:format)', 'route_invoicing', 200, ?1, 69559, '{}')",
            [qt(NOW - 300_000_000)],
        )
        .unwrap();
        conn.execute(
            "UPDATE queries SET explain = 'SEARCH car_invoices USING INDEX idx (customer_id=?)' WHERE group_hash = 'g_invoices' AND execution_id = 'e1'",
            [],
        )
        .unwrap();
        for h in crate::rollup::rollup_job::hours_with_rows(&conn).unwrap() {
            crate::rollup::rollup_job::perform(&conn, &conn, h, &mut crate::rollup::rollup_job::Rng::PerGroup).unwrap();
        }
        let v = f.tool("explain_route", json!({"route": "GET /:org_slug/invoicing(.:format)"}));
        assert_eq!(v["requests"]["count"], 4);
        assert_eq!(v["sampled_requests"], 4);
        // Per request: (3 x 3669.559 + 69.559) / 4 ms; 3 x 2840 ms of commits.
        let t = &v["time_ms_per_request"];
        assert_eq!(t["duration"].as_f64(), Some(2769.6));
        assert_eq!(t["transactions"].as_f64(), Some(2130.0));
        assert_eq!(t["sql"].as_f64(), Some(61.9));
        assert_eq!(v["queries_per_request"].as_f64(), Some(619.5));
        // The N+1 with its fix, and how often it really ran per request.
        let n = &v["n_plus_ones"][0];
        assert_eq!((n["source"].as_str(), n["occurrences"].as_i64()), (Some("app/services/billing/customer_documents.rb:511"), Some(3)));
        assert_eq!(n["suggestion"]["code"], "Customer.includes(:car_invoices)");
        assert_eq!(n["runs_per_request"].as_f64(), Some(45.0));
        // The costliest shape is the N+1's, with its stored plan.
        let q = &v["top_queries"][0];
        assert_eq!((q["group_hash"].as_str(), q["total_ms"].as_f64(), q["in_requests"].as_i64()), (Some("g_invoices"), Some(18.0), Some(3)));
        assert_eq!(q["explain"], "SEARCH car_invoices USING INDEX idx (customer_id=?)");
        // No profile: the note says how to get one.
        assert!(v["profile"].is_null());
        let answer = v["answer"].as_str().unwrap();
        assert!(answer.starts_with("Over 4 sampled requests (avg 2.8 s): N+1 at app/services/billing/customer_documents.rb:511 (45x per request), fix: Customer.includes(:car_invoices)"), "{answer}");
        assert!(answer.contains("2.1 s per request inside transactions but only 56 ms of SQL in them") && answer.contains("Enable the profiler"), "{answer}");
        assert!(v["notes"].to_string().contains("RAILWATCH_PROFILE_SLOW_MS"));
        assert_eq!(v["slowest_requests"][0]["queries"], Value::Null);
        assert_eq!(v["url"], format!("https://rw.example.com/railwatch/apps/{}/envs/{}/requests/routes/route_invoicing", f.app, f.env));

        // A profile of one of them turns the hint into its frames.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, b"Invoicing#index;Billing#sync;Net::HTTP#request 70\nInvoicing#index;render 30\n").unwrap();
        conn.execute(
            "INSERT INTO profiles (execution_id, group_hash, occurred_at, duration, profiler, samples, stacks) VALUES ('e1', 'route_invoicing', ?1, 3600000, 'vernier', 100, ?2)",
            params![qt(NOW - 600_000_000), gz.finish().unwrap()],
        )
        .unwrap();
        let v = f.tool("explain_route", json!({"group_hash": "route_invoicing"}));
        assert_eq!(v["profile"]["frames"][0]["frame"], "Net::HTTP#request");
        assert!(v["answer"].as_str().unwrap().ends_with("profile.frames shows which methods that time is in."));
        assert_eq!(f.tool("explain_route", json!({"route": "GET /nope"}))["error"], "route not found");
    }

    /// No tool's result is bigger than the cap, however much data the
    /// environment holds: each one is called on an environment seeded past
    /// it, and must still return valid JSON.
    #[test]
    fn no_tool_result_exceeds_the_cap() {
        let f = seeded("cap");
        let id = seed_exception_issue(&f);
        let conn = f.tenant();
        conn.execute_batch("BEGIN").unwrap();
        // One huge execution: 5,000 queries with long SQL and 2 KB logs.
        conn.execute(
            "INSERT INTO executions (execution_id, kind, name, group_hash, status, occurred_at, duration) VALUES ('big', 'request', 'GET /big', 'gbig', 200, ?1, 9000000)",
            [qt(NOW - 60_000_000)],
        )
        .unwrap();
        for i in 0..5_000 {
            let sql = format!("SELECT {} FROM wide_{i}", "col, ".repeat(80));
            conn.execute(
                "INSERT INTO queries (execution_id, occurred_at, duration, sql, group_hash, source) VALUES ('big', ?1, ?2, ?3, ?4, 'app/x.rb:1')",
                params![qt(NOW - 60_000_000 + i), 1000 + i, sql, format!("g{i}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO logs (execution_id, occurred_at, level, message) VALUES ('big', ?1, 'error', ?2)",
                params![qt(NOW - 60_000_000 + i), "x".repeat(2_000)],
            )
            .unwrap();
        }
        // Many requests, routes and tenants.
        for i in 0..300i64 {
            conn.execute(
                "INSERT INTO executions (execution_id, kind, name, group_hash, status, occurred_at, duration, app_tenant, method)
                 VALUES (?1, 'request', ?2, ?3, 200, ?4, 5000, ?5, 'GET')",
                params![
                    format!("r{i}"),
                    format!("GET /{}", "p".repeat(200 + i as usize)),
                    format!("gr{i}"),
                    qt(NOW - 120_000_000 - i),
                    format!("tenant-{i}-{}", "t".repeat(200))
                ],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
        for h in crate::rollup::rollup_job::hours_with_rows(&conn).unwrap() {
            crate::rollup::rollup_job::perform(&conn, &conn, h, &mut crate::rollup::rollup_job::Rng::PerGroup).unwrap();
        }

        let generous: Map<String, Value> = [
            ("execution_id", json!("big")),
            ("timeline_limit", json!(200)),
            ("limit", json!(200)),
            ("q", json!("")),
            ("resource", json!("logs")),
            ("route", json!("GET /big")),
            ("group_hash", json!("g1")),
            ("key", json!("SHOP-79")),
            ("issue_id", json!(id)),
            ("frames", json!("all")),
            ("window", json!("30d")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut over = vec![];
        for t in &definitions().tools {
            let name = t["name"].as_str().unwrap();
            if Mcp::write_tool(name) {
                continue;
            }
            // Every argument the tool takes, at its most generous.
            let args: Map<String, Value> =
                t["inputSchema"]["properties"].as_object().unwrap().keys().filter_map(|k| generous.get(k).map(|v| (k.clone(), v.clone()))).collect();
            let text = f.text(name, Value::Object(args));
            serde_json::from_str::<Value>(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            if text.len() > mcp_summary::MAX_RESULT_BYTES {
                over.push((name, text.len()));
            }
        }
        assert!(over.is_empty(), "over the cap: {over:?}");
        // The cap really was needed, and its marker says how to narrow.
        let logs = f.tool("search_logs", json!({"q": "", "limit": 200}));
        assert!(logs["truncated"]["omitted_rows"].is_object(), "{}", logs["truncated"]);
        assert!(logs["truncated"]["hint"].as_str().unwrap().contains("smaller limit"));
    }
}
