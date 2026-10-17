//! UserMailer (password_reset, email_verification) and InvitationMailer
//! (invite) over SMTP with lettre. Without SMTP configured nothing is sent:
//! the link is logged at WARN instead, so a self-hoster without mail can
//! still reset a password from the server log (and `railwatch doctor` says
//! mail is off). Delivery runs in the background (`deliver_later`).

use std::time::Duration;

use lettre::message::{Mailbox, header::ContentType};
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
}

/// `deliver_later`: sends in the background, or logs the mail when SMTP
/// is off. Failures are logged at ERROR with the recipient (never silent).
pub fn deliver_later(mailer: Option<Mailer>, mail: Mail) {
    match mailer {
        None => tracing::warn!(to = mail.to, subject = mail.subject, body = mail.text, "SMTP is not configured; email not sent"),
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
