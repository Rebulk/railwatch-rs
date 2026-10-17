//! DigestMailer#weekly and its two views (app/views/digest_mailer/weekly.
//! {html,text}.erb inside layouts/mailer.{html,text}.erb), rendered to the
//! same bytes Rails' ERB produces: the whitespace the `<% %>` lines leave,
//! the HTML view's singular/plural words (the text view always says "new
//! issues" and "deploys"), `number_with_delimiter`, Ruby's Float#to_s and
//! ERB::Util.html_escape in the HTML part only (text/plain views are not
//! escaped). bench/diff/digest/ diffs it against Rails.

use crate::dashboard::rb;
use crate::jobs::weekly_digest::EnvDigest;
use crate::rjson::ruby::float_to_s;

/// One rendered digest: multipart/alternative, text then HTML.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DigestMail {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// `DigestMailer.with(user:, account:, digest:).weekly`. `base_url` is the
/// server's public URL (config `url`), the host of every link.
pub fn weekly(base_url: &str, user_name: &str, user_email: &str, account_name: &str, environments: &[EnvDigest]) -> DigestMail {
    let n = environments.len();
    let subject = format!("[Railwatch] {account_name}: your week in {n} environment{}", if n == 1 { "" } else { "s" });
    let dashboard_url = format!("{base_url}/dashboard");
    let profile_url = format!("{base_url}/settings/profile");
    let issue_url = |id: i64| format!("{base_url}/issues/{id}");

    // weekly.text.erb
    let mut text = format!("Hey {user_name},\n\nHere's what happened in {account_name} over the last 7 days.\n");
    for env in environments {
        text += &format!(
            "\n{}\n  {} requests, p95 {}ms, {}% 5xx\n  {} job attempts, {} failed\n  {} new issues, {} resolved, {} deploys\n",
            env.name,
            delimit(env.requests),
            float_to_s(env.p95_ms),
            error_rate(env),
            delimit(env.jobs),
            env.failed_jobs,
            env.new_issues,
            env.resolved_issues,
            env.deploys
        );
        for i in &env.top_issues {
            text += &format!("  {} {} ({}) {}\n", i.key, i.title, delimit(i.occurrences), issue_url(i.id));
        }
    }
    text += &format!("\n\nOpen Railwatch: {dashboard_url}\nTurn off this digest: {profile_url}\n");

    // weekly.html.erb
    let h = html_escape;
    let s = |n: i64| if n == 1 { "" } else { "s" };
    let mut body = format!("<p>Hey {},</p>\n\n<p>Here's what happened in <strong>{}</strong> over the last 7 days.</p>\n\n", h(user_name), h(account_name));
    for env in environments {
        body += &format!(
            "  <h3>{}</h3>\n  <ul>\n    <li>{} requests, p95 {}ms, {}% 5xx</li>\n    <li>{} job attempts, {} failed</li>\n    <li>{} new issue{}, {} resolved, {} deploy{}</li>\n  </ul>\n",
            h(&env.name),
            delimit(env.requests),
            float_to_s(env.p95_ms),
            error_rate(env),
            delimit(env.jobs),
            env.failed_jobs,
            env.new_issues,
            s(env.new_issues),
            env.resolved_issues,
            env.deploys,
            s(env.deploys)
        );
        if !env.top_issues.is_empty() {
            body += "    <p>Most frequent open issues:</p>\n    <ul>\n";
            for i in &env.top_issues {
                body += &format!("        <li><a href=\"{}\">{}</a> {} ({})</li>\n", h(&issue_url(i.id)), h(&i.key), h(&i.title), delimit(i.occurrences));
            }
            body += "    </ul>\n";
        }
    }
    body += &format!("\n<p><a href=\"{}\">Open Railwatch</a> · <a href=\"{}\">Turn off this digest</a></p>\n", h(&dashboard_url), h(&profile_url));

    DigestMail {
        to: user_email.to_string(),
        subject,
        // layouts/mailer.text.erb: `<%= yield %>` and a newline.
        text: text + "\n",
        html: format!(
            "<!DOCTYPE html>\n<html>\n  <head>\n    <meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\">\n    <style>\n      /* Email styles need to be inline */\n    </style>\n  </head>\n\n  <body>\n    {body}\n  </body>\n</html>\n"
        ),
    }
}

/// `requests[:count].zero? ? 0 : (errors * 100.0 / count).round(2)`: the
/// Integer 0 renders "0", a Float always has a fraction ("0.0", "14.14").
fn error_rate(env: &EnvDigest) -> String {
    if env.requests == 0 { "0".into() } else { float_to_s(rb::round(env.failed_requests as f64 * 100.0 / env.requests as f64, 2)) }
}

