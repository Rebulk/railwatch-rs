//! Inertia for the web app: omega's layer (asset version 409, redirect
//! rules, flash/errors through the session), the shared props
//! InertiaController shares, and Rails-style redirects.
//!
//! Flash, as the Rails app sends it (checked against rw-baseline):
//! - the `flash` shared prop is always `{alert, warning, notice}`, nulls
//!   included (InertiaController's inertia_share);
//! - the page object's own `flash` field carries only the set `notice` and
//!   `alert` (inertia_rails' default flash_keys); omega writes it from the
//!   session keys it reads.
//!
//! The redirect's flash is stored in the session under `flash` (all three
//! keys) and omega's own key (`inertia.flash_data`, notice/alert); a
//! prefetch never consumes either (kit's prefetch+flash fix: omega gets no
//! session for a prefetch, and the shared prop is read without taking).

use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::WebState;
use super::assets;
use super::document;
use super::session::{Current, Outgoing, SessionHandle, csrf_token};

/// The Rails flash for the next request, in the session.
const FLASH: &str = "flash";

pub fn config() -> omega::Config {
    omega::Config::new()
        .version_with(|| assets::manifest().version.clone())
        .root_view(document::render)
        .with_all_errors(true)
}

fn is_prefetch(parts: &Parts) -> bool {
    ["purpose", "sec-purpose", "x-moz"].iter().any(|n| parts.headers.get(*n).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"prefetch")))
}

pub fn layer(router: Router<Arc<WebState>>) -> Router<Arc<WebState>> {
    let inertia = omega::axum::InertiaLayer::new(config()).session(|parts: &Parts| {
        if is_prefetch(parts) {
            return None;
        }
        parts.extensions.get::<SessionHandle>().cloned()
    });
    router.layer(axum::middleware::from_fn(move |req: Request, next: axum::middleware::Next| {
        let inertia = inertia.clone();
        async move {
            let res = inertia.handle(req, |req| async move { Ok::<_, std::convert::Infallible>(next.run(req).await) }).await;
            let mut res = match res {
                Ok(r) => r,
                Err(never) => match never {},
            };
            if res.headers().get(omega::header::INERTIA).is_some() {
                res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
            }
            res
        }
    }))
}

/// What a handler needs to answer like a Rails controller.
pub struct Ctx {
    pub state: Arc<WebState>,
    pub current: Arc<Current>,
    pub session: SessionHandle,
    pub outgoing: Outgoing,
    pub inertia: omega::Inertia,
    pub prefetch: bool,
    pub path: String,
    pub referer: Option<String>,
}

impl FromRequestParts<Arc<WebState>> for Ctx {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<WebState>) -> Result<Self, Self::Rejection> {
        let missing = || (StatusCode::INTERNAL_SERVER_ERROR, "web layers missing").into_response();
        Ok(Ctx {
            state: state.clone(),
            current: parts.extensions.get::<Arc<Current>>().cloned().ok_or_else(missing)?,
            session: parts.extensions.get::<SessionHandle>().cloned().ok_or_else(missing)?,
            outgoing: parts.extensions.get::<Outgoing>().cloned().ok_or_else(missing)?,
            inertia: parts.extensions.get::<omega::Inertia>().cloned().ok_or_else(missing)?,
            prefetch: is_prefetch(parts),
            path: parts.uri.path().to_string(),
            referer: parts.headers.get(header::REFERER).and_then(|v| v.to_str().ok()).map(str::to_string),
        })
    }
}

/// A 302 with flash/errors for the next page (`redirect_to path, notice:`,
/// `inertia: {errors: ...}`).
#[derive(Default)]
pub struct Redirect {
    pub to: String,
    pub notice: Option<String>,
    pub alert: Option<String>,
    pub errors: Option<Vec<(String, String)>>,
    pub clear_history: bool,
    /// Extra one-request values (`flash[:new_token]`).
    pub extra: Map<String, Value>,
}

impl Redirect {
    pub fn to(path: impl Into<String>) -> Redirect {
        Redirect { to: path.into(), ..Default::default() }
    }
    pub fn notice(mut self, m: impl Into<String>) -> Self {
        self.notice = Some(m.into());
        self
    }
    pub fn alert(mut self, m: impl Into<String>) -> Self {
        self.alert = Some(m.into());
        self
    }
    pub fn errors(mut self, e: Vec<(String, String)>) -> Self {
        self.errors = Some(e);
        self
    }
    pub fn error(self, field: &str, message: &str) -> Self {
        self.errors(vec![(field.into(), message.into())])
    }
    pub fn clear_history(mut self) -> Self {
        self.clear_history = true;
        self
    }
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

/// The path of a Referer, when redirecting to it stays on this site: a
/// single leading `/`. `//host` and `/\host` are protocol-relative to a
/// browser (it treats `\` as `/`), so they are refused, as are control
/// characters (a browser strips tab/CR/LF before parsing).
fn local_path(referer: &str) -> Option<&str> {
    let path = referer.split_once("://").map(|(_, rest)| rest.find('/').map_or("/", |i| &rest[i..])).unwrap_or(referer);
    let b = path.as_bytes();
    let ok = b.first() == Some(&b'/') && !matches!(b.get(1), Some(b'/' | b'\\')) && !b.iter().any(u8::is_ascii_control);
    ok.then_some(path)
}

impl Ctx {
    pub fn user_id(&self) -> i64 {
        self.current.user.as_ref().map(|u| u.id).expect("authenticated route")
    }

