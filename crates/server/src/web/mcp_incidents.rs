//! list_security_incidents and get_security_incident: the security
//! watcher's incidents (security/incidents.rs). An incident is keyed by a
//! client address and its signals quote paths and user agents, so both
//! tools need security access and refuse without it, as ip_activity does:
//! there is no reduced form that would not echo those fields.

use serde_json::{Value, json};

use super::security::access_json;
use super::{Args, Mcp, R, clamp};
use crate::security::{Severity, incidents};

impl Mcp<'_> {
    pub(super) fn list_security_incidents(&self, a: &Args) -> R<String> {
        if !self.security_access() {
            return Ok(json!({"error": "list_security_incidents needs security access", "access": access_json(false)}).to_string());
        }
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let limit = clamp(a.get("limit"), 25, 100)?;
        let status = match a.s("status").as_str() {
            "" => None,
            s @ ("open" | "closed") => Some(s.to_string()),
            other => return Ok(json!({"error": format!("status is open or closed, not {other:?}")}).to_string()),
        };
        let min = match a.s("min_severity").as_str() {
            "" => Severity::Low,
            s => match Severity::parse(s) {
                Some(v) if v >= Severity::Low => v,
                _ => return Ok(json!({"error": "min_severity is low, medium, high or critical"}).to_string()),
            },
        };
        let (from, _) = self.window(a);
        let rows = self.env.control.with(|c| incidents::list(c, env.id, status.as_deref(), min, from, limit))?;
        let mut v = json!({"environment": env.name, "incidents": rows});
        if rows_empty(&v) {
            v["hint"] = json!(
                "No incident in the window. The watcher runs only for applications its policy enables (security_watcher_file, docs/security-watcher.md); a wider window or min_severity low shows more."
            );
        }
        v["next"] = json!("get_security_incident with an incident's id for its signals and timeline; ip_activity on its actor (an address) for the requests.");
        v["access"] = access_json(true);
        Ok(v.to_string())
    }

    pub(super) fn get_security_incident(&self, a: &Args) -> R<String> {
        if !self.security_access() {
            return Ok(json!({"error": "get_security_incident needs security access", "access": access_json(false)}).to_string());
        }
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let id = super::ruby_to_i(a.get("incident_id")).unwrap_or(0);
        let found = self.env.control.with(|c| incidents::incident_json(c, id, 200))?;
        // Only an incident of the environment the token resolved: an id of
        // another environment (or account) is "not found", never its body.
        let Some(mut v) = found.filter(|v| v["environment_id"] == json!(env.id)) else {
            return Ok(
                json!({"error": "incident not found", "hint": "Pass an incident_id from list_security_incidents in the same application and environment."})
                    .to_string(),
            );
        };
        if let Some(ip) = v["actor"].as_str().filter(|a| !a.starts_with("user:")).map(str::to_string) {
            v["next"] = json!([{"tool": "ip_activity", "arguments": {"application_id": env.application_id, "environment": env.name, "ip": ip}}]);
        }
        v["access"] = access_json(true);
        Ok(v.to_string())
    }
}

fn rows_empty(v: &Value) -> bool {
    v["incidents"].as_array().is_none_or(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use serde_json::json;

    const MIN: i64 = 60_000_000;
    const ATTACKER: &str = "203.0.113.66";

    /// The MCP tools need security scope and see only their own environment.
    #[test]
    fn mcp_incident_tools_need_security_access() {
        let f = super::super::tests::seeded("sec-incidents");
        let id = f
            .c
            .with(|x| {
                x.execute(
                    "INSERT INTO security_incidents (environment_id, actor, status, severity, identity, opened_at, last_signal_at, created_at, updated_at) VALUES (?1, ?2, 'open', 'high', 0, ?3, ?3, ?3, ?3)",
                    params![f.env, ATTACKER, super::super::tests::NOW - MIN],
                )?;
                let id = x.last_insert_rowid();
                x.execute(
                    "INSERT INTO security_incident_signals (incident_id, detector, subject, severity, first_at, last_at, count, summary, identity) VALUES (?1, 'secrets_probe', '', 'high', ?2, ?2, 3, '3 probes (/.env)', 0)",
                    params![id, super::super::tests::NOW - MIN],
                )?;
                Ok(id)
            })
            .unwrap();
        let refused = f.tool("list_security_incidents", json!({}));
        assert!(refused["error"].as_str().unwrap().contains("security access"), "{refused}");
        assert!(!refused.to_string().contains(ATTACKER) && !refused.to_string().contains(".env"));
        let refused = f.tool("get_security_incident", json!({"incident_id": id}));
        assert!(!refused.to_string().contains(ATTACKER) && !refused.to_string().contains(".env"), "{refused}");
        f.grant_security();
        let list = f.tool("list_security_incidents", json!({}));
        assert_eq!(list["incidents"][0]["actor"], ATTACKER, "{list}");
        assert_eq!(list["incidents"][0]["detectors"], json!(["secrets_probe"]));
        let one = f.tool("get_security_incident", json!({"incident_id": id}));
        assert_eq!(one["signals"][0]["summary"], "3 probes (/.env)");
        assert_eq!(one["next"][0]["tool"], "ip_activity");
        let none = f.tool("get_security_incident", json!({"incident_id": id + 1000}));
        assert_eq!(none["error"], "incident not found");
        let high_only = f.tool("list_security_incidents", json!({"min_severity": "critical"}));
        assert_eq!(high_only["incidents"], json!([]));
    }
}
