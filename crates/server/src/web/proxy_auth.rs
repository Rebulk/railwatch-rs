//! Trusted proxy authentication (`trusted_proxy_secret_file`, off by
//! default): a reverse proxy that has already authenticated the browser (the
//! railwatch-server gem's mount inside a Rails app, behind that app's own
//! login) says who it is with one signed header per request:
//!
//!   X-Railwatch-Proxy-Auth: v1.<base64url(JSON claims)>.<base64url(HMAC-SHA256(secret, "v1." + claims))>
//!
//! with claims `{"email", "name", "role": "admin"|"member", "exp": <unix s>}`.
//! The security model (docs/SELF-HOST.md, "Running behind an app"):
//!
//! - Off unless the file is configured, readable and at least 32 bytes
//!   (config::read_proxy_secret refuses to start otherwise). Off, the header
//!   is ignored outright: a forged one on a normal install does nothing.
//! - On, `layer` checks every request before any route sees it: a valid
//!   header becomes `Proxied` for the session middleware and the cable
//!   handshake; a header present but invalid is a 401, never a quiet fall
//!   back to the cookie session. The header is removed from the request
//!   either way, so no handler, log line or outbound call can see it.
//! - The MAC is compared in constant time (`Mac::verify_slice`).
//! - `exp` must lie in the future and at most MAX_TTL_SECS ahead. There is
//!   no nonce: a header captured in transit can be replayed until it
//!   expires, the same exposure as the session cookie riding alongside it.
//!   The proxy talks to the server over loopback.
//! - The role is `admin` or `member`. A proxied user never becomes, or
//!   stops being, an owner: ownership moves only by transfer in Settings.
//! - A valid header makes the request that user's, whatever cookies it
//!   carries: created on first sight (verified, with a random password
//!   nobody knows), a member of the proxy's account only, with one
//!   long-lived session row reused on every request. CSRF is unchanged:
//!   a state-changing request still needs the session's token.
//! - The proxy's account is bound in the control database (`trusted_proxy`,
//!   control_schema): the account `railwatch setup` created. A proxied
//!   request is pinned to it (session::Current::proxied): it cannot switch
//!   to, create or mint tokens for another account, even when the email
//!   is also a member elsewhere, and it acts with the claimed role, never
//!   as an owner. With no bound account (deleted, or none yet) proxied
//!   requests are refused; nobody is joined to some other account.
//! - /mcp and /api/v1 take the same claims in place of a token
//!   (mcp::Token::from_request): that user's dashboard access, refused
//!   when the browser marks the request cross-origin (no CSRF token there).
//!
//! The proxy must drop any copy of the header the browser sent.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::control::{self, MAX_NAME_CHARS, Session, User};

pub const HEADER: &str = "x-railwatch-proxy-auth";
/// The longest a signed header may be valid for.
pub const MAX_TTL_SECS: i64 = 300;
/// A header longer than this is refused before it is decoded.
const MAX_HEADER_BYTES: usize = 4096;
/// The user agent of the session row a proxied user is given.
pub const SESSION_AGENT: &str = "railwatch trusted proxy";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    pub email: String,
    pub name: String,
    pub role: String,
    pub exp: i64,
}

/// A request's verified claims, put in its extensions by `layer`.
#[derive(Debug, Clone)]
pub struct Proxied(pub Claims);

fn mac(secret: &[u8], signed: &[u8]) -> Hmac<sha2::Sha256> {
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret).expect("hmac key");
    m.update(signed);
    m
}

/// The header value for `claims` (what the proxy sends; tests and docs).
pub fn sign(secret: &[u8], claims: &serde_json::Value) -> String {
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let signed = format!("v1.{payload}");
    let sig = URL_SAFE_NO_PAD.encode(mac(secret, signed.as_bytes()).finalize().into_bytes());
    format!("{signed}.{sig}")
}

