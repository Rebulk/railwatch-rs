//! HTTP connection limits on the real `railwatch serve` (audit B, finding 4):
//! a client that opens connections and never finishes a request used to
//! hold a task and a file descriptor forever (no header, body or idle
//! timeout, no connection cap). Run with short limits so the test is quick:
//!
//!   cargo test --test security_b_http_limits -- --nocapture

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");

struct Server {
    child: Child,
    addr: SocketAddr,
    dir: std::path::PathBuf,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start(extra: &[(&str, &str)]) -> Server {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    let dir = std::env::temp_dir().join(format!("rw-httplim-{}-{}", std::process::id(), u64::from_le_bytes(b)));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let env = |c: &mut Command| {
        c.env("RAILWATCH_DATA_DIR", &dir)
            .env("RAILWATCH_BIND", addr.to_string())
            .env("RAILWATCH_URL", format!("http://{addr}"))
            .env("RAILWATCH_RATE_LIMIT_IP", "0")
            .env("RAILWATCH_RATE_LIMIT_ENV", "0")
            .env_remove("RAILWATCH_CONFIG")
            .env_remove("LISTEN_FDS");
        for (k, v) in extra {
            c.env(k, v);
        }
    };
    let mut create = Command::new(BIN);
    env(&mut create);
    let out = create.args(["env", "create", "lim"]).output().unwrap();
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let mut serve = Command::new(BIN);
    env(&mut serve);
    let child = serve.arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let t = Instant::now();
    while TcpStream::connect(addr).is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Server { child, addr, dir, token }
}

/// Seconds until the server closes `s` (reads EOF or a reset), capped at `max`.
fn closed_after(mut s: TcpStream, max: Duration) -> Option<f64> {
    let t = Instant::now();
    s.set_read_timeout(Some(max)).unwrap();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return Some(t.elapsed().as_secs_f64()),
            Ok(_) => continue, // a response (e.g. 408/400) before the close
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return None,
            Err(_) => return Some(t.elapsed().as_secs_f64()),
        }
    }
}

fn up(addr: SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return false };
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    if s.write_all(b"GET /up HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    out.starts_with(b"HTTP/1.1 200")
}