    pub fn account_id(&self) -> i64 {
        self.current.account.as_ref().map(|a| a.id).expect("account route")
    }

    /// `redirect_back_or_to`
    pub fn back_or(&self, fallback: &str) -> String {
        self.referer.as_deref().and_then(local_path).map_or_else(|| fallback.to_string(), str::to_string)
    }

    pub fn redirect(&self, r: Redirect) -> Response {
        let mut flash = Map::new();
        flash.insert("alert".into(), r.alert.clone().map_or(Value::Null, Value::String));
        flash.insert("warning".into(), Value::Null);
        flash.insert("notice".into(), r.notice.clone().map_or(Value::Null, Value::String));
        for (k, v) in r.extra {
            flash.insert(k, v);
        }
        self.session.put(FLASH, Value::Object(flash));
        if let Some(n) = r.notice {
            self.inertia.flash("notice", n);
        }
        if let Some(a) = r.alert {
            self.inertia.flash("alert", a);
        }
        if let Some(errors) = r.errors {
            let mut v = omega::ValidationErrors::new();
            for (field, message) in errors {
                v.add(field, message);
            }
            self.inertia.with_errors(v);
        }
        if r.clear_history {
            self.inertia.clear_history();
        }
        let mut res = (StatusCode::FOUND, [(header::LOCATION, r.to)]).into_response();
        res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        res
    }

    /// A one-request value set by the previous redirect (`flash[:key]`),
    /// read without consuming (render takes the whole flash).
    pub fn flash_value(&self, key: &str) -> Option<Value> {
        self.session.get(FLASH)?.get(key).cloned().filter(|v| !v.is_null())
    }

    /// Renders `component` with InertiaController's shared props plus
    /// `extra_shared` (EnvironmentScoped's) and the page's own props.
    pub async fn render(&self, component: &str, props: Value, extra_shared: Option<Value>) -> Response {
        let shared = match self.shared_props().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "shared props failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
            }
        };
        for (k, v) in shared {
            self.inertia.share(k, v);
        }
        if let Some(Value::Object(extra)) = extra_shared {
            for (k, v) in extra {
                self.inertia.share(k, v);
            }
        }
        let mut page = omega::Props::new();
        if let Value::Object(m) = props {
            for (k, v) in m {
                page.insert(k, v);
            }
        }
        let csrf = csrf_token(&self.session);
        self.inertia.render(component, page).with_view_data(document::CSRF, csrf).into_response()
    }

    async fn shared_props(&self) -> rusqlite::Result<Vec<(String, omega::Prop)>> {
        let cur = &self.current;
        let user = cur.user.as_ref().map(|u| {
            json!({"id": u.id, "email": u.email, "name": u.name, "created_at": t_json(&u.created_at), "updated_at": t_json(&u.updated_at),
                   "verified": u.verified, "provider": u.provider, "editor": u.editor, "editor_root": u.editor_root})
        });
        let session = cur
            .session
            .as_ref()
            .map(|s| json!({"id": s.id, "recently_authenticated": crate::control::recently_authenticated(s)}));
        // Rails' flash: the previous redirect's, consumed by this render
        // (not by a prefetch, whose page may never be shown).
        let flash = if self.prefetch { self.session.get(FLASH) } else { self.session.take(FLASH) };
        let flash = flash.and_then(|f| f.as_object().cloned()).unwrap_or_default();
        let pick = |k: &str| flash.get(k).cloned().filter(Value::is_string).unwrap_or(Value::Null);
        let account = cur.account.as_ref().map(|a| json!({"id": a.id, "name": a.name, "slug": a.slug, "plan": a.plan}));
        let user_id = cur.user.as_ref().map(|u| u.id);
        let account_id = cur.account.as_ref().map(|a| a.id);
        let (accounts, applications) = self
            .state
            .db(move |c| {
                c.with(|conn| {
                    let accounts: Vec<Value> = match user_id {
                        None => vec![],
                        Some(uid) => conn
                            .prepare_cached("SELECT a.id, a.name FROM accounts a JOIN memberships m ON m.account_id = a.id WHERE m.user_id = ? ORDER BY a.name")?
                            .query_map([uid], |r| Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?})))?
                            .collect::<rusqlite::Result<_>>()?,
                    };
                    let applications = match account_id {
                        None => vec![],
                        Some(aid) => navigation_applications(conn, aid)?,
                    };
                    Ok((accounts, applications))
                })
            })
            .await?;
        let nav_key = match &cur.account {
            Some(a) => format!("navigation-{}-{}", a.id, rails_to_f(&a.updated_at)),
            None => "navigation-guest".to_string(),
        };
        Ok(vec![
            ("auth".into(), omega::IntoProp::into_prop(json!({"user": user, "session": session}))),
            ("account".into(), omega::IntoProp::into_prop(account.unwrap_or(Value::Null))),
            ("accounts".into(), omega::IntoProp::into_prop(Value::Array(accounts))),
            ("flash".into(), omega::IntoProp::into_prop(json!({"alert": pick("alert"), "warning": pick("warning"), "notice": pick("notice")}))),
            ("google_oauth".into(), omega::IntoProp::into_prop(Value::Bool(self.state.config.google.is_some()))),
            (
                "applications".into(),
                omega::lazy(move || async move { Value::Array(applications) }).once_as(nav_key).until(std::time::Duration::from_secs(30)),
            ),
        ])
    }
}