/// The claims of a header value signed with `secret`, valid at `now` (unix s).
pub fn verify(secret: &[u8], value: &str, now: i64) -> Result<Claims, &'static str> {
    if value.len() > MAX_HEADER_BYTES {
        return Err("too long");
    }
    let (signed, sig) = value.rsplit_once('.').ok_or("malformed")?;
    let payload = signed.strip_prefix("v1.").ok_or("unknown version")?;
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| "malformed signature")?;
    // Constant time: hmac's verify_slice compares with subtle.
    mac(secret, signed.as_bytes()).verify_slice(&sig).map_err(|_| "bad signature")?;
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| "malformed claims")?;
    let c: Claims = serde_json::from_slice(&json).map_err(|_| "malformed claims")?;
    if c.exp <= now {
        return Err("expired");
    }
    if c.exp > now + MAX_TTL_SECS {
        return Err("exp too far ahead");
    }
    if c.email.len() > 254 || !control::valid_email(&control::normalize_email(&c.email)) {
        return Err("invalid email");
    }
    if c.name.chars().count() > MAX_NAME_CHARS || c.name.chars().any(char::is_control) {
        return Err("invalid name");
    }
    if !["admin", "member"].contains(&c.role.as_str()) {
        return Err("role must be admin or member");
    }
    Ok(c)
}

/// Around every route of the web router. With proxy auth off this only
/// removes the header; on, it verifies it first (see the module docs).
pub async fn layer(state: axum::extract::State<std::sync::Arc<super::WebState>>, mut req: Request, next: Next) -> Response {
    let value = req.headers_mut().remove(HEADER);
    let (Some(secret), Some(value)) = (state.config.trusted_proxy_secret.as_deref(), value) else {
        return next.run(req).await;
    };
    let now = crate::time::now_micros().div_euclid(1_000_000);
    match value.to_str().map_err(|_| "not ASCII").and_then(|v| verify(secret, v, now)) {
        Ok(c) => {
            req.extensions_mut().insert(Proxied(c));
            next.run(req).await
        }
        Err(why) => {
            tracing::warn!(why, path = req.uri().path(), "trusted proxy header refused");
            (StatusCode::UNAUTHORIZED, "Unauthorized: X-Railwatch-Proxy-Auth did not verify").into_response()
        }
    }
}

/// The account a trusted proxy's users belong to: the bound one, if it
/// still exists. Never another account in its place.
pub fn bound_account(c: &Connection) -> rusqlite::Result<Option<i64>> {
    c.query_row("SELECT a.id FROM trusted_proxy t JOIN accounts a ON a.id = t.account_id WHERE t.id = 1", [], |r| r.get(0)).optional()
}