#[test]
fn slow_clients_are_cut_off_and_connections_are_capped() {
    let s = start(&[
        ("RAILWATCH_HTTP_HEADER_TIMEOUT_SECONDS", "1"),
        ("RAILWATCH_HTTP_BODY_TIMEOUT_SECONDS", "2"),
        ("RAILWATCH_HTTP_IDLE_TIMEOUT_SECONDS", "3"),
        ("RAILWATCH_MAX_CONNECTIONS", "8"),
    ]);
    let wait = Duration::from_secs(8);

    // 1. A request head that never completes.
    let mut c = TcpStream::connect(s.addr).unwrap();
    c.write_all(b"POST /ingest HTTP/1.1\r\nHost: x\r\n").unwrap();
    let head = closed_after(c, wait);
    eprintln!("partial header: closed after {head:?} s");
    assert!(head.is_some_and(|t| t < 3.0), "an unfinished request head is dropped (header timeout), got {head:?}");

    // 2. A body that stops arriving: headers promise 1 MiB, 10 bytes come.
    let mut c = TcpStream::connect(s.addr).unwrap();
    let req = format!(
        "POST /ingest HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Type: application/x-ndjson\r\nContent-Length: 1048576\r\n\r\n{{\"t\":\"log\"",
        s.token
    );
    c.write_all(req.as_bytes()).unwrap();
    let body = closed_after(c, wait);
    eprintln!("stalled body: closed after {body:?} s");
    assert!(body.is_some_and(|t| t < 4.5), "a stalled body is cut off (body/idle timeout), got {body:?}");

    // 3. An idle keep-alive connection after a complete request.
    let mut c = TcpStream::connect(s.addr).unwrap();
    c.write_all(b"GET /up HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let idle = closed_after(c, wait);
    eprintln!("idle keep-alive: closed after {idle:?} s");
    assert!(idle.is_some_and(|t| t < 3.0), "an idle keep-alive connection is closed, got {idle:?}");

    // 4. The cap: 8 silent connections fill it; a 9th waits in the backlog
    //    (not refused) and is served as soon as one is cut off.
    let hogs: Vec<TcpStream> = (0..8).map(|_| TcpStream::connect(s.addr).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let served = up(s.addr);
    let waited = t.elapsed().as_secs_f64();
    eprintln!("9th connection with 8 open: served={served} after {waited:.2} s");
    assert!(served, "the 9th connection is served once a slot frees");
    assert!(waited > 0.5, "the 9th connection waited for a slot ({waited:.2} s)");
    drop(hogs);
    assert!(up(s.addr), "the server is healthy afterwards");
}

/// The defaults must not cut off a legitimate slow upload: a 32 MiB-class
/// body trickled in chunks with short pauses is accepted whole. Committing
/// the 3,000 records once the body is in is the server's time, not the
/// client's: however long it takes on a slow runner, the idle timeout must
/// not close the connection under it (it did, with an empty response).
#[test]
fn slow_but_steady_upload_is_accepted() {
    let s = start(&[("RAILWATCH_HTTP_IDLE_TIMEOUT_SECONDS", "2")]);
    let line = format!("{{\"t\":\"log\",\"v\":1,\"timestamp\":1792000000.0,\"level\":\"info\",\"message\":\"{}\"}}\n", "x".repeat(1000));
    let body: Vec<u8> = line.as_bytes().repeat(3_000); // ~3 MiB, under MAX_RECORDS
    let mut c = TcpStream::connect(s.addr).unwrap();
    let head = format!(
        "POST /ingest HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {}\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        s.token,
        body.len()
    );
    c.write_all(head.as_bytes()).unwrap();
    for chunk in body.chunks(256 * 1024) {
        c.write_all(chunk).unwrap();
        std::thread::sleep(Duration::from_millis(500)); // well under the 2 s idle limit
    }
    c.set_read_timeout(Some(Duration::from_secs(120))).unwrap(); // the commit, however slow
    let mut out = Vec::new();
    let _ = c.read_to_end(&mut out);
    let status = String::from_utf8_lossy(&out[..out.len().min(12)]).into_owned();
    eprintln!("slow steady upload of {} bytes over ~6 s: {status}", body.len());
    assert!(out.starts_with(b"HTTP/1.1 2"), "a steady slow upload is accepted, got {status:?}");
}

/// HSTS (audit A, finding 9): sent when the server terminates TLS itself,
/// not on a plain-http url. Needs `openssl` on PATH for a throwaway cert.
#[test]
fn hsts_with_built_in_tls_only() {
    let plain = start(&[]);
    let mut c = TcpStream::connect(plain.addr).unwrap();
    c.write_all(b"GET /up HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).unwrap();
    assert!(!String::from_utf8_lossy(&out).to_ascii_lowercase().contains("strict-transport-security"), "no HSTS on plain http");

    let certs = plain.dir.join("certs");
    std::fs::create_dir_all(&certs).unwrap();
    let (cert, key) = (certs.join("c.pem"), certs.join("k.pem"));
    let made = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1", "-subj", "/CN=localhost"])
        .args(["-addext", "subjectAltName=DNS:localhost", "-addext", "basicConstraints=critical,CA:FALSE", "-addext", "extendedKeyUsage=serverAuth", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .stderr(Stdio::null())
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("SKIPPED the TLS half: openssl is not available");
        return;
    }
    let tls = start(&[("RAILWATCH_TLS_CERT_FILE", cert.to_str().unwrap()), ("RAILWATCH_TLS_KEY_FILE", key.to_str().unwrap())]);
    let client = reqwest::blocking::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(&cert).unwrap()).unwrap())
        .resolve("localhost", tls.addr)
        .build()
        .unwrap();
    let res = client.get(format!("https://localhost:{}/up", tls.addr.port())).send().unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers().get("strict-transport-security").and_then(|v| v.to_str().ok()), Some("max-age=31536000"));
}

/// A user of the network service can find the source.
/// Every response carries a `Link` to it, every HTML page names it in the
/// head, and /source redirects there.
#[test]
fn every_response_points_at_the_source() {
    let s = start(&[]);
    let get = |path: &str| {
        let mut c = TcpStream::connect(s.addr).unwrap();
        c.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    };
    let src = railwatch_server::SOURCE_URL;
    for path in ["/up", "/sign_in", "/ingest/ping"] {
        let r = get(path);
        assert!(r.contains(&format!("link: <{src}>; rel=\"license source\"")), "{path}: {}", &r[..r.len().min(400)]);
    }
    assert!(get("/sign_in").contains(&format!("<meta name=\"source\" content=\"{src}\">")));
    let r = get("/source");
    assert!(r.starts_with("HTTP/1.1 303") && r.contains(&format!("location: {src}")), "{}", &r[..r.len().min(300)]);
}
