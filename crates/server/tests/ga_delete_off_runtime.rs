//! PR #107 review L3: deleting an environment waits (up to 5 s) for its
//! busy ingest writer to stop. That wait ran on a tokio worker, so deleting
//! an account with several busy environments could hold one for N x 5 s.
//! It runs on the blocking pool now: on a single-worker runtime, other
//! requests are still answered while the delete waits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use railwatch_server::config::Config;
use railwatch_server::control::Control;
use railwatch_server::followups;
use railwatch_server::tenants::{Job, Tenants};
use railwatch_server::web::{self, WebState};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn deleting_a_busy_environment_does_not_hold_the_async_runtime() {
    let d = std::env::temp_dir().join(format!("rw-ga-l3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    unsafe {
        std::env::set_var("RAILWATCH_DATA_DIR", d.to_str().unwrap());
        std::env::set_var("RAILWATCH_SECRET_KEY_BASE", SECRET);
        std::env::set_var("RAILWATCH_URL", "http://127.0.0.1");
        std::env::remove_var("RAILWATCH_CONFIG");
    }
    let cfg = Config::load().unwrap();
    let control = Arc::new(Control::open(&cfg.control_path()).unwrap());
    let (tx, _rx) = followups::channel();
    let tenants = Tenants::new(cfg.tenants_dir(), Duration::from_secs(300), tx);
    let slug = "busy-web-production";
    tenants.ensure(slug).unwrap();
    let state = Arc::new(WebState::new(control, cfg.clone(), tenants.clone(), web::cable::Hub::new()).unwrap());

    // The writer opens, then its next job waits on a write lock this test
    // holds on the meta file: it is busy for as long as the lock is held.
    let lookup = |t: &Tenants| {
        let (reply, rx) = tokio::sync::oneshot::channel();
        t.send(1, slug, Job::LookupReceipt { producer_id: "p".into(), delivery_id: "d".into(), reply });
        rx
    };
    lookup(&tenants).blocking_recv().unwrap().unwrap();
    let meta = cfg.tenants_dir().join(slug).join("meta.sqlite3");
    let blocker = rusqlite::Connection::open(&meta).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let (reply, _busy) = tokio::sync::oneshot::channel();
    let meta = railwatch_server::ingest::batch::Meta {
        dropped_by_client: railwatch_server::rjson::Value::Int(0),
        backpressure_factor: 1.0,
        bytes: railwatch_server::rjson::Value::Int(0),
        gem_version: None,
        received_at_micros: railwatch_server::time::now_micros(),
    };
    let batch = railwatch_server::ingest::batch::map_all(Vec::new(), 0.0, meta, None);
    tenants.send(1, slug, Job::Write { batch: Box::new(batch), gc_floor_micros: 0, reply });
    std::thread::sleep(Duration::from_millis(100));

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let ticked = rt.block_on(async {
        let deleting = tokio::spawn({
            let state = state.clone();
            async move { web::controllers::destroy_tenants(&state, vec![slug.to_string()]).await }
        });
        // While the delete waits on the writer, the one worker still runs
        // other tasks.
        let started = Instant::now();
        let mut ticks = 0;
        while started.elapsed() < Duration::from_millis(1500) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ticks += 1;
        }
        drop(blocker);
        deleting.await.unwrap();
        ticks
    });
    assert!(ticked >= 20, "the runtime kept running while the delete waited: {ticked} ticks");
    assert!(!cfg.tenants_dir().join(slug).exists(), "and the delete finished");
    tenants.shutdown();
    let _ = std::fs::remove_dir_all(&d);
}
