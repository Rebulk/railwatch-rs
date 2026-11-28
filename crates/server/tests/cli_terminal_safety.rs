//! The client commands write to a terminal text that a server sent, and a
//! server's answers carry text lower-trust parties wrote (an app's ingest
//! token: exception titles, profile frames; any member: issue comments).
//! Run as the real binary against a stub MCP server whose every string
//! holds terminal control sequences:
//!
//! - text output and failure messages carry no control character other
//!   than line feed and tab, no C1 control and no bidi override/isolate
//!   (each is shown as U+FFFD); --json stays exactly what the server sent;
//! - `railwatch open` gives the desktop opener only an http(s) link on the
//!   connected server's origin (a stub `xdg-open` records what it got);
//! - `railwatch upgrade` (as uid 0 of a user namespace, where one can be
//!   made) does not echo an unverified release's control sequences.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");

/// `s` as one POSIX shell word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Clear screen + cursor home + a fake "all clear", conceal; OSC 52
/// (clipboard), OSC 0 (window title), OSC 8 (a link whose text is not its
/// target); C1 CSI/OSC/ST; bidi override and isolate; CR; DEL.
const HOSTILE: &str = "\u{1b}[2J\u{1b}[HNo open issues.\u{1b}[8m \u{1b}]52;c;cnctYXVkaXQ=\u{7}\u{1b}]0;rw-title\u{7}\u{1b}]8;;https://attacker.invalid/\u{1b}\\https://github.com/Rebulk/railwatch-rs\u{1b}]8;;\u{1b}\\ \u{9b}2J\u{9d}0;t\u{9c} \u{202e}bidi\u{2066} cr\rx\u{7f}";

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("rw-cli-terminal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

/// A loopback HTTP server answering every POST with `handler(addr, json
/// body)` -> (status, body). Plain std, one thread, one request per
/// connection.
fn stub(handler: impl Fn(&str, &Value) -> (u16, String) + Send + 'static) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let base = format!("http://{addr}");
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start = loop {
                let Ok(n) = s.read(&mut chunk) else { break None };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(start) = body_start else { continue };
            let head = String::from_utf8_lossy(&buf[..start]).to_ascii_lowercase();
            let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
            while buf.len() < start + len {
                match s.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let req: Value = serde_json::from_slice(&buf[start..]).unwrap_or_default();
            let (status, body) = handler(&addr, &req);
            let _ = s.write_all(
                format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    .as_bytes(),
            );
        }
    });
    base
}

fn rpc_result(req: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": req["id"], "result": result}).to_string()
}

fn tool_text(req: &Value, text: &str) -> String {
    rpc_result(req, json!({"content": [{"type": "text", "text": text}]}))
}

/// What each tool answers: every string holds HOSTILE.
fn served_text(tool: &str) -> Option<String> {
    Some(match tool {
        "list_issues" => json!([{"key": "SHOP-13", "occurrences": 4, "kind": "exception", "last_seen_at": "2026-11-25 23:22:00", "title": format!("ZeroDivisionError: {HOSTILE}")}]).to_string(),
        "investigate_issue" => json!({"key": "SHOP-12", "markdown": format!("# SHOP-12\n\nSimilar issues:\n- SHOP-13 [open] {HOSTILE}\n")}).to_string(),
        "get_profile" => json!({"id": 1, "frames": [{"frame": HOSTILE, "self_samples": 70}, {"frame": "render", "self_samples": 30}]}).to_string(),
        "tool_error" => json!({"error": HOSTILE, "hint": HOSTILE}).to_string(),
        _ => return None,
    })
}

fn hostile_server() -> String {
    stub(|_, req| match req["method"].as_str() {
        Some("tools/list") => (200, rpc_result(req, json!({"tools": [{"name": "get_profile", "description": HOSTILE}]}))),
        Some("tools/call") => {
            let tool = req["params"]["name"].as_str().unwrap_or("");
            match tool {
                "http_error" => (500, format!("upstream: {HOSTILE}")),
                "rpc_error" => (200, json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32000, "message": HOSTILE}}).to_string()),
                "unknown_tool" => (200, json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": HOSTILE}}).to_string()),
                _ => (200, tool_text(req, &served_text(tool).unwrap_or_else(|| json!({"error": "no such tool"}).to_string()))),
            }
        }
        _ => (200, json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "Method not found"}}).to_string()),
    })
}

