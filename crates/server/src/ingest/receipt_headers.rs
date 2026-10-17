//! Port of Ingest::ReceiptHeaders (railwatch-cloud app/models/ingest/
//! receipt_headers.rb): the headers that turn a POST into a delivery we can
//! recognise again. No producer id means the ordinary (direct) path.

use sha2::{Digest, Sha256};

use crate::ingest::batch::ReceiptAttrs;
use crate::rjson::{self, Value, ruby};

/// MAX_PAST = 7.days + 5.minutes, MAX_FUTURE = 5.minutes, in microseconds.
pub const MAX_PAST_MICROS: i64 = (7 * 86_400 + 300) * 1_000_000;
pub const MAX_FUTURE_SECONDS: i64 = 300;
const POLICIES: &[&str] = &["everything-v1"];

#[derive(Debug, Clone, PartialEq)]
pub struct Invalid(pub String);

#[derive(Debug, Clone)]
pub struct ReceiptHeaders {
    pub producer_id: String,
    pub delivery_id: String,
    pub body_sha256: String,
    pub policy_version: String,
    /// Time.at(ms / 1000.0).utc as (seconds, usec), and in microseconds.
    pub created_at: (i64, u32),
    pub metadata_sha256: String,
}

/// Header lookups by suffix ("Producer-Id" -> X-Railwatch-Producer-Id).
pub trait Headers {
    fn header(&self, suffix: &str) -> Option<&[u8]>;
}

impl ReceiptHeaders {
    /// ReceiptHeaders.from: None when the request is not receipted.
    pub fn from(h: &impl Headers) -> Result<Option<ReceiptHeaders>, Invalid> {
        let Some(producer_id) = h.header("Producer-Id").filter(|v| !ruby::blank(v)) else { return Ok(None) };
        let header_value = |s: &str| h.header(s).map(|v| Value::Str(v.to_vec())).unwrap_or(Value::Nil);
        // metadata.sort.to_h.to_json -- keys already in sorted order here.
        let mut meta = rjson::Hash::new();
        meta.insert(b"backpressure_factor".to_vec(), Value::Str(ruby::to_s(&header_value("Backpressure-Factor"))));
        meta.insert(b"dropped".to_vec(), crate::ingest::batch::string_to_i(&ruby::to_s(&header_value("Dropped"))));
        meta.insert(b"dropped_bytes".to_vec(), crate::ingest::batch::string_to_i(&ruby::to_s(&header_value("Dropped-Bytes"))));
        meta.insert(b"policy".to_vec(), header_value("Policy"));
        meta.insert(b"version".to_vec(), header_value("Version"));

        let producer_id = validate(producer_id, is_uuid, "producer id")?.to_ascii_lowercase();
        let delivery_id = validate(h.header("Batch-Id").unwrap_or(b""), is_uuid, "delivery id")?.to_ascii_lowercase();
        let body_sha256 = validate(h.header("Body-SHA256").unwrap_or(b""), is_sha256, "body digest")?.to_ascii_lowercase();
        let policy_version = String::from_utf8_lossy(h.header("Policy").unwrap_or(b"")).into_owned();
        if !POLICIES.contains(&policy_version.as_str()) {
            return Err(Invalid("unsupported policy".into()));
        }
        let created_at = extract_time(&delivery_id)?;
        // Hash#to_json is Active Support's escaping encoder.
        let json = rjson::generate_escaped(&Value::Hash(meta)).map_err(|e| Invalid(e.0))?;
        let metadata_sha256 = hex(&Sha256::digest(json.as_bytes()));
        Ok(Some(ReceiptHeaders { producer_id, delivery_id, body_sha256, policy_version, created_at, metadata_sha256 }))
    }

    pub fn created_at_micros(&self) -> i64 {
        self.created_at.0 * 1_000_000 + self.created_at.1 as i64
    }

    pub fn too_old(&self, now_micros: i64) -> bool {
        self.created_at_micros() < now_micros - MAX_PAST_MICROS
    }

    pub fn too_new(&self, now_micros: i64) -> bool {
        self.created_at_micros() > now_micros + MAX_FUTURE_SECONDS * 1_000_000
    }

