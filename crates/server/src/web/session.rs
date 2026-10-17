//! The per-request middleware around every page route
//! (ApplicationController's before_actions + Rails' cookie session +
//! protect_from_forgery):
//!
//! 1. decrypt `_railwatch_session`, resolve Current from the signed
//!    `session_token` / `account_id` cookies (`perform_authentication`,
//!    `set_current_account`);
//! 2. on POST/PUT/PATCH/DELETE: refuse cross-site requests (Origin must be
//!    this host, `Sec-Fetch-Site` must not be `cross-site`) and require the
//!    session's CSRF token in `X-XSRF-TOKEN` (Inertia/axios, from the
//!    `XSRF-TOKEN` cookie) or `X-CSRF-Token` (the gem's beacon, from the
//!    meta tag): 422 otherwise, as Rails' InvalidAuthenticityToken;
//! 3. after the handler: write the session cookie when it changed, the
//!    auth cookies a handler asked for, and a fresh `XSRF-TOKEN` cookie
//!    when the browser's is missing or stale; the security headers Rails
//!    sends on pages.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use subtle::ConstantTimeEq;

use super::WebState;
use super::cookies::{self, SessionData, SetCookie};
use crate::control::{self, Session, User};

/// The signed-in browser, as Rails' `Current`.
#[derive(Debug, Clone, Default)]
pub struct Current {
    pub session: Option<Session>,
    pub user: Option<User>,
    pub account: Option<Account>,
    pub user_agent: Option<String>,
    pub ip: String,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub plan: String,
    pub updated_at: String,
    pub monthly_event_quota: i64,
    pub additional_events_cap: i64,
    pub retention_days: i64,
    pub auto_resolve_after_days: Option<i64>,
}

pub fn find_account(c: &rusqlite::Connection, user_id: i64, preferred: Option<i64>) -> rusqlite::Result<Option<Account>> {
    use rusqlite::OptionalExtension;
    c.prepare_cached(
        "SELECT a.id, a.name, a.slug, a.plan, a.updated_at, a.monthly_event_quota, a.additional_events_cap, a.retention_days, a.auto_resolve_after_days
         FROM accounts a JOIN memberships m ON m.account_id = a.id WHERE m.user_id = ?1
         ORDER BY a.id = ?2 DESC, a.id LIMIT 1",
    )?
    .query_row(rusqlite::params![user_id, preferred], |r| {
        Ok(Account {
            id: r.get(0)?,
            name: r.get(1)?,
            slug: r.get(2)?,
            plan: r.get(3)?,
            updated_at: r.get(4)?,
            monthly_event_quota: r.get(5)?,
            additional_events_cap: r.get(6)?,
            retention_days: r.get(7)?,
            auto_resolve_after_days: r.get(8)?,
        })
    })
    .optional()
}

/// The request's session map, shared by the handler, omega (flash/errors)
/// and the middleware that writes it back.
#[derive(Clone, Default)]
pub struct SessionHandle(pub Arc<Mutex<SessionData>>);

impl SessionHandle {
    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.lock().unwrap().map.get(key).cloned()
    }
    pub fn put(&self, key: &str, value: Value) {
        let mut s = self.0.lock().unwrap();
        s.map.insert(key.to_string(), value);
        s.dirty = true;
    }
    pub fn take(&self, key: &str) -> Option<Value> {
        let mut s = self.0.lock().unwrap();
        let v = s.map.remove(key);
        if v.is_some() {
            s.dirty = true;
        }
        v
    }
}

impl omega::Session for SessionHandle {
    async fn get(&self, key: &str) -> Option<Value> {
        SessionHandle::get(self, key)
    }
    async fn put(&self, key: &str, value: Value) {
        SessionHandle::put(self, key, value)
    }
    async fn pull(&self, key: &str) -> Option<Value> {
        self.take(key)
    }
}

