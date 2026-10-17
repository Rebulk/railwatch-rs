//! The cloud-only controllers, ported from railwatch-cloud @ 464f0fd:
//! SessionsController, UsersController, Identity::PasswordResets/
//! EmailVerifications, InvitationsController, HomeController,
//! DashboardController, AccountsController, ApplicationsController,
//! EnvironmentsController, and Settings::{Profiles, Passwords, Emails,
//! Sessions, Members, Invitations}. Routes, redirect targets, flash texts
//! and props are the Rails ones.
//!
//! Not here: Google sign-in (sessions_oauth.rs), integrations/alert rules and
//! issues (p2-rollups' tables), usage (an environment page), docs, MCP.

// Guards return the response to send instead (a redirect, a 403), as a
// before_action halts the chain; boxing it buys nothing on this path.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::WebState;
use super::assets;
use super::inertia::{Ctx, Redirect, navigation_applications, t_json, t_json_opt};
use super::mailer;
use super::params::Params;
use crate::control::{self, AcceptError, ModelError, now_db};

type S = Arc<WebState>;

pub fn routes() -> Router<S> {
    Router::new()
        .route("/", get(home))
        .route("/sign_in", get(sign_in_new).post(sign_in_create))
        .route("/sign_up", get(sign_up_new).post(sign_up_create))
        .route("/sessions/{id}", delete(session_destroy))
        .route("/users", delete(user_destroy))
        .route("/identity/email_verification", get(email_verification_show).post(email_verification_create))
        .route("/identity/password_reset/new", get(password_reset_new))
        .route("/identity/password_reset/edit", get(password_reset_edit))
        .route("/identity/password_reset", post(password_reset_create).patch(password_reset_update).put(password_reset_update))
        .route("/invitations/{token}", get(invitation_show))
        .route("/dashboard", get(dashboard))
        .route("/accounts/new", get(account_new))
        .route("/accounts", post(account_create))
        .route("/accounts/{id}", delete(account_destroy))
        .route("/accounts/{id}/switch", post(account_switch))
        .route("/apps/new", get(application_new))
        .route("/apps", post(application_create))
        .route("/apps/{app}", get(application_show).patch(application_update).put(application_update).delete(application_destroy))
        .route("/apps/{app}/edit", get(application_edit))
        .route("/apps/{app}/envs/new", get(environment_new))
        .route("/apps/{app}/envs", post(environment_create))
        .route("/apps/{app}/envs/{env}/edit", get(environment_edit))
        .route("/apps/{app}/envs/{env}", patch(environment_update).put(environment_update).delete(environment_destroy))
        .route("/apps/{app}/envs/{env}/rotate_token", post(environment_rotate_token))
        .route("/apps/{app}/envs/{env}/pause", post(environment_pause))
        .route("/settings/profile", get(profile_show).patch(profile_update).put(profile_update))
        .route("/settings/profile/generate_mcp_token", post(profile_generate_token))
        .route("/settings/profile/revoke_mcp_token", delete(profile_revoke_token))
        .route("/settings/password", get(password_show).patch(password_update).put(password_update))
        .route("/settings/email", get(email_show).patch(email_update).put(email_update))
        .route("/settings/sessions", get(sessions_index))
        .route("/settings/appearance", get(appearance))
        .route("/settings/members", get(members_index).post(members_create))
        .route("/settings/members/{id}", patch(members_update).put(members_update).delete(members_destroy))
        .route("/settings/members/{id}/transfer_ownership", post(members_transfer))
        .route("/settings/invitations/{id}", delete(invitation_destroy))
        .route("/settings/invitations/{id}/resend", post(invitation_resend))
}

pub async fn asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    assets::serve(&path, &headers).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

pub async fn icon(uri: axum::http::Uri, headers: HeaderMap) -> Response {
    assets::serve(uri.path().trim_start_matches('/'), &headers).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

// ---------------------------------------------------------------------------
// Guards (ApplicationController's before_actions)

pub(crate) fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

pub(crate) fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// `authenticate`
pub(crate) fn authenticate(cx: &Ctx) -> Result<(), Response> {
    if cx.current.user.is_some() { Ok(()) } else { Err(cx.redirect(Redirect::to("/sign_in"))) }
}

/// `require_no_authentication`
fn require_no_authentication(cx: &Ctx) -> Result<(), Response> {
    if cx.current.user.is_some() { Err(cx.redirect(Redirect::to("/").notice("You are already signed in"))) } else { Ok(()) }
}

/// `authenticate` + `require_account`
pub(crate) fn require_account(cx: &Ctx) -> Result<i64, Response> {
    authenticate(cx)?;
    match &cx.current.account {
        Some(a) => Ok(a.id),
        None => Err(cx.redirect(Redirect::to("/accounts/new").notice("Create an account to get started"))),
    }
}

/// `require_account_admin`: 403 unless owner/admin.
pub(crate) async fn require_admin(cx: &Ctx) -> Result<i64, Response> {
    let account = require_account(cx)?;
    let user = cx.user_id();
    let role = cx.state.db(move |c| c.with(|x| control::membership_role(x, account, user))).await.map_err(internal)?;
    if matches!(role.as_deref(), Some("owner" | "admin")) { Ok(account) } else { Err(StatusCode::FORBIDDEN.into_response()) }
}

fn rate_limited(cx: &Ctx, limit: &crate::rate_limit::FixedWindow<std::net::IpAddr>, to: &str, alert: &str) -> Result<(), Response> {
    let ip: std::net::IpAddr = cx.current.ip.parse().unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    if limit.exceeded(&ip) { Err(cx.redirect(Redirect::to(to).alert(alert))) } else { Ok(()) }
}

/// The provisioning throttle (30/hour per user), `provisioning_rate_limited`.
fn provisioning(cx: &Ctx) -> Result<(), Response> {
    if cx.state.provisioning_limit.exceeded(&cx.user_id()) {
        Err(cx.redirect(Redirect::to(cx.back_or("/dashboard")).alert("Too many resources created. Try again later.")))
    } else {
        Ok(())
    }
}

macro_rules! guard {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}
pub(crate) use guard;

pub(crate) fn model_errors(e: ModelError) -> Result<Vec<(String, String)>, Response> {
    match e {
        ModelError::Invalid(v) => Ok(v),
        ModelError::Sql(e) => Err(internal(e)),
    }
}

// ---------------------------------------------------------------------------
// Signed tokens (generates_token_for): password reset and email
// verification links. `<user id>.<expiry epoch>.<HMAC>`; the reset token
// also binds the current password digest, so it dies once used.

fn signed_token(cx: &Ctx, purpose: &str, user_id: i64, bind: &str, ttl_secs: i64) -> String {
    let exp = crate::time::now_micros() / 1_000_000 + ttl_secs;
    let value = format!("{user_id}.{exp}");
    let mac = cx.state.keys.sign(purpose, &format!("{value}.{bind}"));
    format!("{value}.{}", mac.rsplit_once("--").map_or("", |(_, m)| m))
}

fn verify_token(cx: &Ctx, purpose: &str, token: &str, bind_for: impl Fn(i64) -> Option<String>) -> Option<i64> {
    let mut parts = token.splitn(3, '.');
    let (id, exp, mac) = (parts.next()?.parse::<i64>().ok()?, parts.next()?.parse::<i64>().ok()?, parts.next()?);
    if exp < crate::time::now_micros() / 1_000_000 {
        return None;
    }
    let bind = bind_for(id)?;
    let value = format!("{id}.{exp}.{bind}");
    use base64::Engine;
    let cookie = format!("{}--{mac}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&value));
    cx.state.keys.verify(purpose, &cookie).map(|_| id)
}

fn reset_bind(digest: &str) -> String {
    // `password_salt.last(10)`: bcrypt's salt is characters 7..29.
    digest.get(19..29).unwrap_or(digest).to_string()
}

// ---------------------------------------------------------------------------
// Home, sign in/up/out

async fn home(cx: Ctx) -> Response {
    // HomeController renders the marketing page; self-hosted, the root is
    // the dashboard (or sign-in).
    if cx.current.user.is_some() { cx.redirect(Redirect::to("/dashboard")) } else { cx.redirect(Redirect::to("/sign_in")) }
}

async fn sign_in_new(cx: Ctx) -> Response {
    guard!(require_no_authentication(&cx));
    cx.render("sessions/new", json!({}), None).await
}

async fn sign_in_create(cx: Ctx, p: Params) -> Response {
    guard!(rate_limited(&cx, &cx.state.sign_in_limit, "/sign_in", "Too many sign-in attempts. Try again in a few minutes."));
    guard!(require_no_authentication(&cx));
    let (email, password) = (p.s("email"), p.s("password"));
    // Failures are counted per email, but never refuse: a refusal would let
    // anyone who knows an address keep its owner out, and a different answer
    // would say the bucket is full. The password is always checked (bcrypt,
    // same cost either way), so the right one signs in and a wrong one gets
    // the usual "incorrect", full bucket or not. Only the per-IP limit above
    // refuses outright.
    let email_key = email.trim().to_lowercase();
    let ua = cx.current.user_agent.clone();
    let ip = cx.current.ip.clone();
    let result = cx
        .state
        .db(move |c| {
            c.with(|x| match control::authenticate_by(x, &email, &password)? {
                Some(u) => Ok(Some(control::create_session(x, u.id, ua.as_deref(), Some(&ip))?)),
                None => Ok(None),
            })
        })
        .await;
    match result {
        Ok(Some(session)) => {
            cx.outgoing.sign_in(session);
            cx.redirect(Redirect::to("/dashboard").notice("Signed in successfully"))
        }
        Ok(None) => {
            if cx.state.sign_in_email_limit.exceeded(&email_key) {
                tracing::warn!(ip = %cx.current.ip, "repeated failed sign-ins for one email (password guessing or a forgotten password)");
            }
            cx.redirect(Redirect::to("/sign_in").alert("That email or password is incorrect"))
        }
        Err(e) => internal(e),
    }
}

async fn sign_up_new(cx: Ctx) -> Response {
    guard!(require_no_authentication(&cx));
    if !cx.state.signup_open() {
        return cx.redirect(Redirect::to("/sign_in").alert("Sign-up is by invitation only. Ask an account owner to invite you."));
    }
    cx.render("users/new", json!({}), None).await
}

async fn sign_up_create(cx: Ctx, p: Params) -> Response {
    if cx.state.sign_up_limit.exceeded(&cx.current.ip.parse().unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]))) {
        return cx.redirect(Redirect::to("/sign_up").alert("Too many sign-up attempts. Try again later."));
    }
    guard!(require_no_authentication(&cx));
    let token = p.s("invitation_token");
    // Where a failed attempt goes back to: the invitation page keeps its
    // props (Rails redirects to sign_up_path, which loses them).
    let back = if token.is_empty() { "/sign_up".to_string() } else { format!("/invitations/{token}") };
    if p.bool("terms") != Some(true) {
        return cx.redirect(Redirect::to(back).error("terms", "Accept the Terms of Service and Privacy Policy to continue"));
    }
    let (name, email, password) = (p.s("name"), control::normalize_email(&p.s("email")), p.s("password"));
    let confirmation = p.get("password_confirmation").map(|_| p.s("password_confirmation"));
    let errors = control::validate_new_user(&name, &email, &password, confirmation.as_deref());
    let open = cx.state.signup_open();
    let ua = cx.current.user_agent.clone();
    let ip = cx.current.ip.clone();
    enum Outcome {
        Invalid(Vec<(String, String)>),
        Joined { session: i64, account: i64, name: String },
        Created { session: i64, user: i64 },
        Closed,
        Unavailable,
    }
    let outcome = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let invitation = control::find_invitation_by_token(tx, &token)?.filter(|i| i.email == email);
                if invitation.is_none() && !open {
                    return Ok(Outcome::Closed);
                }
                if !errors.is_empty() {
                    return Ok(Outcome::Invalid(errors));
                }
                let user_id = match control::insert_user(tx, &name, &email, &password, invitation.is_some()) {
                    Ok(id) => id,
                    Err(ModelError::Invalid(e)) => return Ok(Outcome::Invalid(e)),
                    Err(e) => return Err(e),
                };
                let session = control::create_session(tx, user_id, ua.as_deref(), Some(&ip))?;
                match invitation {
                    Some(inv) => {
                        let user = control::find_user(tx, user_id)?.expect("just created");
                        match control::accept_invitation(tx, &inv, &user, &token)? {
                            Ok(()) => {
                                let account_name: String = tx.query_row("SELECT name FROM accounts WHERE id = ?", [inv.account_id], |r| r.get(0))?;
                                Ok(Outcome::Joined { session, account: inv.account_id, name: account_name })
                            }
                            Err(_) => Err(ModelError::Invalid(vec![])), // roll back; reported below
                        }
                    }
                    None => Ok(Outcome::Created { session, user: user_id }),
                }
            })
            .or_else(|e| match e {
                ModelError::Invalid(v) if v.is_empty() => Ok(Outcome::Unavailable),
                e => Err(e),
            })
        })
        .await;
    match outcome {
        Ok(Outcome::Invalid(e)) => cx.redirect(Redirect::to(back).errors(e)),
        Ok(Outcome::Closed) => cx.redirect(Redirect::to("/sign_in").alert("Sign-up is by invitation only. Ask an account owner to invite you.")),
        Ok(Outcome::Unavailable) => cx.redirect(Redirect::to("/sign_up").error("base", "That invitation has expired or was already used.")),
        Ok(Outcome::Joined { session, account, name }) => {
            cx.outgoing.sign_in(session);
            cx.outgoing.set_account(Some(account));
            cx.redirect(Redirect::to("/dashboard").notice(format!("Welcome! You've joined {name}")))
        }
        Ok(Outcome::Created { session, user }) => {
            cx.outgoing.sign_in(session);
            send_verification(&cx, user).await;
            cx.redirect(Redirect::to("/dashboard").notice("Welcome! You have signed up successfully"))
        }
        Err(e) => match model_errors(e) {
            Ok(v) => cx.redirect(Redirect::to("/sign_up").errors(v)),
            Err(r) => r,
        },
    }
}

