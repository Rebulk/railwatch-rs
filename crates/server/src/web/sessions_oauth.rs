//! Google sign-in: OmniAuth's two routes for the google_oauth2 strategy
//! (`POST /auth/google_oauth2`, the request phase, and the callback
//! phase) and SessionsOauthController (`GET /auth/:provider/callback`,
//! `GET /auth/failure`), with User.from_omniauth. Flash texts and redirects
//! are railwatch-cloud's.
//!
//! Without google_client_id/secret nothing here is reachable, as Rails
//! without the OmniAuth middleware: the request phase is a 404 and a hit
//! on the callback says Google sign-in is not available.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use super::WebState;
use super::controllers::{internal, not_found};
use super::google_oauth2::{self as strategy, Failure, Identity, PROVIDER};
use super::inertia::{Ctx, Redirect};
use super::params::Params;
use super::session::{csrf_token, same_origin};
use crate::config::Signup;
use crate::control::{self, ModelError, now_db};

type S = Arc<WebState>;

/// Exempt from the session layer's header-only CSRF check: the button is a
/// plain HTML form whose token is the `authenticity_token` field
/// (omniauth-rails_csrf_protection), checked by `request_phase` itself.
pub const REQUEST_PATH: &str = "/auth/google_oauth2";

pub fn routes() -> Router<S> {
    Router::new()
        .route(REQUEST_PATH, get(|| async { not_found() }).post(request_phase))
        .route("/auth/{provider}/callback", get(create))
        .route("/auth/failure", get(failure))
}

const UNAVAILABLE: &str = "Google sign-in is not available right now.";
const INVALID: &str = "That Google account's email is unverified or already registered. Sign in with your password instead.";
const INVITE_ONLY: &str = "Sign-up is by invitation only. Ask an account owner to invite you.";

/// OmniAuth's failure endpoint: `/auth/failure?message=<key>&strategy=google_oauth2`.
fn fail(cx: &Ctx, key: &str, reauthenticate: Option<String>) -> Response {
    tracing::warn!(failure = key, "Google sign-in failed");
    // OmniAuth drops omniauth.params before redirecting here; keep a
    // reauthentication for one request so failure returns to its settings
    // page (Rails, without it, sends any signed-in user to the profile).
    if let Some(r) = reauthenticate {
        cx.session.put(strategy::PARAMS, json!({"reauthenticate": r}));
    }
    let message: String = form_urlencoded::byte_serialize(key.as_bytes()).collect();
    cx.redirect(Redirect::to(format!("/auth/failure?message={message}&strategy={PROVIDER}")))
}

/// The request phase: CSRF (omniauth-rails_csrf_protection's
/// `verified_request?`: same origin and the session's token in the form
/// field or `X-CSRF-Token`), then a 302 to Google with state, PKCE and nonce
/// kept in the session.
async fn request_phase(cx: Ctx, headers: HeaderMap, p: Params) -> Response {
    let Some(google) = cx.state.config.google.clone() else { return not_found() };
    let given = p.presence("authenticity_token").or_else(|| headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).map(str::to_string));
    let expected = csrf_token(&cx.session);
    if !same_origin(&headers) || !given.is_some_and(|g| bool::from(g.as_bytes().ct_eq(expected.as_bytes()))) {
        return fail(&cx, "ActionController::InvalidAuthenticityToken", None);
    }
    let auth = match strategy::authorize(&google, &cx.state.config.google_redirect_uri(), p.presence("prompt").as_deref(), p.presence("login_hint").as_deref())
    {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    cx.session.put(strategy::STATE, Value::String(auth.state));
    cx.session.put(strategy::PKCE_VERIFIER, Value::String(auth.pkce_verifier));
    cx.session.put(strategy::NONCE, Value::String(auth.nonce));
    // `session['omniauth.params'] = request.GET`: the button posts to
    // `/auth/google_oauth2?reauthenticate=<user|account>`. Params merges the
    // query with the form, so a form field is accepted too.
    match p.presence("reauthenticate") {
        Some(r) => cx.session.put(strategy::PARAMS, json!({"reauthenticate": r})),
        None => {
            cx.session.take(strategy::PARAMS);
        }
    }
    cx.redirect(Redirect::to(auth.url))
}

