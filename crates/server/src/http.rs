//! The ingest HTTP API, mounted without any page middleware. Ports of
//! railwatch-cloud's Ingest::BaseController, RecordsController,
//! DeploysController and KamalController, plus the middleware in front of
//! them (IngestRequestBodyLimit, IngestRequestRateLimit). Status codes and
//! JSON bodies are the Rails ones; JSON bodies are encoded as Rails renders
//! them (html entities escaped).
//!
//! Order per request, as in Rails: body limit (413) -> IP limit (429) ->
//! route (404) -> authenticate (401) -> environment limit (429) -> action,
//! with the disk floor (503, storage::budget) before any action that writes.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::control::{Control, Environment};
use crate::ingest::batch::{self, Meta};
use crate::ingest::payload::{self, MAX_BYTES};
use crate::ingest::receipt_headers::{self, ReceiptHeaders};
use crate::ingest::writer::WriteError;
use crate::rate_limit::FixedWindow;
use crate::rhash;
use crate::rjson::{self, Value, ruby};
use crate::tenants::{Job, Tenants};

pub struct Config {
    pub data_dir: PathBuf,
    pub rate_limit_ip: u64,
    pub rate_limit_env: u64,
    pub trust_proxy: bool,
}

pub struct AppState {
    pub control: Arc<Control>,
    pub tenants: Tenants,
    pub ip_limit: FixedWindow<IpAddr>,
    pub env_limit: FixedWindow<i64>,
    pub trust_proxy: bool,
    pub sourcemaps_dir: PathBuf,
    /// Bounds parse+map (CPU-bound) to one batch per core. Without it every
    /// in-flight request runs on the blocking pool at once and they all
    /// finish late together: p99 1.2 s at 32 in flight vs 0.13 s at 8.
    pub cpu: Arc<tokio::sync::Semaphore>,
    /// The disk budget's state (storage::budget, `Tenants::disk`): under
    /// min_free_bytes, or over max_data_bytes with nothing left to trim,
    /// writes are refused with 503.
    pub disk: Arc<crate::storage::budget::State>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().fallback(dispatch).with_state(state)
}

/// Rails renders `render json:` with Active Support's escaping encoder.
fn json(status: StatusCode, value: Value) -> Response {
    let body = rjson::generate_escaped(&value).expect("response JSON is always valid UTF-8");
    (status, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], body).into_response()
}

fn with_retry_after(mut r: Response, seconds: u64) -> Response {
    r.headers_mut().insert(header::RETRY_AFTER, seconds.into());
    r
}

fn rate_limited() -> Response {
    with_retry_after(json(StatusCode::TOO_MANY_REQUESTS, rhash! {"error" => "rate limit exceeded", "retry_after" => 60i64}), 60)
}

fn internal_error() -> Response {
    json(StatusCode::INTERNAL_SERVER_ERROR, rhash! {"error" => "internal server error"})
}

fn busy() -> Response {
    with_retry_after(json(StatusCode::SERVICE_UNAVAILABLE, rhash! {"error" => "database busy", "retry_after" => 15i64}), 15)
}

/// Below min_free_bytes, or over max_data_bytes: the gem keeps the batch
/// and retries later.
fn disk_refused(why: crate::storage::budget::Refusal) -> Response {
    let error = match why {
        crate::storage::budget::Refusal::LowSpace => "disk almost full",
        crate::storage::budget::Refusal::OverBudget => "telemetry over max_data_bytes",
    };
    with_retry_after(json(StatusCode::SERVICE_UNAVAILABLE, rhash! {"error" => error, "retry_after" => 60i64}), 60)
}

fn too_large() -> Response {
    json(StatusCode::PAYLOAD_TOO_LARGE, rhash! {"error" => "body too large"})
}

fn bad_request(message: impl Into<String>) -> Response {
    json(StatusCode::BAD_REQUEST, rhash! {"error" => message.into()})
}

fn ingest_path(path: &str) -> bool {
    path == "/ingest" || path.starts_with("/ingest.") || path.starts_with("/ingest/")
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Records,
    Ping,
    Deploys,
    Kamal,
    Sourcemaps,
}

