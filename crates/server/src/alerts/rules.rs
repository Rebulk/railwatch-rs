//! AlertRule (railwatch-cloud app/models/alert_rule.rb): matching, the
//! 30-minute dedupe, and burst collapse; plus Issue#fire_alerts! and the
//! issue-less firing the detectors do.

use std::time::Duration;

use rusqlite::{Connection, params};

use crate::grouping::issue::{self, Issue};
use crate::rjson::{self, Value};
use crate::time;

pub const EVENTS: &[&str] = &[
    "new_issue",
    "regressed_issue",
    "resolved_issue",
    "ignored_issue",
    "assigned_issue",
    "threshold",
    "anomaly",
    "silent_host",
    "quota",
    "crash_free_drop",
    // Rust-only (docs/DIFFERENCES.md): the environment watchdog.
    "silent_environment",
    "environment_recovered",
];
pub const BURST_EVENTS: &[&str] = &["new_issue", "regressed_issue"];
pub const BURST_WINDOW_SECS: i64 = 60;
pub const BURST_LIMIT: i64 = 5;
pub const DEDUPE_SECS: i64 = 30 * 60;
pub const TITLE_PATTERN_MAX_LENGTH: usize = 256;
/// AlertRule::TITLE_PATTERN_TIMEOUT (0.01 s).
pub const TITLE_PATTERN_TIMEOUT: Duration = Duration::from_millis(10);
use crate::grouping::issue::PRIORITIES;

#[derive(Debug, Clone)]
pub struct AlertRule {
    pub id: i64,
    pub application_id: i64,
    pub integration_id: i64,
    pub event: String,
    pub filters: Value,
}

pub fn find(c: &Connection, id: i64) -> rusqlite::Result<Option<AlertRule>> {
    use rusqlite::OptionalExtension;
    c.prepare_cached("SELECT id, application_id, integration_id, event, filters FROM alert_rules WHERE id = ?")?.query_row([id], rule_row).optional()
}

fn rule_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlertRule> {
    let f: String = r.get(4)?;
    Ok(AlertRule {
        id: r.get(0)?,
        application_id: r.get(1)?,
        integration_id: r.get(2)?,
        event: r.get(3)?,
        filters: rjson::parse(f.as_bytes()).unwrap_or(Value::Hash(rjson::Hash::new())),
    })
}

pub fn rules_for(c: &Connection, application_id: i64, event: &str) -> rusqlite::Result<Vec<AlertRule>> {
    let mut s =
        c.prepare_cached("SELECT id, application_id, integration_id, event, filters FROM alert_rules WHERE application_id = ? AND event = ? ORDER BY id")?;
    let rows = s.query_map(params![application_id, event], rule_row)?;
    rows.collect()
}

fn list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Nil => vec![],
        other => vec![other.clone()],
    }
}

fn to_s(v: &Value) -> String {
    String::from_utf8_lossy(&rjson::ruby::to_s(v)).into_owned()
}

fn to_i(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Float(f) => *f as i64,
        Value::Str(s) => match crate::ingest::batch::string_to_i(s) {
            Value::Int(i) => i,
            _ => 0,
        },
        _ => 0,
    }
}

/// What a rule is matched against.
pub struct Subject<'a> {
    pub issue: Option<&'a Issue>,
    pub payload: &'a rjson::Hash,
}

impl AlertRule {
    /// AlertRule#matches?
    pub fn matches(&self, s: &Subject<'_>) -> bool {
        if !self.environment_matches(s) {
            return false;
        }
        let Some(issue) = s.issue else { return true };
        self.kind_matches(issue) && self.priority_matches(issue) && self.title_matches(issue)
    }

