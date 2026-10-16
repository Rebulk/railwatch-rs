//! rw-load: a load generator that speaks the railwatch gem's wire protocol.
//!
//! Every batch is gzip NDJSON with the gem's headers (X-Railwatch-Version,
//! X-Railwatch-Batch-Id as a UUIDv7, User-Agent railwatch-ruby/0.9.0; with
//! --receipted also Producer-Id, Body-SHA256 and Policy). Records come from
//! the gem's wire fixtures, shaped like production: per execution one parent
//! (80% request / 20% job_attempt), ~8 queries drawn from a pool of 200 query
//! shapes whose SQL digests to its group (so query_shapes dedupe applies),
//! 2 cache events, 1 view render, 2 logs, 0.2 outgoing requests and 0.05
//! exceptions, plus occasional health/process/user/visit/span records.
//!
//! All batches are built before the timed phase and each is sent exactly
//! once, so INSERT OR IGNORE cannot make a repeat cheaper.
//!
//!   rw-load --url http://127.0.0.1:8790 --tokens tokens.txt --concurrency 32 \
//!           --rate 0 --duration 30 --xff-rotate 8 --out results.json
//!
//! --spread-days D draws each batch's time uniformly from the last D days
//! (default 0: every record at now), to build multi-day tenants for the
//! dashboard timings (bench/diff/props/perf.sh).

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::{Value, json};
use sha2::Digest as _; // also brings Md5::digest into scope

const FIXTURES: &str = include_str!("../../server/tests/fixtures/wire_fixtures.json");
const RECORDS_PER_BATCH: usize = 500; // the gem's flush_threshold
const GEM_VERSION: &str = "0.9.0";

struct Opts {
    url: String,
    tokens: Vec<String>,
    concurrency: usize,
    rate: f64,
    duration: f64,
    xff_rotate: usize,
    receipted: bool,
    out: Option<String>,
    batches: Option<usize>,
    seed: u64,
    /// Spread each batch's timestamps over the last N days (0 = all at now).
    spread_days: f64,
    /// PEM CA to trust for https:// besides the webpki roots.
    cacert: Option<String>,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        url: "http://127.0.0.1:8790".into(),
        tokens: vec![],
        concurrency: 16,
        rate: 0.0,
        duration: 30.0,
        xff_rotate: 0,
        receipted: false,
        out: None,
        batches: None,
        seed: 1,
        spread_days: 0.0,
        cacert: None,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let mut tokens_file = None;
    let mut token = None;
    while i < args.len() {
        let next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_else(|| die(&format!("{} needs a value", args[*i - 1])))
        };
        match args[i].as_str() {
            "--url" => o.url = next(&mut i),
            "--tokens" => tokens_file = Some(next(&mut i)),
            "--token" => token = Some(next(&mut i)),
            "--concurrency" => o.concurrency = next(&mut i).parse().unwrap_or_else(|_| die("bad --concurrency")),
            "--rate" => o.rate = next(&mut i).parse().unwrap_or_else(|_| die("bad --rate")),
            "--duration" => o.duration = next(&mut i).parse().unwrap_or_else(|_| die("bad --duration")),
            "--xff-rotate" => o.xff_rotate = next(&mut i).parse().unwrap_or_else(|_| die("bad --xff-rotate")),
            "--batches" => o.batches = Some(next(&mut i).parse().unwrap_or_else(|_| die("bad --batches"))),
            "--seed" => o.seed = next(&mut i).parse().unwrap_or_else(|_| die("bad --seed")),
            "--receipted" => o.receipted = true,
            "--spread-days" => o.spread_days = next(&mut i).parse().unwrap_or_else(|_| die("bad --spread-days")),
            "--out" => o.out = Some(next(&mut i)),
            "--cacert" => o.cacert = Some(next(&mut i)),
            "-h" | "--help" => {
                eprintln!(
                    "rw-load --url URL (--tokens FILE | --token T) [--concurrency N] [--rate BATCHES_PER_S, 0 = closed loop]\n        [--duration S] [--batches N] [--xff-rotate N] [--receipted] [--seed N] [--spread-days D] [--cacert ca.pem] [--out results.json]"
                );
                std::process::exit(0);
            }
            other => die(&format!("unknown option {other}")),
        }
        i += 1;
    }
    if let Some(f) = tokens_file {
        o.tokens = std::fs::read_to_string(&f)
            .unwrap_or_else(|e| die(&format!("{f}: {e}")))
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
    }
    if let Some(t) = token {
        o.tokens.push(t);
    }
    if o.tokens.is_empty() {
        die("--tokens FILE or --token T is required");
    }
    o.url = o.url.trim_end_matches('/').to_string();
    if !o.url.ends_with("/ingest") {
        o.url.push_str("/ingest");
    }
    o
}

