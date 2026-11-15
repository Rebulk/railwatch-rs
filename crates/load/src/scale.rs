//! `rw-load --profile profiles.json`: many apps, each sending what a real
//! app sends, open loop, for the app-count scale test (bench/scale/).
//!
//! Every token is one app. Each app, independently:
//! - sends a 1-record `health` batch every `health_interval_s` (random phase),
//!   as production's gem reporters do (most of production's batches);
//! - sends data batches as a Poisson process at `data_batches_per_min`,
//!   modulated over the wall-clock minute by `minute_weights` (twelve 5 s
//!   buckets: production's scheduled tasks pile up at :00-:15);
//! - draws each data batch's size from production's size quantiles, scaled
//!   by that bucket's `minute_size_mult` (the mean is kept);
//! - fills it with executions shaped like production's per-type counts
//!   (`parents` weights, `children` per execution, `standalone` per execution).
//!
//! Apps are `typical` except every `--busy-every`th (0 = none), which is `busy`.
//! Nothing waits on the server: every batch goes out at its scheduled time on
//! its own task (a gem has one reporter per process; production apps run
//! several), so a slow server shows up as latency and in-flight growth, not as
//! a lower offered rate. A send not answered within --request-timeout counts
//! as dropped (status "error"), and so does one that finds --max-in-flight
//! sends already outstanding ("client_full"). Latency percentiles count only
//! sends scheduled after --warmup seconds (a fresh server's first seconds,
//! e.g. Puma and YJIT warming up); drops count for the whole run.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rand::Rng;
use serde_json::{Value, json};

use super::{Builder, GEM_VERSION, die, now_secs, percentile};

struct Profile {
    health_interval_s: f64,
    data_batches_per_min: f64,
    size_quantiles: Vec<f64>,
    minute_weights: Vec<f64>,
    minute_size_mult: Vec<f64>,
    parents: Vec<(String, f64)>,
    children: Vec<(String, f64)>,
    standalone: Vec<(String, f64)>,
}

impl Profile {
    fn from(v: &Value, name: &str) -> Profile {
        let p = v.get(name).unwrap_or_else(|| die(&format!("profile {name} missing")));
        let num = |k: &str| p[k].as_f64().unwrap_or_else(|| die(&format!("{name}.{k} missing")));
        let list = |k: &str| -> Vec<f64> { p[k].as_array().unwrap_or_else(|| die(&format!("{name}.{k} missing"))).iter().filter_map(Value::as_f64).collect() };
        let map = |k: &str| -> Vec<(String, f64)> {
            p[k].as_object().unwrap_or_else(|| die(&format!("{name}.{k} missing"))).iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0))).collect()
        };
        let pr = Profile {
            health_interval_s: num("health_interval_s"),
            data_batches_per_min: num("data_batches_per_min"),
            size_quantiles: list("size_quantiles"),
            minute_weights: list("minute_weights"),
            minute_size_mult: list("minute_size_mult"),
            parents: map("parents"),
            children: map("children"),
            standalone: map("standalone"),
        };
        if pr.minute_weights.len() != 12 || pr.minute_size_mult.len() != 12 || pr.size_quantiles.len() < 2 {
            die(&format!("profile {name}: need 12 minute buckets and size quantiles"));
        }
        pr
    }

    /// Data-batch rate (per second) in the 5 s bucket of the minute at `wall`.
    fn rate_at(&self, wall: f64) -> f64 {
        self.data_batches_per_min / 60.0 * 12.0 * self.minute_weights[bucket(wall)]
    }

    fn max_rate(&self) -> f64 {
        self.data_batches_per_min / 60.0 * 12.0 * self.minute_weights.iter().cloned().fold(0.0, f64::max)
    }
}

fn bucket(wall: f64) -> usize {
    ((wall.rem_euclid(60.0)) / 5.0) as usize % 12
}

