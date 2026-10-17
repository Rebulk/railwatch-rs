//! digest-dump: the weekly digests WeeklyDigestJob would send at a frozen
//! "now", for the digest diff harness (bench/diff/digest/run.sh).
//!
//!   digest-dump DATA_DIR NOW_EPOCH BASE_URL OUT.json
//!
//! Writes [{account_id, to, subject, text, html}] in delivery order, the
//! shape capture_rails.rb writes.

use railwatch_server::control::Control;
use railwatch_server::jobs::weekly_digest::{Outcome, build_all};
use serde_json::{Value, json};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() != 4 {
        eprintln!("usage: digest-dump DATA_DIR NOW_EPOCH BASE_URL OUT");
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&a[0]);
    let control = Control::open(&dir.join("control.sqlite3")).expect("control db");
    let now = a[1].parse::<i64>().expect("NOW_EPOCH") * 1_000_000;
    let runs = build_all(&control, &dir.join("telemetry"), &a[2], now, None).expect("build");
    let mut out = Vec::<Value>::new();
    for run in runs {
        match run.outcome {
            Outcome::Mails(mails) => {
                out.extend(mails.into_iter().map(|m| json!({"account_id": run.id, "to": [m.to], "subject": m.subject, "text": m.text, "html": m.html})))
            }
            Outcome::Failed(e) => panic!("account {}: {e}", run.id),
            Outcome::NoRecipients | Outcome::NoTelemetry => {}
        }
    }
    std::fs::write(&a[3], serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("DUMPED {} mails", out.len());
}
