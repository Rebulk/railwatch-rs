//! UserMailer (password_reset, email_verification) and InvitationMailer
//! (invite) over SMTP with lettre; DigestMailer's multipart digest goes
//! out through `send_alternative` (web/digest_mailer.rs). Without SMTP configured nothing is sent:
//! the recipient and subject are logged at WARN, never the body, whose
//! links (reset, verification, invitation) would let anyone who reads the
//! log take over an account (`railwatch doctor` says mail is off). Delivery
//! runs in the background (`deliver_later`).

use std::time::Duration;

use lettre::message::{Mailbox, MultiPart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::Smtp;

#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

pub struct Mail {
    pub to: String,
    pub subject: String,
    pub text: String,
}

impl Mailer {
    pub fn new(s: &Smtp) -> Result<Mailer, String> {
        let from: Mailbox = s.from.parse().map_err(|e| format!("smtp_from {:?}: {e}", s.from))?;
        let tls = || TlsParameters::new(s.address.clone()).map_err(|e| e.to_string());
        let mut b = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&s.address).port(s.port).timeout(Some(Duration::from_secs(30))).tls(
            match s.security.as_str() {
                "tls" => Tls::Wrapper(tls()?),
                "none" => Tls::None,
                _ => Tls::Required(tls()?),
            },
        );
        if let (Some(u), Some(p)) = (&s.username, &s.password) {
            b = b.credentials(Credentials::new(u.clone(), p.clone()));
        }
        Ok(Mailer { transport: b.build(), from })
    }

    /// Connects and says EHLO (`railwatch doctor`).
    pub async fn check(&self) -> Result<(), String> {
        match self.transport.test_connection().await {
            Ok(true) => Ok(()),
            Ok(false) => Err("server did not accept the connection".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub async fn send(&self, mail: &Mail) -> Result<(), String> {
        let to: Mailbox = mail.to.parse().map_err(|e| format!("{e}"))?;
        let msg = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(&mail.subject)
            .header(ContentType::TEXT_PLAIN)
            .body(mail.text.clone())
            .map_err(|e| e.to_string())?;
        self.transport.send(msg).await.map(drop).map_err(|e| e.to_string())
    }

    /// multipart/alternative, text then HTML, as Action Mailer sends a
    /// mail with both a .text and an .html view.
    pub async fn send_alternative(&self, to: &str, subject: &str, text: &str, html: &str) -> Result<(), String> {
        let to: Mailbox = to.parse().map_err(|e| format!("{e}"))?;
        let msg = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(subject)
            .multipart(MultiPart::alternative_plain_html(text.to_string(), html.to_string()))
            .map_err(|e| e.to_string())?;
        self.transport.send(msg).await.map(drop).map_err(|e| e.to_string())
    }
}

/// `deliver_later`: sends in the background, or logs its recipient and
/// subject (never the body: it holds the link) when SMTP is off. Failures are logged at ERROR with the recipient (never silent).
pub fn deliver_later(mailer: Option<Mailer>, mail: Mail) {
    match mailer {
        None => tracing::warn!(to = mail.to, subject = mail.subject, "SMTP is not configured; email not sent"),
        Some(m) => {
            tokio::spawn(async move {
                match m.send(&mail).await {
                    Ok(()) => tracing::info!(to = mail.to, subject = mail.subject, "email sent"),
                    Err(e) => tracing::error!(to = mail.to, subject = mail.subject, error = e, "email delivery failed"),
                }
            });
        }
    }
}

/// Telemetry text (an issue title is an exception message the monitored
/// app's users can choose) on one line before it goes into a mail body:
/// every CR and LF becomes a space, so no line break in it reaches SMTP.
pub fn one_line(s: &str) -> String {
    s.replace("\r\n", " ").replace(['\r', '\n'], " ")
}

pub fn password_reset(base_url: &str, to: &str, sid: &str) -> Mail {
    Mail {
        to: to.into(),
        subject: "Reset your password".into(),
        text: format!(
            "Someone asked to reset the password for {to} on Railwatch.\n\nReset it here (the link works for 20 minutes):\n{base_url}/identity/password_reset/edit?sid={sid}\n\nIf it was not you, ignore this email.\n"
        ),
    }
}

pub fn email_verification(base_url: &str, to: &str, sid: &str) -> Mail {
    Mail {
        to: to.into(),
        subject: "Verify your email".into(),
        text: format!("Confirm that {to} is your email address for Railwatch:\n{base_url}/identity/email_verification?sid={sid}\n"),
    }
}

pub fn invite(base_url: &str, to: &str, inviter: &str, account: &str, token: &str) -> Mail {
    Mail {
        to: to.into(),
        subject: format!("{inviter} invited you to {account} on Railwatch"),
        text: format!(
            "{inviter} invited you to join {account} on Railwatch.\n\nAccept the invitation (it expires in 7 days):\n{base_url}/invitations/{token}\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn telemetry_text_is_put_on_one_line() {
        assert_eq!(one_line("a\r.\r\nMAIL FROM:<x@evil.test>\nb\rc"), "a . MAIL FROM:<x@evil.test> b c");
        assert_eq!(one_line("plain"), "plain");
    }

    /// Without SMTP the mail is logged, not sent: anyone who reads the log
    /// must not be able to reset a password or accept an invitation with
    /// what it says.
    #[test]
    fn unsent_mail_logs_the_recipient_and_subject_but_never_the_link() {
        let out = Captured::default();
        let w = out.clone();
        let subscriber = tracing_subscriber::fmt().with_writer(move || w.clone()).with_ansi(false).finish();
        tracing::subscriber::with_default(subscriber, || {
            deliver_later(None, password_reset("https://rw.example.com", "ada@example.com", "s3cr3t-sid"));
            deliver_later(None, invite("https://rw.example.com", "bob@example.com", "Ada", "Acme", "inv-t0ken"));
        });
        let log = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("ada@example.com") && log.contains("Reset your password") && log.contains("bob@example.com"), "{log}");
        assert!(!log.contains("s3cr3t-sid") && !log.contains("inv-t0ken"), "{log}");
    }
}