/// Rails' routes: `post "/"`, `get "ping"`, `post "deploys"`, `post "kamal"`,
/// each also matching a trailing slash and a `.format` suffix.
fn route(method: &Method, path: &str) -> Option<Action> {
    let path = path.strip_suffix('/').filter(|p| !p.is_empty()).unwrap_or(path);
    let (base, _format) = match path.rsplit_once('.') {
        Some((b, f)) if !f.contains('/') && !f.is_empty() => (b, Some(f)),
        _ => (path, None),
    };
    match (method, base) {
        (&Method::POST, "/ingest") => Some(Action::Records),
        (&Method::GET, "/ingest/ping") => Some(Action::Ping),
        (&Method::POST, "/ingest/deploys") => Some(Action::Deploys),
        (&Method::POST, "/ingest/kamal") => Some(Action::Kamal),
        // Not in railwatch-cloud (the gem's railwatch:sourcemaps task calls
        // it; the cloud answers 404). Exact path only.
        (&Method::POST, "/ingest/sourcemaps") if path == "/ingest/sourcemaps" => Some(Action::Sourcemaps),
        _ => None,
    }
}

async fn dispatch(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    if path == "/up" && matches!(*req.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!DOCTYPE html><html><body style=\"background-color: green\"></body></html>",
        )
            .into_response();
    }
    if !ingest_path(&path) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // IngestRequestBodyLimit: a declared length over the cap is refused
    // before anything else.
    let declared: Option<u64> = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    if declared.is_some_and(|n| n > MAX_BYTES as u64) {
        return too_large();
    }

    // IngestRequestRateLimit, keyed by the client address.
    let ip = client_ip(&state, &req);
    if state.ip_limit.exceeded(&ip) {
        return rate_limited();
    }

    let Some(action) = route(req.method(), &path) else { return StatusCode::NOT_FOUND.into_response() };

    let (parts, body) = req.into_parts();
    let headers = parts.headers;

    // authenticate_environment!
    let token = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.as_bytes())
        .and_then(|v| v.strip_prefix(b"Bearer "))
        .filter(|t| !t.is_empty() && !t.iter().any(|b| b.is_ascii_whitespace()))
        .and_then(|t| std::str::from_utf8(t).ok());
    let env = match token {
        None => None,
        Some(t) => {
            let control = state.control.clone();
            let t = t.to_string();
            match tokio::task::spawn_blocking(move || control.authenticate(&t)).await {
                Ok(Ok(env)) => env,
                _ => return internal_error(),
            }
        }
    };
    let Some(env) = env else { return StatusCode::UNAUTHORIZED.into_response() };

    if state.env_limit.exceeded(&env.id) {
        return rate_limited();
    }
    if action != Action::Ping
        && let Err(why) = state.disk.admit().await
    {
        return disk_refused(why);
    }

    // The body, capped while reading (a client may lie about its length or
    // send it chunked).
    let raw = match read_bounded(body).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => return too_large(),
        Err(BodyError::Io) => return bad_request("bad request"),
    };

    match action {
        Action::Records => records(&state, env, &headers, raw, declared).await,
        Action::Ping => json(StatusCode::OK, rhash! {"ok" => true, "environment" => env.slug.clone(), "retention_days" => env.retention_days}),
        Action::Deploys => deploys(&state, env, &headers, &raw).await,
        Action::Kamal => kamal(&state, env, &headers, &raw).await,
        Action::Sourcemaps => sourcemaps(&state, env, &headers, raw).await,
    }
}

fn client_ip(state: &AppState, req: &Request) -> IpAddr {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    if state.trust_proxy {
        // The client is the last address before our trusted proxy: Rails'
        // RemoteIp drops trusted (private) hops from the right.
        if let Some(xff) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            let hops: Vec<IpAddr> = xff.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if let Some(ip) = hops.iter().rev().find(|ip| !trusted(ip)).or(hops.first()) {
                return *ip;
            }
        }
    }
    peer
}

fn trusted(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

enum BodyError {
    TooLarge,
    Io,
}

async fn read_bounded(body: Body) -> Result<Bytes, BodyError> {
    use http_body_util::BodyExt;
    let limited = http_body_util::Limited::new(body, MAX_BYTES);
    match limited.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => Err(BodyError::TooLarge),
        Err(_) => Err(BodyError::Io),
    }
}

struct RequestHeaders<'a>(&'a HeaderMap);

