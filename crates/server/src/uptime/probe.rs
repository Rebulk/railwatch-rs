//! One HTTP check of a monitor's URL, from this server.
//!
//! The address policy is the alert webhooks' (alerts::outbound_http): every
//! address the host resolves to must be public (not private, loopback,
//! link-local, CGNAT, metadata or reserved), and the connection is pinned
//! to the checked address while TLS still verifies the hostname, so DNS
//! rebinding between the check and the connect cannot reach an internal
//! address. Two differences: plain http is allowed (an app's health URL
//! need not be https), and redirects are never followed (a 3xx is an
//! answer; following it could lead anywhere, including inside).
//!
//! `RAILWATCH_ALLOW_PRIVATE_MONITOR_TARGETS=1` lifts the address check for
//! a self-hosted server that watches apps on its own network or host. Like
//! the alert flag, it lets anyone who can add a monitor make this server
//! request anything it can reach; only for installs that trust every admin.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::alerts::outbound_http::ensure_public;

/// The most of a body read to look for the keyword.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What this server allows beyond public addresses.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Policy {
    pub allow_private: bool,
}

impl Policy {
    pub fn from_env() -> Policy {
        let on = |v: String| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        Policy { allow_private: std::env::var("RAILWATCH_ALLOW_PRIVATE_MONITOR_TARGETS").is_ok_and(on) }
    }
}

/// Why a check failed. Stored as its `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Dns,
    Connect,
    Tls,
    Timeout,
    /// The status was not the expected one.
    Status,
    /// The body did not contain the keyword.
    Keyword,
    /// The URL or its addresses are not allowed (policy).
    Blocked,
}

impl Failure {
    pub fn as_str(self) -> &'static str {
        match self {
            Failure::Dns => "dns",
            Failure::Connect => "connect",
            Failure::Tls => "tls",
            Failure::Timeout => "timeout",
            Failure::Status => "status",
            Failure::Keyword => "keyword",
            Failure::Blocked => "blocked",
        }
    }
}

/// One check's result.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub status_code: Option<i64>,
    pub latency_ms: i64,
    pub failure: Option<Failure>,
    /// What went wrong, for the dashboard and the alert (never public).
    pub detail: Option<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.failure.is_none()
    }
}

/// What a check asks for.
#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    pub head: bool,
    /// None: any 2xx or 3xx.
    pub expected_status: Option<i64>,
    pub keyword: Option<String>,
    pub timeout: Duration,
}

pub fn user_agent() -> String {
    format!("Railwatch-Uptime/{} (+{})", crate::VERSION, crate::SOURCE_URL)
}

/// The URL alone, without DNS: http(s), a host, no credentials. An IP
/// literal is checked against the policy here; a name when it resolves.
pub fn validate_url(url: &str, policy: Policy) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(url.trim()).map_err(|_| "must be a valid http or https URL".to_string())?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err("must start with http:// or https://".into());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("must not contain credentials".into());
    }
    let host = u.host_str().filter(|h| !h.is_empty()).ok_or("must name a host")?;
    if let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>()
        && !policy.allow_private
    {
        ensure_public(ip).map_err(|_| private_message())?;
    }
    Ok(u)
}

fn private_message() -> String {
    "points at a private or internal address (set RAILWATCH_ALLOW_PRIVATE_MONITOR_TARGETS=1 on a self-hosted server to allow it)".into()
}

/// Runs one check. Never errors: every failure is an Outcome.
pub async fn check(req: &Request, policy: Policy) -> Outcome {
    let started = Instant::now();
    let out = match tokio::time::timeout(req.timeout, run(req, policy)).await {
        Ok(Ok((code, f))) => Outcome { status_code: Some(code), latency_ms: 0, failure: f.as_ref().map(|f| f.0), detail: f.map(|f| f.1) },
        Ok(Err((f, detail))) => Outcome { status_code: None, latency_ms: 0, failure: Some(f), detail: Some(detail) },
        Err(_) => {
            Outcome { status_code: None, latency_ms: 0, failure: Some(Failure::Timeout), detail: Some(format!("no answer within {} s", req.timeout.as_secs())) }
        }
    };
    Outcome { latency_ms: started.elapsed().as_millis() as i64, ..out }
}

