//! plan-audit: which day-file indexes the server's reads use
//! (bench/storage/index_audit.sh, docs/STORAGE.md "Sizing").
//!
//!   plan-audit TENANT_DIR TRACE_FILE... > report.json
//!
//! TENANT_DIR is a per-day environment directory. Each TRACE_FILE holds
//! `SQLTRACE <ms> <sql>` lines (RW_SQL_TRACE=1 on props-dump, mcp-dump,
//! api-dump or `railwatch serve`; binds expanded). Every distinct statement
//! is run under EXPLAIN QUERY PLAN through a union reader over TENANT_DIR,
//! and every `USING [COVERING] INDEX <name>` in its plan is counted. Prints
//! {statements, failed, used: {index: {statements, example}}, unused:
//! [index on a day table that no plan named], plans: {sql: [index...]}}
//! (`plans` lets two runs over different schemas be diffed statement by
//! statement).
//!
//! Expanded binds are literals, so a partial index can show up where the
//! bound statement could not use it: the audit errs toward keeping an index.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: plan-audit TENANT_DIR TRACE_FILE...");
        std::process::exit(2);
    }
    let dir = std::path::Path::new(&a[0]);
    let (conn, _) = railwatch_server::storage::reader::open(dir).expect("open per-day tenant");
    let mut statements = BTreeSet::new();
    for f in &a[1..] {
        for line in std::fs::read_to_string(f).expect("trace file").lines() {
            let Some(rest) = line.strip_prefix("SQLTRACE ") else { continue };
            let Some((_, sql)) = rest.split_once(' ') else { continue };
            let s = sql.trim();
            // The reader's own setup (ATTACH, views, pragmas) is not a read.
            let head = s.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
            if matches!(head.as_str(), "SELECT" | "WITH") {
                statements.insert(s.to_string());
            }
        }
    }
    let re = regex::Regex::new(r"USING (?:COVERING )?INDEX (\w+)").unwrap();
    let mut used: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut failed = vec![];
    let mut plans = serde_json::Map::new();
    for s in &statements {
        let plan: rusqlite::Result<Vec<String>> =
            conn.prepare(&format!("EXPLAIN QUERY PLAN {s}")).and_then(|mut q| q.query_map([], |r| r.get::<_, String>(3))?.collect());
        match plan {
            Ok(rows) => {
                let mut seen = BTreeSet::new();
                for r in &rows {
                    for c in re.captures_iter(r) {
                        seen.insert(c[1].to_string());
                    }
                }
                plans.insert(s.clone(), json!(seen.iter().collect::<Vec<_>>()));
                for i in seen {
                    let e = used.entry(i).or_insert((0, s.chars().take(300).collect()));
                    e.0 += 1;
                }
            }
            Err(e) => failed.push(json!({"sql": s.chars().take(300).collect::<String>(), "error": e.to_string()})),
        }
    }
    // Every index on a day table, from the schema the files are made with.
    let schema = railwatch_server::schema::schema();
    let day_tables = railwatch_server::storage::layout::day_tables();
    let mut all: Vec<(String, String)> = schema
        .ddl
        .iter()
        .filter(|(ty, _, tbl, sql)| ty == "index" && sql.is_some() && day_tables.contains(&tbl.as_str()))
        .map(|(_, name, tbl, _)| (tbl.clone(), name.clone()))
        .collect();
    for (name, sql) in railwatch_server::schema::ADDED_INDEXES {
        let tbl = sql.split(" ON \"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_string();
        all.push((tbl, name.to_string()));
    }
    let unused: Vec<_> = all.iter().filter(|(_, n)| !used.contains_key(n)).map(|(t, n)| json!({"table": t, "index": n})).collect();
    let used_json: serde_json::Map<String, serde_json::Value> = used.into_iter().map(|(k, (n, ex))| (k, json!({"statements": n, "example": ex}))).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"statements": statements.len(), "failed": failed, "used": used_json, "unused": unused, "plans": plans})).unwrap()
    );
}