/// Cookie changes a handler asks for (`cookies.signed.permanent[...] =`).
#[derive(Clone, Default)]
pub struct Outgoing(pub Arc<Mutex<OutgoingState>>);

#[derive(Default)]
pub struct OutgoingState {
    /// Some(Some(id)): sign in as session id; Some(None): sign out.
    pub session_token: Option<Option<i64>>,
    pub account_id: Option<Option<i64>>,
    /// Replace the CSRF secret (sign-in / sign-out).
    pub rotate_csrf: bool,
}

impl Outgoing {
    pub fn sign_in(&self, session_id: i64) {
        let mut o = self.0.lock().unwrap();
        o.session_token = Some(Some(session_id));
        o.rotate_csrf = true;
    }
    pub fn sign_out(&self) {
        let mut o = self.0.lock().unwrap();
        o.session_token = Some(None);
        o.account_id = Some(None);
        o.rotate_csrf = true;
    }
    pub fn set_account(&self, id: Option<i64>) {
        self.0.lock().unwrap().account_id = Some(id);
    }
}

const CSRF_KEY: &str = "_csrf_token";
pub const XSRF_COOKIE: &str = "XSRF-TOKEN";

fn new_csrf() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random");
    URL_SAFE_NO_PAD.encode(b)
}

/// The session's CSRF token (created on first use).
pub fn csrf_token(session: &SessionHandle) -> String {
    if let Some(Value::String(t)) = session.get(CSRF_KEY) {
        return t;
    }
    let t = new_csrf();
    session.put(CSRF_KEY, Value::String(t.clone()));
    t
}

pub fn layer(router: Router<Arc<WebState>>, state: Arc<WebState>) -> Router<Arc<WebState>> {
    router.layer(axum::middleware::from_fn_with_state(state, middleware))
}

pub fn same_origin(headers: &HeaderMap) -> bool {
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return true };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    origin.split_once("://").is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

fn csrf_ok(headers: &HeaderMap, expected: &str) -> bool {
    let given = headers.get("x-xsrf-token").or_else(|| headers.get("x-csrf-token")).and_then(|v| v.to_str().ok());
    // axios sends the cookie value URL-decoded; ours is base64url, which
    // needs no escaping, so a percent sign means it is not ours.
    given.is_some_and(|g| g.as_bytes().ct_eq(expected.as_bytes()).into())
}