type Failed = (Failure, String);

async fn run(req: &Request, policy: Policy) -> Result<(i64, Option<Failed>), Failed> {
    let url = validate_url(&req.url, policy).map_err(|e| (Failure::Blocked, format!("URL {e}")))?;
    let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> =
        tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| (Failure::Dns, format!("{host} did not resolve: {e}")))?.collect();
    if addrs.is_empty() {
        return Err((Failure::Dns, format!("{host} did not resolve")));
    }
    if !policy.allow_private {
        for a in &addrs {
            ensure_public(a.ip()).map_err(|_| (Failure::Blocked, format!("{host} resolved to a private or internal address")))?;
        }
    }
    let client = reqwest::Client::builder()
        .resolve_to_addrs(&host, &addrs)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(user_agent())
        .no_proxy()
        .build()
        .map_err(|e| (Failure::Connect, e.to_string()))?;
    let mut res = if req.head { client.head(url) } else { client.get(url) }.send().await.map_err(classify)?;
    let code = res.status().as_u16() as i64;
    let status_ok = match req.expected_status {
        Some(want) => code == want,
        None => (200..400).contains(&code),
    };
    if !status_ok {
        let want = req.expected_status.map_or("2xx or 3xx".to_string(), |w| w.to_string());
        return Ok((code, Some((Failure::Status, format!("status {code}, expected {want}")))));
    }
    let Some(keyword) = req.keyword.as_deref().filter(|k| !k.is_empty()) else { return Ok((code, None)) };
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(classify)? {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_BODY_BYTES {
            body.truncate(MAX_BODY_BYTES);
            break;
        }
    }
    let found = memfind(&body, keyword.as_bytes());
    Ok((code, (!found).then(|| (Failure::Keyword, format!("response did not contain {keyword:?}")))))
}

fn memfind(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// A transport error's class: the TLS layer's errors name certificates,
/// handshakes or rustls; a refused or reset connection is `connect`.
fn classify(e: reqwest::Error) -> Failed {
    if e.is_timeout() {
        return (Failure::Timeout, e.to_string());
    }
    let mut chain = Vec::new();
    let mut src: Option<&dyn std::error::Error> = Some(&e);
    while let Some(s) = src {
        chain.push(s.to_string());
        src = s.source();
    }
    let text = chain.join(": ");
    let lower = text.to_ascii_lowercase();
    let tls = ["certificate", "handshake", "tls", "rustls", "invalidcontenttype", "corrupt message", "peer sent"].iter().any(|k| lower.contains(k));
    (if tls { Failure::Tls } else { Failure::Connect }, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_metadata_addresses_are_refused_unless_allowed() {
        for bad in [
            "http://127.0.0.1/",
            "https://10.1.2.3/up",
            "http://[::1]:3000/",
            "http://169.254.169.254/latest/meta-data",
            "http://192.168.1.10/",
            "http://100.64.0.1/",
            "http://[fd00::1]/",
            "http://[::ffff:10.0.0.1]/",
        ] {
            assert!(validate_url(bad, Policy::default()).is_err(), "{bad} must be refused");
            assert!(validate_url(bad, Policy { allow_private: true }).is_ok(), "{bad} allowed with the self-host flag");
        }
        for bad in ["ftp://example.com/", "https://u:p@example.com/", "not a url", "file:///etc/passwd"] {
            assert!(validate_url(bad, Policy { allow_private: true }).is_err(), "{bad} is never a monitor URL");
        }
        assert!(validate_url("http://example.com/up", Policy::default()).is_ok(), "plain http to a public host is allowed");
        assert!(validate_url("https://8.8.8.8/", Policy::default()).is_ok());
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_blocked_at_check_time() {
        // `localhost` passes the URL check (not an IP literal) and must be
        // refused once it resolves, before any connection is made.
        let req = Request { url: "http://localhost:9/".into(), head: false, expected_status: None, keyword: None, timeout: Duration::from_secs(5) };
        let out = check(&req, Policy::default()).await;
        assert_eq!(out.failure, Some(Failure::Blocked), "{out:?}");
    }
}
