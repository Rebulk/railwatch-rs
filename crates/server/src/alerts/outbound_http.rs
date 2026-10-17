//! Port of Alerts::OutboundHttp (railwatch-cloud app/models/alerts/
//! outbound_http.rb): the only path from customer-configured destinations to
//! the network. HTTPS only, no credentials or fragment; every resolved
//! address must be public; the connection is pinned to the checked address
//! (reqwest `resolve`) while TLS still verifies the hostname, so DNS
//! rebinding between check and connect is impossible. Responses are capped
//! at 1 MiB and the whole exchange at 15 s.
//!
//! Self-hosting additions (docs/DIFFERENCES.md), both off by default:
//! - `RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1` skips the public-address
//!   check (for ntfy, Gotify, a webhook relay on the LAN or the same box).
//!   HTTPS, no credentials/fragment and the Slack host rule still apply.
//!   It reopens SSRF: anyone who can edit an integration can make this
//!   server POST to anything it can reach (cloud metadata endpoints,
//!   admin ports on localhost). Only for installs where every account
//!   admin is trusted with that.
//! - `RAILWATCH_ALERT_CA_FILE=/path/ca.pem` trusts that PEM bundle in
//!   addition to the built-in web roots, for a receiver with a private CA.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

pub const SLACK_HOST: &str = "hooks.slack.com";
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, PartialEq)]
pub enum Kind {
    Webhook,
    Slack,
}

/// What the operator allows beyond Rails' policy (module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Policy {
    pub allow_private: bool,
}

impl Policy {
    pub fn from_env() -> Policy {
        let on = |v: String| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        Policy { allow_private: std::env::var("RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS").is_ok_and(on) }
    }
}

#[derive(Debug)]
pub struct Unsafe(pub String);

impl std::fmt::Display for Unsafe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct Target {
    pub url: reqwest::Url,
    pub host: String,
    pub port: u16,
}

/// validate!: the URL alone (no DNS), under the environment's policy.
pub fn validate(url: &str, kind: Kind) -> Result<Target, Unsafe> {
    validate_with(url, kind, Policy::from_env())
}

