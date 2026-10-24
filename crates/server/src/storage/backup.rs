//! Backups to S3-compatible object storage (optional, off by default).
//!
//! Bucket layout under the configured prefix:
//!
//! ```text
//! control/latest.sqlite3.zst            the control DB, every snapshot pass
//! tenants/<slug>/meta.sqlite3.zst       each per-day tenant's meta, every pass
//! tenants/<slug>/core/<day>.sqlite3.zst a closed day file, uploaded once
//! tenants/<slug>/detail/<day>.sqlite3.zst
//! tenants/<slug>.sqlite3.zst            a single-file (Rails layout) tenant, every pass
//! ```
//!
//! A meta, control or single-file snapshot is skipped when the database has
//! not changed since the last upload (`unchanged`: the file's and its WAL's
//! size and mtime): an idle environment's meta was re-uploaded every pass.
//!
//! Snapshots use the SQLite online backup API into a temporary file, so a
//! snapshot is a consistent database even while ingest writes; it is then
//! zstd-compressed (level 3, research/IMPROVEMENTS.md 2(i): ~8x) and PUT.
//! A day is closed once it is two UTC days old (late rows arrive up to an
//! hour late, MAX_FUTURE_AGE skew aside); a closed day file never changes
//! again until retention deletes it, so it is uploaded once (HEAD first).
//! Exemplar copies write into an old core file; that file is uploaded again
//! after its exemplars are copied (the marker is in the file, so the
//! uploaded object records whether it has them: `.ex` suffix).
//!
//! Restore downloads everything into an empty data directory.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::layout::{self, Tier};
use super::s3::S3;

#[derive(Debug, Default)]
pub struct Pass {
    pub snapshots: usize,
    /// Snapshots skipped because the database had not changed.
    pub unchanged: usize,
    pub day_files: usize,
    pub skipped: usize,
    pub bytes_raw: u64,
    pub bytes_compressed: u64,
}

const ZSTD_LEVEL: i32 = 3;

/// A consistent copy of `src` at `dst` through the online backup API.
pub fn snapshot(src: &Path, dst: &Path) -> rusqlite::Result<()> {
    let from = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    from.busy_timeout(std::time::Duration::from_secs(10))?;
    let _ = std::fs::remove_file(dst);
    let mut to = Connection::open(dst)?;
    {
        let b = rusqlite::backup::Backup::new(&from, &mut to)?;
        // Big steps: a step holds the source's read lock only briefly in WAL.
        b.run_to_completion(4096, std::time::Duration::from_millis(0), None)?;
    }
    // A backup copies the WAL-mode header; keep the copy self-contained.
    to.execute_batch("PRAGMA journal_mode = DELETE;")?;
    Ok(())
}

fn compress_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
    enc.multithread(2).ok();
    std::io::copy(&mut f, &mut enc)?;
    enc.finish()
}

/// Largest file a restore writes from one object. The bucket is outside
/// the server's control, so a tampered object must not be able to inflate
/// without bound; real files are far smaller (the largest production
/// tenant file is ~40 GB).
pub const MAX_RESTORED_FILE: u64 = 256 * 1024 * 1024 * 1024;
/// A restore stops before leaving less than this free on the data disk.
const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

fn decompress_to(bytes: &[u8], path: &Path) -> std::io::Result<()> {
    decompress_bounded(bytes, path, MAX_RESTORED_FILE, MIN_FREE_BYTES)
}

