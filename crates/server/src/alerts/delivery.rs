//! Port of Alerts::Delivery (railwatch-cloud app/models/alerts/delivery.rb):
//! the text of an alert, and its webhook (HMAC-signed), Slack (Block Kit)
//! and email renderings. Linear is not ported (out of the spike; an alert
//! for a linear integration fails with a message saying so).

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension};
use sha2::Sha256;

use super::as_hash;
use super::outbound_http::{self, Kind};
use super::outbox::Claimed;
use crate::grouping::issue::{self, Issue};
use crate::rjson::{self, Value};

/// Where links point when the server did not configure alerting (tests):
/// RAILWATCH_PUBLIC_URL, default http://127.0.0.1:8790. A running server
/// uses its configured `url` (Alerting::url).
pub fn host_url() -> String {
    std::env::var("RAILWATCH_PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:8790".into()).trim_end_matches('/').to_string()
}

fn heading(event: &str) -> String {
    match event {
        "new_issue" => "New issue",
        "regressed_issue" => "Regressed issue",
        "resolved_issue" => "Resolved issue",
        "ignored_issue" => "Ignored issue",
        "assigned_issue" => "Issue assigned",
        "threshold" => "Threshold exceeded",
        "anomaly" => "Anomaly detected",
        "silent_host" => "Silent host",
        "quota" => "Quota",
        "crash_free_drop" => "Crash-free sessions dropped",
        "silent_environment" => "Silent environment",
        "environment_recovered" => "Environment reporting again",
        other => return humanize(other),
    }
    .to_string()
}

/// String#humanize for snake_case event names.
fn humanize(s: &str) -> String {
    let t = s.replace('_', " ");
    let mut c = t.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn severity_default(event: &str) -> Option<&'static str> {
    match event {
        "new_issue" | "regressed_issue" | "threshold" | "anomaly" | "quota" => Some("normal"),
        "silent_host" | "crash_free_drop" | "silent_environment" => Some("high"),
        _ => None,
    }
}

pub struct Context {
    pub alert: Claimed,
    pub issue: Option<Issue>,
    pub issue_key: Option<String>,
    pub environment_display: Option<String>,
    pub integration_kind: String,
    pub enabled: bool,
    pub settings: Value,
    pub settings_error: Option<String>,
    pub base_url: String,
    /// What the webhook adds for an agent (agent_context), read from the
    /// environment's telemetry by `enrich` once the control lock is free.
    pub agent: Option<AgentContext>,
}

/// The issue's application and environment, and what its telemetry says:
/// enough for an agent to start on the issue without another call.
#[derive(Default)]
pub struct AgentContext {
    pub account_slug: String,
    pub application_slug: String,
    pub application_id: i64,
    pub environment: String,
    environment_slug: String,
    /// The occurrence the webhook describes: frames collapsed as get_issue
    /// shows them, plus where it happened.
    pub occurrence: Option<serde_json::Value>,
    /// get_execution's summary of the execution it happened in.
    pub execution_summary: Option<serde_json::Value>,
}

fn pstr(p: &Value, k: &str) -> Option<String> {
    match p.get(k) {
        Value::Nil => None,
        v => Some(String::from_utf8_lossy(&rjson::ruby::to_s(v)).into_owned()).filter(|s| !rjson::ruby::blank(s.as_bytes())),
    }
}