/// The callback phase (OmniAuth) then SessionsOauthController#create.
async fn create(cx: Ctx, Path(provider): Path<String>, p: Params) -> Response {
    let reauthenticate = cx.session.take(strategy::PARAMS).and_then(|v| v["reauthenticate"].as_str().map(str::to_string)).filter(|r| !r.is_empty());
    let pending = strategy::Pending {
        state: cx.session.take(strategy::STATE).and_then(|v| v.as_str().map(str::to_string)),
        pkce_verifier: cx.session.take(strategy::PKCE_VERIFIER).and_then(|v| v.as_str().map(str::to_string)),
        nonce: cx.session.take(strategy::NONCE).and_then(|v| v.as_str().map(str::to_string)),
    };
    let auth = match (&cx.state.config.google, provider == PROVIDER) {
        (Some(google), true) => {
            let error = p.presence("error_reason").or_else(|| p.presence("error"));
            match strategy::callback(google, &cx.state.config.google_redirect_uri(), &p.s("code"), &p.s("state"), error.as_deref(), pending).await {
                Ok(identity) => Some(identity),
                Err(f) => {
                    if let Failure::InvalidCredentials(why) = &f {
                        tracing::warn!(error = %why, "Google sign-in: invalid credentials");
                    }
                    return fail(&cx, f.message_key(), reauthenticate);
                }
            }
        }
        _ => None,
    };

    // before_action :require_no_authentication_unless_reauthenticating
    let signed_in = cx.current.user.is_some();
    if signed_in && reauthenticate.is_none() {
        return cx.redirect(Redirect::to("/").notice("You are already signed in"));
    }
    let Some(auth) = auth else {
        return cx.redirect(Redirect::to("/sign_in").alert(UNAVAILABLE));
    };
    if let (true, Some(r)) = (signed_in, reauthenticate) {
        return complete_reauthentication(&cx, &auth, &r).await;
    }

    let open = cx.state.config.signup == Signup::Open;
    let ua = cx.current.user_agent.clone();
    let ip = cx.current.ip.clone();
    let outcome = cx
        .state
        .db(move |c| {
            c.tx(|tx| match from_omniauth(tx, &auth, open)? {
                Ok(uid) => Ok(Ok(control::create_session(tx, uid, ua.as_deref(), Some(&ip))?)),
                Err(refused) => Ok(Err(refused)),
            })
        })
        .await;
    match outcome {
        Ok(Ok(session)) => {
            cx.outgoing.sign_in(session);
            cx.redirect(Redirect::to("/dashboard").notice("Signed in with Google"))
        }
        Ok(Err(Refused::Invalid)) | Err(ModelError::Invalid(_)) => cx.redirect(Redirect::to("/sign_in").alert(INVALID)),
        Ok(Err(Refused::InviteOnly)) => cx.redirect(Redirect::to("/sign_in").alert(INVITE_ONLY)),
        Err(ModelError::Sql(e)) => internal(e),
    }
}

/// SessionsOauthController#failure.
async fn failure(cx: Ctx) -> Response {
    let reauthenticate = cx.session.take(strategy::PARAMS).and_then(|v| v["reauthenticate"].as_str().map(str::to_string)).filter(|r| !r.is_empty());
    match (cx.current.user.is_some(), reauthenticate) {
        (true, Some(r)) => cx.redirect(Redirect::to(return_path(&r)).alert("Google could not confirm your identity. Please try again.")),
        // Any failure while signed in is a failed reauthentication (Rails
        // cannot tell which one: OmniAuth drops the params before here).
        (true, None) => cx.redirect(Redirect::to("/settings/profile").alert("Google could not confirm your identity. Please try again.")),
        (false, _) => cx.redirect(Redirect::to("/sign_in").alert("Sign in with Google failed. Please try again.")),
    }
}

fn return_path(reauthenticate: &str) -> &'static str {
    if reauthenticate == "account" { "/settings/members" } else { "/settings/profile" }
}

async fn complete_reauthentication(cx: &Ctx, auth: &Identity, reauthenticate: &str) -> Response {
    let to = return_path(reauthenticate);
    let user = cx.user_id();
    let session = cx.current.session.as_ref().map(|s| s.id);
    let (verified, uid) = (auth.email_verified, auth.uid.clone());
    let confirmed = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let linked: Option<(Option<String>, Option<String>)> =
                    x.query_row("SELECT provider, uid FROM users WHERE id = ?", [user], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                let same_identity = linked.is_some_and(|(p, u)| p.as_deref() == Some(PROVIDER) && u.as_deref() == Some(uid.as_str()));
                if !(verified && same_identity) {
                    return Ok(false);
                }
                // Current.session.mark_authenticated!
                x.execute("UPDATE sessions SET authenticated_at = ?1, updated_at = ?1 WHERE id = ?2", params![now_db(), session])?;
                Ok(true)
            })
        })
        .await;
    match confirmed {
        Ok(true) => cx.redirect(Redirect::to(to).notice("Identity confirmed. You can now continue.")),
        Ok(false) => cx.redirect(Redirect::to(to).alert("Sign in with the Google account already linked to this user.")),
        Err(e) => internal(e),
    }
}

/// Why User.from_omniauth refused (ActiveRecord::RecordInvalid, or the
/// invite-only rule Rails does not have).
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Invalid,
    InviteOnly,
}