impl Builder {
    fn profile_execution(&mut self, p: &Profile, now: f64, out: &mut Vec<Value>) {
        let eid = self.uuid();
        let trace = self.uuid();
        let (e, tr) = (Some(eid.as_str()), Some(trace.as_str()));
        let total: f64 = p.parents.iter().map(|x| x.1).sum();
        let mut pick = self.rng.random_range(0.0..total);
        let mut parent = p.parents[0].0.as_str();
        for (t, w) in &p.parents {
            if pick < *w {
                parent = t;
                break;
            }
            pick -= w;
        }
        let mut rec = self.record(parent, now, e, tr);
        rec["duration"] = json!(self.rng.random_range(1_000..400_000));
        if parent == "request" {
            rec["route"] = json!(format!("/widgets/{}(.:format)", self.rng.random_range(1..40)));
            rec["status_code"] = json!([200, 200, 200, 200, 302, 404, 500][self.rng.random_range(0..7)]);
        }
        out.push(rec);
        for (t, rate) in &p.children {
            let n = rate.floor() as usize + usize::from(self.rng.random_bool(rate.fract()));
            for _ in 0..n {
                let mut r = self.record(t, now, e, tr);
                if t == "query" {
                    let (conn, sql, group) = self.shapes[self.rng.random_range(0..self.shapes.len())].clone();
                    r["connection"] = json!(conn);
                    r["sql"] = json!(sql);
                    r["_group"] = json!(group);
                    r["duration"] = json!(self.rng.random_range(50..30_000));
                    r["row_count"] = json!(self.rng.random_range(0..50));
                }
                out.push(r);
            }
        }
        for (t, rate) in &p.standalone {
            if self.rng.random_bool(rate.min(1.0)) {
                let mut r = self.record(t, now, e, tr);
                if t == "user" || t == "session" {
                    r["id"] = json!(self.rng.random_range(1..5000).to_string());
                }
                out.push(r);
            }
        }
    }

    fn data_size(&mut self, p: &Profile, wall: f64) -> usize {
        let q = &p.size_quantiles;
        let x = self.rng.random_range(0.0..(q.len() - 1) as f64);
        let (i, f) = (x.floor() as usize, x.fract());
        let s = q[i] + (q[i + 1] - q[i]) * f;
        ((s * p.minute_size_mult[bucket(wall)]).round() as usize).max(1)
    }

    fn gzip(records: &[Value]) -> Vec<u8> {
        let mut ndjson = Vec::with_capacity(records.len() * 600);
        for r in records {
            serde_json::to_writer(&mut ndjson, r).unwrap();
            ndjson.push(b'\n');
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&ndjson).unwrap();
        gz.finish().unwrap()
    }
}

struct Sample {
    app: usize,
    busy: bool,
    health: bool,
    at: f64,
    status: u16, // 0 = error/timeout, 1 = client_full (never sent)
    latency_ms: f64,
    late_ms: f64,    // send start minus its scheduled time
    records: usize,  // offered
    accepted: usize, // what the server said it accepted
}

pub fn wanted(args: &[String]) -> bool {
    args.iter().any(|a| a == "--profile")
}