impl Context {
    /// The claimed alert with its issue and integration. `cipher` reads the
    /// integration's encrypted secrets (alerts::integration); a secret that
    /// cannot be read leaves `settings_error` set and delivery fails with it.
    pub fn load(c: &Connection, alerting: Option<&super::integration::Alerting>, alert: Claimed) -> rusqlite::Result<Context> {
        let cipher = alerting.map(|a| &a.cipher);
        let issue = match alert.issue_id {
            Some(id) => issue::find(c, id)?,
            None => None,
        };
        let issue_key = issue.as_ref().map(|i| issue::key(c, i));
        let environment_display = issue.as_ref().map(|i| issue::environment_name(c, i.environment_id));
        let integration = super::integration::find(c, cipher, alert.integration_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let settings = rjson::parse(serde_json::Value::Object(integration.settings).to_string().as_bytes()).unwrap_or(Value::Hash(rjson::Hash::new()));
        let agent = match (&issue, integration.kind.as_str()) {
            (Some(i), "webhook") => agent_names(c, i)?,
            _ => None,
        };
        Ok(Context {
            alert,
            issue,
            issue_key,
            environment_display,
            integration_kind: integration.kind,
            enabled: integration.enabled,
            settings,
            settings_error: integration.settings_error,
            base_url: alerting.map_or_else(host_url, |a| a.url.trim_end_matches('/').to_string()),
            agent,
        })
    }

    /// Reads the occurrence and its execution summary from the issue's
    /// telemetry into `agent`, for webhooks. Best effort: a webhook still
    /// goes out without them (an environment with no telemetry yet, or
    /// occurrences past retention), and a read error is logged, not fatal.
    pub fn enrich(&mut self, tenants_dir: &std::path::Path) {
        let (Some(issue), Some(agent)) = (&self.issue, self.agent.as_mut()) else { return };
        if crate::storage::Layout::detect(tenants_dir, &agent.environment_slug).is_none() {
            return;
        }
        let read = || -> rusqlite::Result<(Option<serde_json::Value>, Option<serde_json::Value>)> {
            let t = crate::dashboard::pool::open_tenant_reader(tenants_dir, &agent.environment_slug)?;
            let sample: serde_json::Value = serde_json::from_str(&rjson::generate(&issue.sample).unwrap_or_default()).unwrap_or_default();
            let occurrence = if issue.kind == "exception" {
                let (first, last) = crate::web::mcp::issue_exception_occurrences(&t, &issue.group_hash, &sample)?;
                // The newest occurrence: the one that fired the alert.
                last.or(first).map(|e| occurrence_for_agent(&crate::web::mcp::exception_occurrence(&e)))
            } else {
                None
            };
            let execution_id = occurrence
                .as_ref()
                .and_then(|o| o.get("execution_id"))
                .or_else(|| sample.get("execution_id"))
                .and_then(serde_json::Value::as_str)
                .filter(|e| !e.is_empty())
                .map(str::to_string);
            let summary = match execution_id {
                Some(eid) => execution_summary(&t, &eid)?,
                None => None,
            };
            Ok((occurrence, summary))
        };
        match read() {
            Ok((occurrence, summary)) => {
                agent.occurrence = occurrence;
                agent.execution_summary = summary;
            }
            Err(e) => tracing::warn!(alert = self.alert.id, issue = issue.id, error = %e, "webhook: issue telemetry unreadable; sending without it"),
        }
    }

    fn payload(&self) -> &Value {
        &self.alert.payload
    }

    fn heading(&self) -> String {
        if self.payload().get("summary").truthy() {
            return "Alert burst".into();
        }
        let h = heading(&self.alert.event);
        match (self.alert.event.as_str(), pstr(self.payload(), "assignee")) {
            ("assigned_issue", Some(a)) => format!("{h} to {a}"),
            _ => h,
        }
    }

    fn environment(&self) -> Option<String> {
        self.environment_display.clone().filter(|s| !s.is_empty()).or_else(|| pstr(self.payload(), "environment"))
    }

    fn message(&self) -> String {
        if let Some(m) = pstr(self.payload(), "message") {
            return m;
        }
        match self.environment() {
            Some(w) => format!("no details recorded ({w})"),
            None => "no details recorded".into(),
        }
    }

    pub fn severity(&self) -> Option<String> {
        self.issue.as_ref().map(|i| i.priority.clone()).filter(|p| !p.is_empty()).or_else(|| severity_default(&self.alert.event).map(String::from))
    }

    /// Delivery#text_for
    pub fn text(&self) -> String {
        let prefix = if self.payload().get("test").truthy() { "[TEST] " } else { "" };
        if let (Some(i), Some(key)) = (&self.issue, &self.issue_key) {
            return format!("{prefix}{} {key} in {}: {}", self.heading(), self.environment().unwrap_or_default(), i.title);
        }
        format!("{prefix}{}: {}", self.heading(), self.message())
    }

    pub fn url(&self) -> Option<String> {
        self.issue.as_ref().map(|i| format!("{}/issues/{}", self.base_url, i.id))
    }

    pub fn link(&self) -> String {
        self.url().unwrap_or_else(|| format!("{}/alerts", self.base_url))
    }

    /// The webhook's JSON (docs/agent-webhooks.md). `alert_id` is the
    /// alert's delivery key: the same on every retry of one alert, so a
    /// receiver dedupes on it (also sent as X-Railwatch-Event-Id).
    pub fn webhook_body(&self) -> Value {
        let deploy = self.issue.as_ref().and_then(|i| pstr(&i.sample, "deploy")).or_else(|| pstr(self.payload(), "deploy"));
        let issue = self.issue.as_ref().map(|i| {
            let mut h = as_hash(crate::rhash! {
                "id" => i.id, "number" => i.number, "title" => i.title.clone(), "status" => i.status.clone(),
                "kind" => i.kind.clone(), "culprit" => i.culprit.clone(), "occurrences" => i.occurrences,
                "key" => self.issue_key.clone(), "priority" => i.priority.clone(), "affected_users" => i.affected_users,
                "first_seen_at" => crate::time::iso8601(i.first_seen_at.div_euclid(1_000_000)),
                "last_seen_at" => crate::time::iso8601(i.last_seen_at.div_euclid(1_000_000)),
                "deploy" => deploy.clone(),
            });
            if let Some(a) = &self.agent {
                h.insert("canonical_key".into(), format!("{}/{}/{}", a.account_slug, a.application_id, self.issue_key.clone().unwrap_or_default()).into());
                h.insert("application".into(), a.application_slug.clone().into());
                h.insert("application_id".into(), a.application_id.into());
                h.insert("environment".into(), a.environment.clone().into());
            }
            Value::Hash(h)
        });
        let mut body = as_hash(crate::rhash! {
            "alert_id" => self.alert.delivery_key.clone(),
            "event" => self.alert.event.clone(),
            "issue" => issue,
            "issue_key" => self.issue_key.clone(),
            "url" => self.url(),
            "payload" => self.payload().clone(),
            "sent_at" => crate::time::iso8601(crate::time::now_micros() / 1_000_000),
        });
        if let (Some(_), Some(key)) = (&self.issue, &self.issue_key) {
            body.insert("mcp".into(), self.mcp_hint(key));
            if let Some(summary) = self.agent_summary() {
                body.insert("summary".into(), summary);
            }
        }
        Value::Hash(body)
    }

    /// Where an agent continues: the server's MCP endpoint and the call
    /// that investigates this issue.
    fn mcp_hint(&self, key: &str) -> Value {
        let canonical = self.agent.as_ref().map(|a| format!("{}/{}/{key}", a.account_slug, a.application_id));
        crate::rhash! {
            "endpoint" => format!("{}/mcp", self.base_url),
            "tool" => "investigate_issue",
            "arguments" => crate::rhash! { "key" => canonical.unwrap_or_else(|| key.to_string()) },
            "fallback_tool" => "get_issue",
        }
    }

    /// The occurrence (collapsed frames) and the execution summary, as the
    /// MCP tools shape them, cut to SUMMARY_MAX_BYTES of JSON.
    fn agent_summary(&self) -> Option<Value> {
        let a = self.agent.as_ref()?;
        if a.occurrence.is_none() && a.execution_summary.is_none() {
            return None;
        }
        let mut v = serde_json::Map::new();
        if let Some(o) = &a.occurrence {
            v.insert("occurrence".into(), o.clone());
        }
        if let Some(e) = &a.execution_summary {
            v.insert("execution".into(), e.clone());
        }
        let v = serde_json::Value::Object(v);
        let text = crate::web::mcp_summary::cap(v.to_string(), SUMMARY_MAX_BYTES, SUMMARY_HINT);
        rjson::parse(text.as_bytes()).ok()
    }

    pub fn slack_body(&self) -> Value {
        let sev = self.severity();
        let text = mrkdwn(&self.text());
        let line = match &sev {
            Some(s) => format!("[{s}] {text}"),
            None => text,
        };
        let icon = match self.alert.event.as_str() {
            "resolved_issue" => Some("✅"),
            "ignored_issue" => Some("🔕"),
            "environment_recovered" => Some("✅"),
            _ => match sev.as_deref() {
                Some("urgent") => Some("🚨"),
                Some("high") => Some("🔴"),
                Some("normal") => Some("🟠"),
                Some("low") => Some("⚪"),
                _ => None,
            },
        };
        let parts: Vec<String> =
            [Some(self.heading()), self.issue.as_ref().map(|i| i.kind.clone()), self.environment()].into_iter().flatten().filter(|s| !s.is_empty()).collect();
        let mut header = [icon.map(String::from), Some(parts.join(" · "))].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        if self.payload().get("test").truthy() {
            header = format!("[TEST] {header}");
        }
        let summary = match (&self.issue, &self.issue_key) {
            (Some(i), Some(k)) => {
                let mut lines = vec![format!("*{k}*  {}", mrkdwn(&i.title))];
                if let Some(c) = i.culprit.as_ref().filter(|c| !c.is_empty()) {
                    lines.push(code(c.rsplit("/gems/").next().unwrap_or(c)));
                }
                lines.join("\n")
            }
            _ => mrkdwn(&self.message()),
        };
        let mut context: Vec<String> = Vec::new();
        if let Some(s) = &sev {
            context.push(format!("{s} priority"));
        }
        if let Some(i) = &self.issue {
            if i.occurrences > 0 {
                context.push(format!("{} occurrence{}", i.occurrences, if i.occurrences == 1 { "" } else { "s" }));
            }
            if i.affected_users > 0 {
                context.push(format!("{} user{} affected", i.affected_users, if i.affected_users == 1 { "" } else { "s" }));
            }
        }
        let deploy = self.issue.as_ref().and_then(|i| pstr(&i.sample, "deploy")).or_else(|| pstr(self.payload(), "deploy"));
        if let Some(d) = deploy {
            context.push(format!("Deploy {}", code(&d)));
        }
        if let Some(i) = &self.issue {
            context.push(format!("First seen {}", first_seen(i.first_seen_at)));
        }
        if self.payload().get("test").truthy() {
            context.push("Sent by *Send test* — no alert rule fired".into());
        }
        let mut blocks = vec![
            crate::rhash! { "type" => "header", "text" => crate::rhash! { "type" => "plain_text", "text" => first(&header, 150) } },
            crate::rhash! { "type" => "section", "text" => crate::rhash! { "type" => "mrkdwn", "text" => first(&summary, 2000) } },
        ];
        if !context.is_empty() {
            blocks.push(crate::rhash! { "type" => "context", "elements" => vec![crate::rhash! { "type" => "mrkdwn", "text" => context.join("  ·  ") }] });
        }
        blocks.push(crate::rhash! { "type" => "actions", "elements" => vec![crate::rhash! {
            "type" => "button", "text" => crate::rhash! { "type" => "plain_text", "text" => "Open in Railwatch" }, "url" => self.link(),
        }] });
        crate::rhash! { "text" => format!("{line}\n{}", self.link()), "blocks" => blocks }
    }
}

/// The most JSON the webhook's `summary` may have: a webhook is not a tool
/// result, and the receivers (a GitHub dispatch's client_payload is 64 KB at
/// most) are smaller. The rest is one MCP call away.
pub const SUMMARY_MAX_BYTES: usize = 16_000;

const SUMMARY_HINT: &str = "Call the tool in `mcp` (investigate_issue, or get_issue then get_execution) for everything.";

/// The issue's account, application and environment, for webhooks.
fn agent_names(c: &Connection, i: &Issue) -> rusqlite::Result<Option<AgentContext>> {
    c.query_row(
        "SELECT ac.slug, ap.slug, ap.id, e.name, e.slug FROM environments e JOIN applications ap ON ap.id = e.application_id
         JOIN accounts ac ON ac.id = ap.account_id WHERE e.id = ?",
        [i.environment_id],
        |r| {
            Ok(AgentContext {
                account_slug: r.get(0)?,
                application_slug: r.get(1)?,
                application_id: r.get(2)?,
                environment: r.get(3)?,
                environment_slug: r.get(4)?,
                ..Default::default()
            })
        },
    )
    .optional()
}

/// get_execution's `summary` of the execution.
fn execution_summary(t: &Connection, execution_id: &str) -> rusqlite::Result<Option<serde_json::Value>> {
    crate::web::mcp::execution_summary_of(t, execution_id)
}

/// An exception occurrence for an agent: what get_issue shows, frames
/// collapsed, without the browser trail and locals (in the MCP result).
fn occurrence_for_agent(o: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for k in ["class_name", "message", "file", "line", "handled", "occurred_at", "deploy", "execution_id", "execution_source", "execution_preview", "cause"] {
        if let Some(v) = o.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(f) = o.get("frames") {
        out.insert("frames".into(), crate::web::mcp_summary::collapse_frames(f));
    }
    serde_json::Value::Object(out)
}

fn first(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn mrkdwn(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn code(s: &str) -> String {
    format!("`{}`", mrkdwn(s).replace('`', ""))
}

/// strftime("%b %-d %H:%M UTC")
fn first_seen(micros: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let s = crate::time::utc_seconds_string(micros.div_euclid(1_000_000));
    let m: usize = s[5..7].parse().unwrap_or(1);
    let d: u32 = s[8..10].parse().unwrap_or(1);
    format!("{} {d} {} UTC", MONTHS[m - 1], &s[11..16])
}

pub fn sign(secret: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body.as_bytes());
    let hex: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// The headers a delivery carries: the event id (the alert's delivery
/// key, stable across retries), the time it was signed, and with a secret
/// the two signatures (module docs; docs/agent-webhooks.md).
pub fn webhook_headers(json: &str, secret: Option<&str>, event_id: &str, now_secs: i64) -> Vec<(&'static str, String)> {
    let mut headers = vec![("User-Agent", "Railwatch-Alerts/1.0".to_string())];
    if !event_id.is_empty() {
        headers.push(("X-Railwatch-Event-Id", event_id.to_string()));
    }
    headers.push(("X-Railwatch-Timestamp", now_secs.to_string()));
    if let Some(s) = secret.filter(|s| !s.is_empty()) {
        headers.push(("X-Railwatch-Signature", sign(s, json)));
        headers.push(("X-Railwatch-Signature-256", sign(s, &format!("{now_secs}.{json}"))));
    }
    headers
}

async fn post(url: &str, body: &Value, secret: Option<&str>, event_id: &str, kind: Kind) -> Result<(), String> {
    let json = rjson::generate(body).map_err(|e| e.0)?;
    let headers = webhook_headers(&json, secret, event_id, crate::time::now_micros() / 1_000_000);
    let res = outbound_http::post_json(url, json, &headers, kind).await?;
    if (200..300).contains(&res.status) {
        Ok(())
    } else {
        let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
        Err(format!("{} from {host}", res.status))
    }
}

/// `"#{exception.class}: #{exception.message}"` as Alert#fail_claim! and
/// #deliver! store a delivery failure: the class Alerts::Delivery /
/// OutboundHttp would have raised for the failures it raises by name.
/// Transport errors (Ruby's Errno/OpenSSL classes) keep the bare message.
pub fn error_with_class(message: &str) -> String {
    const UNSAFE: &[&str] = &["destination must be", "destination resolved to", "destination hostname", "Slack destination must be"];
    let class = if message.starts_with("destination response exceeded") {
        "Alerts::OutboundHttp::ResponseTooLarge"
    } else if message == "destination response timed out" {
        "Alerts::OutboundHttp::ResponseTimeout"
    } else if UNSAFE.iter().any(|p| message.starts_with(p)) {
        "Alerts::OutboundHttp::UnsafeDestination"
    } else if message.split_once(" from ").is_some_and(|(code, _)| code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit())) {
        // `raise "#{res.code} from #{host}"`
        "RuntimeError"
    } else {
        return message.to_string();
    };
    format!("{class}: {message}")
}

/// Integration#deliver. Email goes through the server's SMTP settings
/// (`smtp_*` / RAILWATCH_SMTP_*, the same as sign-in mail); without SMTP an
/// email alert fails visibly.
pub async fn deliver(ctx: &Context, smtp: Option<&crate::config::Smtp>) -> Result<(), String> {
    if let Some(e) = &ctx.settings_error {
        return Err(e.clone());
    }
    let s = |k: &str| pstr(&ctx.settings, k);
    match ctx.integration_kind.as_str() {
        "webhook" => post(&s("url").ok_or("webhook has no url")?, &ctx.webhook_body(), s("secret").as_deref(), &ctx.alert.delivery_key, Kind::Webhook).await,
        "slack" => post(&s("webhook_url").ok_or("slack has no webhook_url")?, &ctx.slack_body(), None, "", Kind::Slack).await,
        "email" => send_email(&s("to").ok_or("email has no recipient")?, ctx, smtp).await,
        "linear" => Err("Linear delivery is not supported by railwatch-rs".into()),
        other => Err(format!("unknown integration kind {other}")),
    }
}

/// AlertMailer#issue_alert: plain text, subject "[Railwatch] <text>".
async fn send_email(to: &str, ctx: &Context, smtp: Option<&crate::config::Smtp>) -> Result<(), String> {
    let smtp = smtp.ok_or("email alerts need SMTP: set smtp_address (RAILWATCH_SMTP_ADDRESS) and friends; see `railwatch doctor`")?;
    let text = ctx.text();
    let mail =
        crate::web::mailer::Mail { to: to.to_string(), subject: format!("[Railwatch] {}", first(&text, 120)), text: format!("{text}\n\n{}\n", ctx.link()) };
    crate::web::mailer::Mailer::new(smtp)?.send(&mail).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_hex_hmac_sha256_like_openssl() {
        // OpenSSL::HMAC.hexdigest("SHA256", "key", "The quick brown fox jumps over the lazy dog")
        assert_eq!(sign("key", "The quick brown fox jumps over the lazy dog"), "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8");
    }

    #[test]
    fn the_timestamped_signature_covers_the_time_and_the_event_id_is_sent() {
        let body = r#"{"event":"new_issue"}"#;
        let h = webhook_headers(body, Some("s3cret"), "4f1c-key", 1_792_000_000);
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("X-Railwatch-Event-Id"), Some("4f1c-key"));
        assert_eq!(get("X-Railwatch-Timestamp"), Some("1792000000"));
        assert_eq!(get("X-Railwatch-Signature"), Some(sign("s3cret", body).as_str()), "the body-only signature is unchanged");
        // What a receiver computes: HMAC over "<timestamp>.<body>". Same
        // vector as `printf '1792000000.{"event":"new_issue"}' | openssl dgst -sha256 -hmac s3cret`.
        assert_eq!(get("X-Railwatch-Signature-256"), Some("sha256=8b012c03658598c25b9b059d35186b605ad0a3a2713f989e8465125d4931aaa0"));
        let unsigned = webhook_headers(body, None, "k", 1);
        assert!(unsigned.iter().all(|(n, _)| !n.starts_with("X-Railwatch-Signature")), "no secret, no signature");
        assert!(webhook_headers(body, Some(""), "k", 1).iter().all(|(n, _)| !n.starts_with("X-Railwatch-Signature")));
    }

    /// An account, an application (SHOP), its production environment with
    /// a telemetry file, a webhook integration, and an exception issue whose
    /// occurrence has `in_app` frames of `code_len` characters of code each.
    fn seeded(name: &str, in_app: usize, code_len: usize) -> (std::path::PathBuf, crate::control::Control, i64, i64) {
        use crate::control::{Control, create_account, create_environment, insert_application, insert_user, test_password};
        let dir = std::env::temp_dir().join(format!("rw-webhook-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let c = Control::open(&dir.join("control.sqlite3")).unwrap();
        let ada = c.tx(|tx| insert_user(tx, "Ada", "ada@example.com", &test_password(), true)).unwrap();
        let account = c.tx(|tx| create_account(tx, "Acme", ada)).unwrap();
        let app = c.tx(|tx| insert_application(tx, account, "Shop", "shop", "SHOP", None, "main")).unwrap();
        let (env, _) = c.tx(|tx| create_environment(tx, app, "production")).unwrap();
        let slug: String = c.with(|x| x.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0))).unwrap();
        let t = crate::tenants::open_tenant(&dir.join("telemetry").join(format!("{slug}.sqlite3"))).unwrap();
        let mut frames = vec![serde_json::json!({"file": "/gems/activerecord-8.1.4/lib/x.rb", "line": 1, "function": "raise_it", "in_app": false})];
        for i in 0..in_app {
            frames.push(serde_json::json!({"file": format!("app/models/order{i}.rb"), "line": 31, "function": "total", "in_app": true,
                                           "code": {"31": "x".repeat(code_len)}}));
        }
        for i in 0..40 {
            frames.push(serde_json::json!({"file": format!("/gems/actionpack-8.1.4/lib/a{i}.rb"), "line": i, "function": "f", "in_app": false}));
        }
        t.execute(
            "INSERT INTO exceptions (id, class_name, message, occurred_at, group_hash, frames, execution_id, execution_source, deploy)
             VALUES (7, 'ZeroDivisionError', 'divided by 0', '2026-10-25 12:00:00', 'gx', ?1, 'e-1', 'request', 'abc1234')",
            [serde_json::Value::Array(frames).to_string()],
        )
        .unwrap();
        // The request it was raised in, with an N+1 for its summary.
        t.execute(
            "INSERT INTO executions (execution_id, kind, name, status, occurred_at, duration) VALUES ('e-1', 'request', 'GET /orders/:id', 500, '2026-10-25 11:59:59', 812000)",
            [],
        )
        .unwrap();
        t.execute(
            "INSERT INTO n_plus_ones (execution_id, occurred_at, count, sql, group_hash, source) VALUES ('e-1', '2026-10-25 11:59:59.5', 12, 'SELECT \"line_items\".* FROM \"line_items\" WHERE \"line_items\".\"order_id\" = ?', 'gl', 'app/views/orders/show.html.erb:7')",
            [],
        )
        .unwrap();
        let cipher = super::super::integration::SettingsCipher::new(b"k");
        let mut settings = serde_json::Map::new();
        settings.insert("url".into(), "https://hooks.example.com/railwatch".into());
        settings.insert("secret".into(), "s3cret".into());
        let integration = c.with(|x| super::super::integration::insert(x, &cipher, account, "webhook", "agent", &settings)).unwrap();
        let issue = c
            .with(|x| {
                x.execute(
                    "INSERT INTO issues (application_id, environment_id, number, title, kind, status, priority, group_hash, occurrences, affected_users,
                       first_seen_at, last_seen_at, created_at, updated_at, sample, culprit)
                     VALUES (?1, ?2, 12, 'ZeroDivisionError: divided by 0', 'exception', 'open', 'normal', 'gx', 4, 2,
                       '2026-10-25 11:00:00', '2026-10-25 12:00:00', '2026-10-25 11:00:00', '2026-10-25 12:00:00', ?3, 'app/models/order.rb:31')",
                    rusqlite::params![app, env, r#"{"exception_id":7,"execution_id":"e-1","deploy":"abc1234"}"#],
                )?;
                Ok(x.last_insert_rowid())
            })
            .unwrap();
        c.configure_alerting(super::super::integration::Alerting { cipher, smtp: None, url: "https://rw.example.com/monitoring".into(), deliver: true });
        (dir, c, integration, issue)
    }

    /// What deliver_pass builds for one new_issue alert: claimed, loaded,
    /// enriched from telemetry. The body as it is sent, and its delivery key.
    fn delivered(c: &crate::control::Control, dir: &std::path::Path, integration: i64, issue: i64) -> (serde_json::Value, String, Context) {
        let conn = c.conn();
        let payload = crate::rhash! { "environment" => "production" };
        super::super::outbox::create(&conn, None, integration, "new_issue", Some(issue), &payload, "pending", None).unwrap();
        let claimed = super::super::outbox::claim_due(&conn, 10).unwrap().pop().unwrap();
        let mut ctx = Context::load(&conn, c.alerting(), claimed).unwrap();
        drop(conn);
        ctx.enrich(&dir.join("telemetry"));
        let json = rjson::generate(&ctx.webhook_body()).unwrap();
        let key = ctx.alert.delivery_key.clone();
        (serde_json::from_str(&json).unwrap(), key, ctx)
    }

    #[test]
    fn a_new_issue_webhook_carries_what_an_agent_needs() {
        let (dir, c, integration, issue) = seeded("payload", 1, 40);
        let (v, key, ctx) = delivered(&c, &dir, integration, issue);
        assert_eq!(v["alert_id"], key.as_str(), "the event id is the alert's delivery key");
        assert_eq!(v["event"], "new_issue");
        let i = &v["issue"];
        assert_eq!(i["key"], "SHOP-12");
        assert_eq!(i["canonical_key"], "acme/1/SHOP-12");
        assert_eq!((i["application"].as_str(), i["environment"].as_str()), (Some("shop"), Some("production")));
        assert_eq!((i["kind"].as_str(), i["culprit"].as_str(), i["occurrences"].as_i64()), (Some("exception"), Some("app/models/order.rb:31"), Some(4)));
        assert_eq!((i["first_seen_at"].as_str(), i["last_seen_at"].as_str()), (Some("2026-10-25T11:00:00Z"), Some("2026-10-25T12:00:00Z")));
        assert_eq!(i["deploy"], "abc1234");
        assert_eq!(v["url"], format!("https://rw.example.com/monitoring/issues/{issue}"));
        assert_eq!(
            v["mcp"],
            serde_json::json!({"endpoint": "https://rw.example.com/monitoring/mcp", "tool": "investigate_issue",
                               "arguments": {"key": "acme/1/SHOP-12"}, "fallback_tool": "get_issue"})
        );
        // The occurrence as get_issue shows it: the raising frame, the
        // in-app frame with its code, the 40 framework frames as one entry.
        let o = &v["summary"]["occurrence"];
        assert_eq!(
            (o["class_name"].as_str(), o["message"].as_str(), o["execution_id"].as_str()),
            (Some("ZeroDivisionError"), Some("divided by 0"), Some("e-1"))
        );
        let frames = o["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 3, "{frames:?}");
        assert_eq!(frames[1]["file"], "app/models/order0.rb");
        assert_eq!(frames[2], serde_json::json!({"omitted_frames": 40, "gems": ["actionpack"]}));
        // And the summary of the request it was raised in, as get_execution leads with.
        let e = &v["summary"]["execution"];
        assert_eq!(e["time_ms"]["duration"].as_f64(), Some(812.0));
        assert_eq!(e["n_plus_ones"][0]["source"], "app/views/orders/show.html.erb:7");
        assert_eq!(e["n_plus_ones"][0]["fix"], "Order.includes(:line_items)");
        // Signed over exactly the bytes sent.
        let json = rjson::generate(&ctx.webhook_body()).unwrap();
        let h = webhook_headers(&json, Some("s3cret"), &key, 1_792_000_000);
        assert!(h.contains(&("X-Railwatch-Signature", sign("s3cret", &json))));
        assert!(h.contains(&("X-Railwatch-Event-Id", key.clone())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_is_capped_and_the_event_id_survives_a_retry() {
        // 60 in-app frames of 2 KB of code: ~120 KB before the cap.
        let (dir, c, integration, issue) = seeded("cap", 60, 2_000);
        let (v, key, _) = delivered(&c, &dir, integration, issue);
        let summary = v["summary"].to_string();
        assert!(summary.len() <= SUMMARY_MAX_BYTES, "{} bytes", summary.len());
        assert!(v["summary"]["truncated"]["hint"].as_str().unwrap().contains("investigate_issue"), "{}", &summary[..300]);
        assert!(v["summary"]["result"]["occurrence"]["class_name"] == "ZeroDivisionError", "what fits is kept");
        assert!(v.to_string().len() < 20_000, "the whole body stays small: {}", v.to_string().len());
        // A failed attempt is retried with the same delivery key.
        {
            let conn = c.conn();
            conn.execute("UPDATE alerts SET status = 'pending', next_delivery_at = NULL, delivery_lease_id = NULL, delivery_lease_expires_at = NULL", [])
                .unwrap();
            let again = super::super::outbox::claim_due(&conn, 10).unwrap();
            assert_eq!(again.iter().map(|a| a.delivery_key.as_str()).collect::<Vec<_>>(), [key.as_str()]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_issue_past_retention_still_gets_its_webhook_without_a_summary() {
        let (dir, c, integration, issue) = seeded("retention", 1, 10);
        std::fs::remove_dir_all(dir.join("telemetry")).unwrap();
        let (v, _, _) = delivered(&c, &dir, integration, issue);
        assert_eq!(v["issue"]["key"], "SHOP-12");
        assert!(v.get("summary").is_none(), "no telemetry, no summary: {v}");
        assert_eq!(v["mcp"]["tool"], "investigate_issue");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mrkdwn_escapes_slack_control_sequences() {
        assert_eq!(mrkdwn("<!channel> & <https://x|y>"), "&lt;!channel&gt; &amp; &lt;https://x|y&gt;");
        assert_eq!(code("a`b<c"), "`ab&lt;c`");
        assert_eq!(first_seen(1_791_907_200_000_000), "Oct 13 16:00 UTC");
    }
}