async fn middleware(State(state): State<Arc<WebState>>, mut req: Request, next: Next) -> Response {
    let jar = cookies::parse(req.headers());
    let session = SessionHandle(Arc::new(Mutex::new(SessionData::load(&state.keys, &jar))));
    let session_id: Option<i64> =
        cookies::get(&jar, cookies::SESSION_TOKEN).and_then(|c| state.keys.verify(cookies::SESSION_TOKEN, c)).and_then(|v| v.parse().ok());
    let account_pref: Option<i64> =
        cookies::get(&jar, cookies::ACCOUNT_ID).and_then(|c| state.keys.verify(cookies::ACCOUNT_ID, c)).and_then(|v| v.parse().ok());
    let ip = state.client_ip(&req).to_string();
    let user_agent = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_string);

    let resolved = state
        .db(move |c| {
            c.with(|conn| {
                let Some(s) = session_id.map(|id| control::find_session(conn, id)).transpose()?.flatten() else {
                    return Ok((None, None, None));
                };
                let user = control::find_user(conn, s.user_id)?;
                let account = match &user {
                    Some(u) => find_account(conn, u.id, account_pref)?,
                    None => None,
                };
                Ok((Some(s), user, account))
            })
        })
        .await;
    let (sess, user, account) = match resolved {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "resolving the session failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };
    // A session row without its user (deleted) is no session.
    let sess = sess.filter(|_| user.is_some());
    let had_session_cookie = session_id.is_some();
    let current = Arc::new(Current { session: sess, user, account, user_agent, ip });

    let token = csrf_token(&session);
    let method = req.method().clone();
    let mutating = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    // Google sign-in's form posts its token as a field; that route checks it.
    let form_checked = method == Method::POST && req.uri().path() == super::sessions_oauth::REQUEST_PATH;
    if mutating && !form_checked && (!same_origin(req.headers()) || !csrf_ok(req.headers(), &token)) {
        tracing::warn!(path = req.uri().path(), "CSRF check failed");
        return (StatusCode::UNPROCESSABLE_ENTITY, "Can't verify CSRF token authenticity.").into_response();
    }
    let browser_xsrf = cookies::get(&jar, XSRF_COOKIE).map(str::to_string);

    let outgoing = Outgoing::default();
    req.extensions_mut().insert(current.clone());
    req.extensions_mut().insert(session.clone());
    req.extensions_mut().insert(outgoing.clone());

    let mut res = next.run(req).await;

    let secure = state.config.secure_cookies();
    let o = std::mem::take(&mut *outgoing.0.lock().unwrap());
    let mut set: Vec<SetCookie> = Vec::new();
    match o.session_token {
        Some(Some(id)) => set.push(SetCookie {
            name: cookies::SESSION_TOKEN,
            value: Some(state.keys.sign(cookies::SESSION_TOKEN, &id.to_string())),
            permanent: true,
            http_only: true,
            secure,
        }),
        Some(None) => set.push(SetCookie { name: cookies::SESSION_TOKEN, value: None, permanent: false, http_only: true, secure }),
        None if had_session_cookie && current.session.is_none() => {
            // A cookie for a session that no longer exists: forget it.
            set.push(SetCookie { name: cookies::SESSION_TOKEN, value: None, permanent: false, http_only: true, secure });
        }
        None => {}
    }
    match o.account_id {
        Some(Some(id)) => set.push(SetCookie {
            name: cookies::ACCOUNT_ID,
            value: Some(state.keys.sign(cookies::ACCOUNT_ID, &id.to_string())),
            permanent: true,
            http_only: true,
            secure,
        }),
        Some(None) => set.push(SetCookie { name: cookies::ACCOUNT_ID, value: None, permanent: false, http_only: true, secure }),
        None => {}
    }
    let token = if o.rotate_csrf {
        let t = new_csrf();
        session.put(CSRF_KEY, Value::String(t.clone()));
        t
    } else {
        token
    };
    let data = session.0.lock().unwrap().clone();
    if data.dirty {
        set.push(SetCookie { name: cookies::SESSION, value: Some(data.encode(&state.keys)), permanent: false, http_only: true, secure });
    }
    if browser_xsrf.as_deref() != Some(token.as_str()) {
        set.push(SetCookie { name: "XSRF-TOKEN", value: Some(token), permanent: false, http_only: false, secure });
    }
    let h = res.headers_mut();
    for c in set {
        h.append(header::SET_COOKIE, c.header());
    }
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.insert("content-security-policy", HeaderValue::from_static("base-uri 'self'; object-src 'none'; frame-ancestors 'none'"));
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_must_be_this_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("rw.example.com"));
        assert!(same_origin(&h), "no Origin: a same-origin navigation or a non-browser client");
        h.insert(header::ORIGIN, HeaderValue::from_static("https://rw.example.com"));
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("https://evil.example.com"));
        assert!(!same_origin(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("https://rw.example.com"));
        h.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&h));
    }

    #[test]
    fn csrf_needs_the_sessions_token_in_either_header() {
        let mut h = HeaderMap::new();
        assert!(!csrf_ok(&h, "abc"));
        h.insert("x-xsrf-token", HeaderValue::from_static("abd"));
        assert!(!csrf_ok(&h, "abc"));
        h.insert("x-xsrf-token", HeaderValue::from_static("abc"));
        assert!(csrf_ok(&h, "abc"));
        let mut h = HeaderMap::new();
        h.insert("x-csrf-token", HeaderValue::from_static("abc"));
        assert!(csrf_ok(&h, "abc"));
    }
}
