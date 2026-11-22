//! Uploaded browser source maps: `<data_dir>/sourcemaps/<slug>/<deploy>/<file>.map`
//! (http.rs `/ingest/sourcemaps`).
//!
//! Bounded per environment (GA review 2026-11-19, security #2): at most
//! MAX_DEPLOYS deploys are kept (the oldest by modification time go first)
//! and at most MAX_ENV_BYTES in all; an upload that would pass the byte cap
//! after the older deploys are pruned is refused. The bytes count in the
//! disk budget (`storage::budget::usage`), and the directory goes with its
//! environment (`destroy`). Files and directories together are capped at
//! MAX_ENV_ENTRIES the same way: the byte count does not see a directory,
//! so deep filenames could otherwise make any number of them.

use std::path::{Path, PathBuf};

/// Bytes of source maps one environment may keep.
pub const MAX_ENV_BYTES: u64 = 200 * 1024 * 1024;
/// Deploys per environment whose maps are kept.
pub const MAX_DEPLOYS: usize = 10;
/// Uploads per environment per minute. A large Vite or Next release uploads
/// hundreds of maps, and the uploader does not fail the deploy on a 429, so
/// a lower limit dropped maps silently (PR #107 review L7). The byte caps
/// bound what is kept.
pub const UPLOADS_PER_MINUTE: u64 = 600;
/// Files and directories one environment may keep. Far above what a large
/// release's maps need (hundreds of files in a few directories per deploy);
/// a bound on what the byte cap cannot see.
pub const MAX_ENV_ENTRIES: u64 = 50_000;

/// Bytes of the files under `p`, and how many files and directories.
#[derive(Clone, Copy, Default)]
struct Usage {
    bytes: u64,
    entries: u64,
}

fn usage(p: &Path) -> Usage {
    let mut n = Usage::default();
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        // Never follows a symlink: nothing here makes one.
        let Ok(m) = e.path().symlink_metadata() else { continue };
        n.entries += 1;
        if m.is_dir() {
            let inner = usage(&e.path());
            n.bytes += inner.bytes;
            n.entries += inner.entries;
        } else {
            n.bytes += m.len();
        }
    }
    n
}

fn dir_bytes(p: &Path) -> u64 {
    usage(p).bytes
}

/// UPLOADS_PER_MINUTE per environment id, process-wide.
pub fn rate_limit() -> &'static crate::rate_limit::FixedWindow<i64> {
    static LIMIT: std::sync::OnceLock<crate::rate_limit::FixedWindow<i64>> = std::sync::OnceLock::new();
    LIMIT.get_or_init(|| crate::rate_limit::FixedWindow::new(UPLOADS_PER_MINUTE))
}

/// Bytes of one environment's maps.
pub fn env_bytes(sourcemaps_dir: &Path, slug: &str) -> u64 {
    dir_bytes(&sourcemaps_dir.join(slug))
}

/// Written into a deploy's directory on every upload: its modification time
/// is the deploy's age. The directory's own mtime is not, since uploads into
/// a subdirectory (`assets/app.js`) do not change it (PR #107 review L2).
const TOUCHED: &str = ".touched";

/// When the deploy last received a map.
fn last_upload(deploy_dir: &Path) -> std::time::SystemTime {
    let at = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    at(&deploy_dir.join(TOUCHED)).or_else(|| at(deploy_dir)).unwrap_or(std::time::UNIX_EPOCH)
}

/// The environment's deploy directories, least recently uploaded to first.
fn deploys(env_dir: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut out: Vec<_> = std::fs::read_dir(env_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (last_upload(&e.path()), e.path()))
        .collect();
    out.sort();
    out
}

/// One lock per environment slug: concurrent uploads of a new deploy each
/// pruned the same oldest deploy, and the losers failed on its half-deleted
/// directory; the byte check could also pass for several at once (PR #107
/// review M1). Uploads of one environment are serial; others run alongside.
fn env_lock(slug: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
    // Unused entries go (only the map holds them), so the map stays small.
    locks.retain(|_, l| std::sync::Arc::strong_count(l) > 1);
    locks.entry(slug.to_string()).or_default().clone()
}