fn client(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The characters a terminal acts on, other than line feed and tab.
fn terminal_controls(bytes: &[u8]) -> Vec<char> {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|&c| (c.is_control() && c != '\n' && c != '\t') || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .collect()
}

#[track_caller]
fn assert_terminal_safe(what: &str, bytes: &[u8]) {
    let bad = terminal_controls(bytes);
    assert!(bad.is_empty(), "{what}: wrote {bad:?} to the terminal: {:?}", String::from_utf8_lossy(bytes));
}

#[test]
fn server_text_reaches_the_terminal_without_control_sequences() {
    let dir = scratch("print");
    let url = hostile_server();
    let run = |args: &[&str]| {
        let mut a = args.to_vec();
        a.extend(["--url", &url]);
        client(&dir.0, &a)
    };

    // Text output (print()): the issue list, the markdown brief, a generic
    // tool rendering, the tools list.
    for args in [&["issues"][..], &["issue", "SHOP-12"], &["call", "get_profile"], &["tools"]] {
        let o = run(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(out.contains('\u{FFFD}') && out.contains("No open issues."), "{args:?}: the text is shown, its controls made visible: {out:?}");
        assert_terminal_safe(&format!("{args:?} stdout"), &o.stdout);
        assert_terminal_safe(&format!("{args:?} stderr"), &o.stderr);
    }

    // Failure messages (stderr): a tool error, an HTTP error body, a
    // JSON-RPC error, an unknown-tool message.
    for (tool, code) in [("tool_error", 2), ("http_error", 1), ("rpc_error", 1), ("unknown_tool", 2)] {
        let o = run(&["call", tool]);
        assert_eq!(o.status.code(), Some(code), "{tool}: {}", String::from_utf8_lossy(&o.stderr));
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains('\u{FFFD}') && err.contains("No open issues."), "{tool}: {err:?}");
        assert_terminal_safe(&format!("{tool} stderr"), &o.stderr);
    }

    // --json is the server's text, byte for byte (JSON's own escaping is
    // what keeps C0 controls out of it).
    for (args, tool) in [(&["call", "get_profile", "--json"][..], "get_profile"), (&["issues", "--json"], "list_issues")] {
        let o = run(args);
        assert_eq!(o.status.code(), Some(0));
        assert_eq!(String::from_utf8(o.stdout).unwrap(), format!("{}\n", served_text(tool).unwrap()), "{args:?}");
    }
}

/// `railwatch open <KEY>` with stdout on a terminal (script(1)) and a stub
/// `xdg-open` first on PATH: what reaches the opener, per server answer.
#[cfg(target_os = "linux")]
#[test]
fn open_gives_the_desktop_opener_only_the_servers_own_links() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("open");
    let stubs = dir.0.join("stubs");
    std::fs::create_dir_all(&stubs).unwrap();
    let log = dir.0.join("opener.log");
    let opener = stubs.join("xdg-open");
    std::fs::write(&opener, "#!/bin/sh\nprintf 'argc=%s argv1=%s\\n' \"$#\" \"$1\" >> \"$RW_OPENER_LOG\"\n").unwrap();
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Path::new("/usr/bin/script").exists(), "this test needs script(1) (util-linux) to give the client a terminal");

    // (what the server answers as `url`, given its own address) -> what the
    // opener must receive, None for not called.
    type Case = (&'static str, fn(&str) -> String, fn(&str) -> Option<String>);
    let cases: [Case; 8] = [
        ("file: URI", |_| "file:///tmp/rw-audit-dummy.command".into(), |_| None),
        ("unknown scheme", |_| "x-audit-scheme://handler/run?arg=1".into(), |_| None),
        ("an option", |_| "--version".into(), |_| None),
        ("another host", |_| "https://attacker.invalid/railwatch".into(), |_| None),
        ("another port", |a| format!("http://{}:1/issues/1", a.split(':').next().unwrap()), |_| None),
        ("https on the http origin", |a| format!("https://{a}/issues/1"), |_| None),
        ("its own origin", |a| format!("http://{a}/issues/1"), |a| Some(format!("http://{a}/issues/1"))),
        ("a path, joined to it", |_| "/issues/1".into(), |a| Some(format!("http://{a}/issues/1"))),
    ];
    for (what, served, expect) in cases {
        let url = stub(move |addr, req| (200, tool_text(req, &json!({"key": "SHOP-1", "url": served(addr)}).to_string())));
        let addr = url.trim_start_matches("http://").to_string();
        let _ = std::fs::remove_file(&log);
        // The terminal stays open briefly after the client exits, as a
        // shell's does (script(1) would otherwise hang up the opener).
        // Quoted, so a checkout path with spaces stays one word.
        let cmd = format!("{} open SHOP-1 --url {}; sleep 0.5", sh_quote(BIN), sh_quote(&url));
        let o = Command::new("/usr/bin/script")
            .args(["-qec", &cmd, "/dev/null"])
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", stubs.display()))
            .env("HOME", &dir.0)
            .env("XDG_CONFIG_HOME", dir.0.join("config"))
            .env("RW_OPENER_LOG", &log)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(o.status.success(), "{what}: {}", String::from_utf8_lossy(&o.stdout));
        let want = expect(&addr);
        if want.is_some() {
            for _ in 0..40 {
                if log.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let got = std::fs::read_to_string(&log).ok();
        let shown = String::from_utf8_lossy(&o.stdout);
        match want {
            Some(link) => assert_eq!(got.as_deref().map(str::trim), Some(format!("argc=1 argv1={link}").as_str()), "{what}: {shown}"),
            None => {
                assert_eq!(got, None, "{what}: the opener must not be called");
                assert!(shown.contains("not opening that link"), "{what}: says why: {shown}");
            }
        }
    }
}

/// `railwatch upgrade`, as uid 0 of a throwaway user namespace, against a
/// loopback release host whose (not yet verified) SHA256SUMS and bundle
/// hold control sequences: refused, with nothing of them echoed raw, and
/// the installer never run. Where user namespaces cannot be made (some CI
/// kernels restrict them) this says so and checks nothing.
#[cfg(target_os = "linux")]
#[test]
fn upgrade_does_not_echo_an_unverified_releases_control_sequences() {
    if !Command::new("/usr/bin/unshare").args(["--user", "--map-root-user", "true"]).status().is_ok_and(|s| s.success()) {
        eprintln!("NOT RUN: upgrade_does_not_echo_an_unverified_releases_control_sequences needs unprivileged user namespaces");
        return;
    }
    fn release_host(files: Vec<(&'static str, Vec<u8>)>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let name = head.split(' ').nth(1).unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
                match files.iter().find(|(f, _)| *f == name) {
                    Some((_, body)) => {
                        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes());
                        let _ = s.write_all(body);
                    }
                    None => {
                        let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        format!("http://{addr}/releases")
    }
    let script = b"echo RW-INSTALL-SH-RAN\nexit 7\n".to_vec();
    // An older tag whose semver build metadata holds control sequences.
    let sums_a = b"aa  railwatch-v0.0.1+\x1b]0;rw-title\x07\x1b[2J\xc2\x9b2J-x86_64-linux-musl.tar.gz\ncc  install.sh\n".to_vec();
    // A newer tag, and a bundle that is not one (its mediaType is echoed).
    let sums_b = b"aa  railwatch-v9.9.9-x86_64-linux-musl.tar.gz\ncc  install.sh\n".to_vec();
    let bundle_b = br#"{"mediaType":"\u001b]0;rw-title\u0007\u001b[2J\u009b2J\u202e"}"#.to_vec();
    let hosts = [
        ("unverified SHA256SUMS tag", release_host(vec![("install.sh", script.clone()), ("SHA256SUMS", sums_a)])),
        ("unverified bundle", release_host(vec![("install.sh", script), ("SHA256SUMS", sums_b), ("SHA256SUMS.sigstore.json", bundle_b)])),
    ];
    for (what, base) in hosts {
        let o = Command::new("/usr/bin/unshare")
            .args(["--user", "--map-root-user", BIN, "upgrade"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("RAILWATCH_RELEASE_BASE", &base)
            .env("RAILWATCH_ALLOW_HTTP_RELEASE", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(!o.status.success() && !out.contains("RW-INSTALL-SH-RAN"), "{what}: refused, nothing run: {out} {err}");
        assert!(err.contains("rw-title") && err.contains('\u{FFFD}'), "{what}: the refusal is shown, its controls made visible: {err:?}");
        assert_terminal_safe(&format!("{what} stderr"), &o.stderr);
        assert_terminal_safe(&format!("{what} stdout"), &o.stdout);
    }
}
