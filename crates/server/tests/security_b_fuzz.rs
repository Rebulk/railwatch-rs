//! security-b randomized robustness fuzzer (std only; no nightly/cargo-fuzz
//! available in this environment). Time-boxed campaigns over the ingest
//! decode/map path, each call under `catch_unwind`, counting executions and
//! panics and printing the first crashing input as hex/debug.
//!
//! Targets:
//!   A. rjson::parse(random bytes)                 — the JSON parser
//!   B. payload::parse_bytes(random bytes, gzip?)  — NDJSON + gzip framing
//!   C. payload::parse_bytes(gzip(random text))    — the gzip inflate path
//!   D. batch::map_all(random Values)              — mapper + validation + batch
//!   E. mapper::row_for / timestamp_string(random) — per-record mapping
//!   F. rjson::generate / generate_escaped(parsed) — the encoders round-trip
//!
//! Run with: cargo test --test security_b_fuzz -- --nocapture
//! Budget via env: SECB_FUZZ_MS (default 8000 total, split across targets).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use railwatch_server::ingest::{batch, mapper, payload};
use railwatch_server::rjson::{self, Value};

/// A tiny deterministic-but-seeded xorshift RNG (no rand dep).
struct Rng(u64);
impl Rng {
    fn new() -> Rng {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).unwrap();
        Rng(u64::from_le_bytes(b) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

#[derive(Default)]
struct Stats {
    execs: u64,
    panics: u64,
}

impl Stats {
    fn run<F: FnOnce()>(&mut self, label: &str, input: impl std::fmt::Debug, f: F) {
        self.execs += 1;
        if catch_unwind(AssertUnwindSafe(f)).is_err() {
            self.panics += 1;
            if self.panics <= 5 {
                eprintln!("[{label}] PANIC on input: {input:?}");
            }
        }
    }
}

fn budget_ms(parts: u64) -> Duration {
    let total: u64 = std::env::var("SECB_FUZZ_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(8000);
    Duration::from_millis((total / parts).max(250))
}

fn rand_bytes(r: &mut Rng, max: usize) -> Vec<u8> {
    let n = r.below(max + 1);
    // Bias toward JSON-ish bytes so the parser gets past the first char often.
    const ALPHABET: &[u8] = b"{}[]\":,0123456789.-+eEtfnul \t\n\r\\/u\x00\x7f\xffNaIyx";
    (0..n)
        .map(|_| if r.bool() { ALPHABET[r.below(ALPHABET.len())] } else { r.byte() })
        .collect()
}

/// A bounded random Ruby-JSON Value (depth-limited).
fn rand_value(r: &mut Rng, depth: u8) -> Value {
    match r.below(if depth == 0 { 6 } else { 9 }) {
        0 => Value::Nil,
        1 => Value::Bool(r.bool()),
        2 => Value::Int(rand_int(r)),
        3 => Value::Float(rand_float(r)),
        4 => Value::Str(rand_small_str(r)),
        5 => Value::BigInt(rand_bigint(r)),
        6 => {
            let n = r.below(4);
            Value::Array((0..n).map(|_| rand_value(r, depth - 1)).collect())
        }
        7 => {
            let n = r.below(4);
            let mut h = rjson::Hash::new();
            for _ in 0..n {
                h.insert(rand_small_str(r), rand_value(r, depth - 1));
            }
            Value::Hash(h)
        }
        _ => Value::Str(rand_small_str(r)),
    }
}

fn rand_int(r: &mut Rng) -> i64 {
    match r.below(6) {
        0 => i64::MIN,
        1 => i64::MAX,
        2 => 0,
        3 => -1,
        _ => r.next() as i64,
    }
}

fn rand_float(r: &mut Rng) -> f64 {
    match r.below(8) {
        0 => f64::NAN,
        1 => f64::INFINITY,
        2 => f64::NEG_INFINITY,
        3 => 0.0,
        4 => -0.0,
        5 => f64::MAX,
        6 => 1e308 * 10.0,
        _ => f64::from_bits(r.next()),
    }
}

fn rand_bigint(r: &mut Rng) -> String {
    let len = 1 + r.below(40);
    let mut s = String::new();
    if r.bool() {
        s.push('-');
    }
    for _ in 0..len {
        s.push((b'0' + r.byte() % 10) as char);
    }
    s
}

/// Short strings mixing identifiers, control chars, multibyte and invalid UTF-8.
fn rand_small_str(r: &mut Rng) -> Vec<u8> {
    let n = r.below(12);
    let mut out = Vec::new();
    for _ in 0..n {
        match r.below(8) {
            0 => out.extend_from_slice("€".as_bytes()),
            1 => out.extend_from_slice("😀".as_bytes()),
            2 => out.push(r.byte()),       // may be invalid UTF-8
            3 => out.push(0),              // NUL / control
            4 => out.push(b'/'),
            5 => out.extend_from_slice(b".."),
            6 => out.push(b'\\'),
            _ => out.push(b'a' + r.below(26) as u8),
        }
    }
    out
}

/// A record hash that looks like a wire record, with fuzzed fields, so the
/// mapper gets past validate_record often and into cast/serialize.
fn rand_record(r: &mut Rng) -> Value {
    const TYPES: &[&str] = &[
        "request", "query", "exception", "log", "llm_call", "attachment", "profile", "user", "session",
        "process", "health", "span", "enqueued_job", "visit", "metric", "unknown",
    ];
    let mut h = rjson::Hash::new();
    h.insert(b"t".to_vec(), Value::Str(TYPES[r.below(TYPES.len())].as_bytes().to_vec()));
    h.insert(b"v".to_vec(), if r.bool() { Value::Int(1) } else { rand_value(r, 1) });
    // timestamp: often numeric, sometimes hostile.
    h.insert(
        b"timestamp".to_vec(),
        match r.below(6) {
            0 => Value::Float(1_792_000_000.0 + (r.next() % 2_000_000) as f64),
            1 => Value::Float(rand_float(r)),
            2 => Value::Int(rand_int(r)),
            3 => Value::BigInt(rand_bigint(r)),
            _ => rand_value(r, 1),
        },
    );
    for k in ["_group", "execution_id", "trace_id", "name", "message", "sql", "class", "level", "duration", "stacks", "data", "bytes", "frames", "stages", "counters", "locals", "id", "email", "tenant"] {
        if r.bool() {
            h.insert(k.as_bytes().to_vec(), rand_value(r, 2));
        }
    }
    Value::Hash(h)
}

fn meta() -> batch::Meta {
    batch::Meta {
        dropped_by_client: Value::Int(0),
        backpressure_factor: 1.0,
        bytes: Value::Int(0),
        gem_version: None,
        received_at_micros: 1_792_000_000_000_000,
    }
}

#[test]
fn fuzz_ingest_decode_and_map() {
    // Quiet the default panic hook so catch_unwind doesn't spam backtraces;
    // we print our own one-line crash record.
    std::panic::set_hook(Box::new(|_| {}));
    let mut r = Rng::new();
    let mut total = Stats::default();

    // A: JSON parser on random bytes.
    let mut a = Stats::default();
    let t = Instant::now();
    let d = budget_ms(6);
    while t.elapsed() < d {
        let input = rand_bytes(&mut r, 512);
        let bytes = input.clone();
        a.run("rjson::parse", Hex(&input), || {
            if let Ok(v) = rjson::parse(&bytes) {
                // F: encoders must never panic on a parsed value.
                let _ = rjson::generate(&v);
                let _ = rjson::generate_escaped(&v);
            }
        });
    }

    // B: NDJSON + gzip framing on random bytes (both flags).
    let mut b = Stats::default();
    let t = Instant::now();
    let d = budget_ms(6);
    while t.elapsed() < d {
        let input = rand_bytes(&mut r, 512);
        let gzip = r.bool();
        let bytes = input.clone();
        b.run("payload::parse_bytes/raw", (gzip, Hex(&input)), || {
            let _ = payload::parse_bytes(&bytes, gzip);
        });
    }

    // C: gzip inflate path on valid gzip of random (compressible) text.
    let mut c = Stats::default();
    let t = Instant::now();
    let d = budget_ms(6);
    while t.elapsed() < d {
        let mut raw = Vec::new();
        let lines = r.below(50);
        for _ in 0..lines {
            raw.extend_from_slice(&rand_bytes(&mut r, 64));
            raw.push(b'\n');
        }
        let gz = gzip(&raw);
        c.run("payload::parse_bytes/gzip", Hex(&gz[..gz.len().min(48)]), || {
            let _ = payload::parse_bytes(&gz, true);
        });
    }

    // D: batch::map_all on vectors of random record Values.
    let mut dd = Stats::default();
    let floor = 1_792_000_000.0 - 30.0 * 86_400.0;
    let t = Instant::now();
    let d = budget_ms(6);
    while t.elapsed() < d {
        let n = r.below(8);
        let recs: Vec<Value> = (0..n).map(|_| rand_record(&mut r)).collect();
        let dbg = format!("{:?}", recs);
        dd.run("batch::map_all", Trunc(&dbg), || {
            let _ = batch::map_all(recs.clone(), floor, meta(), None);
        });
    }

    // E: per-record mapper entry points on arbitrary Values (no range gate).
    let mut e = Stats::default();
    let t = Instant::now();
    let d = budget_ms(6);
    while t.elapsed() < d {
        let v = rand_record(&mut r);
        let dbg = format!("{v:?}");
        e.run("mapper::row_for", Trunc(&dbg), || {
            let mut tr = Vec::new();
            let _ = mapper::validate_record(&v);
            let _ = mapper::row_for(&v, &mut tr);
            let _ = mapper::timestamp_string(v.get("timestamp"));
        });
    }

    let _ = std::panic::take_hook();
    for s in [&a, &b, &c, &dd, &e] {
        total.execs += s.execs;
        total.panics += s.panics;
    }
    eprintln!(
        "security-b fuzz: A(json)={}/{} B(ndjson)={}/{} C(gzip)={}/{} D(map_all)={}/{} E(row_for)={}/{}  => total {} execs, {} panics",
        a.panics, a.execs, b.panics, b.execs, c.panics, c.execs, dd.panics, dd.execs, e.panics, e.execs, total.execs, total.panics
    );
    assert_eq!(total.panics, 0, "fuzzer found {} panics across {} executions", total.panics, total.execs);
}

struct Hex<'a>(&'a [u8]);
impl std::fmt::Debug for Hex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

struct Trunc<'a>(&'a str);
impl std::fmt::Debug for Trunc<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0;
        if s.len() <= 300 { f.write_str(s) } else { write!(f, "{}…", &s[..300]) }
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}