async fn send_verification(cx: &Ctx, user_id: i64) {
    let Ok(Some(user)) = cx.state.db(move |c| c.with(|x| control::find_user(x, user_id))).await else { return };
    let sid = signed_token(cx, "email_verification", user.id, &user.email, 2 * 86_400);
    mailer::deliver_later(cx.state.mailer.clone(), mailer::email_verification(&cx.state.config.url, &user.email, &sid));
}

async fn session_destroy(cx: Ctx, Path(id): Path<i64>) -> Response {
    guard!(authenticate(&cx));
    let user = cx.user_id();
    let deleted = cx.state.db(move |c| c.with(|x| x.execute("DELETE FROM sessions WHERE id = ? AND user_id = ?", params![id, user]))).await;
    match deleted {
        Ok(0) => not_found(),
        Ok(_) => {
            if cx.current.session.as_ref().map(|s| s.id) == Some(id) {
                cx.outgoing.sign_out();
            }
            cx.redirect(Redirect::to("/settings/sessions").notice("That session has been logged out").clear_history())
        }
        Err(e) => internal(e),
    }
}

/// UsersController#destroy
async fn user_destroy(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    let user = cx.current.user.clone().expect("authenticated");
    let uid = user.id;
    let blocking = match cx.state.db(move |c| c.with(|x| sole_owned_accounts(x, uid))).await {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    if !blocking.is_empty() {
        let names = to_sentence(&blocking.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>());
        return cx.redirect(
            Redirect::to("/settings/profile").error("account_ownership", &format!("Transfer or delete {names} before deleting your user.")),
        );
    }
    // UsersController#deletion_confirmation_valid?
    let confirmed = if user.provider.as_deref().is_some_and(|p| !p.is_empty()) {
        p.s("confirmation").to_lowercase() == user.email.to_lowercase() && destructive_authentication(&cx, "").await
    } else {
        destructive_authentication(&cx, &p.s("password_challenge")).await
    };
    if !confirmed {
        return cx.redirect(Redirect::to("/settings/profile").error("password_challenge", "Confirm your identity before deleting your user."));
    }
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                tx.execute("DELETE FROM sessions WHERE user_id = ?", [uid])?;
                tx.execute("DELETE FROM api_tokens WHERE user_id = ?", [uid])?;
                tx.execute("DELETE FROM memberships WHERE user_id = ?", [uid])?;
                tx.execute("DELETE FROM saved_views WHERE user_id = ?", [uid])?;
                tx.execute("DELETE FROM invitations WHERE inviter_id = ?", [uid])?;
                tx.execute("DELETE FROM users WHERE id = ?", [uid])?;
                Ok(())
            })
        })
        .await;
    match r {
        Ok(()) => {
            cx.outgoing.sign_out();
            cx.redirect(Redirect::to("/").notice("Your user has been deleted").clear_history())
        }
        Err(e) => internal(e),
    }
}

fn sole_owned_accounts(x: &rusqlite::Connection, user_id: i64) -> rusqlite::Result<Vec<(i64, String)>> {
    x.prepare(
        "SELECT a.id, a.name FROM accounts a JOIN memberships m ON m.account_id = a.id AND m.user_id = ?1 AND m.role = 'owner'
         WHERE NOT EXISTS (SELECT 1 FROM memberships o WHERE o.account_id = a.id AND o.role = 'owner' AND o.user_id != ?1)",
    )?
    .query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?)))?
    .collect()
}

