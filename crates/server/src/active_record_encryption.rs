//! Reading what Rails' Active Record encryption wrote (activerecord 8.1
//! lib/active_record/encryption), so `import-rails` can carry integration
//! secrets over instead of asking for them again. Decrypt only: this app
//! stores its own secrets with `SettingsCipher` (docs/DIFFERENCES.md).
//!
//! The scheme, as railwatch-cloud configures it (load_defaults 8.1):
//! - key: `ActiveSupport::KeyGenerator.new(primary_key, hash_digest_class:
//!   SHA256).generate_key(key_derivation_salt, 32)`, i.e. PBKDF2-HMAC-SHA256,
//!   2**16 iterations, 32 bytes (`DerivedSecretKeyProvider`). Several primary
//!   keys (a rotation) are each tried, as `Cipher#decrypt` does.
//! - message (`MessageSerializer`): JSON `{"p": b64, "h": {"iv": b64,
//!   "at": b64, "c": true?}}`, strict base64; `p` is AES-256-GCM ciphertext
//!   with empty associated data, `at` its 16-byte tag, `iv` 12 bytes.
//! - `c: true`: the plaintext was `Zlib.deflate`d first (over 140 bytes).
//!
//! Not handled, and reported as such: envelope encryption (`k`, an
//! encrypted data key) and nested messages in headers.

use std::io::Read;
use std::num::NonZeroU32;

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::Value;

/// `ActiveSupport::KeyGenerator`'s default iteration count.
const ITERATIONS: u32 = 1 << 16;

pub struct Keys(Vec<[u8; 32]>);

impl Keys {
    /// `DerivedSecretKeyProvider.new(primary_keys)` with `key_derivation_salt`.
    pub fn derive(primary_keys: &[String], key_derivation_salt: &str) -> Keys {
        Keys(
            primary_keys
                .iter()
                .map(|password| {
                    let mut key = [0u8; 32];
                    ring::pbkdf2::derive(
                        ring::pbkdf2::PBKDF2_HMAC_SHA256,
                        NonZeroU32::new(ITERATIONS).expect("non-zero"),
                        key_derivation_salt.as_bytes(),
                        password.as_bytes(),
                        &mut key,
                    );
                    key
                })
                .collect(),
        )
    }

    /// Is `stored` an Active Record encryption message (not plain JSON)?
    pub fn is_message(stored: &str) -> bool {
        matches!(serde_json::from_str::<Value>(stored), Ok(Value::Object(m)) if m.contains_key("p") && m.contains_key("h"))
    }

    /// `Encryptor#decrypt`: the clear text, or why not.
    pub fn decrypt(&self, stored: &str) -> Result<String, String> {
        let Ok(Value::Object(m)) = serde_json::from_str::<Value>(stored) else { return Err("not an Active Record encryption message".into()) };
        let b64 = |v: Option<&Value>, what: &str| -> Result<Vec<u8>, String> {
            let s = v.and_then(Value::as_str).ok_or_else(|| format!("message has no {what}"))?;
            STANDARD.decode(s).map_err(|_| format!("{what} is not base64"))
        };
        let headers = m.get("h").and_then(Value::as_object).ok_or("message has no headers")?;
        if headers.contains_key("k") {
            return Err("envelope encryption (an encrypted data key) is not supported".into());
        }
        let payload = b64(m.get("p"), "payload")?;
        let iv = b64(headers.get("iv"), "iv")?;
        let tag = b64(headers.get("at"), "auth tag")?;
        if tag.len() != 16 || iv.len() != 12 {
            return Err("iv or auth tag has the wrong length".into());
        }
        let plain = self
            .0
            .iter()
            .find_map(|key| {
                let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("32-byte key"));
                let mut buf = [payload.as_slice(), tag.as_slice()].concat();
                let nonce = Nonce::try_assume_unique_for_key(&iv).ok()?;
                key.open_in_place(nonce, Aad::empty(), &mut buf).ok().map(|p| p.to_vec())
            })
            .ok_or("no key decrypts it (wrong primary_key or key_derivation_salt)")?;
        let plain = if headers.get("c").and_then(Value::as_bool) == Some(true) {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(plain.as_slice()).read_to_end(&mut out).map_err(|e| format!("inflate: {e}"))?;
            out
        } else {
            plain
        };
        String::from_utf8(plain).map_err(|_| "clear text is not UTF-8".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ciphertexts written by Rails 8.1.3.1 (rw-baseline, bench/install/ar_fixture.rb
    /// with RW_AR_MODE=test): fixed test keys, one row encrypted with the
    /// older of two primary keys, one long enough to be compressed.
    const FIXTURE: &str = include_str!("../../../bench/install/ar_fixture_test.json");

    #[test]
    fn decrypts_what_rails_encrypted_including_compressed_and_rotated_keys() {
        let f: Value = serde_json::from_str(FIXTURE).unwrap();
        let keys = Keys::derive(&["old test primary key".into(), "current test primary key".into()], "railwatch-rs test salt");
        let rows = f["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().any(|r| r["compressed"] == true), "the fixture covers a compressed payload");
        for r in rows {
            let ct = r["ciphertext"].as_str().unwrap();
            assert!(Keys::is_message(ct));
            let plain: Value = serde_json::from_str(&keys.decrypt(ct).unwrap()).unwrap();
            assert_eq!(plain, r["settings"], "{}", r["kind"]);
        }
    }

    #[test]
    fn a_wrong_salt_or_key_is_an_error_not_garbage() {
        let f: Value = serde_json::from_str(FIXTURE).unwrap();
        let ct = f["rows"][0]["ciphertext"].as_str().unwrap();
        assert!(Keys::derive(&["current test primary key".into()], "other salt").decrypt(ct).is_err());
        assert!(Keys::derive(&["nope".into()], "railwatch-rs test salt").decrypt(ct).is_err());
        assert!(!Keys::is_message(r#"{"url":"https://x"}"#));
    }
}