    fn environment_matches(&self, s: &Subject<'_>) -> bool {
        // compact_blank.map(&:to_i)
        let ids: Vec<i64> = list(self.filters.get("environment_ids")).iter().filter(|v| v.present()).map(to_i).collect();
        if ids.is_empty() {
            return true;
        }
        let target = match s.issue {
            Some(i) => i.environment_id,
            None => s.payload.get(b"environment_id".as_slice()).map(to_i).unwrap_or(0),
        };
        ids.contains(&target)
    }

    fn kind_matches(&self, issue: &Issue) -> bool {
        let kinds: Vec<String> = list(self.filters.get("kinds")).iter().filter(|v| v.present()).map(to_s).collect();
        kinds.is_empty() || kinds.contains(&issue.kind)
    }

    fn priority_matches(&self, issue: &Issue) -> bool {
        let min = to_s(self.filters.get("min_priority"));
        let Some(min_i) = PRIORITIES.iter().position(|p| *p == min) else { return true };
        PRIORITIES.iter().position(|p| *p == issue.priority).unwrap_or(0) >= min_i
    }

    fn title_matches(&self, issue: &Issue) -> bool {
        let pattern = to_s(self.filters.get("title_pattern"));
        if rjson::ruby::blank(pattern.as_bytes()) {
            return true;
        }
        let mut substring = pattern.as_str();
        if regular_expression_pattern(&pattern) {
            let inner = &pattern[1..pattern.len() - 1];
            match title_regex(inner) {
                Ok(re) => {
                    return match match_with_timeout(re, issue.title.clone()) {
                        Some(m) => m,
                        None => {
                            tracing::warn!(alert_rule_id = self.id, "alert rule title pattern timed out");
                            false
                        }
                    };
                }
                // An unparsable regexp degrades to a substring match on its source.
                Err(_) => substring = inner,
            }
        }
        issue.title.to_lowercase().contains(&substring.to_lowercase())
    }
}

pub fn regular_expression_pattern(p: &str) -> bool {
    p.starts_with('/') && p.ends_with('/') && p.chars().count() > 2
}

/// Case-insensitive, like Regexp::IGNORECASE. Rust's regex engine has no
/// backtracking (linear time), so the catastrophic patterns Ruby's timeout
/// guards against cannot occur; lookaround/backreferences are rejected at
/// compile time instead and fall back to a substring (docs/DIFFERENCES.md).
pub fn title_regex(inner: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(inner).case_insensitive(true).size_limit(1 << 20).build()
}

/// The match, bounded by TITLE_PATTERN_TIMEOUT anyway: None on timeout.
fn match_with_timeout(re: regex::Regex, title: String) -> Option<bool> {
    // Linear-time matching of a <=256-char pattern against a title is
    // microseconds; the bound is belt and braces. Titles are capped to keep
    // the guarantee meaningful.
    let started = std::time::Instant::now();
    let m = re.is_match(&title[..title.len().min(64 * 1024)]);
    if started.elapsed() > TITLE_PATTERN_TIMEOUT { None } else { Some(m) }
}

fn ago(secs: i64) -> String {
    time::quoted_date_micros(time::now_micros() - secs * 1_000_000)
}

/// AlertRule#fire!: nil when deduped; otherwise the new alert's id. The
/// caller holds the control connection (single writer = the rule lock).
pub fn fire(c: &Connection, rule: &AlertRule, event: &str, issue: Option<&Issue>, payload: &rjson::Hash) -> rusqlite::Result<Option<i64>> {
    if let Some(i) = issue {
        let dup: bool = c
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM alerts WHERE alert_rule_id = ? AND issue_id = ? AND event = ? AND created_at > ?)")?
            .query_row(params![rule.id, i.id, event, ago(DEDUPE_SECS)], |r| r.get(0))?;
        if dup {
            return Ok(None);
        }
    }
    let bursting = issue.is_some() && BURST_EVENTS.contains(&event) && {
        let n: i64 = c
            .prepare_cached("SELECT COUNT(*) FROM alerts WHERE alert_rule_id = ? AND event = ? AND created_at >= ? AND issue_id IS NOT NULL")?
            .query_row(params![rule.id, event, ago(BURST_WINDOW_SECS)], |r| r.get(0))?;
        n >= BURST_LIMIT
    };
    let status = if bursting { "collapsed" } else { "pending" };
    Ok(Some(super::outbox::create(c, Some(rule.id), rule.integration_id, event, issue.map(|i| i.id), &Value::Hash(payload.clone()), status, None)?))
}

