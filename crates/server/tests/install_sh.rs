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