fn to_sentence(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// `destructive_authentication_valid?`: a user with a linked provider
/// (Google) confirms through it, so only a recent Google round-trip counts
/// (sessions_oauth's reauthentication), never the password.
async fn destructive_authentication(cx: &Ctx, password: &str) -> bool {
    let Some(user) = cx.current.user.clone() else { return false };
    if user.provider.as_deref().is_some_and(|p| !p.is_empty()) {
        return cx.current.session.as_ref().is_some_and(control::recently_authenticated);
    }
    let password = password.to_string();
    let session = cx.current.session.as_ref().map(|s| s.id);
    cx.state
        .db(move |c| {
            if !control::verify_password(&password, &user.password_digest) {
                return false;
            }
            if let Some(s) = session {
                let _ = c.with(|x| x.execute("UPDATE sessions SET authenticated_at = ?1, updated_at = ?1 WHERE id = ?2", params![now_db(), s]));
            }
            true
        })
        .await
}

// ---------------------------------------------------------------------------
// Identity

async fn email_verification_show(cx: Ctx, p: Params) -> Response {
    let sid = p.s("sid");
    let state = cx.state.clone();
    let user = {
        let users: Vec<(i64, String)> = state
            .db(|c| c.with(|x| x.prepare("SELECT id, email FROM users")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()))
            .await
            .unwrap_or_default();
        verify_token(&cx, "email_verification", &sid, |id| users.iter().find(|(u, _)| *u == id).map(|(_, e)| e.clone()))
    };
    match user {
        Some(id) => {
            let _ = cx.state.db(move |c| c.with(|x| x.execute("UPDATE users SET verified = 1, updated_at = ? WHERE id = ?", params![now_db(), id]))).await;
            cx.redirect(Redirect::to("/").notice("Thank you for verifying your email address"))
        }
        None => cx.redirect(Redirect::to("/settings/email").alert("That email verification link is invalid")),
    }
}

async fn email_verification_create(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    guard!(rate_limited(&cx, &cx.state.email_limit, "/settings/email", "Too many verification emails requested. Try again later."));
    send_verification(&cx, cx.user_id()).await;
    cx.redirect(Redirect::to(cx.back_or("/")).notice("We sent a verification email to your email address"))
}

async fn password_reset_new(cx: Ctx) -> Response {
    cx.render("identity/password_resets/new", json!({}), None).await
}

async fn reset_user(cx: &Ctx, sid: &str) -> Option<control::User> {
    let users: Vec<(i64, String)> = cx
        .state
        .db(|c| c.with(|x| x.prepare("SELECT id, password_digest FROM users")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()))
        .await
        .ok()?;
    let id = verify_token(cx, "password_reset", sid, |id| users.iter().find(|(u, _)| *u == id).map(|(_, d)| reset_bind(d)))?;
    cx.state.db(move |c| c.with(|x| control::find_user(x, id))).await.ok().flatten()
}

async fn password_reset_edit(cx: Ctx, p: Params) -> Response {
    let sid = p.s("sid");
    match reset_user(&cx, &sid).await {
        Some(u) => cx.render("identity/password_resets/edit", json!({"email": u.email, "sid": sid}), None).await,
        None => cx.redirect(Redirect::to("/identity/password_reset/new").alert("That password reset link is invalid")),
    }
}

/// Always the same answer: whether the address exists is not for a
/// stranger to learn. One email per 10 minutes per user (claim by UPDATE).
async fn password_reset_create(cx: Ctx, p: Params) -> Response {
    guard!(rate_limited(&cx, &cx.state.email_limit, "/identity/password_reset/new", "Too many password reset requests. Try again later."));
    let email = control::normalize_email(&p.s("email"));
    let claimed = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let Some(u) = control::find_user_by_email(x, &email)?.filter(|u| u.verified) else { return Ok(None) };
                let cutoff = crate::time::quoted_date_micros(crate::time::now_micros() - 600_000_000);
                let n = x.execute(
                    "UPDATE users SET password_reset_sent_at = ?1 WHERE id = ?2 AND (password_reset_sent_at IS NULL OR password_reset_sent_at <= ?3)",
                    params![now_db(), u.id, cutoff],
                )?;
                Ok((n == 1).then_some(u))
            })
        })
        .await;
    match claimed {
        Ok(Some(u)) => {
            let sid = signed_token(&cx, "password_reset", u.id, &reset_bind(&u.password_digest), 20 * 60);
            mailer::deliver_later(cx.state.mailer.clone(), mailer::password_reset(&cx.state.config.url, &u.email, &sid));
        }
        Ok(None) => {}
        Err(e) => return internal(e),
    }
    cx.redirect(Redirect::to("/sign_in").notice("If that address has a verified account, check your email for reset instructions"))
}

async fn password_reset_update(cx: Ctx, p: Params) -> Response {
    let sid = p.s("sid");
    let Some(user) = reset_user(&cx, &sid).await else {
        return cx.redirect(Redirect::to("/identity/password_reset/new").alert("That password reset link is invalid"));
    };
    let (password, confirmation) = (p.s("password"), p.s("password_confirmation"));
    let mut errors = Vec::new();
    if confirmation != password {
        errors.push(("password_confirmation".to_string(), "doesn't match Password".to_string()));
    }
    errors.extend(control::password_errors(&password));
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(format!("/identity/password_reset/edit?sid={sid}")).errors(errors));
    }
    match cx.state.db(move |c| c.with(|x| control::set_password(x, user.id, &password, None))).await {
        Ok(()) => cx.redirect(Redirect::to("/sign_in").notice("Your password was reset successfully. Please sign in")),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Invitations

async fn invitation_show(cx: Ctx, Path(token): Path<String>) -> Response {
    let t = token.clone();
    let found = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let Some(inv) = control::find_invitation_by_token(x, &t)? else { return Ok(None) };
                let (account, inviter): (String, String) = x.query_row(
                    "SELECT a.name, u.name FROM accounts a, users u WHERE a.id = ? AND u.id = ?",
                    params![inv.account_id, inv.inviter_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                Ok(Some((inv, account, inviter)))
            })
        })
        .await;
    let (inv, account, inviter) = match found {
        Ok(Some(f)) => f,
        Ok(None) => return cx.redirect(Redirect::to("/sign_in").alert("That invitation has expired or was already used.")),
        Err(e) => return internal(e),
    };
    match cx.current.user.clone() {
        Some(user) => {
            let account_id = inv.account_id;
            let accepted = cx.state.db(move |c| c.tx(|tx| control::accept_invitation(tx, &inv, &user, &token))).await;
            match accepted {
                Ok(Ok(())) => {
                    cx.outgoing.set_account(Some(account_id));
                    cx.redirect(Redirect::to("/dashboard").notice(format!("You've joined {account}")))
                }
                Ok(Err(AcceptError::RecipientMismatch)) => {
                    cx.redirect(Redirect::to("/dashboard").alert("This invitation was sent to a different email address."))
                }
                Ok(Err(AcceptError::Unavailable)) => cx.redirect(Redirect::to("/dashboard").alert("That invitation has expired or was already used.")),
                Err(e) => internal(e),
            }
        }
        None => {
            cx.render("users/new", json!({"invitation": {"token": token, "email": inv.email, "account": account, "inviter": inviter}}), None)
                .await
        }
    }
}

// ---------------------------------------------------------------------------
// Dashboard

async fn dashboard(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    let Some(account) = cx.current.account.clone() else { return cx.redirect(Redirect::to("/accounts/new")) };
    let tenants = cx.state.config.tenants_dir();
    let enforce_quota = cx.state.config.enforce_quota;
    let props = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let apps = navigation_applications(x, account.id)?;
                if apps.is_empty() {
                    return Ok(None);
                }
                let mut total = 0i64;
                let mut overview = Vec::new();
                for app in apps {
                    let app_id = app["id"].as_i64().unwrap_or(0);
                    let envs: Vec<Value> = x
                        .prepare_cached(
                            "SELECT id, name, slug, last_seen_at, paused_at IS NOT NULL, events_this_month FROM environments WHERE application_id = ? ORDER BY name",
                        )?
                        .query_map([app_id], |r| {
                            let slug: String = r.get(2)?;
                            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, slug, r.get::<_, Option<String>>(3)?, r.get::<_, bool>(4)?, r.get::<_, i64>(5)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                        .into_iter()
                        .map(|(id, name, slug, seen, paused, events)| {
                            total += events;
                            let (sparkline, error_rate) = request_sparkline(&tenants, &slug);
                            let open_issues = open_issue_count(x, id).unwrap_or(0);
                            json!({"id": id, "name": name, "last_seen_at": t_json_opt(seen), "paused": paused, "open_issues": open_issues,
                                   "events_this_month": events, "request_sparkline": sparkline, "error_rate": error_rate})
                        })
                        .collect();
                    overview.push(json!({"id": app_id, "name": app["name"], "slug": app["slug"], "issue_prefix": app["issue_prefix"], "environments": envs}));
                }
                let recent_issues = dashboard_recent_issues(x, account.id)?;
                Ok(Some(json!({
                    "overview_apps": overview,
                    "recent_issues": recent_issues,
                    // quota: null when not enforced (self-hosted default), which the
                    // dashboard renders as "no quota"; Rails always has a plan quota.
                    "usage": {"events_this_month": total, "quota": if enforce_quota { Some(account.monthly_event_quota) } else { None }, "plan": account.plan},
                })))
            })
        })
        .await;
    match props {
        Ok(Some(p)) => cx.render("dashboard/index", p, None).await,
        Ok(None) => cx.redirect(Redirect::to("/apps/new")),
        Err(e) => internal(e),
    }
}