/// Binds the trusted proxy to `account`, the one `railwatch setup` creates.
/// An existing binding is replaced only when nobody can sign in to the
/// account it names (no members: deleted, or made by `railwatch env create`
/// before setup ran, which the upgrade bound as the lowest account); one
/// that people use is refused, naming it, rather than silently kept or
/// moved under them.
pub fn bind_account(c: &Connection, account: i64) -> control::ModelResult<()> {
    let bound: Option<(i64, Option<String>, i64)> = c
        .query_row(
            "SELECT t.account_id, a.name, (SELECT count(*) FROM memberships m WHERE m.account_id = t.account_id)
             FROM trusted_proxy t LEFT JOIN accounts a ON a.id = t.account_id WHERE t.id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match bound {
        Some((id, _, _)) if id == account => Ok(()),
        Some((id, Some(name), members)) if members > 0 => Err(control::ModelError::Invalid(vec![(
            "base".into(),
            format!(
                "the trusted proxy is bound to account {id} ({name:?}), which has members; \
                 sign in and use that account, or delete it (Settings > Members) before running setup"
            ),
        )])),
        _ => {
            c.execute(
                "INSERT INTO trusted_proxy (id, account_id, created_at) VALUES (1, ?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET account_id = ?1, created_at = ?2",
                params![account, control::now_db()],
            )?;
            Ok(())
        }
    }
}

/// The session, user and account for verified claims, creating what is
/// missing. Reads first and writes only on a change, so the common request
/// is a few SELECTs. Proxied users belong to the proxy's bound account (the
/// one `railwatch setup` created) and only that one: the proxy fronts one
/// app. Invalid when that account is gone or was never bound.
pub fn resolve(c: &Connection, claims: &Claims) -> control::ModelResult<(Session, User, i64)> {
    let account = bound_account(c)?.ok_or_else(|| {
        control::ModelError::Invalid(vec![("base".into(), "the trusted proxy's account does not exist (deleted, or `railwatch setup` has not run)".into())])
    })?;
    let user = match control::find_user_by_email(c, &claims.email)? {
        Some(u) => u,
        None => {
            let mut b = [0u8; 32];
            getrandom::fill(&mut b).expect("random");
            let name = control::clean_name(if claims.name.trim().is_empty() { &claims.email } else { &claims.name });
            let id = control::insert_user(c, &name, &claims.email, &URL_SAFE_NO_PAD.encode(b), true)?;
            tracing::info!(user = id, email = %control::normalize_email(&claims.email), "trusted proxy: created user");
            control::find_user(c, id)?.expect("just created")
        }
    };
    let role: Option<String> =
        c.query_row("SELECT role FROM memberships WHERE account_id = ? AND user_id = ?", params![account, user.id], |r| r.get(0)).optional()?;
    match role.as_deref() {
        None => {
            control::insert_membership(c, account, user.id, &claims.role)?;
        }
        // The owner stays the owner; ownership moves only by transfer.
        Some("owner") => {}
        Some(r) if r != claims.role => {
            c.execute(
                "UPDATE memberships SET role = ?, updated_at = ? WHERE account_id = ? AND user_id = ?",
                params![claims.role, control::now_db(), account, user.id],
            )?;
        }
        Some(_) => {}
    }
    let session: Option<i64> = c
        .query_row("SELECT id FROM sessions WHERE user_id = ? AND user_agent = ? ORDER BY id LIMIT 1", params![user.id, SESSION_AGENT], |r| r.get(0))
        .optional()?;
    let session = match session {
        Some(id) => id,
        None => control::create_session(c, user.id, Some(SESSION_AGENT), None)?,
    };
    Ok((control::find_session(c, session)?.expect("session row"), user, account))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn claims(exp: i64) -> serde_json::Value {
        json!({"email": "Ada@Example.com", "name": "Ada", "role": "admin", "exp": exp})
    }

    #[test]
    fn a_signed_header_names_the_user_for_a_minute_only() {
        let h = sign(SECRET, &claims(1_060));
        let c = verify(SECRET, &h, 1_000).unwrap();
        assert_eq!((c.email.as_str(), c.role.as_str()), ("Ada@Example.com", "admin"));
        assert_eq!(verify(SECRET, &h, 1_060), Err("expired"));
        assert_eq!(verify(SECRET, &sign(SECRET, &claims(1_000 + MAX_TTL_SECS + 1)), 1_000), Err("exp too far ahead"));
    }

    #[test]
    fn forged_or_altered_headers_are_refused() {
        let h = sign(SECRET, &claims(1_060));
        assert_eq!(verify(b"another secret another secret 00", &h, 1_000), Err("bad signature"));
        // Claims swapped for another user's under the same signature.
        let (_, sig) = h.rsplit_once('.').unwrap();
        let other = URL_SAFE_NO_PAD.encode(json!({"email": "eve@example.com", "name": "Eve", "role": "admin", "exp": 1_060}).to_string());
        assert_eq!(verify(SECRET, &format!("v1.{other}.{sig}"), 1_000), Err("bad signature"));
        assert_eq!(verify(SECRET, &h.replacen("v1.", "v2.", 1), 1_000), Err("unknown version"));
        assert_eq!(verify(SECRET, "garbage", 1_000), Err("malformed"));
        let owner = sign(SECRET, &json!({"email": "a@example.com", "name": "A", "role": "owner", "exp": 1_060}));
        assert_eq!(verify(SECRET, &owner, 1_000), Err("role must be admin or member"), "a proxy cannot mint owners");
        let extra = sign(SECRET, &json!({"email": "a@example.com", "name": "A", "role": "admin", "exp": 1_060, "account": 2}));
        assert_eq!(verify(SECRET, &extra, 1_000), Err("malformed claims"), "unknown claims are refused, not ignored");
        let bad_email = sign(SECRET, &json!({"email": "not an email", "name": "A", "role": "admin", "exp": 1_060}));
        assert_eq!(verify(SECRET, &bad_email, 1_000), Err("invalid email"));
        let bad_name = sign(SECRET, &json!({"email": "a@example.com", "name": "A\r\nB", "role": "admin", "exp": 1_060}));
        assert_eq!(verify(SECRET, &bad_name, 1_000), Err("invalid name"));
        let long_name = sign(SECRET, &json!({"email": "a@example.com", "name": "n".repeat(101), "role": "admin", "exp": 1_060}));
        assert_eq!(verify(SECRET, &long_name, 1_000), Err("invalid name"));
        assert_eq!(verify(SECRET, &"a".repeat(MAX_HEADER_BYTES + 1), 1_000), Err("too long"));
        // A signature one byte short of the real one: compared, not prefix-matched.
        let truncated = &h[..h.len() - 2];
        assert!(verify(SECRET, truncated, 1_000).is_err());
    }

    /// PR #106 review: an account made by `railwatch env create` before the
    /// upgrade is bound by the migration (lowest id). Setup, run after, must
    /// leave the proxy on the account it creates; an account with members
    /// is never taken from under them.
    #[test]
    fn setup_rebinds_the_proxy_from_an_empty_account_and_refuses_a_used_one() {
        let dir = std::env::temp_dir().join(format!("rw-proxy-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let control = control::Control::open(&dir.join("control.sqlite3")).unwrap();
        let orphan = control.tx(|tx| control::insert_account(tx, "default", "default", 30)).unwrap();
        control.with(|x| x.execute("INSERT INTO trusted_proxy (id, account_id, created_at) VALUES (1, ?, 'x')", [orphan])).unwrap();
        let owner = control.tx(|tx| control::insert_user(tx, "Owner", "owner@example.com", &control::test_password(), true)).unwrap();
        let made = control.tx(|tx| control::create_account(tx, "Acme", owner)).unwrap();
        control.tx(|tx| bind_account(tx, made)).unwrap();
        assert_eq!(control.with(bound_account).unwrap(), Some(made), "moved off the account nobody uses");

        let other = control.tx(|tx| control::create_account(tx, "Other", owner)).unwrap();
        let err = control.tx(|tx| bind_account(tx, other)).unwrap_err().to_string();
        assert!(err.contains("Acme") && err.contains("has members"), "{err}");
        assert_eq!(control.with(bound_account).unwrap(), Some(made), "a used account keeps the proxy");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn first_sight_provisions_once_and_role_follows_the_proxy() {
        let dir = std::env::temp_dir().join(format!("rw-proxy-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let control = control::Control::open(&dir.join("control.sqlite3")).unwrap();
        let owner = control.tx(|tx| control::insert_user(tx, "Owner", "owner@example.com", &control::test_password(), true)).unwrap();
        let account = control.tx(|tx| control::create_account(tx, "Acme", owner)).unwrap();
        control.tx(|tx| Ok(bind_account(tx, account)?)).unwrap();
        let c = |role: &str| Claims { email: "Ada@Example.com".into(), name: "Ada".into(), role: role.into(), exp: 0 };

        let (s1, u1, a1) = control.tx(|tx| resolve(tx, &c("member"))).unwrap();
        assert_eq!(a1, account);
        assert_eq!((u1.email.as_str(), u1.verified, s1.user_id), ("ada@example.com", true, u1.id));
        let (s2, u2, _) = control.tx(|tx| resolve(tx, &c("admin"))).unwrap();
        assert_eq!((s2.id, u2.id), (s1.id, u1.id), "the same user and session row every time");
        let role = |user: i64| -> String {
            control.with(|x| x.query_row("SELECT role FROM memberships WHERE account_id = ? AND user_id = ?", params![account, user], |r| r.get(0))).unwrap()
        };
        assert_eq!(role(u1.id), "admin");
        let owner_claims = Claims { email: "owner@example.com".into(), name: "Owner".into(), role: "member".into(), exp: 0 };
        control.tx(|tx| resolve(tx, &owner_claims)).unwrap();
        assert_eq!(role(owner), "owner", "the proxy never demotes the owner");

        // A second account (made in the UI) is not the proxy's to grant.
        let other = control.tx(|tx| control::create_account(tx, "Other", owner)).unwrap();
        let (_, eve, _) = control.tx(|tx| resolve(tx, &Claims { email: "eve@example.com".into(), name: "Eve".into(), role: "admin".into(), exp: 0 })).unwrap();
        let accounts: Vec<i64> =
            control.with(|x| x.prepare("SELECT account_id FROM memberships WHERE user_id = ?")?.query_map([eve.id], |r| r.get(0))?.collect()).unwrap();
        assert_eq!(accounts, vec![account], "proxied users join the bound account only, never {other}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
