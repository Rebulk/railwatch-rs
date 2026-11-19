//! deploy/install.sh's version comparison (issue #5): its upgrade path
//! refuses to replace an installed binary with an older release unless
//! --allow-downgrade. The rest of the installer needs root and systemd
//! (bench/install/*.sh); this runs its `semver_lt` with sh against
//! Semantic Versioning 2.0's precedence examples.

use std::process::Command;

fn semver_lt(a: &str, b: &str) -> bool {
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/install.sh")).unwrap();
    let start = script.find("semver_lt() {").expect("install.sh defines semver_lt");
    let end = start + script[start..].find("\n}\n").expect("semver_lt ends") + 3;
    let status = Command::new("sh").arg("-c").arg(format!("{}\nsemver_lt \"$1\" \"$2\"", &script[start..end])).args(["sh", a, b]).status().unwrap();
    status.success()
}

#[test]
fn install_sh_orders_versions_by_semver() {
    // semver.org section 11's examples, ascending, plus the forms the
    // installer sees (`railwatch vX.Y.Z` from `railwatch version`).
    let ascending = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
        "1.1.0",
        "1.10.0",
        "2.0.0",
    ];
    for (i, a) in ascending.iter().enumerate() {
        for (j, b) in ascending.iter().enumerate() {
            assert_eq!(semver_lt(a, b), i < j, "semver_lt {a} {b}");
        }
    }
    assert!(semver_lt("railwatch v0.1.1", "railwatch v0.2.0"), "`railwatch version` output");
    assert!(!semver_lt("railwatch v0.2.0", "railwatch v0.1.1"));
    assert!(!semver_lt("railwatch v0.2.0", "railwatch v0.2.0"), "the same release is not a downgrade");
    assert!(semver_lt("v0.0.0-test.2", "v0.0.0-test.10"));
    assert!(!semver_lt("1.0.0+build.9", "1.0.0+build.10"), "build metadata is ignored");
}