impl receipt_headers::Headers for RequestHeaders<'_> {
    fn header(&self, suffix: &str) -> Option<&[u8]> {
        self.0.get(format!("x-railwatch-{suffix}")).map(|v| v.as_bytes())
    }
}

fn ingest_header<'a>(headers: &'a HeaderMap, suffix: &str) -> Option<&'a [u8]> {
    headers.get(format!("x-railwatch-{suffix}")).map(|v| v.as_bytes())
}

fn current_month() -> String {
    crate::control::month_of(crate::time::now_micros())
}

// ---------------------------------------------------------------------------
// RecordsController#create

async fn records(state: &Arc<AppState>, env: Environment, headers: &HeaderMap, raw: Bytes, declared: Option<u64>) -> Response {
    let receipt = match ReceiptHeaders::from(&RequestHeaders(headers)) {
        Ok(r) => r,
        Err(e) => return bad_request(e.0),
    };
    let gzip = payload::gzip(headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()));
    match receipt {
        None => create_direct(state, env, headers, raw, gzip, declared).await,
        Some(r) => create_receipted(state, env, headers, raw, gzip, declared, r).await,
    }
}

fn meta(headers: &HeaderMap, declared: Option<u64>) -> Meta {
    Meta {
        dropped_by_client: batch::string_to_i(ingest_header(headers, "Dropped").unwrap_or(b"")),
        backpressure_factor: batch::normalize_backpressure_factor(ingest_header(headers, "Backpressure-Factor")),
        bytes: Value::Int(declared.map(|n| n as i64).unwrap_or(0)),
        gem_version: ingest_header(headers, "Version").map(<[u8]>::to_vec),
        received_at_micros: crate::time::now_micros(),
    }
}

async fn create_direct(state: &Arc<AppState>, env: Environment, headers: &HeaderMap, raw: Bytes, gzip: bool, declared: Option<u64>) -> Response {
    if env.paused {
        return json(StatusCode::OK, rhash! {"accepted" => 0i64, "rejected" => 0i64, "reason" => "paused"});
    }
    if env.quota_exhausted(&current_month()) {
        return json(StatusCode::PAYMENT_REQUIRED, rhash! {"accepted" => 0i64, "rejected" => 0i64, "reason" => "quota"});
    }
    let meta = meta(headers, declared);
    let floor = env.ingest_floor(meta.received_at());
    let permit = state.cpu.clone().acquire_owned().await.expect("cpu semaphore closed");
    let prepared = match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let records = payload::parse_bytes(&raw, gzip)?;
        Ok::<_, payload::Malformed>(batch::map_all(records, floor, meta, None))
    })
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(m)) => return bad_request(m.0),
        Err(_) => return internal_error(),
    };
    let rejections = prepared.rejections.clone();
    match write(state, &env, prepared).await {
        Ok(w) => {
            json(StatusCode::OK, rhash! {"accepted" => w.accepted, "rejected" => rejections.len(), "rejections" => batch::rejections_value(&rejections, 10)})
        }
        Err(e) => write_error(&env, e),
    }
}

fn write_error(env: &Environment, e: WriteError) -> Response {
    match e {
        WriteError::Busy => {
            tracing::warn!(env = env.slug, "ingest busy");
            busy()
        }
        WriteError::Expired => json(StatusCode::GONE, rhash! {"error" => "delivery id is too old to accept"}),
        WriteError::Internal(m) => {
            tracing::error!(env = env.slug, error = m, "ingest failed");
            internal_error()
        }
        WriteError::DuplicateReceipt => internal_error(),
    }
}

async fn write(state: &Arc<AppState>, env: &Environment, prepared: batch::Prepared) -> Result<crate::ingest::writer::Written, WriteError> {
    // Batch#write_receipt! re-reads the floor inside the transaction; the
    // control row is read here, right before the hand-off.
    let gc_floor = match prepared.receipt {
        Some(_) => {
            let control = state.control.clone();
            let id = env.id;
            match tokio::task::spawn_blocking(move || control.find(id)).await {
                Ok(Ok(Some(e))) => e.ingest_followup_gc_before,
                _ => return Err(WriteError::Internal("environment vanished".into())),
            }
        }
        None => 0,
    };
    let (tx, rx) = oneshot::channel();
    state.tenants.send(env.id, &env.slug, Job::Write { batch: Box::new(prepared), gc_floor_micros: gc_floor, reply: tx });
    rx.await.unwrap_or_else(|_| Err(WriteError::Internal("writer stopped".into())))
}

