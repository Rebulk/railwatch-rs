//! The stock server, except the listening socket is inherited as fd 3
//! (systemd LISTEN_FDS convention) instead of bound. Lets a holder process keep
//! one socket open across server generations, so a restart never refuses a
//! connection: the kernel queues it on the shared socket until a generation
//! accepts it. Same AppState and router as crates/server/src/main.rs serve().
use std::net::SocketAddr;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use railwatch_server::control::Control;
use railwatch_server::http::{self, AppState};
use railwatch_server::rate_limit::FixedWindow;
use railwatch_server::tenants::Tenants;
use railwatch_server::followups;

fn main() {
    tracing_subscriber::fmt().with_env_filter(std::env::var("RAILWATCH_LOG").unwrap_or_else(|_| "info".into())).init();
    assert_eq!(std::env::var("LISTEN_FDS").as_deref(), Ok("1"), "needs LISTEN_FDS=1 and the socket on fd 3");
    let data = PathBuf::from(std::env::var("RAILWATCH_DATA_DIR").unwrap_or_else(|_| "./data".into()));
    let env_u64 = |n: &str, d: u64| std::env::var(n).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
        let control = Arc::new(Control::open(&data.join("control.sqlite3")).unwrap());
        let (tx, rx) = followups::channel();
        let tenants = Tenants::new(data.join("telemetry"), Duration::from_secs(300), tx);
        let state = Arc::new(AppState {
            control: control.clone(),
            tenants: tenants.clone(),
            ip_limit: FixedWindow::new(env_u64("RAILWATCH_RATE_LIMIT_IP", 2_400)),
            env_limit: FixedWindow::new(env_u64("RAILWATCH_RATE_LIMIT_ENV", 600)),
            trust_proxy: false,
            sourcemaps_dir: data.join("sourcemaps"),
            cpu: Arc::new(tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(4, |n| n.get()))),
        });
        let fu = tokio::spawn(followups::run(control, rx));
        let std_listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
        std_listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let app = http::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                term.recv().await;
            })
            .await
            .unwrap();
        drop(state);
        tokio::task::spawn_blocking(move || tenants.shutdown()).await.ok();
        fu.await.ok();
    });
}