pub fn validate_with(url: &str, kind: Kind, policy: Policy) -> Result<Target, Unsafe> {
    let bad = || Unsafe("destination must be an HTTPS URL without credentials or a fragment".into());
    let u = reqwest::Url::parse(url).map_err(|_| Unsafe("destination must be a valid HTTPS URL".into()))?;
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() || u.fragment().is_some() {
        return Err(bad());
    }
    let host = u.host_str().filter(|h| !h.is_empty()).ok_or_else(bad)?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = u.port_or_known_default().unwrap_or(443);
    if kind == Kind::Slack {
        let path_ok = {
            let segs: Vec<&str> = u.path().trim_start_matches('/').split('/').collect();
            segs.len() == 4
                && segs[0] == "services"
                && segs[1..].iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        };
        if !host.eq_ignore_ascii_case(SLACK_HOST) || port != 443 || u.query().is_some() || !path_ok {
            return Err(Unsafe("Slack destination must be a hooks.slack.com/services/... webhook URL".into()));
        }
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && !policy.allow_private
    {
        ensure_public(ip)?;
    }
    Ok(Target { url: u, host, port })
}

fn in_v4(ip: Ipv4Addr, net: [u8; 4], bits: u32) -> bool {
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    (u32::from(ip) & mask) == (u32::from(Ipv4Addr::from(net)) & mask)
}

fn in_v6(ip: Ipv6Addr, net: Ipv6Addr, bits: u32) -> bool {
    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
    (u128::from(ip) & mask) == (u128::from(net) & mask)
}

/// ensure_public!: IPAddr's private/loopback/link-local plus NON_PUBLIC_RANGES.
pub fn ensure_public(ip: IpAddr) -> Result<(), Unsafe> {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    let blocked = match ip {
        IpAddr::V4(v) => {
            const R: &[([u8; 4], u32)] = &[
                ([10, 0, 0, 0], 8),
                ([172, 16, 0, 0], 12),
                ([192, 168, 0, 0], 16),
                ([127, 0, 0, 0], 8),
                ([169, 254, 0, 0], 16),
                ([0, 0, 0, 0], 8),
                ([100, 64, 0, 0], 10),
                ([192, 0, 0, 0], 24),
                ([192, 0, 2, 0], 24),
                ([198, 18, 0, 0], 15),
                ([198, 51, 100, 0], 24),
                ([203, 0, 113, 0], 24),
                ([224, 0, 0, 0], 4),
                ([240, 0, 0, 0], 4),
            ];
            R.iter().any(|(n, b)| in_v4(v, *n, *b))
        }
        IpAddr::V6(v) => {
            let r: &[(&str, u32)] = &[
                ("fc00::", 7),
                ("::1", 128),
                ("fe80::", 10),
                ("::", 96),
                ("64:ff9b::", 96),
                ("64:ff9b:1::", 48),
                ("100::", 64),
                ("2001::", 32),
                ("2001:2::", 48),
                ("2001:db8::", 32),
                ("2002::", 16),
                ("fec0::", 10),
                ("ff00::", 8),
            ];
            r.iter().any(|(n, b)| in_v6(v, n.parse().unwrap(), *b))
        }
    };
    if blocked { Err(Unsafe("destination resolved to a non-public address".into())) } else { Ok(()) }
}

/// RAILWATCH_ALERT_CA_FILE's certificates; an unreadable file fails the
/// delivery (with the path), never silently falls back.
fn extra_roots() -> Result<Vec<reqwest::Certificate>, String> {
    let Some(path) = std::env::var_os("RAILWATCH_ALERT_CA_FILE").filter(|p| !p.is_empty()) else { return Ok(vec![]) };
    let pem = std::fs::read(&path).map_err(|e| format!("RAILWATCH_ALERT_CA_FILE {}: {e}", path.to_string_lossy()))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| format!("RAILWATCH_ALERT_CA_FILE {}: {e}", path.to_string_lossy()))?;
    if certs.is_empty() {
        return Err(format!("RAILWATCH_ALERT_CA_FILE {}: no PEM certificates in it", path.to_string_lossy()));
    }
    Ok(certs)
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// post_json: `body` is sent as given (a signed body is the sent body).
pub async fn post_json(url: &str, body: String, headers: &[(&str, String)], kind: Kind) -> Result<Response, String> {
    tokio::time::timeout(RESPONSE_TIMEOUT, post_inner(url, body, headers, kind)).await.map_err(|_| "destination response timed out".to_string())?
}

async fn post_inner(url: &str, body: String, headers: &[(&str, String)], kind: Kind) -> Result<Response, String> {
    let policy = Policy::from_env();
    let t = validate_with(url, kind, policy).map_err(|e| e.0)?;
    let addrs: Vec<SocketAddr> =
        tokio::net::lookup_host((t.host.as_str(), t.port)).await.map_err(|e| format!("destination hostname could not be resolved: {e}"))?.collect();
    if addrs.is_empty() {
        return Err("destination hostname did not resolve".into());
    }
    if !policy.allow_private {
        for a in &addrs {
            ensure_public(a.ip()).map_err(|e| e.0)?;
        }
    }
    let mut builder = reqwest::Client::builder()
        .resolve(&t.host, addrs[0])
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(10));
    for cert in extra_roots()? {
        builder = builder.add_root_certificate(cert);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let mut req = client.post(t.url.clone()).header("Content-Type", "application/json").body(body);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let mut res = req.send().await.map_err(|e| e.to_string())?;
    if res.content_length().is_some_and(|l| l as usize > MAX_RESPONSE_BYTES) {
        return Err(format!("destination response exceeded {MAX_RESPONSE_BYTES} bytes"));
    }
    let status = res.status().as_u16();
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
        if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(format!("destination response exceeded {MAX_RESPONSE_BYTES} bytes"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Response { status, body: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_private_and_non_https_destinations() {
        for bad in [
            "http://example.com/x",
            "https://user:pw@example.com/",
            "https://example.com/#f",
            "https://127.0.0.1/",
            "https://10.1.2.3/",
            "https://[::1]/",
            "https://[::ffff:192.168.1.1]/",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/",
            "https://[fd00::1]/",
            "https://0.0.0.0/",
            "not a url",
        ] {
            assert!(validate(bad, Kind::Webhook).is_err(), "{bad} should be refused");
        }
        assert!(validate("https://example.com/hook?x=1", Kind::Webhook).is_ok());
        assert!(validate("https://8.8.8.8/", Kind::Webhook).is_ok());
    }

    #[test]
    fn allow_private_relaxes_only_the_address_check() {
        let lan = Policy { allow_private: true };
        for ok in ["https://127.0.0.1:8443/hook", "https://192.168.1.20/ntfy", "https://[::1]/x", "https://10.0.0.5/"] {
            assert!(validate_with(ok, Kind::Webhook, lan).is_ok(), "{ok} should be allowed on a LAN policy");
            assert!(validate_with(ok, Kind::Webhook, Policy::default()).is_err(), "{ok} refused by default");
        }
        // Still HTTPS only, no credentials, no fragment, Slack's host rule.
        for bad in ["http://192.168.1.20/", "https://u:p@10.0.0.5/", "https://10.0.0.5/#f"] {
            assert!(validate_with(bad, Kind::Webhook, lan).is_err(), "{bad}");
        }
        assert!(validate_with("https://10.0.0.5/services/a/b/c", Kind::Slack, lan).is_err());
    }

    #[test]
    fn slack_must_be_an_incoming_webhook() {
        assert!(validate("https://hooks.slack.com/services/T0/B1/abc_-9", Kind::Slack).is_ok());
        for bad in [
            "https://hooks.slack.com/services/T0/B1",
            "https://evil.com/services/T0/B1/x",
            "https://hooks.slack.com/services/T0/B1/x?y=1",
            "https://hooks.slack.com:8443/services/a/b/c",
        ] {
            assert!(validate(bad, Kind::Slack).is_err(), "{bad}");
        }
    }
}