/// Slugs whose maps were destroyed in this process. A slug is never reused
/// (control::retired_environment_slugs), so an upload that authenticated
/// just before its environment was deleted must not recreate the directory
/// once `destroy` has run (hunt 2, P2): it is refused instead. Keyed by the
/// environment's directory, not the bare slug, so two data dirs in one
/// process (tests) stay apart.
fn destroyed() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static DESTROYED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = std::sync::OnceLock::new();
    DESTROYED.get_or_init(Default::default)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The environment's maps would pass MAX_ENV_BYTES (or MAX_ENV_ENTRIES)
    /// even with every other deploy's pruned.
    TooMuch,
    /// The environment was deleted while the upload was in flight.
    Deleted,
}

/// Writes one map for `deploy`, first pruning the environment's oldest other
/// deploys so at most `max_deploys` remain and the new file fits `max_bytes`
/// and `max_entries` (files and directories).
/// A map that cannot fit even with every other deploy gone is refused
/// before anything is pruned.
#[allow(clippy::too_many_arguments)]
pub fn store(
    sourcemaps_dir: &Path,
    slug: &str,
    deploy: &str,
    filename: &str,
    body: &[u8],
    max_bytes: u64,
    max_deploys: usize,
    max_entries: u64,
) -> Result<Result<(), Refused>, std::io::Error> {
    let lock = env_lock(slug);
    let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let env_dir = sourcemaps_dir.join(slug);
    if destroyed().lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&env_dir) {
        return Ok(Err(Refused::Deleted));
    }
    let this = env_dir.join(deploy);
    let path = this.join(format!("{filename}.map"));
    let replaced = std::fs::metadata(&path).map_or(0, |m| m.len());
    let new = body.len() as u64;
    // The entries this upload adds: the directories of its path that do
    // not exist yet (the deploy's own included), the map, and .touched.
    let missing = |p: &Path| u64::from(std::fs::symlink_metadata(p).is_err());
    let added = path.parent().into_iter().flat_map(Path::ancestors).take_while(|p| p.starts_with(&this)).map(missing).sum::<u64>()
        + missing(&path)
        + missing(&this.join(TOUCHED));
    // Refused first, before pruning (PR #107 review L1): pruning cannot make
    // room for more than this deploy's own maps plus the new one.
    let own = usage(&this);
    if own.bytes.saturating_sub(replaced) + new > max_bytes || own.entries + u64::from(this.exists()) + added > max_entries {
        return Ok(Err(Refused::TooMuch));
    }
    let mut others: Vec<PathBuf> = deploys(&env_dir).into_iter().map(|(_, p)| p).filter(|p| *p != this).collect();
    let all = usage(&env_dir);
    let (mut used, mut entries) = (all.bytes.saturating_sub(replaced), all.entries);
    let keep_others = max_deploys.saturating_sub(1);
    while others.len() > keep_others || ((used + new > max_bytes || entries + added > max_entries) && !others.is_empty()) {
        let old = others.remove(0);
        let gone = usage(&old);
        used = used.saturating_sub(gone.bytes);
        entries = entries.saturating_sub(gone.entries + 1);
        match std::fs::remove_dir_all(&old) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        tracing::info!(tenant = slug, deploy = %old.file_name().unwrap_or_default().to_string_lossy(), "source maps: pruned an older deploy's maps");
    }
    std::fs::create_dir_all(path.parent().expect("a map has a deploy directory"))?;
    let tmp = path.with_extension("map.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    std::fs::write(this.join(TOUCHED), b"")?;
    Ok(Ok(()))
}

