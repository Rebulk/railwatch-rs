//! Integration (railwatch-cloud app/models/integration.rb): where alerts go.
//! `settings` is one JSON object to callers; the sensitive keys
//! (SENSITIVE_SETTING_KEYS) are stored apart, encrypted, in
//! `encrypted_settings`, as Rails does with Active Record encryption.
//!
//! Encryption here is not Rails' (docs/DIFFERENCES.md): XChaCha20-Poly1305
//! with a key derived from secret_key_base (HMAC-SHA256 of the purpose
//! "railwatch integration settings"), stored as `rwenc:v1:<base64url(nonce
//! || ciphertext)>`. Losing secret_key_base loses the integration secrets
//! (they must be re-entered), exactly as losing Rails' primary key does.
//! A column Rails encrypted (`{"p":..,"h":..}`) cannot be read here; such an
//! integration fails delivery with a message saying to re-enter it.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};
use sha2::Sha256;

use super::outbound_http::{self, Kind};
use crate::config::Smtp;

/// Integration::KINDS less Linear, which is not ported (its delivery, OAuth
/// and sync are out of the spike); a linear row imported from Rails still
/// loads, and its alerts fail visibly.
pub const KINDS: &[&str] = &["email", "slack", "webhook"];
const PREFIX: &str = "rwenc:v1:";

/// Integration::SENSITIVE_SETTING_KEYS
pub fn sensitive_keys(kind: &str) -> &'static [&'static str] {
    match kind {
        "slack" => &["webhook_url"],
        "webhook" => &["url", "secret"],
        "linear" => &["api_key", "access_token", "refresh_token", "oauth_refresh_claim", "oauth_refresh_claimed_at"],
        _ => &[],
    }
}

/// What delivery needs beyond the database: the settings key and SMTP.
/// Set once per process on the Control (`Control::configure_alerting`).
#[derive(Clone)]
pub struct Alerting {
    pub cipher: SettingsCipher,
    pub smtp: Option<Smtp>,
    /// The server's public URL (config `url`), for links in alerts.
    pub url: String,
    /// Config `alert_delivery`; false marks every alert skipped unsent.
    pub deliver: bool,
}

#[derive(Clone)]
pub struct SettingsCipher([u8; 32]);