pub async fn run(args: Vec<String>) {
    let mut url = "http://127.0.0.1:8790".to_string();
    let (mut tokens_file, mut profile_file, mut out) = (None, None, None);
    let (mut duration, mut busy_every, mut seed, mut timeout_s, mut max_in_flight) = (600.0f64, 0usize, 1u64, 60.0f64, 8192usize);
    let mut warmup = 0.0f64;
    let mut i = 0;
    while i < args.len() {
        let next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_else(|| die(&format!("{} needs a value", args[*i - 1])))
        };
        match args[i].as_str() {
            "--url" => url = next(&mut i),
            "--tokens" => tokens_file = Some(next(&mut i)),
            "--profile" => profile_file = Some(next(&mut i)),
            "--duration" => duration = next(&mut i).parse().unwrap_or_else(|_| die("bad --duration")),
            "--busy-every" => busy_every = next(&mut i).parse().unwrap_or_else(|_| die("bad --busy-every")),
            "--seed" => seed = next(&mut i).parse().unwrap_or_else(|_| die("bad --seed")),
            "--request-timeout" => timeout_s = next(&mut i).parse().unwrap_or_else(|_| die("bad --request-timeout")),
            "--max-in-flight" => max_in_flight = next(&mut i).parse().unwrap_or_else(|_| die("bad --max-in-flight")),
            "--warmup" => warmup = next(&mut i).parse().unwrap_or_else(|_| die("bad --warmup")),
            "--out" => out = Some(next(&mut i)),
            other => die(&format!(
                "--profile mode: unknown option {other} (takes --url --tokens --profile --duration --busy-every --seed --request-timeout --max-in-flight --warmup --out)"
            )),
        }
        i += 1;
    }
    let tokens: Vec<String> = std::fs::read_to_string(tokens_file.unwrap_or_else(|| die("--tokens FILE is required")))
        .unwrap_or_else(|e| die(&e.to_string()))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let pv: Value = serde_json::from_str(&std::fs::read_to_string(profile_file.unwrap()).unwrap_or_else(|e| die(&e.to_string())))
        .unwrap_or_else(|e| die(&format!("profile: {e}")));
    let profiles = Arc::new([Profile::from(&pv, "typical"), Profile::from(&pv, "busy")]);
    let url = format!("{}/ingest", url.trim_end_matches('/').trim_end_matches("/ingest"));

    let client: Client<_, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).pool_max_idle_per_host(1024).pool_idle_timeout(Duration::from_secs(30)).build_http();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Sample>();
    let t0 = Instant::now();
    let timeout = Duration::from_secs_f64(timeout_s);
    let busy_apps = if busy_every > 0 { tokens.len().div_ceil(busy_every) } else { 0 };
    eprintln!("rw-load --profile: {} apps ({busy_apps} busy), {duration} s, open loop", tokens.len());

    let mut apps = Vec::new();
    for (app, token) in tokens.iter().enumerate() {
        let busy = busy_every > 0 && app % busy_every == 0;
        let (profiles, client, tx, url, in_flight, max_seen) = (profiles.clone(), client.clone(), tx.clone(), url.clone(), in_flight.clone(), max_seen.clone());
        let auth = format!("Bearer {token}");
        let xff = format!("198.18.{}.{}", app / 250, app % 250 + 1);
        apps.push(tokio::spawn(async move {
            let p = &profiles[usize::from(busy)];
            let mut b = Builder::new(seed.wrapping_mul(1_000_003).wrapping_add(app as u64));
            let mut next_health = b.rng.random_range(0.0..p.health_interval_s);
            let max_rate = p.max_rate();
            let exp = |b: &mut Builder| -> f64 { -(1.0 - b.rng.random_range(0.0..1.0f64)).ln() / max_rate };
            let mut next_data = if max_rate > 0.0 { exp(&mut b) } else { f64::INFINITY };
            // the tail of the last execution that did not fit a batch: sent
            // first in the next one, so every execution reaches the server
            // whole and the sent mix is the profile's (at most one execution)
            let mut pending: Vec<Value> = Vec::new();
            loop {
                let health = next_health <= next_data;
                let at = if health { next_health } else { next_data };
                if at >= duration {
                    return;
                }
                tokio::time::sleep_until((t0 + Duration::from_secs_f64(at)).into()).await;
                let wall = now_secs();
                let records = if health {
                    next_health += p.health_interval_s;
                    vec![b.record("health", wall, None, None)]
                } else {
                    next_data += exp(&mut b);
                    // thinning: accept at rate(t)/max_rate
                    if b.rng.random_range(0.0..1.0) >= p.rate_at(wall) / max_rate {
                        continue;
                    }
                    let n = b.data_size(p, wall);
                    let mut r = std::mem::take(&mut pending);
                    while r.len() < n {
                        b.profile_execution(p, wall, &mut r);
                    }
                    if r.len() > n {
                        pending = r.split_off(n);
                    }
                    r
                };
                let n = records.len();
                if in_flight.load(Ordering::Relaxed) >= max_in_flight {
                    let _ = tx.send(Sample { app, busy, health, at, status: 1, latency_ms: 0.0, late_ms: 0.0, records: n, accepted: 0 });
                    continue;
                }
                let body = Bytes::from(Builder::gzip(&records));
                let req = hyper::Request::post(&url)
                    .header("authorization", &auth)
                    .header("content-type", "application/x-ndjson")
                    .header("content-encoding", "gzip")
                    .header("user-agent", format!("railwatch-ruby/{GEM_VERSION}"))
                    .header("x-railwatch-version", GEM_VERSION)
                    .header("x-railwatch-batch-id", uuid::Uuid::now_v7().to_string())
                    .header("x-forwarded-for", &xff)
                    .body(Full::new(body))
                    .unwrap();
                // how late this send starts against its schedule: record
                // building, gzip and the executor all count. Reported, so a
                // client-limited run cannot pass as low latency.
                let late_ms = (t0.elapsed().as_secs_f64() - at).max(0.0) * 1000.0;
                let now_in = in_flight.fetch_add(1, Ordering::Relaxed) + 1;
                max_seen.fetch_max(now_in, Ordering::Relaxed);
                let (client, tx, in_flight) = (client.clone(), tx.clone(), in_flight.clone());
                tokio::spawn(async move {
                    let sent = Instant::now();
                    let exchange = async {
                        let resp = client.request(req).await.ok()?;
                        let s = resp.status().as_u16();
                        let body = resp.into_body().collect().await.ok()?.to_bytes();
                        let accepted =
                            serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("accepted").and_then(Value::as_u64)).unwrap_or(0) as usize;
                        Some((s, accepted))
                    };
                    let (status, accepted) = tokio::time::timeout(timeout, exchange).await.ok().flatten().unwrap_or((0, 0));
                    in_flight.fetch_sub(1, Ordering::Relaxed);
                    let latency_ms = sent.elapsed().as_secs_f64() * 1000.0;
                    let _ = tx.send(Sample { app, busy, health, at, status, latency_ms, late_ms, records: n, accepted });
                });
            }
        }));
    }
    drop(tx);
    // in-flight per second, for the timeline
    let stop = Arc::new(tokio::sync::Notify::new());
    let sampler = {
        let (in_flight, stop) = (in_flight.clone(), stop.clone());
        tokio::spawn(async move {
            let mut v = Vec::new();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => v.push(in_flight.load(Ordering::Relaxed)),
                    _ = stop.notified() => return v,
                }
            }
        })
    };
    let mut samples = Vec::new();
    while let Some(s) = rx.recv().await {
        samples.push(s);
    }
    for a in apps {
        let _ = a.await;
    }
    stop.notify_one();
    let in_flight_timeline = sampler.await.unwrap_or_default();
    report(&samples, warmup, duration, tokens.len(), busy_apps, max_seen.load(Ordering::Relaxed), &in_flight_timeline, timeout_s, out.as_deref());
}

