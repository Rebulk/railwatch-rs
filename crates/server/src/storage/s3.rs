//! The four S3 calls backups need (PUT, GET, HEAD, ListObjectsV2), signed
//! with AWS Signature V4, path-style, against any S3-compatible endpoint
//! (AWS, Hetzner Object Storage, Cloudflare R2, Backblaze B2, MinIO).
//! Blocking (backups run on their own thread). The reqwest dependency is
//! already here for alert delivery; this avoids pulling in the AWS SDK.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct S3 {
    /// "https://s3.eu-central-1.amazonaws.com", "https://fsn1.your-objectstorage.com",
    /// "https://<account>.r2.cloudflarestorage.com", "http://127.0.0.1:9000".
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Key prefix inside the bucket ("" or "railwatch/").
    pub prefix: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding as SigV4 wants it; '/' kept in paths.
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl S3 {
    fn client() -> &'static reqwest::blocking::Client {
        static C: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
        C.get_or_init(|| reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(600)).build().expect("http client"))
    }

    pub fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// Signs and sends one request. `query` pairs are unencoded.
    fn send(&self, method: &str, key: Option<&str>, query: &[(&str, &str)], body: Vec<u8>) -> Result<Response, String> {
        let host_url = self.endpoint.trim_end_matches('/');
        let host = host_url.split("://").nth(1).unwrap_or(host_url).split('/').next().unwrap_or("");
        let path = match key {
            Some(k) => format!("/{}/{}", uri_encode(&self.bucket, false), uri_encode(k, true)),
            None => format!("/{}", uri_encode(&self.bucket, false)),
        };
        let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
        q.sort();
        let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let now = crate::time::now_micros() / 1_000_000;
        let ts = crate::time::utc_seconds_string(now);
        let amz_date = format!("{}{}{}T{}{}{}Z", &ts[0..4], &ts[5..7], &ts[8..10], &ts[11..13], &ts[14..16], &ts[17..19]);
        let date = &amz_date[..8];
        let payload_hash = hex(&Sha256::digest(&body));
        let canonical = format!(
            "{method}\n{path}\n{canonical_query}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload_hash}"
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
        let k = hmac(format!("AWS4{}", self.secret_key).as_bytes(), date);
        let k = hmac(&k, &self.region);
        let k = hmac(&k, "s3");
        let k = hmac(&k, "aws4_request");
        let signature = hex(&hmac(&k, &to_sign));
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
            self.access_key
        );
        let url = if canonical_query.is_empty() { format!("{host_url}{path}") } else { format!("{host_url}{path}?{canonical_query}") };
        let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
        let resp = Self::client()
            .request(m, &url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("authorization", auth)
            .body(body)
            .send()
            .map_err(|e| format!("{method} {url}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().map_err(|e| e.to_string())?.to_vec();
        Ok(Response { status, body })
    }

    pub fn put(&self, name: &str, body: Vec<u8>) -> Result<(), String> {
        let key = self.key(name);
        let r = self.send("PUT", Some(&key), &[], body)?;
        if r.status / 100 != 2 {
            return Err(format!("PUT {key}: {} {}", r.status, String::from_utf8_lossy(&r.body)));
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Vec<u8>, String> {
        let key = self.key(name);
        let r = self.send("GET", Some(&key), &[], vec![])?;
        if r.status / 100 != 2 {
            return Err(format!("GET {key}: {} {}", r.status, String::from_utf8_lossy(&r.body)));
        }
        Ok(r.body)
    }

    pub fn exists(&self, name: &str) -> Result<bool, String> {
        let key = self.key(name);
        let r = self.send("HEAD", Some(&key), &[], vec![])?;
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            s => Err(format!("HEAD {key}: {s}")),
        }
    }

    /// Every key under `prefix` (relative to self.prefix), paging through
    /// ListObjectsV2.
    pub fn list(&self, prefix: &str) -> Result<Vec<String>, String> {
        let full = self.key(prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type", "2"), ("prefix", full.as_str())];
            if let Some(t) = token.as_deref() {
                q.push(("continuation-token", t));
            }
            let r = self.send("GET", None, &q, vec![])?;
            if r.status / 100 != 2 {
                return Err(format!("LIST {full}: {} {}", r.status, String::from_utf8_lossy(&r.body)));
            }
            let xml = String::from_utf8_lossy(&r.body);
            for k in tags(&xml, "Key") {
                out.push(k.strip_prefix(&self.prefix).unwrap_or(&k).to_string());
            }
            token = tags(&xml, "NextContinuationToken").into_iter().next();
            if token.is_none() || !xml.contains("<IsTruncated>true</IsTruncated>") {
                return Ok(out);
            }
        }
    }
}

/// The text of every `<tag>...</tag>`, XML entities decoded (S3 listings
/// are flat; no XML parser needed).
fn tags(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(
            after[..j].replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"),
        );
        rest = &after[j + close.len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses() {
        assert_eq!(uri_encode("a b/c+d", true), "a%20b/c%2Bd");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        let xml = "<ListBucketResult><Contents><Key>p/a&amp;b</Key></Contents><Contents><Key>p/c</Key></Contents></ListBucketResult>";
        assert_eq!(tags(xml, "Key"), vec!["p/a&b", "p/c"]);
    }

    /// AWS's published SigV4 example (GET object, "Example: GET Object").
    #[test]
    fn signing_key_matches_aws_example() {
        let k = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "20130524");
        let k = hmac(&k, "us-east-1");
        let k = hmac(&k, "s3");
        let k = hmac(&k, "aws4_request");
        let to_sign = "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972";
        assert_eq!(hex(&hmac(&k, to_sign)), "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }
}
