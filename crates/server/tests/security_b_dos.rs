//! security-b resource/DoS PoCs with measured numbers.
//!
//!  1. gzip bomb is bounded (SOUND): a tiny gzip that inflates to >>32 MiB is
//!     refused at the 32 MiB ceiling with bounded memory and time.
//!  2. a newline-free body is refused at the NDJSON line cap in linear time.
//!  3. new issues per environment are capped (see the grouping test).

use std::time::Instant;

use railwatch_server::control::{self, Control};
use railwatch_server::grouping::{self, ExceptionRow};
use railwatch_server::ingest::payload;
use railwatch_server::rjson::Value;

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// SOUND: inflating is capped at MAX_BYTES (32 MiB), so a classic gzip bomb
/// cannot exhaust memory through the ingest body path.
#[test]
fn gzip_bomb_is_bounded() {
    // 512 MiB of a single byte: compresses to a few hundred KiB, would
    // inflate to 512 MiB if unbounded.
    // Blank lines, so the NDJSON line cap does not refuse it first.
    let raw = vec![b'\n'; 512 * 1024 * 1024];
    let gz = gzip(&raw);
    drop(raw);
    eprintln!("gzip bomb: compressed {} bytes -> would be 512 MiB", gz.len());
    let t = Instant::now();
    let r = payload::parse_bytes(&gz, true);
    let ms = t.elapsed().as_millis();
    eprintln!("parse_bytes returned {:?} in {ms} ms", r.as_ref().err());
    assert!(gz.len() < 1024 * 1024, "the bomb is small on the wire ({} bytes)", gz.len());
    assert_eq!(r.unwrap_err().0, "payload too large", "must refuse at the ceiling, not OOM");
}

/// Was O(n^2): with no newline in the body, `consume_complete_lines`
/// rescanned the whole growing buffer on every 64 KiB chunk (3.7 s release
/// for 32 MiB) and there was no line-length cap. Now each byte is scanned
/// once and a line over MAX_LINE_BYTES is refused as soon as it is seen.
#[test]
fn ndjson_no_newline_is_refused_quickly() {
    const N: usize = 32 * 1024 * 1024; // the body-size ceiling
    let no_newline = vec![b'a'; N]; // one 32 MiB "line", never completes
    let t = Instant::now();
    let r = payload::parse_bytes(&no_newline, false);
    let ms = t.elapsed().as_millis();
    eprintln!("parse_bytes 32 MiB no-newline: {:?} in {ms} ms", r.as_ref().err());
    assert_eq!(r.unwrap_err().0, "line too long");
    assert!(ms < 1000, "refusing the long line must be cheap ({ms} ms)");

    // Lines just under the cap: every byte is scanned once, so this is linear
    // too (the old rescan read each 4 MiB line ~64 times).
    let mut near_cap = Vec::with_capacity(N);
    while near_cap.len() + payload::MAX_LINE_BYTES < N {
        near_cap.push(b'"');
        near_cap.resize(near_cap.len() + payload::MAX_LINE_BYTES - 3, b'a');
        near_cap.extend_from_slice(b"\"\n");
    }
    let t = Instant::now();
    let r = payload::parse_bytes(&near_cap, false);
    let ms = t.elapsed().as_millis();
    eprintln!("parse_bytes {} bytes of near-cap lines: {} records in {ms} ms", near_cap.len(), r.as_ref().map(|v| v.len()).unwrap_or(0));
    assert_eq!(r.unwrap().len(), 7);
}