/// InertiaController#navigation_applications.
pub fn navigation_applications(conn: &rusqlite::Connection, account_id: i64) -> rusqlite::Result<Vec<Value>> {
    let mut envs: std::collections::HashMap<i64, Vec<Value>> = Default::default();
    let mut s = conn.prepare_cached(
        "SELECT e.application_id, e.id, e.name, e.slug, e.last_seen_at, e.paused_at IS NOT NULL FROM environments e
         JOIN applications ap ON ap.id = e.application_id WHERE ap.account_id = ? ORDER BY e.application_id, e.name",
    )?;
    let rows = s.query_map([account_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            json!({"id": r.get::<_, i64>(1)?, "name": r.get::<_, String>(2)?, "slug": r.get::<_, String>(3)?,
                   "last_seen_at": r.get::<_, Option<String>>(4)?.map(|t| t_json(&t)), "paused": r.get::<_, bool>(5)?}),
        ))
    })?;
    for row in rows {
        let (app, e) = row?;
        envs.entry(app).or_default().push(e);
    }
    let mut s = conn.prepare_cached("SELECT id, name, slug, issue_prefix FROM applications WHERE account_id = ? ORDER BY name")?;
    let apps = s.query_map([account_id], |r| {
        let id: i64 = r.get(0)?;
        Ok(json!({"id": id, "name": r.get::<_, String>(1)?, "slug": r.get::<_, String>(2)?, "issue_prefix": r.get::<_, String>(3)?,
                  "environments": envs.remove(&id).unwrap_or_default()}))
    })?;
    apps.collect()
}

/// `TimeWithZone#as_json` of a datetime(6) column: ISO 8601 with
/// milliseconds and Z ("2026-10-13T18:11:17.778Z").
pub fn t_json(db: &str) -> String {
    match crate::time::parse_db_time(db) {
        Some(us) => {
            let secs = us.div_euclid(1_000_000);
            format!("{}.{:03}Z", crate::time::utc_seconds_string(secs).replacen(' ', "T", 1), us.rem_euclid(1_000_000) / 1000)
        }
        None => db.to_string(),
    }
}

pub fn t_json_opt(db: Option<String>) -> Value {
    db.map_or(Value::Null, |t| Value::String(t_json(&t)))
}

/// `Time#to_f` as Ruby prints it in the navigation cache key.
fn rails_to_f(db: &str) -> String {
    let us = crate::time::parse_db_time(db).unwrap_or(0);
    let mut s = format!("{:.6}", us as f64 / 1e6);
    while s.ends_with('0') && !s.ends_with(".0") {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_paths_stay_on_site() {
        assert_eq!(local_path("https://rw.example/issues?status=open"), Some("/issues?status=open"));
        assert_eq!(local_path("https://rw.example"), Some("/"));
        assert_eq!(local_path("/alerts"), Some("/alerts"));
        for bad in ["https://rw.example//evil.example/x", "https://rw.example/\\evil.example", "//evil.example", "/\\evil.example", "/\t/evil.example", "evil.example", "https://rw.example/a\nb"] {
            assert_eq!(local_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn times_render_as_rails_as_json() {
        assert_eq!(t_json("2026-10-13 18:11:17.778703"), "2026-10-13T18:11:17.778Z");
        assert_eq!(t_json("2026-10-13 18:11:17"), "2026-10-13T18:11:17.000Z");
        assert_eq!(rails_to_f("2026-10-13 19:19:55.627209"), "1791919195.627209");
        assert_eq!(rails_to_f("2026-10-13 19:19:55"), "1791919195.0");
    }
}