/// Deletes the environment's maps (its environment was deleted).
pub fn destroy(sourcemaps_dir: &Path, slug: &str) {
    // Under the upload lock, so an upload in flight either finished before
    // this or sees the slug destroyed and writes nothing.
    let lock = env_lock(slug);
    let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = sourcemaps_dir.join(slug);
    destroyed().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(dir.clone());
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => tracing::info!(path = %dir.display(), "source maps deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::error!(path = %dir.display(), error = %e, "could not delete source maps"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_deploys_are_pruned_and_an_environment_cannot_pass_its_cap() {
        let root = std::env::temp_dir().join(format!("rw-sm-{}", crate::control::base58(8)));
        let kb = vec![b'x'; 1024];
        for d in 0..4 {
            assert_eq!(store(&root, "e", &format!("d{d}"), "a.js", &kb, 10 * 1024, 3, MAX_ENV_ENTRIES).unwrap(), Ok(()));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let left: Vec<_> = deploys(&root.join("e")).into_iter().map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["d1", "d2", "d3"], "only the newest 3 deploys are kept");
        // Bytes: a big upload prunes the others first, then is refused if it still does not fit.
        assert_eq!(store(&root, "e", "d4", "big.js", &vec![b'x'; 9 * 1024], 10 * 1024, 3, MAX_ENV_ENTRIES).unwrap(), Ok(()));
        assert_eq!(deploys(&root.join("e")).len(), 2, "d1 and d2 went to make room, d3 still fits");
        assert_eq!(store(&root, "e", "d4", "more.js", &vec![b'x'; 2048], 10 * 1024, 3, MAX_ENV_ENTRIES).unwrap(), Err(Refused::TooMuch));
        assert_eq!(deploys(&root.join("e")).len(), 2, "refused without pruning d3: d4 alone could never take it");
        assert!(env_bytes(&root, "e") <= 10 * 1024);
        // Another environment is untouched by one's pruning.
        assert_eq!(store(&root, "other", "d0", "a.js", &kb, 10 * 1024, 3, MAX_ENV_ENTRIES).unwrap(), Ok(()));
        destroy(&root, "e");
        assert!(!root.join("e").exists() && root.join("other").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The byte cap does not see directories: deep filenames made any
    /// number of them (audit 2026-11-22, rw-h-ingest). Files and
    /// directories together are capped, older deploys going first.
    #[test]
    fn files_and_directories_are_capped_per_environment() {
        let root = std::env::temp_dir().join(format!("rw-sm-entries-{}", crate::control::base58(8)));
        let deep = |i: usize| format!("{i:03}{}", "/a".repeat(20));
        let up = |d: &str, f: &str| store(&root, "e", d, f, b"{}", 1 << 30, 10, 100).unwrap();
        // "000/a/.../a" is 20 directories and a map: the first upload adds
        // those, the deploy's directory and .touched (23), later ones 21.
        let mut ok = 0;
        while up("d1", &deep(ok)) == Ok(()) {
            ok += 1;
        }
        assert_eq!(ok, 4, "23 + 3 x 21 = 86 entries fit, a fifth would be 107");
        assert_eq!(usage(&root.join("e")).entries, 86);
        // A flat map still fits in the same deploy (one more entry).
        assert_eq!(up("d1", "flat.js"), Ok(()));
        // A new deploy that would pass the cap prunes the oldest first.
        assert_eq!(up("d2", &deep(0)), Ok(()));
        let left: Vec<_> = deploys(&root.join("e")).into_iter().map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["d2"], "d1 went to make room");
        assert_eq!(usage(&root.join("e")).entries, 23);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PR #107 review L1: a map too big to fit even alone is refused
    /// without deleting any other deploy's maps.
    #[test]
    fn a_map_that_can_never_fit_is_refused_without_pruning_anything() {
        let root = std::env::temp_dir().join(format!("rw-sm-l1-{}", crate::control::base58(8)));
        for d in 0..3 {
            assert_eq!(store(&root, "e", &format!("d{d}"), "a.js", &[b'x'; 1024], 10 * 1024, 10, MAX_ENV_ENTRIES).unwrap(), Ok(()));
        }
        assert_eq!(store(&root, "e", "big", "a.js", &vec![b'x'; 11 * 1024], 10 * 1024, 10, MAX_ENV_ENTRIES).unwrap(), Err(Refused::TooMuch));
        assert_eq!(deploys(&root.join("e")).len(), 3, "every other deploy is still there");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PR #107 review L2: a deploy that keeps receiving maps in a
    /// subdirectory is the newest, even though its directory's own mtime
    /// does not change; the deploy actually untouched longest is pruned.
    #[test]
    fn uploads_into_a_subdirectory_keep_their_deploy_newest() {
        let root = std::env::temp_dir().join(format!("rw-sm-l2-{}", crate::control::base58(8)));
        let kb = [b'x'; 16];
        let up = |d: &str, f: &str| assert_eq!(store(&root, "e", d, f, &kb, 1 << 20, 2, MAX_ENV_ENTRIES).unwrap(), Ok(()));
        up("old", "assets/a.js");
        std::thread::sleep(std::time::Duration::from_millis(30));
        up("mid", "b.js");
        std::thread::sleep(std::time::Duration::from_millis(30));
        up("old", "assets/c.js");
        std::thread::sleep(std::time::Duration::from_millis(30));
        up("new", "d.js");
        let left: Vec<_> = deploys(&root.join("e")).into_iter().map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["old", "new"], "mid had the oldest upload");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PR #107 review M1: parallel uploads of a new deploy, when each one
    /// prunes the oldest, all succeed and the caps hold.
    #[test]
    fn parallel_uploads_of_a_new_deploy_all_succeed_within_the_caps() {
        let root = std::env::temp_dir().join(format!("rw-sm-m1-{}", crate::control::base58(8)));
        for run in 0..5 {
            let slug = format!("e{run}");
            let blob = vec![b'x'; 64 * 1024];
            for d in 0..10 {
                assert_eq!(store(&root, &slug, &format!("d{d}"), "a.js", &blob, 1 << 30, 10, MAX_ENV_ENTRIES).unwrap(), Ok(()));
            }
            let errors: Vec<String> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..8)
                    .map(|i| {
                        let (root, slug, blob) = (&root, &slug, &blob);
                        s.spawn(move || store(root, slug, "d10", &format!("f{i}.js"), blob, 1 << 30, 10, MAX_ENV_ENTRIES))
                    })
                    .collect();
                hs.into_iter()
                    .filter_map(|h| match h.join().unwrap() {
                        Ok(Ok(())) => None,
                        other => Some(format!("{other:?}")),
                    })
                    .collect()
            });
            assert!(errors.is_empty(), "run {run}: {errors:?}");
            assert_eq!(deploys(&root.join(&slug)).len(), 10);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Hunt 2, P2: an upload that authenticated just before its environment
    /// was deleted runs after `destroy`; it must not recreate the deleted
    /// slug's directory (slugs are never reused, so it would be an orphan).
    #[test]
    fn an_upload_after_its_environment_was_destroyed_writes_nothing() {
        let root = std::env::temp_dir().join(format!("rw-sm-{}", crate::control::base58(8)));
        let slug = format!("gone-{}", crate::control::base58(6));
        assert_eq!(store(&root, &slug, "d1", "a.js", b"{}", 1 << 20, 10, MAX_ENV_ENTRIES).unwrap(), Ok(()));
        destroy(&root, &slug);
        assert!(!root.join(&slug).exists());
        assert_eq!(store(&root, &slug, "d2", "a.js", b"{}", 1 << 20, 10, MAX_ENV_ENTRIES).unwrap(), Err(Refused::Deleted));
        assert!(!root.join(&slug).exists(), "the deleted environment's directory is not recreated");
        let _ = std::fs::remove_dir_all(&root);
    }
}
