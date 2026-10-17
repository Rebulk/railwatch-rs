//! rw-detectors: replays bench/diff/detectors scenarios through the Rust
//! detectors, grouping, issue lifecycle and alert outbox, under a frozen
//! clock (time::travel_to), for the Rails-equivalence diff.
//!
//!   rw-detectors SCENARIOS.json OUT_DIR HOOK_URL [name,...]
//!
//! Each scenario gets OUT_DIR/<name>/ as its data dir (control.sqlite3 +
//! telemetry/). Steps call what the server's scheduler calls: scan_targets
//! and the scan's detector, quota_check, auto_resolve, grouping::group, and
//! one delivery pass (reconcile, deliver, burst summaries) per "deliver".
//! The receiver's address is private, so run with
//! RAILWATCH_ALLOW_PRIVATE_ALERT_TARGETS=1 and RAILWATCH_ALERT_CA_FILE.

use std::path::Path;
use std::sync::Arc;

use railwatch_server::alerts::integration::{Alerting, SettingsCipher};
use railwatch_server::control::{self, Control};
use railwatch_server::grouping::{self, issue};
use railwatch_server::jobs::{self, detectors};
use railwatch_server::time;
use rusqlite::{Connection, params};
use serde_json::Value;

fn micros(iso: &str) -> i64 {
    time::parse_db_time(iso.trim_end_matches('Z')).expect("step time")
}

fn opt_sql(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => (*b as i64).into(),
        Value::Number(n) if n.is_i64() => n.as_i64().unwrap().into(),
        Value::Number(n) => n.as_f64().unwrap().into(),
        Value::String(s) => s.clone().into(),
        other => other.to_string().into(),
    }
}