/// DashboardController: `Issue.open.where(environment_id:).count`.
fn open_issue_count(c: &rusqlite::Connection, environment_id: i64) -> rusqlite::Result<i64> {
    c.prepare_cached("SELECT COUNT(*) FROM issues WHERE environment_id = ? AND status = 'open'")?.query_row([environment_id], |r| r.get(0))
}

/// DashboardController#index's recent_issues: the account's open issues,
/// most recently seen first, 10 of them.
fn dashboard_recent_issues(c: &rusqlite::Connection, account_id: i64) -> rusqlite::Result<Vec<Value>> {
    c.prepare_cached(
        "SELECT i.id, ap.issue_prefix || '-' || i.number, i.title, i.kind, i.last_seen_at, i.occurrences, ap.name, e.name
         FROM issues i JOIN applications ap ON ap.id = i.application_id JOIN environments e ON e.id = i.environment_id
         WHERE ap.account_id = ? AND i.status = 'open' ORDER BY i.last_seen_at DESC LIMIT 10",
    )?
    .query_map([account_id], |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "key": r.get::<_, String>(1)?, "title": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                  "last_seen_at": t_json_opt(r.get(4)?), "occurrences": r.get::<_, i64>(5)?,
                  "application": r.get::<_, String>(6)?, "environment": r.get::<_, String>(7)?}))
    })?
    .collect()
}

/// DashboardController#environment_row's sparkline and error rate: the
/// last 24 hours of request rollups, 24 hourly buckets (Aggregations.
/// sparkline with coarsen 1). Read-only; a missing file reads as empty.
fn request_sparkline(tenants: &std::path::Path, slug: &str) -> (Vec<i64>, f64) {
    let now = crate::time::now_micros();
    let from = now - 24 * 3_600_000_000;
    let base = from.div_euclid(3_600_000_000) * 3_600_000_000;
    let mut buckets = vec![0i64; 24];
    let (mut count, mut errors) = (0i64, 0i64);
    // Rollups live in the single file, or in a per-day tenant's meta.
    let path = match crate::storage::Layout::detect(tenants, slug) {
        Some(crate::storage::Layout::Days(dir)) => crate::storage::layout::meta_path(&dir),
        _ => tenants.join(format!("{slug}.sqlite3")),
    };
    let mut read = || -> rusqlite::Result<()> {
        let c = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut s = c.prepare("SELECT bucket, count, error_count FROM rollups WHERE record_type = 'request' AND bucket >= ? AND bucket <= ?")?;
        let rows = s.query_map(params![crate::time::quoted_date_micros(base), crate::time::quoted_date_micros(now)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (b, n, e) = row?;
            let i = (crate::time::parse_db_time(&b).unwrap_or(base) - base) / 3_600_000_000;
            if let Some(slot) = buckets.get_mut(i as usize) {
                *slot += n;
            }
            count += n;
            errors += e;
        }
        Ok(())
    };
    if path.exists() && let Err(e) = read() {
        tracing::warn!(path = %path.display(), error = %e, "sparkline read failed");
    }
    (buckets, if count == 0 { 0.0 } else { errors as f64 / count as f64 })
}

// ---------------------------------------------------------------------------
// Accounts

async fn account_new(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    cx.render("accounts/new", json!({}), None).await
}

async fn account_create(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    guard!(provisioning(&cx));
    let name = p.s("name");
    let user = cx.user_id();
    match cx.state.db(move |c| c.tx(|tx| control::create_account(tx, &name, user))).await {
        Ok(id) => {
            cx.outgoing.set_account(Some(id));
            cx.redirect(Redirect::to("/apps/new").notice("Welcome to Railwatch. Add your first application."))
        }
        Err(e) => match model_errors(e) {
            Ok(v) => cx.redirect(Redirect::to("/accounts/new").errors(v)),
            Err(r) => r,
        },
    }
}

async fn account_switch(cx: Ctx, Path(id): Path<i64>) -> Response {
    guard!(authenticate(&cx));
    let user = cx.user_id();
    match cx.state.db(move |c| c.with(|x| control::membership_role(x, id, user))).await {
        Ok(Some(_)) => {
            cx.outgoing.set_account(Some(id));
            cx.redirect(Redirect::to("/dashboard"))
        }
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// AccountsController#destroy: deletes the account, its applications,
/// environments and their telemetry files (DestroyAccountJob, inline).
async fn account_destroy(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    guard!(authenticate(&cx));
    let user = cx.user_id();
    let found = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let role = control::membership_role(x, id, user)?;
                let name: Option<String> = x.query_row("SELECT name FROM accounts WHERE id = ?", [id], |r| r.get(0)).optional()?;
                Ok((role, name))
            })
        })
        .await;
    let (role, name) = match found {
        Ok((Some(role), Some(name))) => (role, name),
        Ok(_) => return not_found(),
        Err(e) => return internal(e),
    };
    if role != "owner" {
        return StatusCode::FORBIDDEN.into_response();
    }
    if p.s("confirmation") != name {
        return cx.redirect(Redirect::to("/settings/members").error("confirmation", "Enter the account name exactly to confirm deletion."));
    }
    if !destructive_authentication(&cx, &p.s("password_challenge")).await {
        return cx.redirect(Redirect::to("/settings/members").error("password_challenge", "Confirm your identity before deleting this account."));
    }
    let slugs = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let slugs: Vec<String> = tx
                    .prepare("SELECT e.slug FROM environments e JOIN applications a ON a.id = e.application_id WHERE a.account_id = ?")?
                    .query_map([id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let apps: Vec<i64> = tx.prepare("SELECT id FROM applications WHERE account_id = ?")?.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                for app in apps {
                    delete_application_rows(tx, app)?;
                }
                tx.execute("DELETE FROM invitations WHERE account_id = ?", [id])?;
                tx.execute("DELETE FROM api_tokens WHERE account_id = ?", [id])?;
                tx.execute("DELETE FROM memberships WHERE account_id = ?", [id])?;
                tx.execute("DELETE FROM accounts WHERE id = ?", [id])?;
                Ok(slugs)
            })
        })
        .await;
    let slugs = match slugs {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    for slug in slugs {
        destroy_tenant(&cx.state, &slug);
    }
    let next = cx.state.db(move |c| c.with(|x| super::session::find_account(x, user, None))).await.ok().flatten();
    cx.outgoing.set_account(next.as_ref().map(|a| a.id));
    cx.redirect(Redirect::to(if next.is_some() { "/dashboard" } else { "/accounts/new" }).notice(format!("{name} and its telemetry are being deleted")))
}

fn delete_environment_rows(tx: &rusqlite::Connection, env: i64) -> rusqlite::Result<()> {
    for table in ["deploys", "saved_views", "ingest_usage_months", "ingest_followup_receipts"] {
        tx.execute(&format!("DELETE FROM {table} WHERE environment_id = ?"), [env])?;
    }
    tx.execute("DELETE FROM environments WHERE id = ?", [env])?;
    Ok(())
}

fn delete_application_rows(tx: &rusqlite::Connection, app: i64) -> rusqlite::Result<()> {
    let envs: Vec<i64> = tx.prepare("SELECT id FROM environments WHERE application_id = ?")?.query_map([app], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for e in envs {
        delete_environment_rows(tx, e)?;
    }
    tx.execute("UPDATE api_tokens SET application_id = NULL, revoked_at = COALESCE(revoked_at, ?) WHERE application_id = ?", params![now_db(), app])?;
    tx.execute("DELETE FROM applications WHERE id = ?", [app])?;
    Ok(())
}

/// Environment#destroy_telemetry_database: the file and its -wal/-shm, or
/// a per-day tenant's whole directory.
fn destroy_tenant(state: &WebState, slug: &str) {
    if let Some(crate::storage::Layout::Days(dir)) = state.tenants.layout(slug) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(path = %dir.display(), "telemetry directory deleted"),
            Err(e) => tracing::error!(path = %dir.display(), error = %e, "could not delete telemetry directory"),
        }
    }
    let path = state.tenants.path(slug);
    for suffix in ["", "-wal", "-shm"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&p) {
            Ok(()) => tracing::info!(path = %p.display(), "telemetry file deleted"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::error!(path = %p.display(), error = %e, "could not delete telemetry file"),
        }
    }
}

// ---------------------------------------------------------------------------
// Applications

async fn application_new(cx: Ctx) -> Response {
    guard!(require_account(&cx));
    cx.render("applications/new", json!({}), None).await
}

