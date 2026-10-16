//! The control-database rows the environment pages read (deploys, issues,
//! thresholds, anomaly rules, alerts, the environment's own settings). The
//! control DB is p3-control's; pages take it through `ControlData`, so the
//! page code never depends on how it is stored.
//!
//! Each method mirrors the Active Record query the controller runs, so an
//! implementation is a direct translation; `StaticControl` implements them
//! all over in-memory rows (tests, and the props-diff harness, which loads
//! Rails' rows from JSON).

use serde::Deserialize;
use serde_json::Value;

use super::rb;

#[derive(Debug, Clone, Deserialize)]
pub struct Deploy {
    pub id: i64,
    pub deploy: String,
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub server: Option<String>,
    /// Epoch micros.
    pub deployed_at: i64,
    pub previous_ref: Option<String>,
    #[serde(default)]
    pub commits: Vec<Value>,
    #[serde(default)]
    pub detail: serde_json::Map<String, Value>,
}

impl Deploy {
    /// `ref.to_s.first(7).presence || deploy.first(12)`
    pub fn short_ref(&self) -> String {
        let r = rb::first_chars(self.ref_.as_deref().unwrap_or(""), 7);
        if rb::present(Some(r)) { r.to_string() } else { rb::first_chars(&self.deploy, 12).to_string() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub id: i64,
    /// "PREFIX-NUMBER"
    pub key: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub priority: String,
    pub occurrences: i64,
    pub affected_users: i64,
    pub culprit: Option<String>,
    pub group_hash: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub resolved_in_deploy: Option<String>,
    #[serde(default)]
    pub sample: serde_json::Map<String, Value>,
}

/// The rows a page needs from the control DB, for one environment.
pub trait ControlData {
    /// `environment.deploys` (all of them; callers filter/sort as Rails does).
    fn deploys(&self) -> Vec<Deploy>;
    /// `environment.issues`.
    fn issues(&self) -> Vec<Issue>;
    /// `environment.expected_servers`.
    fn expected_servers(&self) -> Vec<String> {
        vec![]
    }
    /// The thresholds page's `thresholds` and `anomaly_rules` props, already
    /// shaped (they are control rows with no telemetry in them).
    fn thresholds_props(&self) -> Value {
        Value::Array(vec![])
    }
    fn anomaly_rules_props(&self) -> Value {
        Value::Array(vec![])
    }
}

/// `ControlData` over plain rows.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StaticControl {
    #[serde(default)]
    pub deploys: Vec<Deploy>,
    #[serde(default)]
    pub issues: Vec<Issue>,
    #[serde(default)]
    pub expected_servers: Vec<String>,
    #[serde(default)]
    pub thresholds: Option<Value>,
    #[serde(default)]
    pub anomaly_rules: Option<Value>,
}

impl ControlData for StaticControl {
    fn deploys(&self) -> Vec<Deploy> {
        self.deploys.clone()
    }
    fn issues(&self) -> Vec<Issue> {
        self.issues.clone()
    }
    fn expected_servers(&self) -> Vec<String> {
        self.expected_servers.clone()
    }
    fn thresholds_props(&self) -> Value {
        self.thresholds.clone().unwrap_or(Value::Array(vec![]))
    }
    fn anomaly_rules_props(&self) -> Value {
        self.anomaly_rules.clone().unwrap_or(Value::Array(vec![]))
    }
}

// -- The scopes pages run over these rows ------------------------------------

/// `deploys.recent`: deployed_at desc. The tie order is SQLite's for equal
/// keys (by rowid in an index scan), i.e. id order; kept stable by id desc
/// as the (environment_id, deployed_at) index walks backwards.
pub fn recent_deploys(mut d: Vec<Deploy>) -> Vec<Deploy> {
    d.sort_by(|a, b| b.deployed_at.cmp(&a.deployed_at).then(b.id.cmp(&a.id)));
    d
}

/// `issues.recent`: last_seen_at desc (same tie rule).
pub fn recent_issues(mut i: Vec<Issue>) -> Vec<Issue> {
    i.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at).then(b.id.cmp(&a.id)));
    i
}

/// EnvironmentScoped#deploys_in_window.
pub fn deploys_in_window(control: &dyn ControlData, from: i64, to: i64) -> Value {
    let rows: Vec<Value> = recent_deploys(control.deploys())
        .into_iter()
        .filter(|d| d.deployed_at >= from && d.deployed_at <= to)
        .take(50)
        .map(|d| crate::obj! { "deploy" => d.deploy.clone(), "ref" => d.short_ref(), "at" => rb::t_json(d.deployed_at) })
        .collect();
    Value::Array(rows)
}

// Unordered issue scopes come back in the order of the index SQLite walks
// (EXPLAIN QUERY PLAN on the baseline's primary DB, 2026-10-13):
// - where(group_hash: [...])       index (environment_id, group_hash)
// - open.where(group_hash LIKE ..) index (environment_id, status, last_seen_at)
// - where(resolved_in_deploy: x)   index (environment_id) -> rowid (id)

/// `environment.issues.where(group_hash: groups)` in group_hash order.
pub fn issues_in_groups(control: &dyn ControlData, groups: &[&str]) -> Vec<Issue> {
    let mut v: Vec<Issue> = control.issues().into_iter().filter(|i| groups.contains(&i.group_hash.as_str())).collect();
    v.sort_by(|a, b| a.group_hash.cmp(&b.group_hash));
    v
}

/// `environment.issues.open.where("group_hash LIKE 'missed:%'")`.
pub fn open_missed_issues(control: &dyn ControlData) -> Vec<Issue> {
    let mut v: Vec<Issue> = control.issues().into_iter().filter(|i| i.status == "open" && i.group_hash.starts_with("missed:")).collect();
    v.sort_by(|a, b| a.last_seen_at.cmp(&b.last_seen_at).then(a.id.cmp(&b.id)));
    v
}

/// `environment.issues.where(resolved_in_deploy: deploy)` in id order.
pub fn issues_resolved_in(control: &dyn ControlData, deploy: &str) -> Vec<Issue> {
    let mut v: Vec<Issue> = control.issues().into_iter().filter(|i| i.resolved_in_deploy.as_deref() == Some(deploy)).collect();
    v.sort_by_key(|i| i.id);
    v
}