#[allow(clippy::too_many_arguments)]
async fn create_receipted(
    state: &Arc<AppState>,
    env: Environment,
    headers: &HeaderMap,
    raw: Bytes,
    gzip: bool,
    declared: Option<u64>,
    receipt: ReceiptHeaders,
) -> Response {
    let digest = receipt_headers::hex(&Sha256::digest(&raw));
    if digest != receipt.body_sha256 {
        return bad_request("body digest mismatch");
    }
    // A delivery already committed is acknowledged before anything that
    // could turn it away.
    match lookup(state, &env, &receipt).await {
        Ok(Some(stored)) => return acknowledge_duplicate(stored, &receipt),
        Ok(None) => {}
        Err(_) => return internal_error(),
    }
    if let Some(refusal) = refuse_new(&env, &receipt) {
        return refusal;
    }
    let meta = meta(headers, declared);
    let floor = env.ingest_floor(meta.received_at());
    let attrs = receipt.to_attributes();
    let permit = state.cpu.clone().acquire_owned().await.expect("cpu semaphore closed");
    let prepared = match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let records = payload::parse_bytes(&raw, gzip)?;
        Ok::<_, payload::Malformed>(batch::map_all(records, floor, meta, Some(attrs)))
    })
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(m)) => return bad_request(m.0),
        Err(_) => return internal_error(),
    };
    let rejections = prepared.rejections.clone();
    match write(state, &env, prepared).await {
        Ok(w) => {
            let stored = rhash! {"accepted" => w.accepted, "rejected" => rejections.len(), "rejections" => batch::rejections_value(&rejections, 10)};
            json(StatusCode::OK, ack(&stored, "committed", &receipt, w.committed_at_micros.map(crate::time::iso8601_6)))
        }
        // Two senders raced with the same delivery: the winner's receipt
        // is the answer.
        Err(WriteError::DuplicateReceipt) => match lookup(state, &env, &receipt).await {
            Ok(Some(stored)) => acknowledge_duplicate(stored, &receipt),
            _ => internal_error(),
        },
        Err(e) => write_error(&env, e),
    }
}

async fn lookup(state: &Arc<AppState>, env: &Environment, r: &ReceiptHeaders) -> Result<Option<crate::tenants::StoredReceipt>, String> {
    let (tx, rx) = oneshot::channel();
    state.tenants.send(env.id, &env.slug, Job::LookupReceipt { producer_id: r.producer_id.clone(), delivery_id: r.delivery_id.clone(), reply: tx });
    rx.await.unwrap_or_else(|_| Err("writer stopped".into()))
}

fn acknowledge_duplicate(stored: crate::tenants::StoredReceipt, r: &ReceiptHeaders) -> Response {
    if stored.body_sha256 != r.body_sha256 || stored.metadata_sha256 != r.metadata_sha256 {
        return json(StatusCode::CONFLICT, rhash! {"error" => "delivery id already used for different content"});
    }
    let ack_value = rjson::parse(stored.ack.as_bytes()).unwrap_or(Value::Nil);
    let committed = crate::time::parse_db_time(&stored.committed_at).map(crate::time::iso8601_6);
    json(StatusCode::OK, ack(&ack_value, "already_committed", r, committed))
}

/// RecordsController#refuse_new: permanent reasons first.
fn refuse_new(env: &Environment, r: &ReceiptHeaders) -> Option<Response> {
    let now = crate::time::now_micros();
    if r.too_new(now) {
        let body = rhash! {"disposition" => "deferred", "reason" => "clock skew", "accepted" => 0i64, "rejected" => 0i64};
        return Some(with_retry_after(json(StatusCode::SERVICE_UNAVAILABLE, body), receipt_headers::MAX_FUTURE_SECONDS as u64));
    }
    if r.too_old(now) || r.created_at_micros() < env.ingest_followup_gc_before {
        return Some(json(StatusCode::GONE, rhash! {"error" => "delivery id is too old to accept"}));
    }
    if env.paused {
        return Some(json(StatusCode::OK, rhash! {"disposition" => "deferred", "reason" => "paused", "accepted" => 0i64, "rejected" => 0i64}));
    }
    if env.quota_exhausted(&current_month()) {
        return Some(json(StatusCode::PAYMENT_REQUIRED, rhash! {"disposition" => "deferred", "reason" => "quota", "accepted" => 0i64, "rejected" => 0i64}));
    }
    None
}

