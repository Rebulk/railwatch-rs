//! Uploaded browser source maps: `<data_dir>/sourcemaps/<slug>/<deploy>/<file>.map`
//! (http.rs `/ingest/sourcemaps`).
//!
//! Bounded per environment (GA review 2026-11-19, security #2): at most
//! MAX_DEPLOYS deploys are kept (the oldest by modification time go first)
//! and at most MAX_ENV_BYTES in all; an upload that would pass the byte cap
//! after the older deploys are pruned is refused. The bytes count in the
//! disk budget (`storage::budget::usage`), and the directory goes with its
//! environment (`destroy`).

use std::path::{Path, PathBuf};

/// Bytes of source maps one environment may keep.
pub const MAX_ENV_BYTES: u64 = 200 * 1024 * 1024;
/// Deploys per environment whose maps are kept.
pub const MAX_DEPLOYS: usize = 10;
/// Uploads per environment per minute (a release uploads tens of maps).
pub const UPLOADS_PER_MINUTE: u64 = 120;

fn dir_bytes(p: &Path) -> u64 {
    let mut n = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        // Never follows a symlink: nothing here makes one.
        let Ok(m) = e.path().symlink_metadata() else { continue };
        n += if m.is_dir() { dir_bytes(&e.path()) } else { m.len() };
    }
    n
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

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The environment's maps would pass MAX_ENV_BYTES even with every
    /// other deploy's pruned.
    TooMuch,
}

/// Writes one map for `deploy`, first pruning the environment's oldest other
/// deploys so at most `max_deploys` remain and the new file fits `max_bytes`.
/// A map that cannot fit even with every other deploy gone is refused
/// before anything is pruned.
pub fn store(
    sourcemaps_dir: &Path,
    slug: &str,
    deploy: &str,
    filename: &str,
    body: &[u8],
    max_bytes: u64,
    max_deploys: usize,
) -> Result<Result<(), Refused>, std::io::Error> {
    let lock = env_lock(slug);
    let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let env_dir = sourcemaps_dir.join(slug);
    let this = env_dir.join(deploy);
    let path = this.join(format!("{filename}.map"));
    let replaced = std::fs::metadata(&path).map_or(0, |m| m.len());
    let new = body.len() as u64;
    // Refused first, before pruning (PR #107 review L1): pruning cannot make
    // room for more than this deploy's own maps plus the new one.
    if dir_bytes(&this).saturating_sub(replaced) + new > max_bytes {
        return Ok(Err(Refused::TooMuch));
    }
    let mut others: Vec<PathBuf> = deploys(&env_dir).into_iter().map(|(_, p)| p).filter(|p| *p != this).collect();
    let mut used = dir_bytes(&env_dir).saturating_sub(replaced);
    let keep_others = max_deploys.saturating_sub(1);
    while others.len() > keep_others || (used + new > max_bytes && !others.is_empty()) {
        let old = others.remove(0);
        used = used.saturating_sub(dir_bytes(&old));
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
    let dir = sourcemaps_dir.join(slug);
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
            assert_eq!(store(&root, "e", &format!("d{d}"), "a.js", &kb, 10 * 1024, 3).unwrap(), Ok(()));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let left: Vec<_> = deploys(&root.join("e")).into_iter().map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["d1", "d2", "d3"], "only the newest 3 deploys are kept");
        // Bytes: a big upload prunes the others first, then is refused if it still does not fit.
        assert_eq!(store(&root, "e", "d4", "big.js", &vec![b'x'; 9 * 1024], 10 * 1024, 3).unwrap(), Ok(()));
        assert_eq!(deploys(&root.join("e")).len(), 2, "d1 and d2 went to make room, d3 still fits");
        assert_eq!(store(&root, "e", "d4", "more.js", &vec![b'x'; 2048], 10 * 1024, 3).unwrap(), Err(Refused::TooMuch));
        assert_eq!(deploys(&root.join("e")).len(), 2, "refused without pruning d3: d4 alone could never take it");
        assert!(env_bytes(&root, "e") <= 10 * 1024);
        // Another environment is untouched by one's pruning.
        assert_eq!(store(&root, "other", "d0", "a.js", &kb, 10 * 1024, 3).unwrap(), Ok(()));
        destroy(&root, "e");
        assert!(!root.join("e").exists() && root.join("other").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// PR #107 review L1: a map too big to fit even alone is refused
    /// without deleting any other deploy's maps.
    #[test]
    fn a_map_that_can_never_fit_is_refused_without_pruning_anything() {
        let root = std::env::temp_dir().join(format!("rw-sm-l1-{}", crate::control::base58(8)));
        for d in 0..3 {
            assert_eq!(store(&root, "e", &format!("d{d}"), "a.js", &[b'x'; 1024], 10 * 1024, 10).unwrap(), Ok(()));
        }
        assert_eq!(store(&root, "e", "big", "a.js", &vec![b'x'; 11 * 1024], 10 * 1024, 10).unwrap(), Err(Refused::TooMuch));
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
        let up = |d: &str, f: &str| assert_eq!(store(&root, "e", d, f, &kb, 1 << 20, 2).unwrap(), Ok(()));
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
                assert_eq!(store(&root, &slug, &format!("d{d}"), "a.js", &blob, 1 << 30, 10).unwrap(), Ok(()));
            }
            let errors: Vec<String> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..8)
                    .map(|i| {
                        let (root, slug, blob) = (&root, &slug, &blob);
                        s.spawn(move || store(root, slug, "d10", &format!("f{i}.js"), blob, 1 << 30, 10))
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
}