/// The upgrade path calls semver_lt before swapping, and only
/// --allow-downgrade skips it.
#[test]
fn install_sh_refuses_a_downgrade_without_the_flag() {
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/install.sh")).unwrap();
    let guard = script.find(r#"semver_lt "$NEW_VERSION" "$OLD_VERSION""#).expect("the downgrade check");
    let swap = script.find(r#"mv -f "$BIN.new" "$BIN""#).expect("the binary swap");
    assert!(guard < swap, "checked before the binary is replaced");
    let line = script[..guard].rsplit('\n').next().unwrap();
    assert!(line.contains(r#"[ "$ALLOW_DOWNGRADE" = 0 ]"#), "only --allow-downgrade skips it: {line}");
    assert!(script.contains("--allow-downgrade) ALLOW_DOWNGRADE=1"));
}

/// With no controlling terminal (a container, cloud-init, CI) under
/// `curl | sh`, `[ -r /dev/tty ]` still passes and the open fails; the
/// script must say what to pass, not die on a bare "cannot open /dev/tty".
/// The probe is a subshell: a failed `exec` redirect ends a POSIX shell.
#[test]
fn install_sh_explains_a_missing_terminal() {
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/install.sh")).unwrap();
    let probe = script.find("(exec 3< /dev/tty) 2> /dev/null").expect("the subshell probe");
    let open = script.find("    exec 3< /dev/tty\n").expect("the real open");
    assert!(probe < open, "probed before the shell's own descriptor is opened");
    assert!(script[probe..open].contains("--password-file"), "the message names the fix");
    let code = script.lines().filter(|l| !l.trim_start().starts_with('#'));
    assert!(code.into_iter().all(|l| !l.contains("[ -r /dev/tty ]")), "-r passes without a terminal");
}

fn install_sh() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/install.sh")).unwrap()
}

/// The script's option parser alone (the `while` loop with its helpers),
/// run with `args`; prints the variables a secret can end up in.
fn parse_options(args: &[&str], stdin: &str) -> (bool, String) {
    let script = install_sh();
    let helpers = &script[script.find("die() {").unwrap()..script.find("started=$(date").unwrap()];
    let start = script.find("while [ $# -gt 0 ]; do").unwrap();
    let end = start + script[start..].find("\ndone\n").unwrap() + 6;
    let vars = script.lines().filter(|l| l.starts_with("ALERT_ARGS=") || l.starts_with("ALERT_WEBHOOK_SECRET=")).collect::<Vec<_>>().join("\n");
    let body = format!(
        "{vars}\n{helpers}\n{}\nprintf 'S3=%s\\nHOOK=%s\\nSLACK=%s\\nARGS=%s\\n' \"$S3_SECRET\" \"$ALERT_WEBHOOK_SECRET\" \"$ALERT_SLACK\" \"$ALERT_ARGS\"",
        &script[start..end]
    );
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(body)
        .arg("install.sh")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// Security PLAUSIBLE 1: a secret on the installer's command line is in
/// `ps` and /proc/<pid>/cmdline for every local user while it runs. Each
/// one has a -file form (- is stdin) that reads it instead; the old flags
/// still work.
#[test]
fn install_sh_reads_secrets_from_files_or_stdin() {
    let dir = std::env::temp_dir().join(format!("rw-install-secrets-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = |name: &str, v: &str| {
        let p = dir.join(name);
        std::fs::write(&p, v).unwrap();
        p.to_str().unwrap().to_string()
    };
    let (s3, hook, slack) = (file("s3", "s3-SECRET\n"), file("hook", "hook-SECRET"), file("slack", "https://hooks.slack.com/services/T/B/x\nignored\n"));
    let (ok, out) = parse_options(
        &[
            "--backup-s3-secret-access-key-file",
            &s3,
            "--alert-webhook",
            "https://h.example/x",
            "--alert-webhook-secret-file",
            &hook,
            "--alert-slack-file",
            &slack,
        ],
        "",
    );
    assert!(ok, "{out}");
    assert!(out.contains("S3=s3-SECRET\n") && out.contains("HOOK=hook-SECRET\n") && out.contains("SLACK=https://hooks.slack.com/services/T/B/x\n"), "{out}");
    assert!(!out.contains("ARGS=") || !out.lines().find(|l| l.starts_with("ARGS=")).unwrap().contains("SECRET"), "secrets are not in setup's argv: {out}");
    let (ok, out) = parse_options(&["--alert-webhook-secret-file", "-"], "from-STDIN\n");
    assert!(ok && out.contains("HOOK=from-STDIN\n"), "{out}");
    let hostile = "a'b\"c$(touch PWNED)`id` ;x";
    let (ok, out) = parse_options(&["--backup-s3-secret-access-key-file", &file("hostile", hostile)], "");
    assert!(ok && out.contains(&format!("S3={hostile}\n")), "taken literally, never run: {out}");
    let (ok, out) = parse_options(&["--backup-s3-secret-access-key", "old-flag"], "");
    assert!(ok && out.contains("S3=old-flag\n"), "the old flags still work: {out}");
    let (ok, out) = parse_options(&["--alert-webhook-secret-file", dir.join("missing").to_str().unwrap()], "");
    assert!(!ok && out.contains("not readable"), "{out}");
    let (ok, out) = parse_options(&["--alert-slack-file", &file("empty", "")], "");
    assert!(!ok && out.contains("empty"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The webhook secret and the Slack URL reach `railwatch setup` through its
/// environment (read by setup_alerts), never on its command line.
#[test]
fn install_sh_never_puts_alert_secrets_on_setups_command_line() {
    let script = install_sh();
    let parse = &script[script.find("while [ $# -gt 0 ]; do").unwrap()..script.find("\ndone\n").unwrap()];
    for flag in ["--alert-webhook-secret)", "--alert-slack)", "--alert-webhook-secret-file)", "--alert-slack-file)"] {
        let line = parse.lines().find(|l| l.trim_start().starts_with(flag)).unwrap_or_else(|| panic!("{flag}"));
        assert!(!line.contains("ALERT_ARGS"), "{line}");
    }
    let run = script.find("\"$BIN\" \"$@\" <&3").expect("setup's invocation");
    let env = &script[script[..run].rfind("( cd /").unwrap()..run];
    assert!(
        env.contains("RAILWATCH_SETUP_ALERT_WEBHOOK_SECRET=\"$ALERT_WEBHOOK_SECRET\"") && env.contains("RAILWATCH_SETUP_ALERT_SLACK=\"$ALERT_SLACK\""),
        "{env}"
    );
    let main = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    assert!(main.contains("\"RAILWATCH_SETUP_ALERT_WEBHOOK_SECRET\"") && main.contains("\"RAILWATCH_SETUP_ALERT_SLACK\""), "setup reads them");
}

/// `railwatch setup` takes the webhook secret and Slack URL from its
/// environment (what install.sh passes), and stores them as the flags would.
#[test]
fn setup_reads_alert_secrets_from_its_environment() {
    let dir = std::env::temp_dir().join(format!("rw-setup-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_railwatch"))
        .args(["setup", "--email", "a@example.com", "--password-stdin", "--account", "a", "--app", "a", "--env", "production"])
        .args(["--alert-webhook", "https://hooks.example.com/railwatch"])
        .env_clear()
        .env("RAILWATCH_DATA_DIR", &dir)
        .env("RAILWATCH_SETUP_ALERT_WEBHOOK_SECRET", "hook-SECRET")
        .env("RAILWATCH_SETUP_ALERT_SLACK", "https://hooks.slack.com/services/T0/B0/slack-SECRET")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"correct horse battery staple\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let shown = std::process::Command::new(env!("CARGO_BIN_EXE_railwatch"))
        .args(["integrations", "--show-secrets"])
        .env_clear()
        .env("RAILWATCH_DATA_DIR", &dir)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(text.contains("hook-SECRET") && text.contains("slack-SECRET"), "{text}{}", String::from_utf8_lossy(&shown.stderr));
    std::fs::remove_dir_all(&dir).unwrap();
}
