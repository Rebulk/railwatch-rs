//! api-dump: run HTTP requests through the JSON API (web::api::call) at a
//! frozen "now", for the API diff harness (bench/diff/api/run.sh).
//!
//!   api-dump DATA_DIR CASES.json NOW_EPOCH OUT.json [WARM_RUNS]
//!
//! DATA_DIR is a server data dir (control.sqlite3, telemetry/,
//! secret_key_base). Each case is {name, method, path, token?,
//! authorization?, headers?, body?, content_type?, write?, burst?,
//! cursor_from?}; `token` names an entry in DATA_DIR/api-tokens.json.
//! Writes {name => {status, headers, body}} to OUT.json (body parsed when
//! JSON, else the raw text) and per-case sorted timings (ms) to
//! OUT.json.timings.json. Write cases run once (warm runs would repeat the
//! write). The rate limit is a fresh counter per run, as the Rails capture
//! clears its cache; `burst: N` sends the request N times first.
//! `cursor_from: CASE` appends `&cursor=` with that earlier case's own
//! results_meta.next_cursor (each server signs cursors with its own key).

use std::time::Instant;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, header};
use railwatch_server::config::Config;
use railwatch_server::control::Control;
use railwatch_server::dashboard::CursorKey;
use railwatch_server::dashboard::pool::ReadPools;
use railwatch_server::tenants::Tenants;
use railwatch_server::web::api::{ApiRequest, call, rate_limit};
use railwatch_server::web::mcp::Env;
use serde_json::{Value, json};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("usage: api-dump DATA_DIR CASES NOW_EPOCH OUT [WARM]");
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

    let tokens: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("api-tokens.json")).expect("api-tokens.json")).unwrap();
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let now = (a[2].parse::<f64>().unwrap() * 1e6).round() as i64;
    let warm: usize = a.get(4).map_or(0, |w| w.parse().unwrap());
    // Model callbacks (now_db in issue activities, comments) read the clock.
    railwatch_server::time::travel_to(now);

    let mut out = serde_json::Map::new();
    let mut timings = serde_json::Map::new();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let mut headers = HeaderMap::new();
        let auth = c["token"].as_str().and_then(|t| tokens[t].as_str()).map(|raw| format!("Bearer {raw}"));
        if let Some(auth) = c["authorization"].as_str().map(str::to_string).or(auth) {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&auth).unwrap());
        }
        if let Some(h) = c["headers"].as_object() {
            for (k, v) in h {
                headers.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v.as_str().unwrap()).unwrap());
            }
        }
        let body = match &c["body"] {
            Value::Null => Bytes::new(),
            Value::String(s) => Bytes::from(s.clone()),
            v => Bytes::from(v.to_string()),
        };
        if !body.is_empty() {
            let ct = c["content_type"].as_str().unwrap_or("application/json");
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(ct).unwrap());
        }
        let mut full = c["path"].as_str().unwrap().to_string();
        if let Some(from) = c["cursor_from"].as_str() {
            let cursor = out.get(from).and_then(|o| o["body"]["results_meta"]["next_cursor"].as_str()).unwrap_or("");
            let enc: String = form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
            full.push_str(&format!("&cursor={enc}"));
        }
        let (path, query) = match full.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (full.to_string(), None),
        };
        let req = ApiRequest { method: Method::from_bytes(c["method"].as_str().unwrap_or("GET").as_bytes()).unwrap(), path, query, headers, body };
        let runs = if c["write"].as_bool() == Some(true) { 1 } else { warm + 1 };
        let mut samples = vec![];
        let mut result = None;
        let burst = c["burst"].as_u64().unwrap_or(0);
        for _ in 0..runs {
            let limit = rate_limit();
            for _ in 0..burst {
                call(&env, &limit, now, &req);
            }
            let t0 = Instant::now();
            result = Some(call(&env, &limit, now, &req));
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        if samples.len() > 1 {
            samples.remove(0);
        }
        samples.sort_by(|x, y| x.partial_cmp(y).unwrap());
        timings.insert(name.into(), json!(samples));
        let r = result.unwrap();
        let mut h = serde_json::Map::new();
        for (k, v) in &r.headers {
            h.insert(k.as_str().to_string(), Value::String(v.to_str().unwrap_or("").to_string()));
        }
        let text = String::from_utf8_lossy(&r.body).into_owned();
        let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        let mut entry = json!({"status": r.status.as_u16(), "headers": h, "body": parsed});
        // API_DUMP_RAW=1: keep the exact bytes too (escaping, number format).
        if std::env::var_os("API_DUMP_RAW").is_some() {
            entry["raw"] = Value::String(String::from_utf8_lossy(&r.body).into_owned());
        }
        out.insert(name.into(), entry);
    }
    std::fs::write(&a[3], serde_json::to_string(&Value::Object(out)).unwrap()).unwrap();
    std::fs::write(format!("{}.timings.json", a[3]), serde_json::to_string(&Value::Object(timings)).unwrap()).unwrap();
}
