//! Port of Alerts::Delivery (railwatch-cloud app/models/alerts/delivery.rb):
//! the text of an alert, and its webhook (HMAC-signed), Slack (Block Kit)
//! and email renderings. Linear is not ported (out of the spike; an alert
//! for a linear integration fails with a message saying so).

use hmac::{Hmac, Mac};
use rusqlite::Connection;
use sha2::Sha256;

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
        })
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

    pub fn webhook_body(&self) -> Value {
        let issue = self.issue.as_ref().map(|i| {
            crate::rhash! {
                "id" => i.id, "number" => i.number, "title" => i.title.clone(), "status" => i.status.clone(),
                "kind" => i.kind.clone(), "culprit" => i.culprit.clone(), "occurrences" => i.occurrences,
            }
        });
        crate::rhash! {
            "alert_id" => self.alert.delivery_key.clone(),
            "event" => self.alert.event.clone(),
            "issue" => issue,
            "issue_key" => self.issue_key.clone(),
            "url" => self.url(),
            "payload" => self.payload().clone(),
            "sent_at" => crate::time::iso8601(crate::time::now_micros() / 1_000_000),
        }
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

async fn post(url: &str, body: &Value, secret: Option<&str>, kind: Kind) -> Result<(), String> {
    let json = rjson::generate(body).map_err(|e| e.0)?;
    let mut headers = vec![("User-Agent", "Railwatch-Alerts/1.0".to_string())];
    if let Some(s) = secret.filter(|s| !s.is_empty()) {
        headers.push(("X-Railwatch-Signature", sign(s, &json)));
    }
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
        "webhook" => post(&s("url").ok_or("webhook has no url")?, &ctx.webhook_body(), s("secret").as_deref(), Kind::Webhook).await,
        "slack" => post(&s("webhook_url").ok_or("slack has no webhook_url")?, &ctx.slack_body(), None, Kind::Slack).await,
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
    fn mrkdwn_escapes_slack_control_sequences() {
        assert_eq!(mrkdwn("<!channel> & <https://x|y>"), "&lt;!channel&gt; &amp; &lt;https://x|y&gt;");
        assert_eq!(code("a`b<c"), "`ab&lt;c`");
        assert_eq!(first_seen(1_791_907_200_000_000), "Oct 13 16:00 UTC");
    }
}
