//! GA review 2026-11-19, finding 1: lettre 0.11.23's advisories
//! (RUSTSEC-2026-0560..0565), reached through the server's own Mailer. An
//! email alert's body carries the issue title, which is telemetry (an
//! exception message an attacker can choose).
//!
//! The SMTP server here is a lenient one, in process: it records every raw
//! byte and ends DATA on a bare CR or LF dot line as well as CRLF, as some
//! older MTAs and smarthosts do.

use std::sync::{Arc, Mutex};

use railwatch_server::config::Smtp;
use railwatch_server::web::mailer::{Mail, Mailer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Starts the fake server; returns its port and what it received.
async fn fake_smtp() -> (u16, Arc<Mutex<Vec<u8>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let raw = Arc::new(Mutex::new(Vec::new()));
    let out = raw.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = listener.accept().await else { return };
            let out = out.clone();
            tokio::spawn(async move {
                let _ = c.write_all(b"220 fake ESMTP\r\n").await;
                let (mut buf, mut data) = (Vec::<u8>::new(), false);
                let mut chunk = [0u8; 65536];
                loop {
                    let Ok(n) = c.read(&mut chunk).await else { return };
                    if n == 0 {
                        return;
                    }
                    out.lock().unwrap().extend_from_slice(&chunk[..n]);
                    buf.extend_from_slice(&chunk[..n]);
                    loop {
                        if data {
                            let ends = [&b"\r\n.\r\n"[..], b"\n.\n", b"\r.\r"];
                            let Some((i, len)) = ends.iter().filter_map(|e| find(&buf, e).map(|i| (i, e.len()))).min() else { break };
                            buf.drain(..i + len);
                            data = false;
                            let _ = c.write_all(b"250 queued\r\n").await;
                            continue;
                        }
                        let Some(j) = buf.iter().position(|&b| b == b'\n') else { break };
                        let line: Vec<u8> = buf.drain(..=j).collect();
                        let u = String::from_utf8_lossy(&line).trim_end().to_ascii_uppercase();
                        let reply: &[u8] = if u.starts_with("EHLO") {
                            b"250-fake\r\n250 8BITMIME\r\n"
                        } else if u.starts_with("DATA") {
                            data = true;
                            b"354 go\r\n"
                        } else if u.starts_with("QUIT") {
                            let _ = c.write_all(b"221 bye\r\n").await;
                            return;
                        } else {
                            b"250 ok\r\n"
                        };
                        let _ = c.write_all(reply).await;
                    }
                }
            });
        }
    });
    (port, raw)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn smtp(port: u16) -> Smtp {
    Smtp { address: "127.0.0.1".into(), port, username: None, password: None, security: "none".into(), from: "Railwatch <railwatch@example.test>".into() }
}

/// RUSTSEC-2026-0561: a body with a bare CR dot line must be dot-stuffed (or
/// encoded), or a lenient server ends DATA there and reads what follows as
/// new SMTP commands on Railwatch's session.
#[tokio::test]
async fn a_bare_cr_dot_line_in_a_mail_body_cannot_end_the_data_early() {
    let (port, raw) = fake_smtp().await;
    let m = Mailer::new(&smtp(port)).unwrap();
    let text = "NoMethodError in production: x\r.\rRCPT TO:<smuggled@evil.test>\r\n";
    m.send(&Mail { to: "ops@example.test".into(), subject: "[Railwatch] t".into(), text: text.into() }).await.unwrap();
    let r = String::from_utf8_lossy(&raw.lock().unwrap()).into_owned();
    assert!(r.contains("RCPT TO:<ops@example.test>"), "the mail went out: {r:?}");
    assert!(!r.contains("\r.\rRCPT TO:<smuggled@evil.test>"), "bare-CR dot line reached the server unstuffed: {r:?}");
    assert!(!r.contains("\nRCPT TO:<smuggled@evil.test>"), "a smuggled command line reached the server: {r:?}");
}

/// RUSTSEC-2026-0563: the envelope recipient is the address the header
/// names, never a different one parsed out of a quoted local part.
#[tokio::test]
async fn a_quoted_local_part_does_not_change_the_envelope_recipient() {
    let (port, raw) = fake_smtp().await;
    let m = Mailer::new(&smtp(port)).unwrap();
    let _ = m.send(&Mail { to: "\"victim \"@example.test".into(), subject: "s".into(), text: "b".into() }).await;
    let r = String::from_utf8_lossy(&raw.lock().unwrap()).into_owned();
    assert!(!r.contains("RCPT TO:<victim@example.test>"), "the envelope differs from the address given: {r:?}");
}