/// User.from_omniauth: by provider+uid, else link onto the user with the
/// same *verified* email, else create a verified user with a random
/// password (sign-up through Google accepts the terms). Google must have
/// verified the email for any of it. With invite-only sign-up, a new user
/// is created only for an email with a pending invitation.
pub fn from_omniauth(tx: &rusqlite::Connection, auth: &Identity, signup_open: bool) -> control::ModelResult<Result<i64, Refused>> {
    let Some(email) = auth.email.as_deref().map(control::normalize_email).filter(|e| auth.email_verified && !e.is_empty()) else {
        return Ok(Err(Refused::Invalid));
    };
    let by_uid: Option<i64> = tx.query_row("SELECT id FROM users WHERE provider = ?1 AND uid = ?2", params![PROVIDER, auth.uid], |r| r.get(0)).optional()?;
    if let Some(id) = by_uid {
        return Ok(Ok(id));
    }
    let by_email: Option<i64> = tx.query_row("SELECT id FROM users WHERE email = ? AND verified", [&email], |r| r.get(0)).optional()?;
    if let Some(id) = by_email {
        tx.execute("UPDATE users SET provider = ?1, uid = ?2, updated_at = ?3 WHERE id = ?4", params![PROVIDER, auth.uid, now_db(), id])?;
        return Ok(Ok(id));
    }
    if !signup_open {
        let invited: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM invitations WHERE email = ? AND accepted_at IS NULL AND expires_at > ?)",
            params![email, now_db()],
            |r| r.get(0),
        )?;
        if !invited {
            return Ok(Err(Refused::InviteOnly));
        }
    }
    if !control::valid_email(&email) {
        return Ok(Err(Refused::Invalid));
    }
    let name = auth.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&email).to_string();
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).expect("random");
    let id = match control::insert_user(tx, &name, &email, &crate::ingest::receipt_headers::hex(&random), true) {
        Ok(id) => id,
        // The email belongs to a user who never verified it.
        Err(ModelError::Invalid(_)) => return Ok(Err(Refused::Invalid)),
        Err(e) => return Err(e),
    };
    tx.execute("UPDATE users SET provider = ?1, uid = ?2 WHERE id = ?3", params![PROVIDER, auth.uid, id])?;
    Ok(Ok(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, insert_user, test_password};

    fn identity(email: &str, uid: &str, verified: bool) -> Identity {
        Identity { uid: uid.into(), email: Some(email.into()), email_verified: verified, name: Some("Ada Lovelace".into()) }
    }

    fn control(name: &str) -> (Control, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rw-oauth-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (Control::open(&dir.join("control.sqlite3")).unwrap(), dir)
    }

    fn row(c: &Control, id: i64) -> (String, Option<String>, Option<String>, bool, bool) {
        c.with(|x| {
            x.query_row("SELECT email, provider, uid, verified, terms_accepted_at IS NOT NULL FROM users WHERE id = ?", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
        })
        .unwrap()
    }

    #[test]
    fn from_omniauth_creates_links_and_refuses_like_rails() {
        let (c, dir) = control("rails");
        // First login: a verified user with the Google identity, terms accepted.
        let new = c.tx(|tx| from_omniauth(tx, &identity("New@Example.com", "g-1", true), true)).unwrap().unwrap();
        assert_eq!(row(&c, new), ("new@example.com".into(), Some(PROVIDER.into()), Some("g-1".into()), true, true));
        // Same uid again: the same user, even if Google's email changed.
        assert_eq!(c.tx(|tx| from_omniauth(tx, &identity("renamed@example.com", "g-1", true), true)).unwrap(), Ok(new));
        // An existing verified-email user is linked, not duplicated.
        let linked = c.tx(|tx| insert_user(tx, "Linked", "linked@example.com", &test_password(), true)).unwrap();
        assert_eq!(c.tx(|tx| from_omniauth(tx, &identity("linked@example.com", "g-2", true), true)).unwrap(), Ok(linked));
        assert_eq!(row(&c, linked).2.as_deref(), Some("g-2"));
        // An unverified-email user is neither linked nor duplicated.
        let unverified = c.tx(|tx| insert_user(tx, "U", "unverified@example.com", &test_password(), false)).unwrap();
        assert_eq!(c.tx(|tx| from_omniauth(tx, &identity("unverified@example.com", "g-3", true), true)).unwrap(), Err(Refused::Invalid));
        assert_eq!(row(&c, unverified).2, None);
        // Google has not verified the email: nothing linked or created.
        assert_eq!(c.tx(|tx| from_omniauth(tx, &identity("linked@example.com", "attacker", false), true)).unwrap(), Err(Refused::Invalid));
        assert_eq!(row(&c, linked).2.as_deref(), Some("g-2"));
        let users: i64 = c.with(|x| x.query_row("SELECT count(*) FROM users", [], |r| r.get(0))).unwrap();
        assert_eq!(users, 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invite_only_signs_existing_users_in_but_creates_only_the_invited() {
        let (c, dir) = control("invite");
        let existing = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        assert_eq!(c.tx(|tx| from_omniauth(tx, &identity("ada@example.com", "g-ada", true), false)).unwrap(), Ok(existing));
        assert_eq!(c.tx(|tx| from_omniauth(tx, &identity("stranger@example.com", "g-x", true), false)).unwrap(), Err(Refused::InviteOnly));
        let account = c.tx(|tx| control::create_account(tx, "Acme", existing)).unwrap();
        c.tx(|tx| control::create_invitation(tx, account, existing, "invited@example.com", "member").map(|_| ())).unwrap();
        let invited = c.tx(|tx| from_omniauth(tx, &identity("invited@example.com", "g-inv", true), false)).unwrap().unwrap();
        assert!(row(&c, invited).3, "created verified");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
