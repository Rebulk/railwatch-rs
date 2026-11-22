//! Audit 2026-11-22 (rw-h-ssrf, uptime scheduling): the probe loop runs at
//! most eight checks at once for the whole server and handed free slots to
//! due monitors in ascending id, stopping at the first one it could not
//! seat. A check holds its slot for its whole timeout when the target never
//! answers, so one account with more slow monitors than slots kept every
//! monitor with a higher id, in any account, from ever being checked.
//!
//! The real `run_forever` loop against two loopback servers: one accepts
//! connections and never answers, one answers 200 at once. Dummy data,
//! loopback only. Real time: about 12 s.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use railwatch_server::control::{self, Control};
use railwatch_server::uptime::{self, MonitorInput};

/// Accepts every connection and never answers, so a check waits out its
/// whole timeout.
fn silent_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in l.incoming().flatten() {
            held.push(s);
        }
    });
    port
}

/// Answers every request with 200 at once.
fn ok_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            });
        }
    });
    port
}

#[test]
fn one_accounts_slow_monitors_do_not_keep_another_accounts_monitor_from_being_checked() {
    // Loopback targets need the self-host flag; read when the loop starts.
    unsafe { std::env::set_var("RAILWATCH_ALLOW_PRIVATE_MONITOR_TARGETS", "1") };
    let dir = std::env::temp_dir().join(format!("rw-uptime-fair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let control = Arc::new(Control::open(&dir.join("control.sqlite3")).unwrap());
    let environment = |name: &str, email: &str, slug: &str| -> i64 {
        let user = control.tx(|tx| control::insert_user(tx, name, email, &control::base58(16), true)).unwrap();
        let account = control.tx(|tx| control::create_account(tx, name, user)).unwrap();
        let app = control.tx(|tx| control::insert_application(tx, account, "Shop", slug, &slug.to_uppercase(), None, "main")).unwrap();
        control.tx(|tx| control::create_environment(tx, app, "production")).unwrap().0
    };
    let (slow, fast) = (silent_server(), ok_server());
    let a = environment("Acme", "ada@acme.test", "acme");
    let b = environment("Bobco", "bob@bobco.test", "bobco");
    let add = |env: i64, m: MonitorInput| control.with(|x| uptime::insert(x, env, &m)).unwrap().unwrap();
    // Account A: 16 monitors (two per slot) whose checks each hold a slot
    // for their whole 2 s timeout and are due again 3 s after they start.
    let slow_ids: Vec<i64> = (0..16)
        .map(|i| {
            let url = format!("http://127.0.0.1:{slow}/up");
            add(a, MonitorInput { name: format!("slow-{i}"), url, timeout_seconds: 2, interval_seconds: 3, ..Default::default() })
        })
        .collect();
    // Account B: one monitor, created later (a higher id), answered at once.
    let url = format!("http://127.0.0.1:{fast}/up");
    let other = add(b, MonitorInput { name: "fast".into(), url, timeout_seconds: 1, interval_seconds: 3, ..Default::default() });

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let looping = rt.spawn(railwatch_server::jobs::uptime::run_forever(control.clone(), stopped));
    let checked =
        |id: i64| -> Option<i64> { control.with(|x| x.query_row("SELECT last_checked_at FROM uptime_monitors WHERE id = ?", [id], |r| r.get(0))).unwrap() };
    let slow_checked = || slow_ids.iter().filter(|id| checked(**id).is_some()).count();
    // Four of B's intervals.
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline && (checked(other).is_none() || slow_checked() == 0) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let (other_checked, slow_checked) = (checked(other).is_some(), slow_checked());
    let _ = stop.send(true);
    rt.block_on(looping).unwrap();
    drop(rt);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(slow_checked > 0, "control: the loop ran and recorded account A's timed-out checks");
    assert!(other_checked, "account B's monitor was never checked in 12 s (4 of its intervals) while account A's slow monitors held the slots");
}