async fn application_create(cx: Ctx, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(provisioning(&cx));
    let name = p.s("name");
    let (slug, prefix) = control::application_defaults(&name, p.presence("issue_prefix").as_deref());
    let repo = p.presence("repository_url");
    let branch = p.presence("default_branch").unwrap_or_else(|| "main".into());
    let env_name = p.presence("environment_name").unwrap_or_else(|| "production".into());
    let created = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let app = control::insert_application(tx, account, name.trim(), &slug, &prefix, repo.as_deref(), &branch)?;
                let (env, token) = control::create_environment(tx, app, &env_name)?;
                Ok((app, env, token))
            })
        })
        .await;
    match created {
        Ok((app, env, token)) => {
            let slug = cx.state.db(move |c| c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get::<_, String>(0)))).await;
            if let Ok(slug) = slug {
                ensure_tenant(&cx.state, &slug);
            }
            cx.redirect(
                Redirect::to(format!("/apps/{app}")).notice("Application created").with("new_token", json!({"environment_id": env, "token": token})),
            )
        }
        Err(e) => match model_errors(e) {
            Ok(v) => cx.redirect(Redirect::to("/apps/new").errors(v)),
            Err(r) => r,
        },
    }
}

/// Environment's after_create_commit: the tenant file exists from the start.
fn ensure_tenant(state: &WebState, slug: &str) {
    if let Err(e) = state.tenants.ensure(slug) {
        tracing::error!(tenant = slug, error = %e, "could not create the telemetry file");
    }
}

struct App {
    id: i64,
    name: String,
    slug: String,
    issue_prefix: String,
    issues_count: i64,
    repository_url: Option<String>,
    default_branch: String,
}

impl App {
    fn props(&self) -> Value {
        json!({"id": self.id, "name": self.name, "slug": self.slug, "issue_prefix": self.issue_prefix, "issues_count": self.issues_count,
               "repository_url": self.repository_url, "default_branch": self.default_branch})
    }
}

async fn find_app(cx: &Ctx, account: i64, id: i64) -> Result<App, Response> {
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.query_row(
                    "SELECT id, name, slug, issue_prefix, issues_count, repository_url, default_branch FROM applications WHERE id = ? AND account_id = ?",
                    params![id, account],
                    |r| {
                        Ok(App {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            slug: r.get(2)?,
                            issue_prefix: r.get(3)?,
                            issues_count: r.get(4)?,
                            repository_url: r.get(5)?,
                            default_branch: r.get(6)?,
                        })
                    },
                )
                .optional()
            })
        })
        .await;
    match r {
        Ok(Some(a)) => Ok(a),
        Ok(None) => Err(not_found()),
        Err(e) => Err(internal(e)),
    }
}

async fn application_show(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_account(&cx));
    let app = guard!(find_app(&cx, account, id).await);
    let envs = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.prepare(
                    "SELECT e.id, e.name, e.slug, e.token_prefix, e.last_seen_at, e.paused_at IS NOT NULL, e.events_this_month,
                            (SELECT count(*) FROM deploys d WHERE d.environment_id = e.id)
                     FROM environments e WHERE e.application_id = ? ORDER BY e.name",
                )?
                .query_map([id], |r| {
                    Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "slug": r.get::<_, String>(2)?,
                              "token_prefix": r.get::<_, String>(3)?, "last_seen_at": t_json_opt(r.get(4)?), "paused": r.get::<_, bool>(5)?,
                              "events_this_month": r.get::<_, i64>(6)?, "deploys_count": r.get::<_, i64>(7)?, "open_issues": 0}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await;
    let envs = match envs {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    // ApplicationsController#new_token: only on the page of its environment.
    let new_token = cx.flash_value("new_token").and_then(|t| {
        let env = t.get("environment_id")?.as_i64()?;
        envs.iter().any(|e| e["id"].as_i64() == Some(env)).then(|| t.get("token").cloned()).flatten()
    });
    cx.render(
        "applications/show",
        json!({"application": app.props(), "environments": envs, "new_token": new_token, "issues_open": 0}),
        None,
    )
    .await
}

async fn application_edit(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_account(&cx));
    let app = guard!(find_app(&cx, account, id).await);
    let auto = cx.current.account.as_ref().and_then(|a| a.auto_resolve_after_days);
    cx.render("applications/edit", json!({"application": app.props(), "auto_resolve_after_days": auto}), None).await
}

async fn application_update(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    let app = guard!(find_app(&cx, account, id).await);
    let name = if p.has("name") { p.s("name") } else { app.name.clone() };
    let prefix = if p.has("issue_prefix") { p.s("issue_prefix") } else { app.issue_prefix.clone() };
    let repo = if p.has("repository_url") { p.presence("repository_url") } else { app.repository_url.clone() };
    let branch = if p.has("default_branch") { p.s("default_branch") } else { app.default_branch.clone() };
    let auto = p.has("auto_resolve_after_days").then(|| p.i64("auto_resolve_after_days"));
    let mut errors = control::validate_application(&name, &app.slug, &prefix, repo.as_deref());
    if branch.trim().is_empty() {
        errors.push(("default_branch".into(), "can't be blank".into()));
    }
    if !errors.is_empty() {
        return cx.redirect(Redirect::to(format!("/apps/{id}/edit")).errors(errors));
    }
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                if let Some(days) = auto {
                    tx.execute("UPDATE accounts SET auto_resolve_after_days = ? WHERE id = ?", params![days, account])?;
                }
                tx.execute(
                    "UPDATE applications SET name = ?, issue_prefix = ?, repository_url = ?, default_branch = ?, updated_at = ? WHERE id = ?",
                    params![name, prefix, repo, branch, now_db(), id],
                )?;
                control::touch_account(tx, account)?;
                Ok(())
            })
        })
        .await;
    match r {
        Ok(()) => cx.redirect(Redirect::to(format!("/apps/{id}")).notice("Application updated")),
        Err(e) => internal(e),
    }
}

async fn application_destroy(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(find_app(&cx, account, id).await);
    let slugs = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let slugs: Vec<String> =
                    tx.prepare("SELECT slug FROM environments WHERE application_id = ?")?.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                delete_application_rows(tx, id)?;
                control::touch_account(tx, account)?;
                Ok(slugs)
            })
        })
        .await;
    match slugs {
        Ok(slugs) => {
            for s in slugs {
                destroy_tenant(&cx.state, &s);
            }
            cx.redirect(Redirect::to("/dashboard").notice("Application deleted"))
        }
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Environments (settings; the telemetry pages are env_pages.rs)

async fn environment_new(cx: Ctx, Path(app): Path<i64>) -> Response {
    let account = guard!(require_account(&cx));
    let a = guard!(find_app(&cx, account, app).await);
    cx.render("environments/new", json!({"application": {"id": a.id, "name": a.name}}), None).await
}

async fn environment_create(cx: Ctx, Path(app): Path<i64>, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(provisioning(&cx));
    guard!(find_app(&cx, account, app).await);
    let name = p.s("name");
    match cx.state.db(move |c| c.tx(|tx| control::create_environment(tx, app, &name))).await {
        Ok((env, token)) => {
            let slug = cx.state.db(move |c| c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get::<_, String>(0)))).await;
            if let Ok(slug) = slug {
                ensure_tenant(&cx.state, &slug);
            }
            cx.redirect(
                Redirect::to(format!("/apps/{app}"))
                    .notice("Environment created. Copy the token now; it is shown once.")
                    .with("new_token", json!({"environment_id": env, "token": token})),
            )
        }
        Err(e) => match model_errors(e) {
            Ok(v) => cx.redirect(Redirect::to(format!("/apps/{app}/envs/new")).errors(v)),
            Err(r) => r,
        },
    }
}

/// The environment of an application of the current account.
async fn find_env(cx: &Ctx, account: i64, app: i64, id: i64) -> Result<(String, bool), Response> {
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.query_row(
                    "SELECT e.slug, e.paused_at IS NOT NULL FROM environments e JOIN applications a ON a.id = e.application_id
                     WHERE e.id = ? AND e.application_id = ? AND a.account_id = ?",
                    params![id, app, account],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
            })
        })
        .await;
    match r {
        Ok(Some(e)) => Ok(e),
        Ok(None) => Err(not_found()),
        Err(e) => Err(internal(e)),
    }
}