    pub fn to_attributes(&self) -> ReceiptAttrs {
        ReceiptAttrs {
            producer_id: self.producer_id.clone(),
            delivery_id: self.delivery_id.clone(),
            body_sha256: self.body_sha256.clone(),
            metadata_sha256: self.metadata_sha256.clone(),
            policy_version: self.policy_version.clone(),
            delivery_created_at: self.created_at,
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn validate<'a>(value: &'a [u8], ok: fn(&[u8]) -> bool, name: &str) -> Result<String, Invalid> {
    if ok(value) { Ok(String::from_utf8(value.to_vec()).unwrap()) } else { Err(Invalid(format!("invalid {name}"))) }
}

fn is_uuid(s: &[u8]) -> bool {
    s.len() == 36 && s.iter().enumerate().all(|(i, b)| if [8, 13, 18, 23].contains(&i) { *b == b'-' } else { b.is_ascii_hexdigit() })
}

fn is_sha256(s: &[u8]) -> bool {
    s.len() == 64 && s.iter().all(u8::is_ascii_hexdigit)
}

/// A UUIDv7 carries its creation time in its first 48 bits.
fn extract_time(id: &str) -> Result<(i64, u32), Invalid> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    let b = hex.as_bytes();
    if !(b[12] == b'7' && b"89ab".contains(&b[16])) {
        return Err(Invalid("delivery id must be a v7 uuid".into()));
    }
    let ms = i64::from_str_radix(&hex[0..12], 16).unwrap();
    Ok(ruby::time_at_float(ms as f64 / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct H(HashMap<&'static str, &'static str>);
    impl Headers for H {
        fn header(&self, s: &str) -> Option<&[u8]> {
            self.0.get(s).map(|v| v.as_bytes())
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> H {
        H(pairs.iter().copied().collect())
    }

    const OK: &[(&str, &str)] = &[
        ("Producer-Id", "0F8FAD5B-D9CB-469F-A165-70867728950E"),
        ("Batch-Id", "0199d9a6-2b1b-7c3e-9f1a-2b3c4d5e6f70"),
        ("Body-SHA256", "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"),
        ("Policy", "everything-v1"),
        ("Version", "0.9.0"),
    ];

    #[test]
    fn absent_producer_is_the_direct_path() {
        assert!(ReceiptHeaders::from(&headers(&[("Batch-Id", "x")])).unwrap().is_none());
        assert!(ReceiptHeaders::from(&headers(&[("Producer-Id", "  ")])).unwrap().is_none());
    }

    #[test]
    fn valid_headers_canonicalise_and_digest_metadata() {
        let r = ReceiptHeaders::from(&headers(OK)).unwrap().unwrap();
        assert_eq!(r.producer_id, "0f8fad5b-d9cb-469f-a165-70867728950e");
        assert_eq!(r.body_sha256, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        // SHA-256 of {"backpressure_factor":"","dropped":0,"dropped_bytes":0,"policy":"everything-v1","version":"0.9.0"}
        let json = r#"{"backpressure_factor":"","dropped":0,"dropped_bytes":0,"policy":"everything-v1","version":"0.9.0"}"#;
        assert_eq!(r.metadata_sha256, hex(&Sha256::digest(json.as_bytes())));
        assert_eq!(r.created_at.0, 0x0199d9a62b1b / 1000);
    }

    #[test]
    fn invalid_headers_raise_in_ruby_order() {
        let with = |k: &'static str, v: &'static str| {
            let mut h = headers(OK);
            h.0.insert(k, v);
            ReceiptHeaders::from(&h).unwrap_err().0
        };
        assert_eq!(with("Producer-Id", "nope"), "invalid producer id");
        assert_eq!(with("Batch-Id", "nope"), "invalid delivery id");
        assert_eq!(with("Body-SHA256", "abc"), "invalid body digest");
        assert_eq!(with("Policy", "everything-v2"), "unsupported policy");
        assert_eq!(with("Batch-Id", "0f8fad5b-d9cb-469f-a165-70867728950e"), "delivery id must be a v7 uuid");
        assert_eq!(with("Batch-Id", "0199d9a6-2b1b-7c3e-1f1a-2b3c4d5e6f70"), "delivery id must be a v7 uuid");
    }

    #[test]
    fn age_window() {
        let r = ReceiptHeaders::from(&headers(OK)).unwrap().unwrap();
        let created = r.created_at_micros();
        assert!(!r.too_new(created));
        assert!(r.too_new(created - 301 * 1_000_000));
        assert!(!r.too_old(created + MAX_PAST_MICROS));
        assert!(r.too_old(created + MAX_PAST_MICROS + 1));
    }
}