/// RecordsController#ack
fn ack(stored: &Value, disposition: &str, r: &ReceiptHeaders, committed_at: Option<String>) -> Value {
    let int = |v: &Value| match v {
        Value::Int(i) => *i,
        _ => 0,
    };
    let rejections = match stored.get("rejections") {
        Value::Array(a) => Value::Array(a.iter().take(10).cloned().collect()),
        Value::Nil => Value::Array(vec![]),
        other => Value::Array(vec![other.clone()]),
    };
    rhash! {
        "disposition" => disposition,
        "producer_id" => r.producer_id.clone(),
        "delivery_id" => r.delivery_id.clone(),
        "body_sha256" => r.body_sha256.clone(),
        "metadata_sha256" => r.metadata_sha256.clone(),
        "committed_at" => committed_at,
        "accepted" => int(stored.get("accepted")),
        "rejected" => int(stored.get("rejected")),
        "rejections" => rejections,
    }
}

// ---------------------------------------------------------------------------
// Deploys, Kamal, Sourcemaps

/// ActionDispatch's JSON params: the body parsed as a Hash when the request
/// says it is JSON. Anything else is no params.
fn json_params(headers: &HeaderMap, raw: &[u8]) -> Result<Value, Box<Response>> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.contains("json") || raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Hash(rjson::Hash::new()));
    }
    match rjson::parse(raw) {
        Ok(v @ Value::Hash(_)) => Ok(v),
        // A JSON array lands under params[:_json]; nothing the controllers read.
        Ok(_) => Ok(Value::Hash(rjson::Hash::new())),
        Err(_) => Err(Box::new(bad_request("bad request"))),
    }
}

/// BaseController#bounded_string
fn bounded_string(value: &Value, length: usize) -> String {
    let s = ruby::to_s(value);
    let s = crate::ingest::mapper::sanitize_string(&s);
    let s = if s.len() <= length {
        s
    } else {
        let cut = &s[..length];
        let valid = std::str::from_utf8(cut).map(|_| cut.len()).unwrap_or_else(|e| e.valid_up_to());
        cut[..valid].to_vec()
    };
    // Parameters arrive as Strings; a Hash or Array to_s is its inspect.
    String::from_utf8(s).unwrap_or_default()
}

fn presence(s: String) -> Option<String> {
    if ruby::blank(s.as_bytes()) { None } else { Some(s) }
}

async fn deploys(state: &Arc<AppState>, env: Environment, headers: &HeaderMap, raw: &[u8]) -> Response {
    let params = match json_params(headers, raw) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    // params.require(:deploy): missing or blank is ParameterMissing, a 400.
    if !params.get("deploy").present() {
        return bad_request("param is missing or the value is empty or invalid: deploy");
    }
    let deploy = bounded_string(params.get("deploy"), 128);
    if ruby::blank(deploy.as_bytes()) {
        return json(StatusCode::UNPROCESSABLE_ENTITY, rhash! {"error" => "Deploy can't be blank"});
    }
    let opt = |k: &str, n: usize| presence(bounded_string(params.get(k), n));
    let url = opt("url", 1024);
    // An explicit timestamp sets deployed_at; without one save_deploy keeps
    // an existing deploy's and stamps a new one now.
    let deployed_at = if params.get("timestamp").present() {
        match parse_time(&bounded_string(params.get("timestamp"), 64)) {
            Some(t) => Some(t),
            None => return json(StatusCode::UNPROCESSABLE_ENTITY, rhash! {"error" => "Deployed at can't be blank"}),
        }
    } else {
        None
    };
    if let Some(u) = &url
        && !web_url(u)
    {
        return json(StatusCode::UNPROCESSABLE_ENTITY, rhash! {"error" => "Url must be a single http:// or https:// URL"});
    }
    let commits: Vec<Value> = match params.get("commits") {
        Value::Array(list) => list
            .iter()
            .take(50)
            .filter_map(|c| {
                let sha = bounded_string(c.get("sha"), 64);
                if ruby::blank(sha.as_bytes()) {
                    return None;
                }
                Some(rhash! {
                    "sha" => sha,
                    "author" => presence(bounded_string(c.get("author"), 120)),
                    "message" => presence(bounded_string(c.get("message"), 500)),
                    "at" => presence(bounded_string(c.get("at"), 64)),
                })
            })
            .collect(),
        _ => vec![],
    };
    let mut detail = rjson::Hash::new();
    for k in ["performer", "destination", "service"] {
        if let Some(v) = presence(bounded_string(params.get(k), 255)) {
            detail.insert(k.as_bytes().to_vec(), Value::from(v));
        }
    }
    let record = DeployRecord {
        deploy: deploy.clone(),
        r#ref: opt("ref", 128),
        name: opt("name", 255),
        url,
        server: opt("server", 255),
        deployed_at,
        previous_ref: opt("previous_ref", 128),
        commits,
        detail: Value::Hash(detail),
    };
    let control = state.control.clone();
    let env_id = env.id;
    match tokio::task::spawn_blocking(move || save_deploy(&control, env_id, record)).await {
        Ok(Ok(())) => json(StatusCode::CREATED, rhash! {"ok" => true, "deploy" => deploy}),
        _ => internal_error(),
    }
}