fn die(msg: &str) -> ! {
    eprintln!("rw-load: {msg}");
    std::process::exit(2)
}

// ---------------------------------------------------------------------------
// Record building

struct Builder {
    fixtures: serde_json::Map<String, Value>,
    shapes: Vec<(String, String, String)>, // (connection, sql, group)
    rng: SmallRng,
}

fn md5_hex(s: &str) -> String {
    md5::Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Builder {
    fn new(seed: u64) -> Builder {
        let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
        let tables = ["widgets", "gadgets", "users", "orders", "line_items", "accounts", "sessions", "events"];
        let shapes = (0..200)
            .map(|i| {
                let t = tables[i % tables.len()];
                let sql = format!(r#"SELECT "{t}".* FROM "{t}" WHERE "{t}"."id" = ? AND "{t}"."c{i}" = ? LIMIT ?"#);
                let conn = if i % 5 == 0 { "replica" } else { "primary" };
                let group = md5_hex(&format!("{conn},{sql}"));
                (conn.to_string(), sql, group)
            })
            .collect();
        Builder { fixtures: fixtures.as_object().unwrap().clone(), shapes, rng: SmallRng::seed_from_u64(seed) }
    }

    fn uuid(&mut self) -> String {
        uuid::Builder::from_random_bytes(self.rng.random()).into_uuid().to_string()
    }

    fn record(&mut self, t: &str, now: f64, eid: Option<&str>, trace: Option<&str>) -> Value {
        let mut r = self.fixtures[t].clone();
        r["timestamp"] = json!(now - self.rng.random_range(0.0..2.0));
        if r.get("execution_id").is_some() {
            r["execution_id"] = json!(eid);
        }
        if r.get("trace_id").is_some() {
            r["trace_id"] = json!(trace);
        }
        r
    }

    fn chance(&mut self, p: f64) -> bool {
        self.rng.random_bool(p)
    }

    fn execution(&mut self, now: f64, out: &mut Vec<Value>) {
        let eid = self.uuid();
        let trace = self.uuid();
        let (e, tr) = (Some(eid.as_str()), Some(trace.as_str()));
        let parent = if self.chance(0.8) { "request" } else { "job_attempt" };
        let mut p = self.record(parent, now, e, tr);
        p["duration"] = json!(self.rng.random_range(1_000..400_000));
        if parent == "request" {
            p["route"] = json!(format!("/widgets/{}(.:format)", self.rng.random_range(1..40)));
            p["status_code"] = json!([200, 200, 200, 200, 302, 404, 500][self.rng.random_range(0..7)]);
        }
        out.push(p);
        for _ in 0..self.rng.random_range(6..=10) {
            let mut q = self.record("query", now, e, tr);
            let (conn, sql, group) = self.shapes[self.rng.random_range(0..self.shapes.len())].clone();
            q["connection"] = json!(conn);
            q["sql"] = json!(sql);
            q["_group"] = json!(group);
            q["duration"] = json!(self.rng.random_range(50..30_000));
            q["row_count"] = json!(self.rng.random_range(0..50));
            out.push(q);
        }
        for _ in 0..2 {
            let mut c = self.record("cache_event", now, e, tr);
            c["key"] = json!(format!("views/widgets/{}", self.rng.random_range(0..5000)));
            out.push(c);
        }
        out.push(self.record("view_render", now, e, tr));
        for msg in ["Started GET \"/widgets\" for 127.0.0.1", "Completed 200 OK in 12ms (Views: 3.1ms | ActiveRecord: 4.2ms)"] {
            let mut l = self.record("log", now, e, tr);
            l["message"] = json!(msg);
            out.push(l);
        }
        if self.chance(0.2) {
            out.push(self.record("outgoing_request", now, e, tr));
        }
        if self.chance(0.05) {
            out.push(self.record("exception", now, e, tr));
        }
        if self.chance(0.03) {
            let t = ["health", "process", "user", "visit", "span"][self.rng.random_range(0..5)];
            let mut r = self.record(t, now, e, tr);
            if t == "user" {
                r["id"] = json!(self.rng.random_range(1..5000).to_string());
            }
            out.push(r);
        }
    }

    fn batch(&mut self, now: f64) -> Vec<u8> {
        let mut records = Vec::with_capacity(RECORDS_PER_BATCH + 16);
        while records.len() < RECORDS_PER_BATCH {
            self.execution(now, &mut records);
        }
        records.truncate(RECORDS_PER_BATCH);
        let mut ndjson = Vec::with_capacity(records.len() * 600);
        for r in &records {
            serde_json::to_writer(&mut ndjson, r).unwrap();
            ndjson.push(b'\n');
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&ndjson).unwrap();
        gz.finish().unwrap()
    }
}

struct Prebuilt {
    body: Bytes,
    headers: Vec<(&'static str, String)>,
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

// ---------------------------------------------------------------------------
// Sending

#[derive(Default)]
struct Sample {
    at: f64,
    status: u16, // 0 = transport error
    latency_ms: f64,
    events: usize,
}

#[tokio::main]
async fn main() {
    let o = parse_args();
    let count = o.batches.unwrap_or_else(|| {
        if o.rate > 0.0 { (o.rate * o.duration).ceil() as usize } else { (o.concurrency * 400).max(2000) }
    });
    eprintln!("building {count} batches of {RECORDS_PER_BATCH} records...");
    let started_build = Instant::now();
    let producer = uuid::Uuid::new_v4().to_string();
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let per = count.div_ceil(workers);
    let now = now_secs();
    let mut handles = Vec::new();
    for w in 0..workers {
        let n = per.min(count.saturating_sub(w * per));
        let seed = o.seed.wrapping_mul(1_000_003).wrapping_add(w as u64);
        let (receipted, producer, spread) = (o.receipted, producer.clone(), o.spread_days * 86_400.0);
        handles.push(std::thread::spawn(move || {
            let mut b = Builder::new(seed);
            (0..n)
                .map(|_| {
                    // --spread-days: one anchor per batch, uniform over the
                    // window (records keep their 0-2 s jitter around it).
                    let at = if spread > 0.0 { now - b.rng.random_range(0.0..spread) } else { now };
                    let body = b.batch(at);
                    let mut headers = vec![
                        ("content-type", "application/x-ndjson".to_string()),
                        ("content-encoding", "gzip".to_string()),
                        ("user-agent", format!("railwatch-ruby/{GEM_VERSION}")),
                        ("x-railwatch-version", GEM_VERSION.to_string()),
                        ("x-railwatch-batch-id", uuid::Uuid::now_v7().to_string()),
                    ];
                    if receipted {
                        let digest: String = sha2::Sha256::digest(&body).iter().map(|x| format!("{x:02x}")).collect();
                        headers.push(("x-railwatch-producer-id", producer.clone()));
                        headers.push(("x-railwatch-body-sha256", digest));
                        headers.push(("x-railwatch-policy", "everything-v1".to_string()));
                    }
                    Prebuilt { body: Bytes::from(body), headers }
                })
                .collect::<Vec<_>>()
        }));
    }
    let batches: Vec<Prebuilt> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let bytes: usize = batches.iter().map(|b| b.body.len()).sum();
    eprintln!(
        "built {} batches ({} records, {:.1} MiB gzipped) in {:.1}s",
        batches.len(),
        batches.len() * RECORDS_PER_BATCH,
        bytes as f64 / 1048576.0,
        started_build.elapsed().as_secs_f64()
    );

    let batches = Arc::new(batches);
    let next = Arc::new(AtomicUsize::new(0));
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(o.concurrency)
        .pool_idle_timeout(Duration::from_secs(30))
        .build(https_connector(o.cacert.as_deref()));
    let opts = Arc::new(o);
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs_f64(opts.duration);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Sample>();

    // Open loop: a ticker hands out send slots at --rate; closed loop: each
    // worker sends its next batch as soon as the previous one answers.
    let permits = Arc::new(tokio::sync::Semaphore::new(0));
    if opts.rate > 0.0 {
        let permits = permits.clone();
        let rate = opts.rate;
        tokio::spawn(async move {
            let mut sent = 0u64;
            loop {
                let due = (t0.elapsed().as_secs_f64() * rate) as u64;
                if due > sent {
                    permits.add_permits((due - sent) as usize);
                    sent = due;
                }
                if Instant::now() >= deadline {
                    permits.close();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
    }

    let mut tasks = Vec::new();
    for _ in 0..opts.concurrency {
        let (client, batches, next, opts, tx, permits) =
            (client.clone(), batches.clone(), next.clone(), opts.clone(), tx.clone(), permits.clone());
        tasks.push(tokio::spawn(async move {
            loop {
                if Instant::now() >= deadline {
                    return;
                }
                if opts.rate > 0.0 {
                    match permits.acquire().await {
                        Ok(p) => p.forget(),
                        Err(_) => return,
                    }
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(b) = batches.get(i) else { return };
                let token = &opts.tokens[i % opts.tokens.len()];
                let mut req = hyper::Request::post(&opts.url).header("authorization", format!("Bearer {token}"));
                for (k, v) in &b.headers {
                    req = req.header(*k, v);
                }
                if opts.xff_rotate > 0 {
                    let n = i % opts.xff_rotate;
                    req = req.header("x-forwarded-for", format!("198.18.{}.{}", n / 250, n % 250 + 1));
                }
                let req = req.body(Full::new(b.body.clone())).unwrap();
                let sent = Instant::now();
                let (status, events) = match client.request(req).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp.into_body().collect().await.map(|c| c.to_bytes()).unwrap_or_default();
                        let accepted = serde_json::from_slice::<Value>(&body)
                            .ok()
                            .and_then(|v| v.get("accepted").and_then(Value::as_u64))
                            .unwrap_or(0) as usize;
                        (status, accepted)
                    }
                    Err(_) => (0, 0),
                };
                let _ = tx.send(Sample {
                    at: sent.duration_since(t0).as_secs_f64(),
                    status,
                    latency_ms: sent.elapsed().as_secs_f64() * 1000.0,
                    events,
                });
            }
        }));
    }
    drop(tx);
    let mut samples = Vec::new();
    while let Some(s) = rx.recv().await {
        samples.push(s);
    }
    for t in tasks {
        let _ = t.await;
    }
    let elapsed = t0.elapsed().as_secs_f64().min(opts.duration.max(0.001)).max(samples.iter().map(|s| s.at).fold(0.0, f64::max));
    report(&opts, &samples, elapsed, batches.len());
}

/// http:// as before; https:// verified against webpki roots plus `cacert`.
fn https_connector(cacert: Option<&str>) -> hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = cacert {
        for c in CertificateDer::pem_file_iter(path).unwrap_or_else(|e| die(&format!("--cacert {path}: {e}"))) {
            roots.add(c.unwrap_or_else(|e| die(&format!("--cacert {path}: {e}")))).unwrap_or_else(|e| die(&format!("--cacert {path}: {e}")));
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(tls).https_or_http().enable_http1().wrap_connector(http)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx]
}

fn report(o: &Opts, samples: &[Sample], elapsed: f64, built: usize) {
    let ok: Vec<&Sample> = samples.iter().filter(|s| s.status == 200).collect();
    let events: usize = ok.iter().map(|s| s.events).sum();
    let mut by_status: BTreeMap<u16, Vec<f64>> = BTreeMap::new();
    for s in samples {
        by_status.entry(s.status).or_default().push(s.latency_ms);
    }
    let mut latency = serde_json::Map::new();
    println!("rw-load: {} posts in {:.1}s against {} ({} env tokens, concurrency {}, rate {})",
        samples.len(), elapsed, o.url, o.tokens.len(), o.concurrency,
        if o.rate > 0.0 { format!("{}/s open loop", o.rate) } else { "closed loop".into() });
    if samples.len() == built {
        println!("  WARNING: every prebuilt batch was sent; the run may have been limited by --batches, not the server");
    }
    println!("  batches/s (200): {:.1}   events/s accepted: {:.0}", ok.len() as f64 / elapsed, events as f64 / elapsed);
    println!("  {:>6} {:>8} {:>9} {:>9} {:>9} {:>9}", "status", "count", "p50 ms", "p90 ms", "p99 ms", "max ms");
    for (status, lat) in &mut by_status {
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let label = if *status == 0 { "error".to_string() } else { status.to_string() };
        println!(
            "  {:>6} {:>8} {:>9.1} {:>9.1} {:>9.1} {:>9.1}",
            label, lat.len(), percentile(lat, 50.0), percentile(lat, 90.0), percentile(lat, 99.0), lat.last().copied().unwrap_or(0.0)
        );
        latency.insert(
            label,
            json!({"count": lat.len(), "p50_ms": percentile(lat, 50.0), "p90_ms": percentile(lat, 90.0),
                   "p99_ms": percentile(lat, 99.0), "max_ms": lat.last().copied().unwrap_or(0.0)}),
        );
    }
    let secs = elapsed.ceil() as usize;
    let mut timeline = vec![(0usize, 0usize, 0usize); secs.max(1)];
    for s in samples {
        let i = (s.at as usize).min(timeline.len() - 1);
        timeline[i].0 += 1;
        if s.status == 200 {
            timeline[i].1 += 1;
            timeline[i].2 += s.events;
        }
    }
    println!("  timeline (s: posts / ok / events):");
    let line: Vec<String> = timeline.iter().enumerate().map(|(i, t)| format!("{i}:{}/{}/{}", t.0, t.1, t.2)).collect();
    for chunk in line.chunks(10) {
        println!("    {}", chunk.join("  "));
    }
    if let Some(path) = &o.out {
        let out = json!({
            "url": o.url, "tokens": o.tokens.len(), "concurrency": o.concurrency, "rate": o.rate,
            "duration_s": elapsed, "receipted": o.receipted, "records_per_batch": RECORDS_PER_BATCH,
            "posts": samples.len(), "batches_ok": ok.len(),
            "batches_per_s": ok.len() as f64 / elapsed, "events_per_s": events as f64 / elapsed,
            "status_counts": by_status.iter().map(|(k, v)| (k.to_string(), json!(v.len()))).collect::<serde_json::Map<_, _>>(),
            "latency": latency,
            "timeline": timeline.iter().enumerate().map(|(i, t)| json!({"s": i, "posts": t.0, "ok": t.1, "events": t.2})).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_string_pretty(&out).unwrap()).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        println!("  results -> {path}");
    }
}
