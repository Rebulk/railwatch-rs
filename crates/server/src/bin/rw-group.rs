//! rw-group: run the exception-grouping port over a tenant file's
//! exceptions, in id-ordered chunks, against a fresh control database, and
//! print the issues as JSON (bench/diff/grouping_diff.sh).
//!
//!   rw-group TENANT.sqlite3 CONTROL.sqlite3 [--chunk 50] [--deploy-at "YYYY-MM-DD HH:MM:SS"]

use railwatch_server::control::Control;
use railwatch_server::grouping;
use railwatch_server::rjson::{self, Value};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (tenant, control_path) = (&args[0], &args[1]);
    let mut chunk = 50usize;
    let mut deploy_at = None;
    let mut repeat = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--chunk" => chunk = args[i + 1].parse().unwrap(),
            "--deploy-at" => deploy_at = Some(args[i + 1].clone()),
            "--resolve-and-repeat" => {
                repeat = true;
                i += 1;
                continue;
            }
            o => panic!("unknown {o}"),
        }
        i += 2;
    }
    let control = Control::open(std::path::Path::new(control_path)).unwrap();
    let env = control.create("grouping-diff", &"0".repeat(64), 30).unwrap();
    if let Some(at) = deploy_at {
        control
            .conn()
            .execute(
                "INSERT INTO deploys (environment_id, deploy, deployed_at, created_at, updated_at) VALUES (?, 'd', ?, ?, ?)",
                rusqlite::params![env.id, at, at, at],
            )
            .unwrap();
    }
    let t = railwatch_server::jobs::worker::open_read(std::path::Path::new(tenant)).unwrap();
    let ids: Vec<i64> = t.prepare("SELECT id FROM exceptions ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    // One affected-users cache across chunks, as the worker keeps one per tenant.
    let mut users = grouping::AffectedUsers::default();
    for slice in ids.chunks(chunk) {
        let rows = grouping::load_exceptions(&t, slice).unwrap();
        grouping::group_with(&control.conn(), &t, env.id, rows, None, &mut users).unwrap();
    }
    if repeat {
        let ids_: Vec<i64> =
            control.conn().prepare("SELECT id FROM issues ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        for (n, id) in ids_.iter().enumerate() {
            let c = control.conn();
            let issue = grouping::issue::find(&c, *id).unwrap().unwrap();
            grouping::issue::resolve(&c, &issue, if n % 2 == 0 { Some("abc123") } else { None }).unwrap();
        }
        for slice in ids.chunks(chunk) {
            let rows = grouping::load_exceptions(&t, slice).unwrap();
            grouping::group_with(&control.conn(), &t, env.id, rows, None, &mut users).unwrap();
        }
    }
    let c = control.conn();
    let mut s = c.prepare("SELECT id, group_hash, kind, title, culprit, status, occurrences, affected_users, first_seen_at, last_seen_at, source, sample, number FROM issues ORDER BY id").unwrap();
    let issues: Vec<(i64, Value)> = s
        .query_map([], |r| {
            let iso = |s: String| {
                let m = railwatch_server::time::parse_db_time(&s).unwrap();
                railwatch_server::time::iso8601_6(m)
            };
            let sample: String = r.get(11)?;
            Ok((
                r.get::<_, i64>(0)?,
                railwatch_server::rhash! {
                    "group_hash" => r.get::<_, String>(1)?, "kind" => r.get::<_, String>(2)?, "title" => r.get::<_, String>(3)?,
                    "culprit" => r.get::<_, Option<String>>(4)?, "status" => r.get::<_, String>(5)?, "occurrences" => r.get::<_, i64>(6)?,
                    "affected_users" => r.get::<_, i64>(7)?, "first_seen_at" => iso(r.get(8)?), "last_seen_at" => iso(r.get(9)?),
                    "source" => r.get::<_, Option<String>>(10)?, "sample" => rjson::parse(sample.as_bytes()).unwrap(), "number_rank" => r.get::<_, i64>(12)?,
                },
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let min = issues.iter().map(|(_, v)| if let Value::Int(n) = v.get("number_rank") { *n } else { 0 }).min().unwrap_or(0);
    let out: Vec<Value> = issues
        .into_iter()
        .map(|(id, mut v)| {
            let acts: Vec<Value> = c
                .prepare("SELECT kind, data FROM issue_activities WHERE issue_id = ? ORDER BY id")
                .unwrap()
                .query_map([id], |r| Ok(Value::Array(vec![Value::from(r.get::<_, String>(0)?), rjson::parse(r.get::<_, String>(1)?.as_bytes()).unwrap()])))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            if let Value::Hash(h) = &mut v {
                if let Some(Value::Int(n)) = h.get_mut(b"number_rank".as_slice()) {
                    *n -= min;
                }
                h.insert(b"activities".to_vec(), Value::Array(acts));
            }
            v
        })
        .collect();
    println!("RWJSON {}", rjson::generate(&Value::Array(out)).unwrap());
}
