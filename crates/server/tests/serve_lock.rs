//! One server per data directory (main.rs `lock_data_dir`). A restart that
//! starts the next server before stopping the last (the production run
//! model, deploy/prod/railwatch-swap) must never have two processes writing
//! the same tenant files: the writers cache their high-water marks, so two
//! of them would hand out the same ids. The second `serve` waits for the
//! first to drain and exit, and offline commands refuse while one runs.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn railwatch(data: &Path, port: u16) -> Command {
    let mut c = Command::new(BIN);
    c.env("RAILWATCH_DATA_DIR", data)
        .env("RAILWATCH_BIND", format!("127.0.0.1:{port}"))
        .env("RAILWATCH_SECRET_KEY_BASE", "00".repeat(64))
        .env("RAILWATCH_LOG", "warn")
        .env_remove("RAILWATCH_CONFIG");
    c
}

fn listening(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let started = Instant::now();
    while !f() {
        assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn term(child: &mut Child) {
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    child.wait().unwrap();
}

#[test]
fn a_second_server_waits_for_the_first_to_exit_and_offline_commands_refuse() {
    let dir = Dir(std::env::temp_dir().join(format!("rw-serve-lock-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let (a, b) = (free_port(), free_port());

    let mut first = railwatch(&dir.0, a).arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    wait_until("the first server", || listening(a));

    let mut second = railwatch(&dir.0, b).arg("serve").stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(second.try_wait().unwrap().is_none(), "the second server must wait, not exit");
    assert!(!listening(b), "the second server must not serve while the first holds the data dir");

    for args in [&["split", "--all"][..], &["restore"][..]] {
        let out = railwatch(&dir.0, 0).args(args).output().unwrap();
        assert!(!out.status.success(), "{args:?} must refuse while a server runs");
        assert!(String::from_utf8_lossy(&out.stderr).contains("serve.lock is held"), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    term(&mut first);
    wait_until("the second server after the first exited", || listening(b));
    term(&mut second);
    let mut err = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut err).unwrap();
    assert!(err.contains("acquired after"), "the second server reports the hand-over: {err}");
}