/// The payload ExceptionGrouping / detectors send: issue_key, title, environment.
pub fn issue_payload(c: &Connection, issue: &Issue) -> rjson::Hash {
    let mut h = rjson::Hash::new();
    h.insert(b"issue_key".to_vec(), Value::from(issue::key(c, issue)));
    h.insert(b"title".to_vec(), Value::from(issue.title.clone()));
    h.insert(b"environment".to_vec(), Value::from(issue::environment_name(c, issue.environment_id)));
    h
}

/// ExceptionGrouping#alert / the detectors' loops: every rule for `event`
/// that matches, with the plain payload.
pub fn fire_for_issue_plain(c: &Connection, issue: &Issue, event: &str) -> rusqlite::Result<usize> {
    let payload = issue_payload(c, issue);
    let mut n = 0;
    for rule in rules_for(c, issue.application_id, event)? {
        if rule.matches(&Subject { issue: Some(issue), payload: &payload }) && fire(c, &rule, event, Some(issue), &payload)?.is_some() {
            n += 1;
        }
    }
    Ok(n)
}

/// Issue#fire_alerts!(event, extra) from background work, where
/// Current.user is nil: payload adds actor (nil) and extra.
pub fn fire_for_issue(c: &Connection, issue: &Issue, event: &str, extra: rjson::Hash) -> rusqlite::Result<usize> {
    fire_for_issue_by(c, issue, event, None, extra)
}

/// Issue#fire_alerts!(event, extra) with `actor` = Current.user&.name.
pub fn fire_for_issue_by(c: &Connection, issue: &Issue, event: &str, actor: Option<&str>, extra: rjson::Hash) -> rusqlite::Result<usize> {
    let mut n = 0;
    for rule in rules_for(c, issue.application_id, event)? {
        let mut payload = issue_payload(c, issue);
        payload.insert(b"actor".to_vec(), Value::from(actor));
        for (k, v) in &extra {
            payload.insert(k.clone(), v.clone());
        }
        if rule.matches(&Subject { issue: Some(issue), payload: &payload }) && fire(c, &rule, event, Some(issue), &payload)?.is_some() {
            n += 1;
        }
    }
    Ok(n)
}

/// Issue-less events (silent_host, crash_free_drop, quota, watchdog):
/// every matching rule of the application.
pub fn fire_issueless(c: &Connection, application_id: i64, event: &str, payload: &rjson::Hash) -> rusqlite::Result<Vec<i64>> {
    let mut ids = Vec::new();
    for rule in rules_for(c, application_id, event)? {
        if rule.matches(&Subject { issue: None, payload })
            && let Some(id) = fire(c, &rule, event, None, payload)?
        {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Alerts already sent for these rules/event since `secs` ago whose
/// payload[key] equals `value` (the per-host / per-deploy cool-downs).
pub fn alerted_recently(c: &Connection, application_id: i64, event: &str, secs: i64, key: &str, value: &Value) -> rusqlite::Result<bool> {
    let mut s = c.prepare_cached(
        "SELECT a.payload FROM alerts a JOIN alert_rules r ON r.id = a.alert_rule_id WHERE r.application_id = ? AND a.event = ? AND a.created_at >= ?",
    )?;
    let payloads: Vec<String> = s.query_map(params![application_id, event, ago(secs)], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(payloads.iter().any(|p| rjson::parse(p.as_bytes()).map(|v| v.get(key) == value).unwrap_or(false)))
}
