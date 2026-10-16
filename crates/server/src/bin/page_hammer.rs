//! page-hammer: run one dashboard page in a loop on N threads against a
//! tenant file (read-only pooled connections) for S seconds, and print how
//! many pages completed and their median/p99. Used to show that a slow page
//! cannot starve ingest (bench/diff/props/isolation.sh runs it next to
//! rw-load against the same tenant).
//!
//!   page-hammer TENANT.sqlite3 PAGE WINDOW THREADS SECONDS

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use railwatch_server::dashboard::control::StaticControl;
use railwatch_server::dashboard::pages::*;
use railwatch_server::dashboard::pool::ReadPools;
use railwatch_server::dashboard::{CursorKey, PageContext};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 5 {
        eprintln!("usage: page-hammer TENANT PAGE WINDOW THREADS SECONDS");
        std::process::exit(2);
    }
    let path = std::path::PathBuf::from(&a[0]);
    let slug = path.file_stem().unwrap().to_string_lossy().to_string();
    let threads: usize = a[3].parse().unwrap();
    let secs: f64 = a[4].parse().unwrap();
    let pools = ReadPools::new(path.parent().unwrap().to_path_buf(), threads);
    let control = StaticControl::default();
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::<f64>::new()));
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (pools, slug, control, stop, samples, page, window) = (&pools, &slug, &control, stop.clone(), samples.clone(), a[1].clone(), a[2].clone());
            s.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let now = railwatch_server::time::now_micros();
                    let params = [("window".to_string(), window.clone())].into_iter().collect();
                    let ctx = PageContext::new(1, now, params, CursorKey(b"x".to_vec()));
                    let conn = pools.get(slug).expect("reader");
                    let t0 = Instant::now();
                    let r = match page.as_str() {
                        "queries_index" => queries_index(&conn, &ctx, control),
                        "overview_show" => overview_show(&conn, &ctx, control),
                        "requests_index" => requests_index(&conn, &ctx, control),
                        "tenants_index" => tenants_index(&conn, &ctx, control),
                        "logs_index" => logs_index(&conn, &ctx, control),
                        other => panic!("page {other} not wired"),
                    };
                    r.expect("page");
                    samples.lock().unwrap().push(t0.elapsed().as_secs_f64() * 1000.0);
                }
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    let mut v = samples.lock().unwrap().clone();
    v.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let q = |p: f64| v.get(((v.len() as f64 * p).ceil() as usize).saturating_sub(1)).copied().unwrap_or(0.0);
    println!("pages={} median_ms={:.1} p99_ms={:.1}", v.len(), q(0.5), q(0.99));
}