/// A token holder's exceptions with N distinct group hashes used to create N
/// issues in the shared control DB. Now new issues are capped per batch and
/// per environment-hour; the excess is counted onto one overflow issue, never
/// dropped. Measures rows created and control.sqlite3 growth.
#[test]
fn exception_grouping_caps_new_issues_per_env() {
    let dir = std::env::temp_dir().join(format!("rw-secb-issues-{}", control::base58(8)));
    std::fs::create_dir_all(&dir).unwrap();
    let control_path = dir.join("control.sqlite3");
    let control = Control::open_with(&control_path, true).unwrap();
    let env = control.create("victim-env", &control::digest(&control::new_token()), 30).unwrap();
    // An empty tenant file (grouping reads exceptions for the affected-user
    // count; empty is fine for the PoC).
    let tenant = railwatch_server::tenants::open_tenant(&dir.join("t.sqlite3")).unwrap();

    const N: usize = 5000;
    let mut rows = Vec::with_capacity(N);
    for i in 0..N {
        // A distinct, valid-looking 32-hex group hash per record: exactly what
        // validate_record accepts for `_group`, and exactly what an attacker
        // can mint at will.
        let g = format!("{i:032x}");
        rows.push(ExceptionRow {
            id: i as i64 + 1,
            group_hash: Some(g),
            class_name: "RuntimeError".into(),
            message: b"boom".to_vec(),
            file: Some("app.rb".into()),
            line: Some(1),
            occurred_at: 1_792_000_000_000_000,
            deploy: None,
            user_ref: None,
            source: None,
            handled: false,
            execution_id: None,
            execution_preview: None,
            fingerprint: Value::Nil,
            fingerprint_source: None,
        });
    }

    control.with(|c| c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")).ok();
    let before = std::fs::metadata(&control_path).map(|m| m.len()).unwrap_or(0);
    let t = Instant::now();
    let summary = control.with(|c| Ok(grouping::group(c, &tenant, env.id, rows, None).unwrap())).unwrap();
    let ms = t.elapsed().as_millis();
    control.with(|c| c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")).ok();
    let after = std::fs::metadata(&control_path).map(|m| m.len()).unwrap_or(0);

    let issues: i64 = control.with(|c| c.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get(0))).unwrap();
    let activities: i64 = control.with(|c| c.query_row("SELECT COUNT(*) FROM issue_activities", [], |r| r.get(0))).unwrap();
    eprintln!(
        "grouping {N} distinct group hashes: new={} in {ms} ms; issues={issues}, issue_activities={activities}; control.sqlite3 {} -> {} bytes (+{} KiB)",
        summary.new,
        before,
        after,
        (after.saturating_sub(before)) / 1024
    );
    let cap = grouping::Limits::default().new_issues_per_batch;
    assert_eq!(issues, cap as i64 + 1, "the batch cap's worth of issues plus the overflow issue");
    assert_eq!(summary.overflowed, N - cap, "every exception past the cap is accounted for");
    let (title, occurrences): (String, i64) = control
        .with(|c| c.query_row("SELECT title, occurrences FROM issues WHERE group_hash = ?", [grouping::OVERFLOW_GROUP_HASH], |r| Ok((r.get(0)?, r.get(1)?))))
        .unwrap();
    assert_eq!(title, grouping::OVERFLOW_TITLE);
    assert_eq!(occurrences, (N - cap) as i64, "the overflow issue counts every capped exception");

    // Later batches: the per-hour cap holds across them, an existing group
    // still records onto its own issue, and the next hour's cap is fresh.
    let limits = grouping::Limits { new_issues_per_hour: 250, new_issues_per_batch: 200 };
    let mut users = grouping::AffectedUsers::default();
    let batch = |from: usize, n: usize| -> Vec<ExceptionRow> {
        (from..from + n).map(|i| ExceptionRow { id: (N + i) as i64 + 1, group_hash: Some(format!("{i:032x}")), ..rows_template() }).collect()
    };
    let g = control.with(|c| Ok(grouping::group_with_limits(c, &tenant, env.id, batch(N, 100), None, &mut users, limits).unwrap())).unwrap();
    // 201 rows this hour already (200 + the overflow issue): 49 more reach 250.
    assert_eq!((g.new, g.overflowed), (49, 51));
    let g = control.with(|c| Ok(grouping::group_with_limits(c, &tenant, env.id, batch(0, 10), None, &mut users, limits).unwrap())).unwrap();
    assert_eq!((g.new, g.seen, g.overflowed), (0, 10, 0), "existing groups are never diverted");
    let _ = std::fs::remove_dir_all(&dir);
}

fn rows_template() -> ExceptionRow {
    ExceptionRow {
        id: 0,
        group_hash: None,
        class_name: "RuntimeError".into(),
        message: b"boom".to_vec(),
        file: Some("app.rb".into()),
        line: Some(1),
        occurred_at: 1_792_000_000_000_000,
        deploy: None,
        user_ref: None,
        source: None,
        handled: false,
        execution_id: None,
        execution_preview: None,
        fingerprint: Value::Nil,
        fingerprint_source: None,
    }
}