struct DeployRecord {
    deploy: String,
    r#ref: Option<String>,
    name: Option<String>,
    url: Option<String>,
    server: Option<String>,
    /// None: keep the existing deploy's, or now for a new one.
    deployed_at: Option<i64>,
    previous_ref: Option<String>,
    commits: Vec<Value>,
    detail: Value,
}

/// Deploy find_or_initialize_by + save!, with its before_validation hooks
/// (assign_previous_ref, trim_commits).
fn save_deploy(control: &Control, env_id: i64, mut d: DeployRecord) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;
    let conn = control.conn();
    let deployed_at = match d.deployed_at {
        Some(t) => crate::time::quoted_date_micros(t),
        None => conn
            .query_row("SELECT deployed_at FROM deploys WHERE environment_id = ? AND deploy = ?", rusqlite::params![env_id, d.deploy], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| crate::time::quoted_date_micros(crate::time::now_micros())),
    };
    if d.previous_ref.is_none() {
        d.previous_ref = conn
            .query_row(
                "SELECT ref FROM deploys WHERE environment_id = ? AND deployed_at < ? AND deploy != ? ORDER BY deployed_at DESC LIMIT 1",
                rusqlite::params![env_id, deployed_at, d.deploy],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
    }
    if let Some(prev) = d.previous_ref.as_deref().filter(|p| !p.is_empty())
        && let Some(i) = d.commits.iter().position(|c| ruby::to_s(c.get("sha")).starts_with(prev.as_bytes()))
    {
        d.commits.truncate(i);
    }
    let now = crate::time::quoted_date_micros(crate::time::now_micros());
    let commits = rjson::generate(&Value::Array(d.commits)).unwrap_or_else(|_| "[]".into());
    let detail = rjson::generate(&d.detail).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO deploys (environment_id, deploy, ref, name, url, server, previous_ref, deployed_at, commits, detail, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT (environment_id, deploy) DO UPDATE SET ref = excluded.ref, name = excluded.name, url = excluded.url,
           server = excluded.server, previous_ref = excluded.previous_ref, deployed_at = excluded.deployed_at,
           commits = excluded.commits, detail = excluded.detail, updated_at = excluded.updated_at",
        rusqlite::params![env_id, d.deploy, d.r#ref, d.name, d.url, d.server, d.previous_ref, deployed_at, commits, detail, now],
    )?;
    conn.execute("UPDATE environments SET last_seen_at = ? WHERE id = ?", rusqlite::params![now, env_id])?;
    Ok(())
}

/// Time.zone.parse for the shapes deploy hooks send (ISO 8601 with or
/// without offset, "YYYY-MM-DD HH:MM[:SS]"). nil for anything unparseable.
/// A subset of Ruby's Date._parse; see docs/DIFFERENCES.md.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (main, offset_secs) = split_offset(s);
    let main = main.replace('T', " ");
    let (date, time) = main.split_once(' ').unwrap_or((main.as_str(), "00:00:00"));
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next().unwrap_or("0").parse().ok()?;
    let se: i64 = t.next().unwrap_or("0").parse().ok()?;
    if h > 24 || mi > 59 || se > 60 {
        return None;
    }
    let usec: i64 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(6).collect();
        format!("{digits:0<6}").parse().ok()?
    };
    // Any i64 year parses; keep days_from_civil's own arithmetic in range
    // and let the checked steps below refuse what doesn't fit in micros.
    if y.unsigned_abs() > 1_000_000_000 {
        return None;
    }
    let days = crate::time::days_from_civil(y, m, day);
    let secs = days.checked_mul(86_400)?.checked_add(h * 3600 + mi * 60 + se)?.checked_sub(offset_secs)?;
    secs.checked_mul(1_000_000)?.checked_add(usec)
}