fn lat_stats(mut v: Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({"count": v.len(), "p50_ms": percentile(&v, 50.0), "p90_ms": percentile(&v, 90.0), "p99_ms": percentile(&v, 99.0),
           "p999_ms": percentile(&v, 99.9), "max_ms": v.last().copied().unwrap_or(0.0)})
}

#[allow(clippy::too_many_arguments)]
fn report(
    samples: &[Sample],
    warmup: f64,
    duration: f64,
    apps: usize,
    busy_apps: usize,
    max_in_flight: usize,
    in_flight: &[usize],
    timeout_s: f64,
    out: Option<&str>,
) {
    let ok = |s: &&Sample| s.status == 200;
    let measured = |s: &&Sample| s.status == 200 && s.at >= warmup;
    let mut status_counts: BTreeMap<String, usize> = BTreeMap::new();
    for s in samples {
        let k = match s.status {
            0 => "error".to_string(),
            1 => "client_full".to_string(),
            n => n.to_string(),
        };
        *status_counts.entry(k).or_default() += 1;
    }
    let ok_lat = |f: &dyn Fn(&Sample) -> bool| lat_stats(samples.iter().filter(measured).filter(|s| f(s)).map(|s| s.latency_ms).collect());
    // records the server accepted (its "accepted"), not offered: a 200 can
    // carry fewer (rejected lines) or none (paused)
    let records_ok: usize = samples.iter().filter(ok).map(|s| s.accepted).sum();
    let records_offered: usize = samples.iter().filter(|s| s.status != 1).map(|s| s.records).sum();
    let mut late: Vec<f64> = samples.iter().filter(|s| s.status != 1 && s.at >= warmup).map(|s| s.late_ms).collect();
    late.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let not_ok = samples.iter().filter(|s| s.status != 200).count();
    let over_3s = samples.iter().filter(|s| s.status == 200 && s.latency_ms > 3000.0).count();
    // per 10 s window: offered batches, ok, p99 of everything sent in it
    let windows = (duration / 10.0).ceil() as usize;
    let mut w: Vec<Vec<&Sample>> = vec![Vec::new(); windows.max(1)];
    for s in samples {
        w[((s.at / 10.0) as usize).min(windows.max(1) - 1)].push(s);
    }
    let timeline: Vec<Value> = w
        .iter()
        .enumerate()
        .map(|(i, ss)| {
            let mut l: Vec<f64> = ss.iter().map(|s| if s.status == 0 { timeout_s * 1000.0 } else { s.latency_ms }).collect();
            l.sort_by(|a, b| a.partial_cmp(b).unwrap());
            json!({"t": i * 10, "offered": ss.len(), "ok": ss.iter().filter(|s| s.status == 200).count(), "p50_ms": percentile(&l, 50.0), "p99_ms": percentile(&l, 99.0),
                   "max_ms": l.last().copied().unwrap_or(0.0)})
        })
        .collect();
    let all: Vec<f64> = samples.iter().filter(measured).map(|s| s.latency_ms).collect();
    let summary = json!({
        "mode": "profile", "warmup_s": warmup, "apps": apps, "busy_apps": busy_apps, "duration_s": duration,
        "posts": samples.len(), "ok": samples.len() - not_ok, "not_ok": not_ok, "status_counts": status_counts,
        "records_ok": records_ok, "records_offered": records_offered, "records_per_s": records_ok as f64 / duration,
        "send_late_p99_ms": percentile(&late, 99.0), "send_late_max_ms": late.last().copied().unwrap_or(0.0), "batches_per_s": samples.len() as f64 / duration,
        "over_3s": over_3s, "max_in_flight": max_in_flight, "request_timeout_s": timeout_s,
        "latency": lat_stats(all), "latency_health": ok_lat(&|s| s.health), "latency_data": ok_lat(&|s| !s.health),
        "latency_busy_apps": ok_lat(&|s| s.busy),
        "apps_with_errors": samples.iter().filter(|s| s.status != 200).map(|s| s.app).collect::<std::collections::BTreeSet<_>>().len(),
        "timeline_10s": timeline, "in_flight_1s": in_flight,
    });
    let l = &summary["latency"];
    println!(
        "rw-load --profile: {apps} apps, {} posts ({:.1}/s), {} not 200 {:?}, {:.0} records/s; p50 {:.1} ms p99 {:.1} ms max {:.1} ms; max in flight {max_in_flight}",
        samples.len(),
        samples.len() as f64 / duration,
        not_ok,
        status_counts,
        records_ok as f64 / duration,
        l["p50_ms"].as_f64().unwrap_or(0.0),
        l["p99_ms"].as_f64().unwrap_or(0.0),
        l["max_ms"].as_f64().unwrap_or(0.0)
    );
    if let Some(path) = out {
        std::fs::write(path, serde_json::to_string_pretty(&summary).unwrap()).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    }
}