impl SettingsCipher {
    pub fn new(secret_key_base: &[u8]) -> SettingsCipher {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret_key_base).expect("any key length");
        m.update(b"railwatch integration settings");
        SettingsCipher(m.finalize().into_bytes().into())
    }

    pub fn encrypt(&self, plain: &Map<String, Value>) -> String {
        let cipher = XChaCha20Poly1305::new((&self.0).into());
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).expect("random");
        let json = serde_json::to_vec(plain).expect("a JSON object serializes");
        let mut out = nonce.to_vec();
        out.extend(cipher.encrypt(XNonce::from_slice(&nonce), json.as_slice()).expect("encrypt"));
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(out))
    }

    /// The stored column back to its object. Plain JSON (a row written by
    /// hand or by an earlier build) is read as is.
    pub fn decrypt(&self, stored: &str) -> Result<Map<String, Value>, String> {
        if let Some(b64) = stored.strip_prefix(PREFIX) {
            let raw = URL_SAFE_NO_PAD.decode(b64).map_err(|_| "integration secrets are corrupt (bad encoding)".to_string())?;
            if raw.len() < 24 {
                return Err("integration secrets are corrupt (too short)".into());
            }
            let cipher = XChaCha20Poly1305::new((&self.0).into());
            let plain = cipher
                .decrypt(XNonce::from_slice(&raw[..24]), &raw[24..])
                .map_err(|_| "integration secrets cannot be decrypted with this secret_key_base; re-enter them in Settings > Integrations".to_string())?;
            return match serde_json::from_slice(&plain) {
                Ok(Value::Object(m)) => Ok(m),
                _ => Err("integration secrets are corrupt (not an object)".into()),
            };
        }
        match serde_json::from_str::<Value>(stored) {
            Ok(Value::Object(m)) if m.contains_key("p") && m.contains_key("h") => {
                Err("integration secrets were encrypted by Rails (Active Record encryption) and cannot be read here; re-enter them in Settings > Integrations"
                    .into())
            }
            Ok(Value::Object(m)) => Ok(m),
            _ => Err("integration secrets are unreadable".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Integration {
    pub id: i64,
    pub account_id: i64,
    pub kind: String,
    pub name: String,
    pub enabled: bool,
    /// Plain and decrypted settings merged (Integration#settings).
    pub settings: Map<String, Value>,
    /// Why the encrypted half could not be read, if it could not.
    pub settings_error: Option<String>,
    pub last_delivered_at: Option<String>,
    pub last_received_at: Option<String>,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
}

const COLUMNS: &str = "id, account_id, kind, name, enabled, settings, encrypted_settings, last_delivered_at, last_received_at, last_synced_at, last_error";

fn row(r: &rusqlite::Row<'_>, cipher: Option<&SettingsCipher>) -> rusqlite::Result<Integration> {
    let mut settings = match serde_json::from_str::<Value>(&r.get::<_, String>(5)?) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut settings_error = None;
    if let Some(stored) = r.get::<_, Option<String>>(6)?.filter(|s| !s.is_empty()) {
        let decrypted = match cipher {
            Some(c) => c.decrypt(&stored),
            // Without a key only the legacy plain-JSON form is readable.
            None => SettingsCipher([0; 32])
                .decrypt(&stored)
                .and_then(|m| if stored.starts_with(PREFIX) { Err("integration secrets key is not configured".into()) } else { Ok(m) }),
        };
        match decrypted {
            Ok(m) => settings.extend(m),
            Err(e) => settings_error = Some(e),
        }
    }
    Ok(Integration {
        id: r.get(0)?,
        account_id: r.get(1)?,
        kind: r.get(2)?,
        name: r.get(3)?,
        enabled: r.get(4)?,
        settings,
        settings_error,
        last_delivered_at: r.get(7)?,
        last_received_at: r.get(8)?,
        last_synced_at: r.get(9)?,
        last_error: r.get(10)?,
    })
}

pub fn find(c: &Connection, cipher: Option<&SettingsCipher>, id: i64) -> rusqlite::Result<Option<Integration>> {
    c.prepare_cached(&format!("SELECT {COLUMNS} FROM integrations WHERE id = ?"))?.query_row([id], |r| row(r, cipher)).optional()
}

/// `Current.account.integrations.find(id)`
pub fn find_in_account(c: &Connection, cipher: Option<&SettingsCipher>, account_id: i64, id: i64) -> rusqlite::Result<Option<Integration>> {
    c.prepare_cached(&format!("SELECT {COLUMNS} FROM integrations WHERE id = ? AND account_id = ?"))?
        .query_row(params![id, account_id], |r| row(r, cipher))
        .optional()
}

/// `Current.account.integrations.order(:name)`
pub fn for_account(c: &Connection, cipher: Option<&SettingsCipher>, account_id: i64) -> rusqlite::Result<Vec<Integration>> {
    c.prepare_cached(&format!("SELECT {COLUMNS} FROM integrations WHERE account_id = ? ORDER BY name, id"))?
        .query_map([account_id], |r| row(r, cipher))?
        .collect()
}

fn blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        Some(Value::Bool(b)) => !b,
        _ => false,
    }
}

/// Integration's validations: kind inclusion, name presence,
/// settings_present, destination_safe. Errors keyed by attribute.
pub fn validate(kind: &str, name: &str, settings: &Map<String, Value>) -> Vec<(String, String)> {
    let mut errors = Vec::new();
    if !KINDS.contains(&kind) {
        errors.push(("kind".into(), "is not included in the list".into()));
    }
    if name.trim().is_empty() {
        errors.push(("name".into(), "can't be blank".into()));
    }
    let required: &[&str] = match kind {
        "email" => &["to"],
        "slack" => &["webhook_url"],
        "webhook" => &["url"],
        _ => &[],
    };
    for key in required {
        if blank(settings.get(*key)) {
            errors.push(("settings".into(), format!("needs {key}")));
        }
    }
    let target = match kind {
        "slack" => Some(("webhook_url", Kind::Slack)),
        "webhook" => Some(("url", Kind::Webhook)),
        _ => None,
    };
    if let Some((key, k)) = target
        && let Some(url) = settings.get(key).and_then(Value::as_str).filter(|u| !u.trim().is_empty())
        && let Err(e) = outbound_http::validate(url, k)
    {
        errors.push(("settings".into(), e.0));
    }
    errors
}

/// `settings=`: the plain half, and the sensitive half (compact_blank,
/// nil when empty) for `encrypted_settings`.
pub fn split(kind: &str, settings: &Map<String, Value>) -> (Map<String, Value>, Option<Map<String, Value>>) {
    let keys = sensitive_keys(kind);
    let plain: Map<String, Value> = settings.iter().filter(|(k, _)| !keys.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    let secret: Map<String, Value> =
        settings.iter().filter(|(k, v)| keys.contains(&k.as_str()) && !blank(Some(v))).map(|(k, v)| (k.clone(), v.clone())).collect();
    (plain, (!secret.is_empty()).then_some(secret))
}

fn columns(cipher: &SettingsCipher, kind: &str, settings: &Map<String, Value>) -> (String, Option<String>) {
    let (plain, secret) = split(kind, settings);
    (Value::Object(plain).to_string(), secret.map(|s| cipher.encrypt(&s)))
}

pub fn insert(c: &Connection, cipher: &SettingsCipher, account_id: i64, kind: &str, name: &str, settings: &Map<String, Value>) -> rusqlite::Result<i64> {
    let (plain, enc) = columns(cipher, kind, settings);
    let now = crate::control::now_db();
    c.execute(
        "INSERT INTO integrations (account_id, created_at, enabled, encrypted_settings, kind, name, settings, updated_at) VALUES (?, ?, 1, ?, ?, ?, ?, ?)",
        params![account_id, now, enc, kind, name.trim(), plain, now],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn update(c: &Connection, cipher: &SettingsCipher, i: &Integration, name: &str, enabled: bool, settings: &Map<String, Value>) -> rusqlite::Result<()> {
    let (plain, enc) = columns(cipher, &i.kind, settings);
    c.execute(
        "UPDATE integrations SET name = ?, enabled = ?, settings = ?, encrypted_settings = ?, updated_at = ? WHERE id = ?",
        params![name.trim(), enabled, plain, enc, crate::control::now_db(), i.id],
    )?;
    Ok(())
}

/// `integration.destroy!`: its alert rules and alerts go with it
/// (dependent: :destroy on both).
pub fn destroy(c: &Connection, id: i64) -> rusqlite::Result<()> {
    c.execute("UPDATE alerts SET summary_alert_id = NULL WHERE summary_alert_id IN (SELECT id FROM alerts WHERE integration_id = ?)", [id])?;
    c.execute("DELETE FROM alerts WHERE integration_id = ?", [id])?;
    c.execute("DELETE FROM alert_rules WHERE integration_id = ?", [id])?;
    c.execute("DELETE FROM integrations WHERE id = ?", [id])?;
    Ok(())
}

impl Integration {
    /// Integration#destination
    pub fn destination(&self) -> Option<String> {
        let s = |k: &str| self.settings.get(k).and_then(Value::as_str).map(str::to_string);
        match self.kind.as_str() {
            "email" => s("to"),
            "webhook" => s("url"),
            // settings["webhook_url"].to_s[%r{/(B[A-Z0-9]+)(?:/|\z)}, 1]
            "slack" => s("webhook_url").and_then(|u| {
                u.split('/')
                    .find(|seg| seg.len() > 1 && seg.starts_with('B') && seg.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
                    .map(str::to_string)
            }),
            "linear" => {
                let team_id = s("team_id");
                let team = self.settings.get("team_options").and_then(Value::as_array).and_then(|teams| {
                    teams
                        .iter()
                        .find(|t| t.get("id").and_then(Value::as_str) == team_id.as_deref())
                        .and_then(|t| t.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                let parts: Vec<String> = [s("organization_name"), team.or(team_id)].into_iter().flatten().collect();
                Some(parts.join(" / ")).filter(|p| !p.is_empty())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn secrets_are_stored_encrypted_and_only_this_key_reads_them() {
        let c = SettingsCipher::new(b"0123456789abcdef0123456789abcdef");
        let (plain, secret) = split("webhook", &obj(json!({"url": "https://example.com/h", "secret": "s3cret", "note": "x"})));
        assert_eq!(Value::Object(plain), json!({"note": "x"}), "url and secret never land in the plaintext column");
        let stored = c.encrypt(&secret.unwrap());
        assert!(stored.starts_with("rwenc:v1:") && !stored.contains("s3cret") && !stored.contains("example.com"));
        assert_eq!(Value::Object(c.decrypt(&stored).unwrap()), json!({"url": "https://example.com/h", "secret": "s3cret"}));
        let other = SettingsCipher::new(b"another secret_key_base, 32 chars+");
        assert!(other.decrypt(&stored).unwrap_err().contains("re-enter"), "a different secret_key_base cannot read them");
        // Rails' Active Record encryption envelope: refused with a reason, not misread.
        assert!(c.decrypt(r#"{"p":"abc","h":{"iv":"x","at":"y"}}"#).unwrap_err().contains("Rails"));
        // A blank sensitive value is dropped (compact_blank), so nothing is encrypted.
        assert!(split("slack", &obj(json!({"webhook_url": ""}))).1.is_none());
    }

    #[test]
    fn validation_follows_the_rails_model() {
        let e = validate("webhook", "", &obj(json!({})));
        assert_eq!(e, vec![("name".to_string(), "can't be blank".to_string()), ("settings".to_string(), "needs url".to_string())]);
        assert_eq!(validate("pager", "x", &obj(json!({})))[0], ("kind".to_string(), "is not included in the list".to_string()));
        assert!(validate("linear", "x", &obj(json!({"api_key": "k"}))).iter().any(|(k, _)| k == "kind"), "linear is not ported");
        let e = validate("webhook", "Hook", &obj(json!({"url": "http://example.com/"})));
        assert_eq!(e[0].0, "settings");
        assert!(e[0].1.contains("HTTPS"), "{e:?}");
        assert!(validate("slack", "S", &obj(json!({"webhook_url": "https://example.com/services/a/b/c"}))).iter().any(|(_, m)| m.contains("hooks.slack.com")));
        assert!(validate("email", "Ops", &obj(json!({"to": "ops@example.com"}))).is_empty());
    }

    #[test]
    fn destination_names_what_the_integration_points_at() {
        let i = |kind: &str, s: Value| Integration {
            id: 1,
            account_id: 1,
            kind: kind.into(),
            name: "n".into(),
            enabled: true,
            settings: obj(s),
            settings_error: None,
            last_delivered_at: None,
            last_received_at: None,
            last_synced_at: None,
            last_error: None,
        };
        assert_eq!(i("slack", json!({"webhook_url": "https://hooks.slack.com/services/T0/B07ABC/xyz"})).destination().as_deref(), Some("B07ABC"));
        assert_eq!(i("email", json!({"to": "a@b.c"})).destination().as_deref(), Some("a@b.c"));
        assert_eq!(i("webhook", json!({"url": "https://h/x"})).destination().as_deref(), Some("https://h/x"));
    }
}
