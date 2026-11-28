//! deploy/install.sh's version comparison (issue #5): its upgrade path
//! refuses to replace an installed binary with an older release unless
//! --allow-downgrade. The rest of the installer needs root and systemd
//! (bench/install/*.sh); this runs its `semver_lt` with sh against
//! Semantic Versioning 2.0's precedence examples.

use std::path::Path;
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

/// The binary step of install.sh (from `step "binary"` through running the
/// downloaded binary) with the script's own defaults and helpers, against a
/// release served by a stub `curl` from `release`, on a PATH holding only
/// the tools it needs, the stubs, and `cosign` when given. `pinned` stands
/// in for the checksum bin/release bakes into a released copy.
/// (exit ok, stdout, stderr, whether the downloaded binary ran)
fn binary_step(release: &[(&str, Vec<u8>)], cosign: Option<&str>, pinned: Option<&str>) -> (bool, String, String, bool) {
    use std::os::unix::fs::{PermissionsExt, symlink};
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!("rw-install-cosign-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (bin, rel) = (root.join("bin"), root.join("release"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&rel).unwrap();
    for name in ["sh", "uname", "mktemp", "grep", "head", "cut", "sha256sum", "tar", "gzip", "mv", "chmod", "rm", "cp", "cat"] {
        let real = ["/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists()).unwrap_or_else(|| panic!("this test needs {name}"));
        symlink(real, bin.join(name)).unwrap();
    }
    let exe = |name: &str, body: &str| {
        let p = bin.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    // curl -fsSL ... -o OUT URL: the file named by the URL's last segment,
    // or curl's own 404 failure.
    exe(
        "curl",
        "#!/bin/sh\nout=\"\"\nwhile [ $# -gt 1 ]; do [ \"$1\" = -o ] && out=$2; shift; done\nf=\"$RW_RELEASE/${1##*/}\"\n[ -f \"$f\" ] || { echo \"curl: (22) The requested URL returned error: 404\" >&2; exit 22; }\ncp \"$f\" \"$out\"\n",
    );
    if let Some(body) = cosign {
        exe("cosign", body);
    }
    for (name, bytes) in release {
        std::fs::write(rel.join(name), bytes).unwrap();
    }
    let script = install_sh();
    let header = &script[..script.find("die() {").unwrap()];
    let header = match pinned {
        Some(p) => header
            .replace("\nRAILWATCH_SHA256_X86_64=\n", &format!("\nRAILWATCH_SHA256_X86_64={p}\n"))
            .replace("\nRAILWATCH_SHA256_AARCH64=\n", &format!("\nRAILWATCH_SHA256_AARCH64={p}\n")),
        None => {
            assert!(header.contains("\nRAILWATCH_SHA256_X86_64=\n") && header.contains("\nRAILWATCH_SHA256_AARCH64=\n"), "the repository copy is unbaked");
            header.to_string()
        }
    };
    let helpers = &script[script.find("die() {").unwrap()..script.find("started=$(date").unwrap()];
    let fetch = &script[script.find("fetch() {").unwrap()..script.find("step \"binary\"").unwrap()];
    let start = script.find("step \"binary\"").unwrap();
    let run_line = "\"$TMP/railwatch\" version > /dev/null 2>&1 || die";
    let end = start + script[start..].find(run_line).expect("the step runs the downloaded binary");
    let end = end + script[end..].find('\n').unwrap();
    let body = format!("{header}\nRELEASE_URL=https://release.invalid/rel\n{helpers}\n{fetch}\n{}\necho STEP-DONE\n", &script[start..end]);
    let marker = root.join("ran");
    let o = Command::new(bin.join("sh"))
        .arg("-c")
        .arg(body)
        .env_clear()
        .env("PATH", &bin)
        .env("TMPDIR", &root)
        .env("RW_RELEASE", &rel)
        .env("RW_MARKER", &marker)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let ran = marker.exists();
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(!o.status.success() || out.contains("STEP-DONE"), "{out}{err}");
    std::fs::remove_dir_all(&root).unwrap();
    (o.status.success(), out, err, ran)
}

/// A release: SHA256SUMS and the tarball for this machine, whose
/// `railwatch` records that it ran. (files, the tarball's sha256)
fn release() -> (Vec<(&'static str, Vec<u8>)>, String) {
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;
    let arch = std::env::consts::ARCH;
    let name = format!("railwatch-v9.9.9-{arch}-linux-musl");
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stage = std::env::temp_dir().join(format!("rw-install-cosign-stage-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(stage.join(&name)).unwrap();
    let fake = stage.join(&name).join("railwatch");
    std::fs::write(&fake, "#!/bin/sh\necho ran >> \"$RW_MARKER\"\necho 'railwatch v9.9.9'\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new("tar").arg("-czf").arg("-").arg("-C").arg(&stage).arg(&name).output().unwrap();
    assert!(out.status.success());
    std::fs::remove_dir_all(&stage).unwrap();
    let sum: String = sha2::Sha256::digest(&out.stdout).iter().map(|b| format!("{b:02x}")).collect();
    let tarball: &'static str = Box::leak(format!("{name}.tar.gz").into_boxed_str());
    (vec![("SHA256SUMS", format!("{sum}  {tarball}\n").into_bytes()), (tarball, out.stdout)], sum)
}

const COSIGN_OK: &str = "#!/bin/sh\n[ \"$1\" = verify-blob ] || exit 2\nexit 0\n";
const COSIGN_FAILS: &str = "#!/bin/sh\nexit 1\n";

/// Security (audit: missing bundle skips the signature check): with cosign
/// installed and no checksum baked into the script, the release host could
/// switch the cosign check off by not serving SHA256SUMS.sigstore.json; the
/// script then said "cosign is not installed", trusted SHA256SUMS as
/// served and ran the downloaded binary as root. Whether a signature is
/// required now follows from cosign being installed, and a bundle that
/// cannot be downloaded stops the install, with the download's error shown.
#[test]
fn install_sh_with_cosign_requires_the_signature_bundle() {
    let (files, _) = release();
    let (ok, out, err, ran) = binary_step(&files, Some(COSIGN_OK), None);
    assert!(!ok && !ran, "no bundle served: refused, the downloaded binary never runs\n{out}{err}");
    assert!(err.contains("cosign is installed but") && err.contains("SHA256SUMS.sigstore.json could not be downloaded"), "{err}");
    assert!(err.contains("returned error: 404"), "the download's own error is not discarded: {err}");
    assert!(!out.contains("cosign is not installed"), "{out}");
}

/// The other branches keep their behaviour: a served bundle is verified
/// (refused when cosign refuses it); with no cosign an unbaked script
/// trusts SHA256SUMS and says so; a baked checksum still carries a release
/// whose host serves no bundle.
#[test]
fn install_sh_signature_branches() {
    let (mut files, sum) = release();
    let (ok, out, err, ran) = binary_step(&files, None, None);
    assert!(ok && ran && out.contains("has no pinned checksums and cosign is not installed: trusting"), "no cosign: {out}{err}");
    let (ok, out, err, ran) = binary_step(&files, Some(COSIGN_OK), Some(&sum));
    assert!(ok && ran && out.contains("relying on the checksums pinned in this installer"), "baked, no bundle: {out}{err}");
    files.push(("SHA256SUMS.sigstore.json", b"{}".to_vec()));
    let (ok, out, err, ran) = binary_step(&files, Some(COSIGN_OK), None);
    assert!(ok && ran && out.contains("SHA256SUMS: signature verified (v9.9.9)"), "bundle verifies: {out}{err}");
    let (ok, out, err, ran) = binary_step(&files, Some(COSIGN_FAILS), None);
    assert!(!ok && !ran && err.contains("signature does not verify"), "bundle does not verify: {out}{err}");
}
