//! Mapper.row_for, record for record, against the Ruby gem (0.9.0) on the
//! same input. tests/fixtures/mapper_corpus.ndjson comes from
//! bench/diff/mapper_corpus.py (every wire fixture plus ~250 edge-case
//! mutations); mapper_corpus.ruby.json is what Railwatch::Ingest::Mapper
//! returned for each line inside rw-baseline (bench/diff/rowfor.rb).

use railwatch_server::ingest::mapper::{self, Bind};
use railwatch_server::ingest::payload;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[test]
fn row_for_matches_ruby_on_every_corpus_line() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    let corpus = std::fs::read(format!("{dir}mapper_corpus.ndjson")).unwrap();
    let expected: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}mapper_corpus.ruby.json")).unwrap()).unwrap();
    let records = payload::parse_bytes(&corpus, false).unwrap();
    assert_eq!(records.len(), expected.len());
    let mut failures = Vec::new();
    for (i, (rec, want)) in records.iter().zip(&expected).enumerate() {
        let got = if rec.get("t") == &railwatch_server::rjson::Value::from("user") {
            match mapper::person_record(rec) {
                Ok(p) => serde_json::json!({"person": {"id": p.id, "name": p.name, "email": p.email, "tenant": p.tenant}}),
                Err(e) => serde_json::json!({"error": first_chars(&e.rejection_reason(), 200)}),
            }
        } else {
            let mut truncations = Vec::new();
            match mapper::row_for(rec, &mut truncations) {
                Ok(None) => serde_json::json!({"unknown": true}),
                Ok(Some(m)) => {
                    let row: Vec<serde_json::Value> = m
                        .row
                        .iter()
                        .map(|(k, v)| {
                            let enc = match v {
                                Bind::Null => serde_json::json!(["null", null]),
                                Bind::Int(i) => serde_json::json!(["int", i.to_string()]),
                                Bind::Float(f) => serde_json::json!(["float", format!("{:016x}", f.to_bits())]),
                                Bind::Text(s) => serde_json::json!(["text", hex(s.as_bytes())]),
                                Bind::Blob(b) => serde_json::json!(["blob", hex(b)]),
                                Bind::Unbindable(c) => serde_json::json!(["other", if *c == "TrueClass" { "true" } else { "false" }]),
                            };
                            serde_json::json!([k, enc])
                        })
                        .collect();
                    serde_json::json!({"table": m.table, "row": row, "truncations": truncations})
                }
                Err(e) => serde_json::json!({"error": first_chars(&e.rejection_reason(), 200)}),
            }
        };
        // The Ruby dump keeps class/message for "invalid record" for debugging only.
        let mut want = want.clone();
        if let Some(o) = want.as_object_mut() {
            o.remove("class");
            o.remove("message");
        }
        if got != want {
            failures.push(format!("line {}:\n  ruby: {}\n  rust: {}", i + 1, want, got));
        }
    }
    assert!(failures.is_empty(), "{} of {} lines differ:\n{}", failures.len(), records.len(), failures.join("\n"));
}