fn decompress_bounded(bytes: &[u8], path: &Path, max: u64, min_free: u64) -> std::io::Result<()> {
    let tmp = path.with_extension("restoring");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let result = (|| {
        let mut out = std::fs::File::create(&tmp)?;
        let mut dec = zstd::stream::Decoder::new(bytes)?;
        // zstd's default window limit (128 MiB) bounds memory; this bounds disk.
        dec.window_log_max(27)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        loop {
            let n = dec.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > max {
                return Err(std::io::Error::other(format!("decompresses to more than {max} bytes; refusing")));
            }
            if written % (64 << 20) < n as u64 && free_bytes(path.parent().unwrap_or(path)).is_some_and(|f| f < min_free) {
                return Err(std::io::Error::other(format!("less than {min_free} bytes left on the data disk; refusing")));
            }
            out.write_all(&buf[..n])?;
        }
        out.sync_all()
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
}

/// Free bytes for an unprivileged user on the filesystem holding `dir`.
fn free_bytes(dir: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs is plain data; zeroed is a valid value for it.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: path is a valid C string, s a properly sized out-param.
    (unsafe { libc::statvfs(path.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
}

/// Snapshots `src` and uploads it compressed as `key`.
/// What a database looks like on disk: the main file's and its WAL's length
/// and mtime. Every commit changes the WAL (or, after a checkpoint, the main
/// file), so an equal stamp means nothing was written since.
fn stamp(src: &Path) -> Option<String> {
    let one = |p: &Path| {
        std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())))
    };
    let wal = src.with_file_name(format!("{}-wal", src.file_name()?.to_string_lossy()));
    Some(format!("{:?} {:?}", one(src)?, one(&wal)))
}

/// Uploads `src` unless it is unchanged since this process last uploaded it.
fn upload_if_changed(s3: &S3, src: &Path, key: &str, scratch: &Path, pass: &mut Pass, last: &mut Uploaded) -> Result<(), String> {
    let before = stamp(src);
    if before.is_some() && last.get(key) == before.as_ref() {
        pass.unchanged += 1;
        return Ok(());
    }
    upload_snapshot(s3, src, key, scratch, pass)?;
    // The stamp from before the snapshot: a write during it changes the
    // stamp, so the next pass uploads again rather than missing it.
    match before {
        Some(s) => last.insert(key.to_string(), s),
        None => last.remove(key),
    };
    Ok(())
}

/// The stamp of each object as last uploaded, per process (a restart
/// uploads everything once).
pub type Uploaded = std::collections::HashMap<String, String>;

fn upload_snapshot(s3: &S3, src: &Path, key: &str, scratch: &Path, pass: &mut Pass) -> Result<(), String> {
    let tmp = scratch.join("snapshot.sqlite3");
    snapshot(src, &tmp).map_err(|e| format!("snapshot {}: {e}", src.display()))?;
    pass.bytes_raw += std::fs::metadata(&tmp).map_or(0, |m| m.len());
    let z = compress_file(&tmp).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    pass.bytes_compressed += z.len() as u64;
    s3.put(key, z)?;
    pass.snapshots += 1;
    Ok(())
}

/// Is `day` closed at `now`: every row it can still receive has arrived.
pub fn closed(day: &str, now_micros: i64) -> bool {
    crate::time::parse_db_time(&format!("{day} 00:00:00")).is_some_and(|start| start + 2 * 86_400_000_000 <= now_micros)
}

/// One backup pass over the whole data directory.
pub fn run_pass(s3: &S3, data_dir: &Path, tenants_dir: &Path, now_micros: i64, last: &mut Uploaded) -> Result<Pass, String> {
    let mut pass = Pass::default();
    let scratch = data_dir.join("backup-tmp");
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let control = data_dir.join("control.sqlite3");
    if control.is_file() {
        upload_if_changed(s3, &control, "control/latest.sqlite3.zst", &scratch, &mut pass, last)?;
    }
    let mut existing: Option<std::collections::HashSet<String>> = None;
    for entry in std::fs::read_dir(tenants_dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() && layout::meta_path(&path).is_file() {
            upload_if_changed(s3, &layout::meta_path(&path), &format!("tenants/{name}/meta.sqlite3.zst"), &scratch, &mut pass, last)?;
            let known = match &existing {
                Some(k) => k,
                None => existing.insert(s3.list("tenants/")?.into_iter().collect()),
            };
            for tier in [Tier::Core, Tier::Detail] {
                for day in layout::days(&path, tier) {
                    if !closed(&day, now_micros) {
                        continue;
                    }
                    let file = layout::day_path(&path, tier, &day);
                    // A core day whose exemplars were copied is a new object.
                    let ex = tier == Tier::Core && has_exemplars(&file);
                    let key = format!("tenants/{name}/{}/{day}{}.sqlite3.zst", tier.dir(), if ex { ".ex" } else { "" });
                    if known.contains(&key) {
                        pass.skipped += 1;
                        continue;
                    }
                    upload_snapshot(s3, &file, &key, &scratch, &mut pass)?;
                    pass.snapshots -= 1;
                    pass.day_files += 1;
                }
            }
        } else if let Some(slug) = name.strip_suffix(".sqlite3")
            && path.is_file()
        {
            upload_if_changed(s3, &path, &format!("tenants/{slug}.sqlite3.zst"), &scratch, &mut pass, last)?;
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(pass)
}

fn has_exemplars(file: &Path) -> bool {
    Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(|c| super::exemplars::done(&c)).unwrap_or(false)
}

#[derive(Debug, Default)]
pub struct Restored {
    pub files: usize,
    pub bytes: u64,
}

/// Downloads a backup into `data_dir`, which must not hold a control DB.
/// For a core day uploaded both before and after its exemplar copy, the
/// `.ex` object wins.
pub fn restore(s3: &S3, data_dir: &Path) -> Result<Restored, String> {
    let control = data_dir.join("control.sqlite3");
    if control.exists() {
        return Err(format!("{} already exists; restore into an empty data directory", control.display()));
    }
    let tenants = data_dir.join("telemetry");
    let mut out = Restored::default();
    let keys = s3.list("")?;
    let mut fetch = |key: &str, path: PathBuf| -> Result<(), String> {
        let z = s3.get(key)?;
        decompress_to(&z, &path).map_err(|e| format!("{key}: {e}"))?;
        out.files += 1;
        out.bytes += std::fs::metadata(&path).map_or(0, |m| m.len());
        Ok(())
    };
    for key in &keys {
        if key == "control/latest.sqlite3.zst" {
            continue;
        }
        let Some(rest) = key.strip_prefix("tenants/").and_then(|k| k.strip_suffix(".sqlite3.zst")) else { continue };
        let target = match rest.split('/').collect::<Vec<_>>().as_slice() {
            [slug] => tenants.join(format!("{slug}.sqlite3")),
            [slug, "meta"] => layout::meta_path(&tenants.join(slug)),
            [slug, tier @ ("core" | "detail"), day] => {
                let (day, ex) = match day.strip_suffix(".ex") {
                    Some(d) => (d, true),
                    None => (*day, false),
                };
                if !ex && keys.contains(&format!("tenants/{slug}/{tier}/{day}.ex.sqlite3.zst")) {
                    continue;
                }
                if layout::day_of(day) != Some(day) {
                    continue;
                }
                tenants.join(slug).join(tier).join(format!("{day}.sqlite3"))
            }
            _ => continue,
        };
        fetch(key, target)?;
    }
    // Control last: a data dir with a control DB is a finished restore.
    if keys.iter().any(|k| k == "control/latest.sqlite3.zst") {
        fetch("control/latest.sqlite3.zst", control)?;
    }
    Ok(out)
}

/// Backup settings (Config): all of endpoint, bucket, access key and secret
/// set turns backups on.
#[derive(Debug, Clone)]
pub struct Settings {
    pub s3: S3,
    pub interval_secs: u64,
}

/// `<data_dir>/backup-last-ok`: the unix time of the last successful pass,
/// written by the backup thread and `railwatch backup`, read by doctor.
pub fn last_ok_path(data_dir: &Path) -> PathBuf {
    data_dir.join("backup-last-ok")
}

pub fn record_ok(data_dir: &Path) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = std::fs::write(last_ok_path(data_dir), format!("{now}\n"));
}

/// The backup thread: one pass every `interval_secs`, errors logged.
pub fn spawn(settings: Settings, data_dir: PathBuf, tenants_dir: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("backup".into())
        .spawn(move || {
            let mut last = Uploaded::new();
            loop {
                let started = std::time::Instant::now();
                match run_pass(&settings.s3, &data_dir, &tenants_dir, crate::time::now_micros(), &mut last) {
                    Ok(p) => {
                        record_ok(&data_dir);
                        tracing::info!(
                            snapshots = p.snapshots,
                            unchanged = p.unchanged,
                            day_files = p.day_files,
                            skipped = p.skipped,
                            raw = p.bytes_raw,
                            compressed = p.bytes_compressed,
                            ms = started.elapsed().as_millis() as u64,
                            "backup pass"
                        )
                    }
                    Err(e) => tracing::error!(error = %e, "backup pass failed"),
                }
                std::thread::sleep(std::time::Duration::from_secs(settings.interval_secs));
            }
        })
        .expect("spawn backup thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp that lets a pass skip an unchanged database must change on
    /// every commit, WAL-mode included, or a backup would silently miss data.
    #[test]
    fn a_commit_changes_the_stamp_and_nothing_else_does() {
        let dir = std::env::temp_dir().join(format!("rw-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (x INTEGER);").unwrap();
        let first = stamp(&path).unwrap();
        assert_eq!(stamp(&path).unwrap(), first, "no write, same stamp");
        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let _: i64 = reader.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(stamp(&path).unwrap(), first, "a read does not change it");
        std::thread::sleep(std::time::Duration::from_millis(10));
        conn.execute("INSERT INTO t VALUES (1)", []).unwrap();
        let after_insert = stamp(&path).unwrap();
        assert_ne!(after_insert, first, "a commit into the WAL changes it");
        std::thread::sleep(std::time::Duration::from_millis(10));
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        assert_ne!(stamp(&path).unwrap(), after_insert, "a checkpoint into the main file changes it");
        assert!(stamp(&dir.join("missing.sqlite3")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A small object that inflates past the cap is refused and leaves no
    /// partial file behind; one under the cap restores whole.
    #[test]
    fn restore_refuses_an_object_that_inflates_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("rw-zbomb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = vec![0u8; 64 << 20];
        let z = zstd::bulk::compress(&raw, 3).unwrap();
        assert!(z.len() < 64 * 1024, "the bomb is small ({} bytes)", z.len());
        let path = dir.join("t.sqlite3");
        let err = decompress_bounded(&z, &path, 8 << 20, 0).unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");
        assert!(!path.exists() && !path.with_extension("restoring").exists());
        decompress_bounded(&z, &path, 64 << 20, 0).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
