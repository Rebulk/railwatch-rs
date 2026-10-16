//! props-dump: compute dashboard props over a tenant file, for the
//! props-diff harness (bench/diff/props/run.sh) and the page timings.
//!
//!   props-dump TENANT.sqlite3 CASES.json CONTROL.json NOW_EPOCH ENV_ID OUT.json [WARM_RUNS]
//!
//! TENANT may also be a per-day directory (`railwatch split`): pages then
//! read through the union reader (storage::reader).
//!
//! Each case is {name, page, path, params}. `page` names the dashboard
//! function. Writes {name => {component, props} | {status}} to OUT.json and
//! per-case sorted timings (ms) to OUT.json with ".timings.json" appended.

use std::collections::HashMap;
use std::time::Instant;

use railwatch_server::dashboard::control::StaticControl;
use railwatch_server::dashboard::pages::*;
use railwatch_server::dashboard::pool::open_reader;
use railwatch_server::dashboard::{CursorKey, PageContext, PageError, Params, shared_props};
use rusqlite::Connection;
use serde_json::Value;

type Page = fn(&Connection, &PageContext, &dyn railwatch_server::dashboard::ControlData) -> Result<Value, PageError>;

fn page(name: &str) -> Option<(Page, &'static str)> {
    Some(match name {
        "overview_show" => (overview_show as Page, "overview/show"),
        "requests_index" => (requests_index, "requests/index"),
        "requests_route" => (requests_route, "requests/route"),
        "jobs_index" => (jobs_index, "jobs/index"),
        "jobs_klass" => (jobs_klass, "jobs/klass"),
        "scheduled_tasks_index" => (scheduled_tasks_index, "scheduled_tasks/index"),
        "commands_index" => (commands_index, "commands/index"),
        "executions_show" => (executions_show, "executions/show"),
        "traces_show" => (traces_show, "traces/show"),
        "people_index" => (people_index, "people/index"),
        "people_show" => (people_show, "people/show"),
        "tenants_index" => (tenants_index, "tenants/index"),
        "tenants_show" => (tenants_show, "tenants/show"),
        "queries_index" => (queries_index, "queries/index"),
        "queries_show" => (queries_show, "queries/show"),
        "exceptions_index" => (exceptions_index, "exceptions/index"),
        "logs_index" => (logs_index, "logs/index"),
        "spans_index" => (spans_index, "spans/index"),
        "spans_show" => (spans_show, "spans/show"),
        "profiles_index" => (profiles_index, "profiles/index"),
        "profiles_show" => (profiles_show, "profiles/show"),
        "view_renders_index" => (view_renders_index, "view_renders/index"),
        "transactions_index" => (transactions_index, "transactions/index"),
        "deprecations_index" => (deprecations_index, "deprecations/index"),
        "cache_events_index" => (cache_events_index, "cache_events/index"),
        "mails_index" => (mails_index, "mails/index"),
        "notifications_index" => (notifications_index, "notifications/index"),
        "broadcasts_index" => (broadcasts_index, "broadcasts/index"),
        "outgoing_requests_index" => (outgoing_requests_index, "outgoing_requests/index"),
        "llm_calls_index" => (llm_calls_index, "llm_calls/index"),
        "storage_ops_index" => (storage_ops_index, "storage_ops/index"),
        "visits_index" => (visits_index, "visits/index"),
        "processes_index" => (processes_index, "processes/index"),
        "releases_index" => (releases_index, "releases/index"),
        "releases_show" => (releases_show, "releases/show"),
        "deploys_index" => (deploys_index, "deploys/index"),
        "deploys_show" => (deploys_show, "deploys/show"),
        "thresholds_index" => (thresholds_index, "thresholds/index"),
        "usage_show" => (usage_show, "usage/show"),
        _ => return None,
    })
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 6 {
        eprintln!("usage: props-dump TENANT CASES CONTROL NOW_EPOCH ENV_ID OUT [WARM]");
        std::process::exit(2);
    }
    let tenant_path = std::path::Path::new(&a[0]);
    let conn = if tenant_path.is_dir() {
        railwatch_server::storage::reader::open(tenant_path).expect("open per-day tenant").0
    } else {
        open_reader(tenant_path).expect("open tenant")
    };
    // RW_SQL_PROFILE=1: every statement's time on stderr (finding slow SQL).
    if std::env::var_os("RW_SQL_PROFILE").is_some() {
        use rusqlite::trace::{TraceEvent, TraceEventCodes};
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_PROFILE, Some(|e: TraceEvent<'_>| {
            if let TraceEvent::Profile(s, d) = e {
                eprintln!("SQLPROF {:.3} {}", d.as_secs_f64() * 1e3, s.sql().replace('\n', " "));
            }
        }));
    }
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let control: StaticControl = serde_json::from_str(&std::fs::read_to_string(&a[2]).unwrap()).unwrap();
    let now = (a[3].parse::<f64>().unwrap() * 1e6).round() as i64;
    let env_id: i64 = a[4].parse().unwrap();
    let warm: usize = a.get(6).map_or(1, |w| w.parse().unwrap());
    let key = CursorKey(b"props-diff".to_vec());
    // overview_show_parallel reads through a pool over the same file.
    let slug = tenant_path.file_stem().unwrap().to_string_lossy().to_string();
    let pools = railwatch_server::dashboard::pool::ReadPools::new(tenant_path.parent().unwrap().to_path_buf(), 8);
    let mut out = serde_json::Map::new();
    let mut timings = serde_json::Map::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let mut params: Params = HashMap::new();
        if let Some(Value::Object(p)) = c.get("params") {
            for (k, v) in p {
                params.insert(k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string));
            }
        }
        if let Some(Value::Object(p)) = c.get("path_params") {
            for (k, v) in p {
                params.insert(k.clone(), v.as_str().unwrap_or("").to_string());
            }
        }
        // "<page>_parallel" runs the concurrent variant of <page>.
        let requested = c["page"].as_str().unwrap();
        let parallel = requested.strip_suffix("_parallel");
        let Some((f, component)) = page(parallel.unwrap_or(requested)) else {
            out.insert(name.into(), serde_json::json!({"status": "no such page"}));
            continue;
        };
        let ctx = PageContext::new(env_id, now, params, key.clone());
        let mut samples = vec![];
        let mut result = None;
        for _ in 0..=warm {
            let t0 = Instant::now();
            let r = match parallel {
                Some("overview_show") => overview_show_parallel(&pools, &slug, &ctx, &control),
                Some("queries_index") => queries_index_parallel(&pools, &slug, &ctx, &control),
                Some(other) => panic!("no parallel variant of {other}"),
                None => f(&conn, &ctx, &control),
            };
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
            result = Some(r);
        }
        samples.remove(0);
        samples.sort_by(|x, y| x.partial_cmp(y).unwrap());
        timings.insert(name.into(), serde_json::json!(samples));
        let v = match result.unwrap() {
            Ok(props) => {
                let mut p = shared_props(&ctx).as_object().unwrap().clone();
                if let Value::Object(m) = props {
                    p.extend(m);
                }
                serde_json::json!({"component": component, "props": p})
            }
            Err(PageError::NotFound) => serde_json::json!({"status": 404}),
            Err(PageError::Unprocessable(m)) => serde_json::json!({"status": 422, "body": m}),
            Err(PageError::Sql(e)) => serde_json::json!({"status": 500, "body": e.to_string()}),
        };
        out.insert(name.into(), v);
    }
    std::fs::write(&a[5], serde_json::to_string(&Value::Object(out)).unwrap()).unwrap();
    std::fs::write(format!("{}.timings.json", a[5]), serde_json::to_string(&Value::Object(timings)).unwrap()).unwrap();
}
