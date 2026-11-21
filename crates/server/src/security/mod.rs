//! The security watcher (docs/security-watcher.md), a Rust-only addition:
//! a scheduled job that reads each watched environment's recent requests
//! and `security.*` events, runs fixed detectors over them, and keeps one
//! incident per (environment, client address or actor) in the control DB.
//! An incident opens, escalates (severity only goes up) and closes after a
//! quiet period; each step can fire a `security_incident` alert.
//!
//! Everything here is code plus the policy file (policy.rs): the same
//! requests and events give the same incidents, severities and alerts.
//!
//! - detect.rs: the detectors, pure functions of an `Input`;
//! - read.rs: what a run reads from a tenant (bounded);
//! - incidents.rs: the incident state machine, alerts and the digest;
//! - watcher.rs: the loop (its own task, never the ingest path).
//!
//! Off by default: nothing runs without `security_watcher_file`, and an
//! application is watched only when its policy says `enabled = true`.

pub mod detect;
pub mod incidents;
pub mod lists;
pub mod policy;
pub mod read;
pub mod watcher;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Mutex;

/// The longest detector window, in minutes (policy validation).
pub const MAX_WINDOW_MIN: i64 = 60;

/// Characters kept of any client-written text (paths, user agents, OAuth
/// client names) in a signal summary.
pub const TEXT_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub const ALL: [Severity; 5] = [Severity::Info, Severity::Low, Severity::Medium, Severity::High, Severity::Critical];

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    pub fn parse(s: &str) -> Option<Severity> {
        Severity::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// One request as the detectors see it. Times are epoch microseconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Req {
    pub at: i64,
    pub ip: Option<String>,
    pub ua: Option<String>,
    pub path: String,
    pub status: i64,
    pub user_ref: Option<String>,
    pub halted: Option<String>,
}

/// What a `security.*` event says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Signup,
    OAuthClientRegistered,
    LoginFailed,
    AccountLocked,
    Other,
}

impl EventKind {
    /// From the event's name (`security.signup`, `security.oauth_client.registered`,
    /// `security.login.failed`, ...): tolerant of how the app spells it.
    pub fn of(name: &str) -> EventKind {
        let n = name.to_ascii_lowercase();
        let n = n.strip_prefix("security.").unwrap_or(&n);
        if n.contains("oauth") {
            if n.contains("regist") || n.ends_with("client.created") || n.ends_with("client_created") {
                EventKind::OAuthClientRegistered
            } else {
                EventKind::Other
            }
        } else if n.contains("signup") || n.contains("sign_up") || n.contains("registration") {
            EventKind::Signup
        } else if n.contains("login") && n.contains("fail") || n.contains("sign_in") && n.contains("fail") {
            EventKind::LoginFailed
        } else if n.contains("lock") {
            EventKind::AccountLocked
        } else {
            EventKind::Other
        }
    }
}

/// One `security.*` event (a `level=event` log row the gem forwarded from
/// `Rails.event`). Fields the app did not send are None.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub at: i64,
    pub kind: EventKind,
    pub name: String,
    pub ip: Option<String>,
    pub ua: Option<String>,
    pub user_ref: Option<String>,
    pub email_domain: Option<String>,
    /// The app's own verdict that the address is disposable.
    pub disposable: bool,
    pub client_id: Option<String>,
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

impl Event {
    pub fn new(at: i64, name: &str) -> Event {
        Event {
            at,
            kind: EventKind::of(name),
            name: name.to_string(),
            ip: None,
            ua: None,
            user_ref: None,
            email_domain: None,
            disposable: false,
            client_id: None,
            client_name: None,
            redirect_uris: vec![],
        }
    }
}

/// A detector's finding about one actor.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub detector: &'static str,
    pub severity: Severity,
    /// The client address, or `user:<ref>` when no address is known.
    pub actor: String,
    /// What distinguishes two signals of one detector on one actor (the
    /// account of a signup, the OAuth client); "" for address detectors.
    pub subject: String,
    /// When the detector's rule was first met (from the data, not the clock).
    pub at: i64,
    /// The newest request or event behind it.
    pub last_at: i64,
    pub count: i64,
    pub summary: String,
    /// Touches an account, a session or an OAuth client.
    pub identity: bool,
}

/// The environments a `security.*` event arrived for since the watcher
/// last looked: set from the ingest follow-ups (one insert, never a wait),
/// read by the watcher, which evaluates them without waiting for the tick.
#[derive(Default)]
pub struct Wake {
    pending: Mutex<HashSet<i64>>,
    notify: tokio::sync::Notify,
}

impl Wake {
    pub fn wake(&self, environment_id: i64) {
        let mut p = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.len() < 10_000 {
            p.insert(environment_id);
        }
        drop(p);
        self.notify.notify_one();
    }

    pub fn take(&self) -> HashSet<i64> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub async fn notified(&self) {
        self.notify.notified().await
    }
}

/// Client-written text for a summary: cut, control and bidi characters replaced.
pub fn text(s: &str) -> String {
    crate::dashboard::request_scan::untrusted(s, TEXT_CHARS)
}