async fn environment_edit(cx: Ctx, Path((app, id)): Path<(i64, i64)>) -> Response {
    let account = guard!(require_account(&cx));
    let a = guard!(find_app(&cx, account, app).await);
    let (slug, _) = guard!(find_env(&cx, account, app, id).await);
    let env = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.query_row(
                    "SELECT id, name, slug, token_prefix, paused_at, expected_servers, kamal_config, crash_free_threshold FROM environments WHERE id = ?",
                    [id],
                    |r| {
                        let parse = |s: String| serde_json::from_str::<Value>(&s).unwrap_or(Value::Null);
                        Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "slug": r.get::<_, String>(2)?,
                                  "token_prefix": r.get::<_, String>(3)?, "paused_at": t_json_opt(r.get(4)?),
                                  "expected_servers": parse(r.get(5)?), "kamal_config": parse(r.get(6)?),
                                  "crash_free_threshold": r.get::<_, Option<f64>>(7)?, "application_id": app}))
                    },
                )
            })
        })
        .await;
    let env = match env {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    let expected: Vec<String> = env["expected_servers"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let silent = silent_servers(&cx.state, &slug, &expected);
    cx.render("environments/edit", json!({"application": {"id": a.id, "name": a.name}, "environment": env, "silent_servers": silent}), None)
        .await
}

/// Environment#silent_servers: expected servers with no health sample or
/// execution in the last 10 minutes.
fn silent_servers(state: &WebState, slug: &str, expected: &[String]) -> Vec<String> {
    if expected.is_empty() {
        return vec![];
    }
    let since = crate::time::quoted_date_micros(crate::time::now_micros() - 600_000_000);
    let seen = || -> rusqlite::Result<std::collections::HashSet<String>> {
        if state.tenants.layout(slug).is_none() {
            return Ok(Default::default());
        }
        let c = state.readers.get(slug)?;
        let mut out = std::collections::HashSet::new();
        let mut s = c.prepare("SELECT DISTINCT server FROM health_samples WHERE sampled_at >= ?")?;
        for v in s.query_map([&since], |r| r.get::<_, Option<String>>(0))? {
            out.extend(v?);
        }
        for kind in ["request", "job_attempt", "scheduled_task", "command", "channel_action"] {
            let mut s = c.prepare_cached("SELECT DISTINCT server FROM executions WHERE kind = ? AND occurred_at >= ?")?;
            for v in s.query_map(params![kind, &since], |r| r.get::<_, Option<String>>(0))? {
                out.extend(v?);
            }
        }
        Ok(out)
    };
    let seen = seen().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "silent servers: telemetry read failed");
        Default::default()
    });
    expected.iter().filter(|s| !seen.contains(*s)).cloned().collect()
}

async fn environment_update(cx: Ctx, Path((app, id)): Path<(i64, i64)>, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(find_env(&cx, account, app, id).await);
    let name = p.has("name").then(|| p.s("name"));
    // normalize_expected_servers: a textarea string or an array.
    let servers = p.get("expected_servers").map(|v| {
        let list: Vec<String> = match v {
            Value::Array(a) => a.iter().map(|s| s.as_str().map_or_else(|| s.to_string(), str::to_string)).collect(),
            other => other.as_str().unwrap_or("").split(|c: char| c.is_whitespace() || c == ',').map(str::to_string).collect(),
        };
        let mut list: Vec<String> = list.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        list.sort();
        list.dedup();
        serde_json::to_string(&list).expect("json")
    });
    let threshold = p.has("crash_free_threshold").then(|| p.s("crash_free_threshold").trim().parse::<f64>().ok());
    // Rails ignores a failed update here (no error shown); an invalid name
    // is simply not saved.
    let name = name.filter(|n| control::validate_environment_name(n).is_none());
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                if let Some(n) = name {
                    let taken: bool = tx
                        .query_row("SELECT 1 FROM environments WHERE application_id = ? AND name = ? AND id != ?", params![app, n, id], |_| Ok(true))
                        .optional()?
                        .unwrap_or(false);
                    if !taken {
                        tx.execute("UPDATE environments SET name = ? WHERE id = ?", params![n, id])?;
                    }
                }
                if let Some(s) = servers {
                    tx.execute("UPDATE environments SET expected_servers = ? WHERE id = ?", params![s, id])?;
                }
                if let Some(t) = threshold {
                    tx.execute("UPDATE environments SET crash_free_threshold = ? WHERE id = ?", params![t, id])?;
                }
                tx.execute("UPDATE environments SET updated_at = ? WHERE id = ?", params![now_db(), id])?;
                control::touch_application(tx, app)?;
                Ok(())
            })
        })
        .await;
    match r {
        Ok(()) => cx.redirect(Redirect::to(format!("/apps/{app}")).notice("Environment updated")),
        Err(e) => internal(e),
    }
}

async fn environment_destroy(cx: Ctx, Path((app, id)): Path<(i64, i64)>) -> Response {
    let account = guard!(require_admin(&cx).await);
    let (slug, _) = guard!(find_env(&cx, account, app, id).await);
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                delete_environment_rows(tx, id)?;
                control::touch_application(tx, app)?;
                Ok(())
            })
        })
        .await;
    match r {
        Ok(()) => {
            destroy_tenant(&cx.state, &slug);
            cx.redirect(Redirect::to(format!("/apps/{app}")).notice("Environment and its telemetry deleted"))
        }
        Err(e) => internal(e),
    }
}

async fn environment_rotate_token(cx: Ctx, Path((app, id)): Path<(i64, i64)>) -> Response {
    let account = guard!(require_admin(&cx).await);
    guard!(find_env(&cx, account, app, id).await);
    match cx.state.db(move |c| c.with(|x| control::rotate_token(x, id))).await {
        Ok(token) => cx.redirect(
            Redirect::to(format!("/apps/{app}"))
                .notice("Token rotated. Update RAILWATCH_TOKEN in the app.")
                .with("new_token", json!({"environment_id": id, "token": token})),
        ),
        Err(e) => internal(e),
    }
}

async fn environment_pause(cx: Ctx, Path((app, id)): Path<(i64, i64)>) -> Response {
    let account = guard!(require_admin(&cx).await);
    let (_, paused) = guard!(find_env(&cx, account, app, id).await);
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let at = if paused { None } else { Some(now_db()) };
                tx.execute("UPDATE environments SET paused_at = ?, updated_at = ? WHERE id = ?", params![at, now_db(), id])?;
                control::touch_application(tx, app)?;
                Ok(())
            })
        })
        .await;
    match r {
        Ok(()) => cx.redirect(Redirect::to(format!("/apps/{app}")).notice(if paused { "Ingest resumed" } else { "Ingest paused" })),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Settings

async fn appearance(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    cx.render("settings/appearance", json!({}), None).await
}

const EDITORS: [&str; 7] = ["github", "gitlab", "vscode", "cursor", "zed", "rubymine", "textmate"];
const EXPIRY_CHOICES: [&str; 5] = ["0", "7", "30", "90", "365"];

async fn profile_show(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    let user = cx.current.user.clone().expect("authenticated");
    let uid = user.id;
    let data = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let tokens: Vec<Value> = x
                    .prepare(
                        "SELECT t.id, t.name, t.scopes, t.expires_at, t.last_used_at, t.revoked_at, t.created_at,
                                ap.name, apa.name, a.name
                         FROM api_tokens t LEFT JOIN applications ap ON ap.id = t.application_id LEFT JOIN accounts apa ON apa.id = ap.account_id
                         LEFT JOIN accounts a ON a.id = t.account_id WHERE t.user_id = ? ORDER BY t.created_at DESC",
                    )?
                    .query_map([uid], |r| {
                        let restriction = match (r.get::<_, Option<String>>(7)?, r.get::<_, Option<String>>(8)?, r.get::<_, Option<String>>(9)?) {
                            (Some(app), Some(acct), _) => format!("Application: {acct} / {app}"),
                            (_, _, Some(acct)) => format!("Account: {acct}"),
                            _ => "All my accounts and applications".to_string(),
                        };
                        Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?,
                                  "scopes": serde_json::from_str::<Value>(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
                                  "restriction": restriction, "expires_at": t_json_opt(r.get(3)?), "last_used_at": t_json_opt(r.get(4)?),
                                  "revoked_at": t_json_opt(r.get(5)?), "created_at": t_json(&r.get::<_, String>(6)?)}))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut targets = vec![json!({"value": "", "label": "All my accounts and applications"})];
                let accounts: Vec<(i64, String)> = x
                    .prepare("SELECT a.id, a.name FROM accounts a JOIN memberships m ON m.account_id = a.id WHERE m.user_id = ? ORDER BY a.name")?
                    .query_map([uid], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (id, name) in &accounts {
                    targets.push(json!({"value": format!("account:{id}"), "label": format!("Account: {name}")}));
                    let apps: Vec<(i64, String)> =
                        x.prepare("SELECT id, name FROM applications WHERE account_id = ? ORDER BY name")?.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
                    for (aid, aname) in apps {
                        targets.push(json!({"value": format!("application:{aid}"), "label": format!("Application: {name} / {aname}")}));
                    }
                }
                let blocking: Vec<Value> = sole_owned_accounts(x, uid)?.into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect();
                Ok((tokens, targets, blocking))
            })
        })
        .await;
    let (tokens, targets, blocking) = match data {
        Ok(d) => d,
        Err(e) => return internal(e),
    };
    cx.render(
        "settings/profiles/show",
        json!({"mcp_token": cx.flash_value("generated_api_token"), "api_tokens": tokens, "token_targets": targets,
               "token_scopes": ["api", "mcp", "read", "write"], "weekly_digest": user.weekly_digest,
               "mcp_url": format!("{}/mcp", cx.state.config.url), "deletion": {"blocking_accounts": blocking}}),
        None,
    )
    .await
}

