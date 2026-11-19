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

/// Bytes under `sourcemaps_dir` (every environment).
pub fn usage(sourcemaps_dir: &Path) -> u64 {
    dir_bytes(sourcemaps_dir)
}

/// Bytes of one environment's maps.
pub fn env_bytes(sourcemaps_dir: &Path, slug: &str) -> u64 {
    dir_bytes(&sourcemaps_dir.join(slug))
}

/// The environment's deploy directories, oldest first (by modification time,
/// which every upload into it renews).
fn deploys(env_dir: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut out: Vec<_> = std::fs::read_dir(env_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
        .collect();
    out.sort();
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The environment's maps would pass MAX_ENV_BYTES even with every
    /// other deploy's pruned.
    TooMuch,
}

/// Writes one map for `deploy`, first pruning the environment's oldest other
/// deploys so at most `max_deploys` remain and the new file fits `max_bytes`.
pub fn store(
    sourcemaps_dir: &Path,
    slug: &str,
    deploy: &str,
    filename: &str,
    body: &[u8],
    max_bytes: u64,
    max_deploys: usize,
) -> Result<Result<(), Refused>, std::io::Error> {
    let env_dir = sourcemaps_dir.join(slug);
    let path = env_dir.join(deploy).join(format!("{filename}.map"));
    let replaced = std::fs::metadata(&path).map_or(0, |m| m.len());
    let this = env_dir.join(deploy);
    let mut others: Vec<PathBuf> = deploys(&env_dir).into_iter().map(|(_, p)| p).filter(|p| *p != this).collect();
    let mut used = dir_bytes(&env_dir).saturating_sub(replaced);
    let keep_others = max_deploys.saturating_sub(1);
    while others.len() > keep_others || (used + body.len() as u64 > max_bytes && !others.is_empty()) {
        let old = others.remove(0);
        used = used.saturating_sub(dir_bytes(&old));
        std::fs::remove_dir_all(&old)?;
        tracing::info!(tenant = slug, deploy = %old.file_name().unwrap_or_default().to_string_lossy(), "source maps: pruned an older deploy's maps");
    }
    if used + body.len() as u64 > max_bytes {
        return Ok(Err(Refused::TooMuch));
    }
    std::fs::create_dir_all(path.parent().expect("a map has a deploy directory"))?;
    let tmp = path.with_extension("map.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
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
        assert_eq!(deploys(&root.join("e")).len(), 1, "d3 was pruned too, and it still does not fit");
        assert!(env_bytes(&root, "e") <= 10 * 1024);
        // Another environment is untouched by one's pruning.
        assert_eq!(store(&root, "other", "d0", "a.js", &kb, 10 * 1024, 3).unwrap(), Ok(()));
        destroy(&root, "e");
        assert!(!root.join("e").exists() && root.join("other").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
