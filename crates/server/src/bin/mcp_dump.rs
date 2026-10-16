//! mcp-dump: run JSON-RPC requests through the MCP server (web::mcp::call)
//! at a frozen "now", for the MCP diff harness (bench/diff/mcp/run.sh).
//!
//!   mcp-dump DATA_DIR CASES.json NOW_EPOCH OUT.json [WARM_RUNS]
//!
//! DATA_DIR is a server data dir (control.sqlite3, telemetry/,
//! secret_key_base). Each case is {name, token, body, content_type?,
//! write?} (content_type is sent to Rails only: /mcp reads the raw body
//! as JSON whatever it says); `token` names an entry in DATA_DIR/mcp-tokens.json. Writes
//! {name => {status, body}} to OUT.json and per-case sorted timings (ms) to
//! OUT.json.timings.json. Write cases run once (warm runs would repeat the
//! write).

use std::time::Instant;

use axum::body::Bytes;
use railwatch_server::config::Config;
use railwatch_server::control::Control;
use railwatch_server::dashboard::CursorKey;
use railwatch_server::dashboard::pool::ReadPools;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::mcp::{Env, call};
use serde_json::Value;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("usage: mcp-dump DATA_DIR CASES NOW_EPOCH OUT [WARM]");
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&a[0]);
    // SAFETY: single-threaded here, before anything reads the environment.
    unsafe { std::env::set_var("RAILWATCH_DATA_DIR", &dir) };
    let cfg = Config::load().expect("config");
    let control = Control::open(&cfg.control_path()).expect("control db");
    let (followups, _rx) = railwatch_server::followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), std::time::Duration::from_secs(60), followups);
    let readers = ReadPools::new(cfg.tenants_dir(), 4);
    let secret = cfg.secret().expect("secret");
    let cursor_key = CursorKey(railwatch_server::web::cursor_key_bytes(&secret));
    let env = Env { control: &control, tenants: &tenants, readers: &readers, cursor_key: &cursor_key };

    let tokens: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("mcp-tokens.json")).expect("mcp-tokens.json")).unwrap();
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let now = (a[2].parse::<f64>().unwrap() * 1e6).round() as i64;
    let warm: usize = a.get(4).map_or(0, |w| w.parse().unwrap());

    let mut out = serde_json::Map::new();
    let mut timings = serde_json::Map::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let auth = c["token"].as_str().and_then(|t| tokens[t].as_str()).map(|raw| format!("Bearer {raw}"));
        let auth = c["authorization"].as_str().map(str::to_string).or(auth);
        let body = match &c["body"] {
            Value::String(s) => Bytes::from(s.clone()),
            v => Bytes::from(v.to_string()),
        };
        let runs = if c["write"].as_bool() == Some(true) { 1 } else { warm + 1 };
        let mut samples = vec![];
        let mut result = None;
        for _ in 0..runs {
            let t0 = Instant::now();
            result = Some(call(&env, now, auth.as_deref(), &body));
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        if samples.len() > 1 {
            samples.remove(0);
        }
        samples.sort_by(|x, y| x.partial_cmp(y).unwrap());
        timings.insert(name.into(), serde_json::json!(samples));
        let (status, v) = result.unwrap();
        out.insert(name.into(), serde_json::json!({"status": status.as_u16(), "body": v}));
    }
    std::fs::write(&a[3], serde_json::to_string(&Value::Object(out)).unwrap()).unwrap();
    std::fs::write(format!("{}.timings.json", a[3]), serde_json::to_string(&Value::Object(timings)).unwrap()).unwrap();
}