/// Splits a trailing "Z", " UTC" or "+hh:mm"/"-hh:mm" off. Byte-safe: the
/// input is client text and may hold multibyte characters anywhere.
fn split_offset(s: &str) -> (&str, i64) {
    if let Some(stripped) = s.strip_suffix('Z').or_else(|| s.strip_suffix(" UTC")) {
        return (stripped, 0);
    }
    if let Some(cut) = s.len().checked_sub(6)
        && cut > 0
        && let [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] = s.as_bytes()[cut..]
        && [h1, h2, m1, m2].iter().all(u8::is_ascii_digit)
    {
        let h = ((h1 - b'0') * 10 + (h2 - b'0')) as i64;
        let m = ((m1 - b'0') * 10 + (m2 - b'0')) as i64;
        let sign = if sign == b'-' { -1 } else { 1 };
        // The six bytes are ASCII, so `cut` is a char boundary.
        return (&s[..cut], sign * (h * 3600 + m * 60));
    }
    (s, 0)
}

/// Deploy#url_is_a_web_address: one http(s) URL with a host, no spaces.
pub(crate) fn web_url(u: &str) -> bool {
    if u.chars().any(char::is_whitespace) {
        return false;
    }
    let rest = if let Some(r) = u.strip_prefix("http://") {
        r
    } else if let Some(r) = u.strip_prefix("https://") {
        r
    } else {
        let lower = u.to_ascii_lowercase();
        match lower.split_once("://") {
            Some(("http" | "https", _)) => &u[u.find("://").unwrap() + 3..],
            _ => return false,
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    !host.is_empty()
}

async fn kamal(state: &Arc<AppState>, env: Environment, headers: &HeaderMap, raw: &[u8]) -> Response {
    let params = match json_params(headers, raw) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let list = |v: &Value, length: usize| -> Vec<String> {
        let items: Vec<Value> = match v {
            Value::Nil => vec![],
            Value::Array(a) => a.clone(),
            other => vec![other.clone()],
        };
        let mut out: Vec<String> =
            items.iter().take(100).map(|i| bounded_string(i, length).trim().to_string()).filter(|s| !ruby::blank(s.as_bytes())).collect();
        let mut seen = std::collections::HashSet::new();
        out.retain(|s| seen.insert(s.clone()));
        out.sort();
        out
    };
    let string = |k: &str, n: usize| presence(bounded_string(params.get(k), n));
    let hosts = list(params.get("hosts"), 255);
    let to_values = |v: &[String]| Value::Array(v.iter().cloned().map(Value::from).collect());
    let config = rhash! {
        "version" => string("version", 128),
        "roles" => to_values(&list(params.get("roles"), 64)),
        "performer" => string("performer", 255),
        "destination" => string("destination", 128),
        "service" => string("service", 255),
        "recorded_at" => string("recorded_at", 64),
        "hosts" => to_values(&hosts),
        "command" => string("command", 64),
        "subcommand" => string("subcommand", 64),
        "received_at" => crate::time::iso8601(crate::time::now_micros().div_euclid(1_000_000)),
    };
    let hosts_json = rjson::generate(&to_values(&hosts)).unwrap_or_else(|_| "[]".into());
    let config_json = rjson::generate(&config).unwrap_or_else(|_| "{}".into());
    let control = state.control.clone();
    let id = env.id;
    let now = crate::time::quoted_date_micros(crate::time::now_micros());
    let saved = tokio::task::spawn_blocking(move || {
        control.set_kamal(id, &hosts_json, &config_json)?;
        control.touch_last_seen(id, &now)
    })
    .await;
    match saved {
        Ok(Ok(())) => json(StatusCode::CREATED, rhash! {"ok" => true, "hosts" => to_values(&hosts)}),
        _ => internal_error(),
    }
}

/// Not in railwatch-cloud. The gem (Railwatch::SourceMaps#upload_file) posts
/// one map per request with X-Railwatch-Deploy and X-Railwatch-Filename, and
/// requires `{ok: true, filename:, bytes:}` back.
async fn sourcemaps(state: &Arc<AppState>, env: Environment, headers: &HeaderMap, raw: Bytes) -> Response {
    const MAX: usize = 10 * 1024 * 1024;
    let text = |k: &str| ingest_header(headers, k).and_then(|v| std::str::from_utf8(v).ok()).map(str::to_string);
    let (Some(deploy), Some(filename)) = (text("Deploy"), text("Filename")) else {
        return json(StatusCode::UNPROCESSABLE_ENTITY, rhash! {"error" => "X-Railwatch-Deploy and X-Railwatch-Filename are required"});
    };
    if raw.len() > MAX {
        return too_large();
    }
    let safe = |s: &str| {
        !s.is_empty()
            && s.len() <= 255
            && !s.starts_with('/')
            && s.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
            && !s.chars().any(|c| c == '\\' || c.is_control())
    };
    if !safe(&deploy) || deploy.contains('/') || !safe(&filename) {
        return json(StatusCode::UNPROCESSABLE_ENTITY, rhash! {"error" => "invalid deploy or filename"});
    }
    let path = state.sourcemaps_dir.join(&env.slug).join(&deploy).join(format!("{filename}.map"));
    let bytes = raw.len();
    let written = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("map.tmp");
        std::fs::write(&tmp, &raw)?;
        std::fs::rename(&tmp, &path)
    })
    .await;
    match written {
        Ok(Ok(())) => json(StatusCode::CREATED, rhash! {"ok" => true, "filename" => filename, "bytes" => bytes}),
        _ => internal_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_like_rails() {
        assert!(route(&Method::POST, "/ingest") == Some(Action::Records));
        assert!(route(&Method::POST, "/ingest/") == Some(Action::Records));
        assert!(route(&Method::POST, "/ingest.json") == Some(Action::Records));
        assert!(route(&Method::GET, "/ingest").is_none());
        assert!(route(&Method::GET, "/ingest/ping") == Some(Action::Ping));
        assert!(route(&Method::POST, "/ingest/ping").is_none());
        assert!(route(&Method::POST, "/ingest/deploys/") == Some(Action::Deploys));
        assert!(route(&Method::POST, "/ingest/nope").is_none());
    }

    #[test]
    fn deploy_urls_and_times() {
        assert!(web_url("https://ci.example.com/runs/1"));
        assert!(!web_url("javascript:alert(1)"));
        assert!(!web_url("https://?x"));
        assert!(!web_url("https://a b"));
        assert_eq!(parse_time("2026-10-13T10:00:00Z"), Some(1_791_885_600_000_000));
        assert_eq!(parse_time("2026-10-13 10:00"), Some(1_791_885_600_000_000));
        assert_eq!(parse_time("2026-10-13T12:00:00+02:00"), Some(1_791_885_600_000_000));
        assert_eq!(parse_time("garbage"), None);
        // Client text: multibyte anywhere must not panic (security-b PoC).
        for t in [
            "\u{20ac}a\u{20ac}",
            "aa\u{20ac}\u{20ac}a",
            "\u{1F600}\u{1F600}",
            "a\u{1F600}aa\u{1F600}",
            "2026-10-13 10:00:00.\u{20ac}\u{20ac}\u{20ac}",
            "+\u{20ac}:00",
            "2026-10-13T10:00+\u{e9}:0",
        ] {
            assert_eq!(parse_time(t), None, "{t:?}");
        }
        assert_eq!(parse_time("2026-10-13 10:00:00.123\u{20ac}"), None);
        // A 19-digit year used to wrap; out-of-range years are unparseable.
        assert_eq!(parse_time("9223372036854775807-01-01"), None);
        assert_eq!(parse_time("-9223372036854775808-01-01"), None);
        assert_eq!(parse_time("300000-01-01"), None);
        assert_eq!(parse_time("2026-10-13T10:00:00.5-01:30"), Some(1_791_885_600_000_000 + 5_400_000_000 + 500_000));
    }
}