async fn profile_update(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    let user = cx.current.user.clone().expect("authenticated");
    let name = if p.has("name") { p.s("name") } else { user.name.clone() };
    let editor = if p.has("editor") { p.s("editor") } else { user.editor.clone() };
    let root = if p.has("editor_root") { p.presence("editor_root") } else { user.editor_root.clone() };
    let digest = if p.has("weekly_digest") { p.bool("weekly_digest").unwrap_or(false) } else { user.weekly_digest };
    let mut errors = Vec::new();
    if name.trim().is_empty() {
        errors.push(("name".to_string(), "can't be blank".to_string()));
    }
    if !EDITORS.contains(&editor.as_str()) {
        errors.push(("editor".to_string(), "is not included in the list".to_string()));
    }
    if !errors.is_empty() {
        return cx.redirect(Redirect::to("/settings/profile").errors(errors));
    }
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.execute(
                    "UPDATE users SET name = ?, editor = ?, editor_root = ?, weekly_digest = ?, updated_at = ? WHERE id = ?",
                    params![name, editor, root, digest, now_db(), user.id],
                )
            })
        })
        .await;
    match r {
        Ok(_) => cx.redirect(Redirect::to("/settings/profile").notice("Your profile has been updated")),
        Err(e) => internal(e),
    }
}

async fn profile_generate_token(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    let uid = cx.user_id();
    let restriction = p.s("restriction");
    let expiry = p.presence("expires_in_days");
    if expiry.as_deref().is_some_and(|d| !EXPIRY_CHOICES.contains(&d)) {
        return cx.redirect(Redirect::to("/settings/profile").error("expires_in_days", "is not supported"));
    }
    let expires_at = expiry
        .and_then(|d| d.parse::<i64>().ok())
        .filter(|d| *d > 0)
        .map(|d| crate::time::quoted_date_micros(crate::time::now_micros() + d * 86_400_000_000));
    let name = p.presence("name").unwrap_or_else(|| "API & MCP token".into());
    let scopes = if p.has("scopes_present") { p.strings("scopes") } else { ["api", "mcp", "read", "write"].map(String::from).to_vec() };
    let replace = p.i64("replace_token_id");
    enum Out {
        Bad,
        Ok(String, bool),
        Invalid(Vec<(String, String)>),
    }
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let (kind, id) = restriction.split_once(':').map_or((restriction.as_str(), ""), |(k, i)| (k, i));
                let id: Option<i64> = id.parse().ok();
                let (account, app) = match kind {
                    "" => (None, None),
                    "account" => match id.filter(|a| control::membership_role(tx, *a, uid).ok().flatten().is_some()) {
                        Some(a) => (Some(a), None),
                        None => return Ok(Out::Bad),
                    },
                    "application" => {
                        let ok: Option<i64> = match id {
                            Some(a) => tx
                                .query_row(
                                    "SELECT ap.id FROM applications ap JOIN memberships m ON m.account_id = ap.account_id WHERE ap.id = ? AND m.user_id = ?",
                                    params![a, uid],
                                    |r| r.get(0),
                                )
                                .optional()?,
                            None => None,
                        };
                        match ok {
                            Some(a) => (None, Some(a)),
                            None => return Ok(Out::Bad),
                        }
                    }
                    _ => return Ok(Out::Bad),
                };
                if let Some(rid) = replace {
                    let mine: bool = tx.query_row("SELECT 1 FROM api_tokens WHERE id = ? AND user_id = ?", params![rid, uid], |_| Ok(true)).optional()?.unwrap_or(false);
                    if !mine {
                        return Ok(Out::Bad);
                    }
                }
                let raw = match control::issue_api_token(tx, uid, &name, &scopes, account, app, expires_at) {
                    Ok((_, raw)) => raw,
                    Err(ModelError::Invalid(e)) => return Ok(Out::Invalid(e)),
                    Err(e) => return Err(e),
                };
                if let Some(rid) = replace {
                    tx.execute("UPDATE api_tokens SET revoked_at = ?1, updated_at = ?1 WHERE id = ?2", params![now_db(), rid])?;
                }
                Ok(Out::Ok(raw, replace.is_some()))
            })
        })
        .await;
    match r {
        Ok(Out::Ok(raw, replaced)) => cx.redirect(
            Redirect::to("/settings/profile")
                .notice(if replaced {
                    "API token regenerated. The previous token is revoked. Copy the new one now; it is shown once."
                } else {
                    "API token generated. Copy it now; it is shown once."
                })
                .with("generated_api_token", Value::String(raw)),
        ),
        Ok(Out::Bad) => cx.redirect(Redirect::to("/settings/profile").error("restriction", "is not accessible")),
        Ok(Out::Invalid(e)) => cx.redirect(Redirect::to("/settings/profile").errors(e)),
        Err(e) => internal(e),
    }
}

async fn profile_revoke_token(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    let uid = cx.user_id();
    let Some(id) = p.i64("token_id") else { return not_found() };
    match cx.state.db(move |c| c.with(|x| x.execute("UPDATE api_tokens SET revoked_at = ?1, updated_at = ?1 WHERE id = ?2 AND user_id = ?3", params![now_db(), id, uid]))).await {
        Ok(0) => not_found(),
        Ok(_) => cx.redirect(Redirect::to("/settings/profile").notice("API token revoked.")),
        Err(e) => internal(e),
    }
}

async fn password_show(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    cx.render("settings/passwords/show", json!({}), None).await
}

async fn password_update(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    let user = cx.current.user.clone().expect("authenticated");
    let (password, confirmation, challenge) = (p.s("password"), p.s("password_confirmation"), p.s("password_challenge"));
    let mut errors = Vec::new();
    if !control::verify_password(&challenge, &user.password_digest) {
        errors.push(("password_challenge".to_string(), "is invalid".to_string()));
    }
    if confirmation != password {
        errors.push(("password_confirmation".to_string(), "doesn't match Password".to_string()));
    }
    errors.extend(control::password_errors(&password));
    if !errors.is_empty() {
        return cx.redirect(Redirect::to("/settings/password").errors(errors));
    }
    let keep = cx.current.session.as_ref().map(|s| s.id);
    match cx.state.db(move |c| c.with(|x| control::set_password(x, user.id, &password, keep))).await {
        Ok(()) => cx.redirect(Redirect::to("/settings/password").notice("Your password has been changed")),
        Err(e) => internal(e),
    }
}

async fn email_show(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    cx.render("settings/emails/show", json!({}), None).await
}

async fn email_update(cx: Ctx, p: Params) -> Response {
    guard!(authenticate(&cx));
    let user = cx.current.user.clone().expect("authenticated");
    let email = control::normalize_email(&p.s("email"));
    let mut errors = Vec::new();
    if !control::verify_password(&p.s("password_challenge"), &user.password_digest) {
        errors.push(("password_challenge".to_string(), "is invalid".to_string()));
    }
    if email.is_empty() {
        errors.push(("email".to_string(), "can't be blank".to_string()));
    } else if !control::valid_email(&email) {
        errors.push(("email".to_string(), "is invalid".to_string()));
    }
    if !errors.is_empty() {
        return cx.redirect(Redirect::to("/settings/email").errors(errors));
    }
    if email == user.email {
        return cx.redirect(Redirect::to("/settings/email"));
    }
    let uid = user.id;
    let r = cx
        .state
        .db(move |c| c.with(|x| x.execute("UPDATE users SET email = ?, verified = 0, updated_at = ? WHERE id = ?", params![email, now_db(), uid])))
        .await;
    match r {
        Ok(_) => {
            send_verification(&cx, uid).await;
            cx.redirect(Redirect::to("/settings/email").notice("Your email has been changed"))
        }
        Err(rusqlite::Error::SqliteFailure(f, _)) if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => {
            cx.redirect(Redirect::to("/settings/email").error("email", "has already been taken"))
        }
        Err(e) => internal(e),
    }
}