fn update_columns(c: &Connection, table: &str, id: i64, cols: &serde_json::Map<String, Value>) {
    for (k, v) in cols {
        if k == "at" || k == "do" {
            continue;
        }
        c.execute(&format!("UPDATE {table} SET \"{k}\" = ? WHERE id = ?"), params![opt_sql(v), id]).unwrap_or_else(|e| panic!("{table}.{k}: {e}"));
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let scenarios: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[0]).expect("scenarios")).expect("json");
    let out = Path::new(&a[1]);
    let hook = &a[2];
    let only: Vec<&str> = a.get(3).map(|s| s.split(',').collect()).unwrap_or_default();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mut done = 0;
    for (index, sc) in scenarios.iter().enumerate() {
        let name = sc["name"].as_str().unwrap();
        if !only.is_empty() && !only.contains(&name) {
            continue;
        }
        let dir = out.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("telemetry")).unwrap();
        let steps = sc["steps"].as_array().unwrap();
        time::travel_to(micros(steps[0]["at"].as_str().unwrap()) - 3_600_000_000);
        let control = Arc::new(Control::open(&dir.join("control.sqlite3")).unwrap());
        let cipher = SettingsCipher::new(b"rw-detectors");
        control.configure_alerting(Alerting { cipher: cipher.clone(), smtp: None, url: "https://railwatch.rebulk.com".into(), deliver: true });
        let (env, slug) = {
            let c = control.conn();
            let ada = control::insert_user(&c, "Ada", "ada@example.com", "correct horse battery", true).ok().unwrap();
            let grace = control::insert_user(&c, "Grace", "grace@example.com", "correct horse battery", true).ok().unwrap();
            let account = control::insert_account(&c, &format!("dets{index}"), &format!("dets{index}"), 7).ok().unwrap();
            control::insert_membership(&c, account, ada, "owner").ok().unwrap();
            control::insert_membership(&c, account, grace, "member").ok().unwrap();
            c.execute("UPDATE accounts SET plan = 'free' WHERE id = ?", [account]).unwrap();
            update_columns(&c, "accounts", account, sc["account"].as_object().unwrap());
            let app = control::insert_application(&c, account, "Shop", "shop", "DET", None, "main").ok().unwrap();
            let (env, _) = control::create_environment(&c, app, "production").ok().unwrap();
            let mut e = sc["env"].as_object().unwrap().clone();
            if let Some(s) = e.get("expected_servers").cloned() {
                e.insert("expected_servers".into(), Value::String(s.to_string()));
            }
            update_columns(&c, "environments", env, &e);
            let integrations: Vec<i64> = sc["integrations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| {
                    let mut settings = serde_json::Map::new();
                    settings.insert("url".into(), Value::String(format!("{hook}/rust/{name}/{}", i["path"].as_str().unwrap())));
                    settings.insert("secret".into(), Value::String("det-secret".into()));
                    let id = railwatch_server::alerts::integration::insert(&c, &cipher, account, "webhook", i["name"].as_str().unwrap(), &settings).unwrap();
                    c.execute("UPDATE integrations SET enabled = ? WHERE id = ?", params![i["enabled"].as_bool().unwrap(), id]).unwrap();
                    id
                })
                .collect();
            let now = control::now_db();
            for r in sc["rules"].as_array().unwrap() {
                c.execute(
                    "INSERT INTO alert_rules (application_id, created_at, event, filters, integration_id, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                    params![app, now, r["event"].as_str().unwrap(), r["filters"].to_string(), integrations[r["integration"].as_u64().unwrap() as usize], now],
                )
                .unwrap();
            }
            for t in sc["thresholds"].as_array().unwrap() {
                c.execute(
                    "INSERT INTO thresholds (environment_id, target_kind, metric, \"limit\", target, window_minutes, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![env, t["target_kind"].as_str(), t["metric"].as_str(), t["limit"].as_f64(), t["target"].as_str(), t["window_minutes"].as_i64(), now, now],
                )
                .unwrap();
            }
            for r in sc["anomaly_rules"].as_array().unwrap() {
                c.execute(
                    "INSERT INTO anomaly_rules (environment_id, target_kind, metric, target, deviation, window_minutes, baseline_days, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![env, r["target_kind"].as_str(), r["metric"].as_str(), r["target"].as_str(), r["deviation"].as_f64(), r["window_minutes"].as_i64(), r["baseline_days"].as_i64(), r["enabled"].as_bool(), now, now],
                )
                .unwrap();
            }
            let slug: String = c.query_row("SELECT slug FROM environments WHERE id = ?", [env], |r| r.get(0)).unwrap();
            (env, slug)
        };
        let tenant_path = dir.join("telemetry").join(format!("{slug}.sqlite3"));
        let tenant = railwatch_server::tenants::open_tenant(&tenant_path).unwrap();
        for step in steps {
            time::travel_to(micros(step["at"].as_str().unwrap()));
            let now = time::now_micros() / 1_000_000;
            let c = control.conn();
            match step["do"].as_str().unwrap() {
                "tenant_sql" => {
                    for s in step["sql"].as_array().unwrap() {
                        tenant.execute_batch(s.as_str().unwrap()).unwrap();
                    }
                }
                "deploy" => {
                    let n = control::now_db();
                    c.execute(
                        "INSERT INTO deploys (environment_id, deploy, ref, deployed_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                        params![env, step["deploy"].as_str(), step["ref"].as_str(), step["deployed_at"].as_str(), n, n],
                    )
                    .unwrap();
                }
                "usage" => {
                    let n = control::now_db();
                    c.execute("DELETE FROM ingest_usage_months WHERE environment_id = ? AND month = ?", params![env, step["month"].as_str()]).unwrap();
                    c.execute(
                        "INSERT INTO ingest_usage_months (environment_id, month, events, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
                        params![env, step["month"].as_str(), step["events"].as_i64(), n, n],
                    )
                    .unwrap();
                }
                "env" => update_columns(&c, "environments", env, step.as_object().unwrap()),
                "account" => {
                    let account: i64 = c
                        .query_row("SELECT ap.account_id FROM environments e JOIN applications ap ON ap.id = e.application_id WHERE e.id = ?", [env], |r| {
                            r.get(0)
                        })
                        .unwrap();
                    update_columns(&c, "accounts", account, step.as_object().unwrap());
                }
                "scan" => {
                    let scan = step["scan"].as_str().unwrap();
                    drop(c);
                    let r = match scan {
                        "quota_check" => detectors::quota_check(&control.conn(), now),
                        "auto_resolve_issues" => detectors::auto_resolve(&control.conn(), now),
                        _ => {
                            let s = jobs::SCANS.iter().find(|s| s.name == scan).expect("scan");
                            let mut n = 0;
                            for (id, _) in jobs::scan_targets(&control, s, now) {
                                n += (s.run)(&control.conn(), &tenant, id, now).unwrap_or_else(|e| panic!("{name} {scan}: {e}"));
                            }
                            Ok(n)
                        }
                    };
                    r.unwrap_or_else(|e| panic!("{name} {scan}: {e}"));
                }
                "group" => {
                    let ids: Vec<i64> = step["ids"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
                    let rows = grouping::load_exceptions(&tenant, &ids).unwrap();
                    grouping::group(&c, &tenant, env, rows, None).unwrap();
                }
                "issue" => {
                    let id: i64 = c
                        .query_row("SELECT id FROM issues WHERE environment_id = ? AND number = ?", params![env, step["number"].as_i64()], |r| r.get(0))
                        .unwrap();
                    let i = issue::find(&c, id).unwrap().unwrap();
                    c.execute_batch("BEGIN IMMEDIATE").unwrap();
                    match step["action"].as_str().unwrap() {
                        "resolve" => {
                            issue::resolve_by(&c, &i, step["deploy"].as_str(), None).unwrap();
                        }
                        "ignore" => {
                            issue::ignore(&c, &i, None).unwrap();
                        }
                        "reopen" => {
                            issue::reopen(&c, &i, None).unwrap();
                        }
                        "assign" => {
                            let to = step["value"].as_i64().map(|u| {
                                let n: String = c.query_row("SELECT name FROM users WHERE id = ?", [u], |r| r.get(0)).unwrap();
                                (u, n)
                            });
                            issue::set_assignee(&c, &i, to.as_ref().map(|(u, n)| (*u, n.as_str())), None).unwrap();
                        }
                        "priority" => {
                            issue::set_priority(&c, &i, step["value"].as_str().unwrap(), None).unwrap();
                        }
                        other => panic!("unknown action {other}"),
                    }
                    c.execute_batch("COMMIT").unwrap();
                }
                "deliver" => {
                    drop(c);
                    rt.block_on(railwatch_server::alerts::deliver_pass(&control)).unwrap();
                }
                other => panic!("unknown step {other}"),
            }
        }
        // IssueDetectionPresenter over every issue at the last step's time.
        {
            let c = control.conn();
            let mut presented = serde_json::Map::new();
            let issues: Vec<(i64, String, i64, String)> = c
                .prepare("SELECT number, kind, occurrences, sample FROM issues WHERE environment_id = ? ORDER BY id")
                .unwrap()
                .query_map([env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            for (number, kind, occurrences, sample) in issues {
                let sample: Value = serde_json::from_str(&sample).unwrap_or(Value::Null);
                let p = railwatch_server::detection::presenter::Presented { kind: &kind, occurrences, sample: &sample };
                presented.insert(number.to_string(), railwatch_server::detection::presenter::as_json(&p, &tenant, time::now_micros()).unwrap());
            }
            std::fs::write(dir.join("presented.json"), Value::Object(presented).to_string()).unwrap();
        }
        time::travel_back();
        done += 1;
    }
    println!("RUST DONE {done}");
}