/// `number_with_delimiter` for an Integer: "24859" -> "24,859".
pub fn delimit(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// ERB::Util.html_escape.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::weekly_digest::TopIssue;

    #[test]
    fn number_with_delimiter() {
        assert_eq!(delimit(0), "0");
        assert_eq!(delimit(999), "999");
        assert_eq!(delimit(1000), "1,000");
        assert_eq!(delimit(24_859), "24,859");
        assert_eq!(delimit(1_234_567), "1,234,567");
        assert_eq!(delimit(-1234), "-1,234");
    }

    #[test]
    fn html_escape_is_erb_utils() {
        assert_eq!(html_escape(r#"a&b<c>"d'e"#), "a&amp;b&lt;c&gt;&quot;d&#39;e");
    }

    fn env(name: &str, requests: i64, failed_requests: i64, p95_ms: f64, top: Vec<TopIssue>) -> EnvDigest {
        EnvDigest {
            name: name.into(),
            requests,
            failed_requests,
            p95_ms,
            jobs: 6196,
            failed_jobs: 0,
            new_issues: 1,
            resolved_issues: 0,
            deploys: 0,
            top_issues: top,
        }
    }

    /// Bytes captured from Rails (bench/diff/digest, rw-baseline @ 464f0fd)
    /// for two of its environments, the server URL normalised.
    #[test]
    fn renders_what_rails_renders() {
        let top = |id, key: &str, title: &str, occ| TopIssue { id, key: key.into(), title: title.into(), occurrences: occ };
        let envs = vec![
            env("bench · env-0001", 24_859, 3515, 379.8, vec![top(1, "BENC-1", "ArgumentError: kaboom", 1768)]),
            EnvDigest { jobs: 0, new_issues: 2, deploys: 1, ..env("Shop <&> · staging", 0, 0, 0.0, vec![]) },
        ];
        let m = weekly("https://rw.example.com", "Props Diff", "props-diff@example.com", "bench", &envs);
        assert_eq!(m.to, "props-diff@example.com");
        assert_eq!(m.subject, "[Railwatch] bench: your week in 2 environments");
        assert_eq!(
            m.text,
            "Hey Props Diff,\n\nHere's what happened in bench over the last 7 days.\n\nbench · env-0001\n  24,859 requests, p95 379.8ms, 14.14% 5xx\n  6,196 job attempts, 0 failed\n  1 new issues, 0 resolved, 0 deploys\n  BENC-1 ArgumentError: kaboom (1,768) https://rw.example.com/issues/1\n\nShop <&> · staging\n  0 requests, p95 0.0ms, 0% 5xx\n  0 job attempts, 0 failed\n  2 new issues, 0 resolved, 1 deploys\n\n\nOpen Railwatch: https://rw.example.com/dashboard\nTurn off this digest: https://rw.example.com/settings/profile\n\n"
        );
        assert_eq!(
            m.html,
            "<!DOCTYPE html>\n<html>\n  <head>\n    <meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\">\n    <style>\n      /* Email styles need to be inline */\n    </style>\n  </head>\n\n  <body>\n    <p>Hey Props Diff,</p>\n\n<p>Here's what happened in <strong>bench</strong> over the last 7 days.</p>\n\n  <h3>bench · env-0001</h3>\n  <ul>\n    <li>24,859 requests, p95 379.8ms, 14.14% 5xx</li>\n    <li>6,196 job attempts, 0 failed</li>\n    <li>1 new issue, 0 resolved, 0 deploys</li>\n  </ul>\n    <p>Most frequent open issues:</p>\n    <ul>\n        <li><a href=\"https://rw.example.com/issues/1\">BENC-1</a> ArgumentError: kaboom (1,768)</li>\n    </ul>\n  <h3>Shop &lt;&amp;&gt; · staging</h3>\n  <ul>\n    <li>0 requests, p95 0.0ms, 0% 5xx</li>\n    <li>0 job attempts, 0 failed</li>\n    <li>2 new issues, 0 resolved, 1 deploy</li>\n  </ul>\n\n<p><a href=\"https://rw.example.com/dashboard\">Open Railwatch</a> · <a href=\"https://rw.example.com/settings/profile\">Turn off this digest</a></p>\n\n  </body>\n</html>\n"
        );
        let one = weekly("https://rw.example.com", "A", "a@example.com", "Acme", &envs[..1]);
        assert_eq!(one.subject, "[Railwatch] Acme: your week in 1 environment");
    }
}