async fn sessions_index(cx: Ctx) -> Response {
    guard!(authenticate(&cx));
    let uid = cx.user_id();
    let sessions = cx
        .state
        .db(move |c| {
            c.with(|x| {
                x.prepare("SELECT id, user_agent, ip_address, created_at FROM sessions WHERE user_id = ? ORDER BY created_at DESC")?
                    .query_map([uid], |r| {
                        Ok(json!({"id": r.get::<_, i64>(0)?, "user_agent": r.get::<_, Option<String>>(1)?,
                                  "ip_address": r.get::<_, Option<String>>(2)?, "created_at": t_json(&r.get::<_, String>(3)?)}))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
        })
        .await;
    match sessions {
        Ok(s) => cx.render("settings/sessions/index", json!({"sessions": s}), None).await,
        Err(e) => internal(e),
    }
}

async fn members_index(cx: Ctx) -> Response {
    let account = guard!(require_account(&cx));
    let uid = cx.user_id();
    let data = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let members: Vec<Value> = x
                    .prepare("SELECT m.id, u.name, u.email, m.role, m.user_id FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = ? ORDER BY m.id")?
                    .query_map([account], |r| {
                        Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "email": r.get::<_, String>(2)?,
                                  "role": r.get::<_, String>(3)?, "user_id": r.get::<_, i64>(4)?}))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                let invitations: Vec<Value> = x
                    .prepare("SELECT id, email, role, expires_at FROM invitations WHERE account_id = ? AND accepted_at IS NULL AND expires_at > ? ORDER BY created_at")?
                    .query_map(params![account, now_db()], |r| {
                        Ok(json!({"id": r.get::<_, i64>(0)?, "email": r.get::<_, String>(1)?, "role": r.get::<_, String>(2)?,
                                  "expires_at": t_json(&r.get::<_, String>(3)?)}))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                let owner = control::membership_role(x, account, uid)?.as_deref() == Some("owner");
                Ok((members, invitations, owner))
            })
        })
        .await;
    match data {
        Ok((members, invitations, owner)) => {
            cx.render(
                "settings/members/index",
                json!({"members": members, "invitations": invitations, "roles": control::ROLES,
                       "ownership": {"current_user_id": uid, "can_transfer": owner, "can_delete_account": owner}}),
                None,
            )
            .await
        }
        Err(e) => internal(e),
    }
}

async fn members_create(cx: Ctx, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    let email = control::normalize_email(&p.s("email"));
    let role = Some(p.s("role")).filter(|r| r == "admin" || r == "member").unwrap_or_else(|| "member".into());
    let inviter = cx.current.user.clone().expect("authenticated");
    let account_name = cx.current.account.as_ref().map(|a| a.name.clone()).unwrap_or_default();
    enum Out {
        Added(String),
        Invited(control::NewInvitation),
        Invalid(Vec<(String, String)>),
    }
    let e2 = email.clone();
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                if let Some(u) = control::find_user_by_email(tx, &e2)?.filter(|u| u.verified) {
                    control::insert_membership(tx, account, u.id, &role)?;
                    return Ok(Out::Added(u.name));
                }
                match control::create_invitation(tx, account, inviter.id, &e2, &role) {
                    Ok(inv) => Ok(Out::Invited(inv)),
                    Err(ModelError::Invalid(e)) => Ok(Out::Invalid(e)),
                    Err(e) => Err(e),
                }
            })
        })
        .await;
    match r {
        Ok(Out::Added(name)) => cx.redirect(Redirect::to("/settings/members").notice(format!("{name} added"))),
        Ok(Out::Invited(inv)) => {
            let user = cx.current.user.as_ref().map(|u| u.name.clone()).unwrap_or_default();
            mailer::deliver_later(cx.state.mailer.clone(), mailer::invite(&cx.state.config.url, &email, &user, &account_name, &inv.token));
            cx.redirect(Redirect::to("/settings/members").notice(format!("Invitation sent to {email}")))
        }
        Ok(Out::Invalid(e)) => cx.redirect(Redirect::to("/settings/members").errors(e)),
        Err(e) => internal(e),
    }
}

async fn members_update(cx: Ctx, Path(id): Path<i64>, p: Params) -> Response {
    let account = guard!(require_admin(&cx).await);
    let role = p.s("role");
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let current: Option<String> = tx.query_row("SELECT role FROM memberships WHERE id = ? AND account_id = ?", params![id, account], |r| r.get(0)).optional()?;
                match current.as_deref() {
                    None => Ok(None),
                    Some("owner") => Ok(Some(true)),
                    Some(_) => {
                        if role == "admin" || role == "member" {
                            tx.execute("UPDATE memberships SET role = ?, updated_at = ? WHERE id = ?", params![role, now_db(), id])?;
                        }
                        Ok(Some(false))
                    }
                }
            })
        })
        .await;
    match r {
        Ok(None) => not_found(),
        Ok(Some(true)) => cx.redirect(Redirect::to("/settings/members").alert("Owners keep the owner role")),
        Ok(Some(false)) => cx.redirect(Redirect::to("/settings/members").notice("Role updated")),
        Err(e) => internal(e),
    }
}

async fn members_destroy(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let role: Option<String> = tx.query_row("SELECT role FROM memberships WHERE id = ? AND account_id = ?", params![id, account], |r| r.get(0)).optional()?;
                match role.as_deref() {
                    None => Ok(None),
                    Some("owner") => Ok(Some(true)),
                    Some(_) => {
                        tx.execute("DELETE FROM memberships WHERE id = ?", [id])?;
                        Ok(Some(false))
                    }
                }
            })
        })
        .await;
    match r {
        Ok(None) => not_found(),
        Ok(Some(true)) => cx.redirect(Redirect::to("/settings/members").alert("Owners cannot be removed")),
        Ok(Some(false)) => cx.redirect(Redirect::to("/settings/members").notice("Member removed")),
        Err(e) => internal(e),
    }
}

/// Account#transfer_ownership!: the new owner becomes owner, the old one
/// admin, in one transaction.
async fn members_transfer(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_account(&cx));
    let uid = cx.user_id();
    let r = cx
        .state
        .db(move |c| {
            c.tx(|tx| {
                let target: Option<(i64, String)> = tx
                    .query_row("SELECT m.user_id, u.name FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.id = ? AND m.account_id = ?", params![id, account], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .optional()?;
                let Some((target_user, name)) = target else { return Ok(Err(None)) };
                if control::membership_role(tx, account, uid)?.as_deref() != Some("owner") {
                    return Ok(Err(Some("Only an owner can transfer ownership")));
                }
                if target_user == uid {
                    return Ok(Err(Some("Choose another account member")));
                }
                let now = now_db();
                tx.execute("UPDATE memberships SET role = 'owner', updated_at = ? WHERE id = ?", params![now, id])?;
                tx.execute("UPDATE memberships SET role = 'admin', updated_at = ? WHERE account_id = ? AND user_id = ?", params![now, account, uid])?;
                Ok(Ok(name))
            })
        })
        .await;
    match r {
        Ok(Ok(name)) => cx.redirect(Redirect::to("/settings/members").notice(format!("Ownership transferred to {name}"))),
        Ok(Err(None)) => not_found(),
        Ok(Err(Some(m))) => cx.redirect(Redirect::to("/settings/members").alert(m)),
        Err(e) => internal(e),
    }
}

async fn invitation_destroy(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    match cx.state.db(move |c| c.with(|x| x.execute("DELETE FROM invitations WHERE id = ? AND account_id = ?", params![id, account]))).await {
        Ok(0) => not_found(),
        Ok(_) => cx.redirect(Redirect::to("/settings/members").notice("Invitation withdrawn")),
        Err(e) => internal(e),
    }
}

/// Invitation#resend!: a new token, 7 more days, the email again.
async fn invitation_resend(cx: Ctx, Path(id): Path<i64>) -> Response {
    let account = guard!(require_admin(&cx).await);
    let token = control::base58(32);
    let t = token.clone();
    let r = cx
        .state
        .db(move |c| {
            c.with(|x| {
                let email: Option<String> = x
                    .query_row("SELECT email FROM invitations WHERE id = ? AND account_id = ? AND accepted_at IS NULL AND expires_at > ?", params![id, account, now_db()], |r| r.get(0))
                    .optional()?;
                if email.is_some() {
                    let expires = crate::time::quoted_date_micros(crate::time::now_micros() + 7 * 86_400_000_000);
                    x.execute("UPDATE invitations SET token_digest = ?, expires_at = ?, updated_at = ? WHERE id = ?", params![control::digest(&t), expires, now_db(), id])?;
                }
                Ok(email)
            })
        })
        .await;
    match r {
        Ok(Some(email)) => {
            let inviter = cx.current.user.as_ref().map(|u| u.name.clone()).unwrap_or_default();
            let account_name = cx.current.account.as_ref().map(|a| a.name.clone()).unwrap_or_default();
            mailer::deliver_later(cx.state.mailer.clone(), mailer::invite(&cx.state.config.url, &email, &inviter, &account_name, &token));
            cx.redirect(Redirect::to("/settings/members").notice("Invitation resent"))
        }
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Control, create_account, create_environment, insert_application, insert_user};

    #[test]
    fn dashboard_counts_and_lists_only_open_issues_of_the_account() {
        let dir = std::env::temp_dir().join(format!("rw-dash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let user = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", "correct horse battery", true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", user)).unwrap();
        let other = c.tx(|tx| create_account(tx, "Other", user)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let app2 = c.tx(|tx| insert_application(tx, other, "Else", "else", "ELSE", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let (env2, _) = c.tx(|tx| create_environment(tx, app2, "production")).unwrap();
        c.with(|x| {
            for (a, e, n, status, seen) in [(app, env, 1, "open", "03:01"), (app, env, 2, "open", "03:05"), (app, env, 3, "resolved", "03:09"), (app2, env2, 1, "open", "03:09")] {
                x.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, status, group_hash, first_seen_at, last_seen_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'Boom', ?4, 'g' || ?3, '2026-10-14 03:00:00', '2026-10-14 ' || ?5 || ':00', '2026-10-14 03:00:00', '2026-10-14 03:00:00')",
                    rusqlite::params![a, e, n, status, seen],
                )?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(c.with(|x| open_issue_count(x, env)).unwrap(), 2);
        let recent = c.with(|x| dashboard_recent_issues(x, account)).unwrap();
        let keys: Vec<&str> = recent.iter().map(|i| i["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["SHOP-2", "SHOP-1"], "open only, this account only, latest first");
        assert_eq!((recent[0]["application"].as_str(), recent[0]["environment"].as_str()), (Some("Shop"), Some("production")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
