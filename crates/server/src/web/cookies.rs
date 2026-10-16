//! Cookies the way the Rails app keeps them, minus the Rails formats:
//!
//! - `session_token`: signed (HMAC-SHA256), permanent, HttpOnly: the
//!   `sessions.id` of the signed-in browser (`cookies.signed.permanent`).
//!   No idle expiry, by design: sessions are long-lived and revocable from
//!   Settings > Sessions.
//! - `account_id`: signed, permanent: the account switcher.
//! - `_railwatch_session`: encrypted (XChaCha20-Poly1305), HttpOnly, a
//!   browser-session cookie holding a small JSON map: the CSRF secret and
//!   what Rails keeps in its cookie session (flash, Inertia errors).
//!
//! The signing/encryption keys are derived from secret_key_base. Rails'
//! cookies are not readable here and vice versa (DIFFERENCES.md); an
//! imported install signs everyone in again.

use axum::http::{HeaderMap, HeaderValue, header};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;

pub const SESSION_TOKEN: &str = "session_token";
pub const ACCOUNT_ID: &str = "account_id";
pub const SESSION: &str = "_railwatch_session";
/// 20 years, as `cookies.permanent`.
const PERMANENT_SECONDS: u64 = 20 * 365 * 86_400;

#[derive(Clone)]
pub struct Keys {
    sign: Vec<u8>,
    encrypt: [u8; 32],
}

fn derive(secret: &[u8], purpose: &str) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("any key length");
    m.update(purpose.as_bytes());
    m.finalize().into_bytes().into()
}

impl Keys {
    pub fn new(secret: &[u8]) -> Keys {
        Keys { sign: derive(secret, "railwatch signed cookie").to_vec(), encrypt: derive(secret, "railwatch encrypted cookie") }
    }

    fn mac(&self, name: &str, value: &str) -> Vec<u8> {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.sign).expect("any key length");
        // The cookie name is part of the MAC, so one cookie's value cannot be
        // replayed as another's.
        m.update(name.as_bytes());
        m.update(b"=");
        m.update(value.as_bytes());
        m.finalize().into_bytes().to_vec()
    }

    pub fn sign(&self, name: &str, value: &str) -> String {
        format!("{}--{}", URL_SAFE_NO_PAD.encode(value), URL_SAFE_NO_PAD.encode(self.mac(name, value)))
    }

    pub fn verify(&self, name: &str, cookie: &str) -> Option<String> {
        let (v, mac) = cookie.rsplit_once("--")?;
        let value = String::from_utf8(URL_SAFE_NO_PAD.decode(v).ok()?).ok()?;
        let mac = URL_SAFE_NO_PAD.decode(mac).ok()?;
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.sign).expect("any key length");
        m.update(name.as_bytes());
        m.update(b"=");
        m.update(value.as_bytes());
        m.verify_slice(&mac).ok()?;
        Some(value)
    }

    pub fn encrypt(&self, plain: &[u8]) -> String {
        let cipher = XChaCha20Poly1305::new((&self.encrypt).into());
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).expect("random");
        let ct = cipher.encrypt(XNonce::from_slice(&nonce), plain).expect("encrypt");
        let mut out = nonce.to_vec();
        out.extend(ct);
        URL_SAFE_NO_PAD.encode(out)
    }

    pub fn decrypt(&self, cookie: &str) -> Option<Vec<u8>> {
        let raw = URL_SAFE_NO_PAD.decode(cookie).ok()?;
        if raw.len() < 24 {
            return None;
        }
        let cipher = XChaCha20Poly1305::new((&self.encrypt).into());
        cipher.decrypt(XNonce::from_slice(&raw[..24]), &raw[24..]).ok()
    }
}

/// The request's cookies by name (the first wins on duplicates).
pub fn parse(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for v in headers.get_all(header::COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        for pair in s.split(';') {
            if let Some((k, v)) = pair.trim().split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

pub fn get<'a>(cookies: &'a [(String, String)], name: &str) -> Option<&'a str> {
    cookies.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// One Set-Cookie value.
pub struct SetCookie {
    pub name: &'static str,
    /// None deletes the cookie.
    pub value: Option<String>,
    pub permanent: bool,
    pub http_only: bool,
    pub secure: bool,
}

impl SetCookie {
    pub fn header(&self) -> HeaderValue {
        let mut s = format!("{}={}; Path=/; SameSite=Lax", self.name, self.value.as_deref().unwrap_or(""));
        match (&self.value, self.permanent) {
            (None, _) => s.push_str("; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"),
            (Some(_), true) => s.push_str(&format!("; Max-Age={PERMANENT_SECONDS}")),
            (Some(_), false) => {}
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        if self.secure {
            s.push_str("; Secure");
        }
        HeaderValue::from_str(&s).expect("cookie values are base64url")
    }
}

/// The decrypted `_railwatch_session` map, with change tracking so an
/// untouched session is never rewritten.
#[derive(Debug, Clone, Default)]
pub struct SessionData {
    pub map: Map<String, Value>,
    pub dirty: bool,
}

impl SessionData {
    pub fn load(keys: &Keys, cookies: &[(String, String)]) -> SessionData {
        let map = get(cookies, SESSION)
            .and_then(|c| keys.decrypt(c))
            .and_then(|b| serde_json::from_slice::<Map<String, Value>>(&b).ok())
            .unwrap_or_default();
        SessionData { map, dirty: false }
    }

    pub fn encode(&self, keys: &Keys) -> String {
        keys.encrypt(&serde_json::to_vec(&self.map).expect("json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_values_round_trip_and_reject_tampering_and_renaming() {
        let k = Keys::new(b"0123456789abcdef0123456789abcdef");
        let c = k.sign(SESSION_TOKEN, "42");
        assert_eq!(k.verify(SESSION_TOKEN, &c).as_deref(), Some("42"));
        assert_eq!(k.verify(ACCOUNT_ID, &c), None, "a session token is not an account id");
        let forged = format!("{}--{}", URL_SAFE_NO_PAD.encode("43"), c.rsplit_once("--").unwrap().1);
        assert_eq!(k.verify(SESSION_TOKEN, &forged), None);
        assert_eq!(Keys::new(b"another secret another secret 00").verify(SESSION_TOKEN, &c), None);
    }

    #[test]
    fn encrypted_session_round_trips_and_rejects_tampering() {
        let k = Keys::new(b"0123456789abcdef0123456789abcdef");
        let mut s = SessionData::default();
        s.map.insert("csrf".into(), "x".into());
        let c = s.encode(&k);
        let back = SessionData::load(&k, &[(SESSION.into(), c.clone())]);
        assert_eq!(back.map.get("csrf"), Some(&Value::from("x")));
        let mut raw = URL_SAFE_NO_PAD.decode(&c).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        assert!(SessionData::load(&k, &[(SESSION.into(), URL_SAFE_NO_PAD.encode(raw))]).map.is_empty());
    }
}
