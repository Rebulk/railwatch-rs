//! GA security review 2026-11-19, #9: a bare-binary install under the usual
//! umask 022 left the data dir 755 and the control DB (password digests,
//! sessions, token digests) and telemetry 644, readable by every local
//! user, and `doctor` said nothing. The binary now creates everything for
//! its user only, and `doctor` warns about a dir that is not.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_railwatch");

fn railwatch(data: &Path, args: &[&str], stdin: &[u8]) -> std::process::Output {
    let mut c = Command::new(BIN);
    c.args(args)
        .env("RAILWATCH_DATA_DIR", data)
        .env("RAILWATCH_SECRET_KEY_BASE", "00".repeat(64))
        .env_remove("RAILWATCH_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The shell's usual umask, as on a bare-binary install.
    // SAFETY: umask in the child before exec; no allocation or locking.
    unsafe {
        c.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    let mut child = c.spawn().unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn setup_under_umask_022_leaves_nothing_readable_by_other_users_and_doctor_warns_when_it_is() {
    let root = std::env::temp_dir().join(format!("rw-ga-perms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let data = root.join("data");
    let out = railwatch(
        &data,
        &["setup", "--email", "a@example.com", "--password-stdin", "--account", "a", "--app", "a", "--env", "production"],
        b"a-long-enough-password\n",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let export = root.join("data").join("export.sqlite3");
    let out = railwatch(&data, &["export", "a-a-production", export.to_str().unwrap()], b"");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(mode(&data), 0o700, "the data dir");
    assert_eq!(mode(&data.join("control.sqlite3")), 0o600, "the control DB");
    assert_eq!(mode(&data.join("telemetry")), 0o700);
    assert_eq!(mode(&data.join("telemetry/a-a-production")), 0o700);
    assert_eq!(mode(&data.join("telemetry/a-a-production/meta.sqlite3")), 0o600);
    assert_eq!(mode(&export), 0o600, "an export");
    let doctor = String::from_utf8_lossy(&railwatch(&data, &["doctor"], b"").stdout).into_owned();
    assert!(doctor.contains("ok    data dir permissions"), "{doctor}");

    // A dir an older version left readable is reported.
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(data.join("control.sqlite3"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let doctor = String::from_utf8_lossy(&railwatch(&data, &["doctor"], b"").stdout).into_owned();
    assert!(doctor.contains("warn  readable by other local users") && doctor.contains("control.sqlite3 (644)"), "{doctor}");
    let _ = std::fs::remove_dir_all(&root);
}

/// PR #107 review L8: the image's `/data` was created 0755, so `doctor`
/// warned on every Docker install and other uids in the container could
/// list it. (Checked by building the image: `drwx------ 10001/10001 data/`.)
#[test]
fn the_docker_images_data_dir_is_0700() {
    let dockerfile = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Dockerfile")).unwrap();
    assert!(dockerfile.contains("chown 10001:10001 /out/data"), "the volume belongs to the server's uid");
    assert!(dockerfile.contains("chmod 0700 /out/data"), "and only it can read the volume");
}
