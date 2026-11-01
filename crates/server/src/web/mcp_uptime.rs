//! The `uptime` tool: an environment's uptime monitors (uptime/mod.rs)
//! with their current state, uptime over 24h/7d/30d/90d, yesterday's and
//! today's latency, and the recent incidents. Compact as the other answer
//! tools: no null fields, times as ISO 8601, one dashboard link.

use serde_json::{Map, Value, json};

use super::{Args, Mcp, R, clamp};
use crate::uptime;

impl Mcp<'_> {
    pub(super) fn uptime(&self, a: &Args) -> R<String> {
        let Some(env) = self.resolve_environment(a)? else { return self.no_environment(a) };
        let limit = clamp(a.get("incidents"), 10, 50)?;
        let only = a.presence("monitor");
        let now = self.now;
        let t = |m: i64| json!(crate::time::iso8601(m.div_euclid(1_000_000)));
        let (monitors, incidents, page) = self.env.control.with(|c| {
            let mut out = Vec::new();
            for m in uptime::monitors(c, env.id)? {
                if only.as_deref().is_some_and(|n| !n.eq_ignore_ascii_case(&m.name)) {
                    continue;
                }
                let mut v = Map::new();
                v.insert("name".into(), json!(m.name));
                v.insert("url".into(), json!(m.url));
                v.insert("status".into(), json!(m.status));
                if let Some(s) = uptime::status_since(c, &m)? {
                    v.insert("since".into(), t(s));
                }
                if let Some(at) = m.last_checked_at {
                    let mut last = Map::new();
                    last.insert("at".into(), t(at));
                    if let Some(code) = m.last_status_code {
                        last.insert("status_code".into(), json!(code));
                    }
                    if let Some(ms) = m.last_latency_ms {
                        last.insert("latency_ms".into(), json!(ms));
                    }
                    if let Some(e) = &m.last_error {
                        last.insert("error".into(), json!(e));
                    }
                    v.insert("last_check".into(), Value::Object(last));
                }
                // Windows without a check are left out, not null.
                let mut windows = uptime::uptime_windows(c, m.id, now)?;
                if let Value::Object(w) = &mut windows {
                    w.retain(|_, v| !v.is_null());
                }
                if windows.as_object().is_some_and(|w| !w.is_empty()) {
                    v.insert("uptime".into(), windows);
                }
                let days = uptime::daily(c, m.id, now, 2)?;
                let latency: Map<String, Value> = ["yesterday", "today"]
                    .iter()
                    .zip(&days)
                    .filter(|(_, d)| !d["p50_ms"].is_null())
                    .map(|(k, d)| (k.to_string(), json!({"p50_ms": d["p50_ms"], "p95_ms": d["p95_ms"]})))
                    .collect();
                if !latency.is_empty() {
                    v.insert("latency".into(), Value::Object(latency));
                }
                if !m.enabled {
                    v.insert("enabled".into(), json!(false));
                }
                if m.public {
                    v.insert("public".into(), json!(true));
                }
                out.push(Value::Object(v));
            }
            let incidents: Vec<Value> = uptime::incidents(c, env.id, limit, false)?
                .into_iter()
                .filter(|i| only.as_deref().is_none_or(|n| n.eq_ignore_ascii_case(&i.monitor)))
                .map(|i| {
                    let mut v = Map::new();
                    v.insert("monitor".into(), json!(i.monitor));
                    v.insert("started_at".into(), t(i.started_at));
                    match i.ended_at {
                        Some(e) => v.insert("ended_at".into(), t(e)),
                        None => v.insert("ongoing".into(), json!(true)),
                    };
                    v.insert("duration_seconds".into(), json!(i.duration_secs(now)));
                    v.insert("cause".into(), json!(i.cause));
                    if let Some(d) = i.detail {
                        v.insert("detail".into(), json!(d));
                    }
                    Value::Object(v)
                })
                .collect();
            Ok((out, incidents, uptime::status_page(c, env.id)?))
        })?;
        let mut v = Map::new();
        v.insert("environment".into(), json!(env.name));
        v.insert("monitors".into(), Value::Array(monitors.clone()));
        v.insert("incidents".into(), Value::Array(incidents));
        if let Some(p) = page.filter(|p| p.enabled) {
            v.insert("status_page".into(), self.url(&format!("/status/{}", p.slug)));
        }
        v.insert("url".into(), self.url(&format!("/apps/{}/envs/{}/uptime", env.application_id, env.id)));
        if monitors.is_empty() {
            let hint = if only.is_some() {
                "No monitor by that name: call uptime without monitor to list them."
            } else {
                "No uptime monitors in this environment: an admin adds them on the Uptime page (url)."
            };
            v.insert("hint".into(), json!(hint));
        }
        Ok(Value::Object(v).to_string())
    }
}
