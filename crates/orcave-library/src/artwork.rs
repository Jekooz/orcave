//! Orcave Library - Artwork extraction and on-disk caching.
//!
//! Embedded artwork is extracted once, encoded to PNG, and cached inside the
//! app cache directory. Source files are never modified.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use tracing::debug;

use crate::metadata::extract_artwork_png;

static ISOLATED_CACHE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Route artwork to the validated test profile before any library scan starts.
pub fn set_isolated_cache_root(root: PathBuf) -> Result<()> {
    anyhow::ensure!(
        ISOLATED_CACHE_ROOT.set(root).is_ok(),
        "artwork cache root already configured"
    );
    Ok(())
}

fn isolated_artwork_cache_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.join("caches").join("library_artwork");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the app cache directory, creating it if needed.
pub fn cache_dir() -> Result<PathBuf> {
    if let Some(root) = ISOLATED_CACHE_ROOT.get() {
        return isolated_artwork_cache_dir(root);
    }
    let proj =
        ProjectDirs::from("", "", "Orcave").context("could not determine app cache directory")?;
    let dir = proj.cache_dir().join("library_artwork");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create artwork cache dir: {}", dir.display()))?;
    Ok(dir)
}

/// Cache filename for a track id: sha/blake of the id + .png
pub fn artwork_cache_path(track_id: &str) -> Result<PathBuf> {
    use blake3::Hasher;
    let mut hasher = Hasher::new();
    hasher.update(track_id.as_bytes());
    let hash = hasher.finalize().to_hex().to_string();
    Ok(cache_dir()?.join(format!("{hash}.png")))
}

/// Store extracted artwork for a track, returning the cached path.
/// Overwrites any existing cached art for the same track id.
pub fn cache_artwork(track_id: &str, path: &Path) -> Result<Option<PathBuf>> {
    let png_bytes = match extract_artwork_png(path)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let out_path = artwork_cache_path(track_id)?;
    fs::write(&out_path, &png_bytes)
        .with_context(|| format!("failed to write artwork cache: {}", out_path.display()))?;
    debug!(track_id, path = %out_path.display(), "cached artwork");
    Ok(Some(out_path))
}

/// Remove cached artwork for a track id.
pub fn remove_cached_artwork(track_id: &str) -> Result<()> {
    if let Ok(p) = artwork_cache_path(track_id) {
        let _ = fs::remove_file(&p);
    }
    Ok(())
}

/// Return the cached artwork path if it exists on disk.
pub fn cached_artwork_path(track_id: &str) -> Option<PathBuf> {
    artwork_cache_path(track_id).ok().filter(|p| p.exists())
}

/// Prune artwork cache entries no longer referenced by any track id.
pub fn prune_artwork_cache(referenced: &[String]) -> Result<usize> {
    let dir = cache_dir()?;
    let mut removed = 0usize;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !referenced.iter().any(|id| blake3_hash(id) == stem) {
                if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

fn blake3_hash(s: &str) -> String {
    use blake3::Hasher;
    let mut h = Hasher::new();
    h.update(s.as_bytes());
    h.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn isolated_artwork_cache_stays_under_test_root() {
        let root = tempfile::tempdir().unwrap();
        let cache = isolated_artwork_cache_dir(root.path()).unwrap();
        assert_eq!(cache, root.path().join("caches").join("library_artwork"));
        assert!(cache.is_dir());
    }

    #[test]
    fn artwork_cache_path_is_deterministic() {
        let a = artwork_cache_path("track-1").unwrap();
        let b = artwork_cache_path("track-1").unwrap();
        assert_eq!(a, b);
        let c = artwork_cache_path("track-2").unwrap();
        assert_ne!(a, c);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("png"));
    }

    #[test]
    fn cache_artwork_returns_none_for_file_without_art() {
        // A WAV file has no embedded artwork.
        let wav = crate::metadata::make_wav(440.0, 100, 48000);
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&wav).unwrap();
        let result = cache_artwork("no-art-track", file.path());
        assert!(
            result.is_ok(),
            "cache_artwork should not error on no-art file"
        );
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn cached_artwork_path_none_before_written() {
        assert!(cached_artwork_path("never-cached").is_none());
    }

    #[test]
    fn blake3_hash_is_deterministic() {
        assert_eq!(blake3_hash("abc"), blake3_hash("abc"));
        assert_ne!(blake3_hash("abc"), blake3_hash("abd"));
    }

    #[test]
    fn prune_artwork_cache_removes_unreferenced() {
        let dir = cache_dir().unwrap();
        // Write two cache files, reference only one.
        let kept = artwork_cache_path("keep-me").unwrap();
        let drop = artwork_cache_path("drop-me").unwrap();
        fs::write(&kept, b"fake").unwrap();
        fs::write(&drop, b"fake").unwrap();

        let removed = prune_artwork_cache(&["keep-me".to_string()]).unwrap();
        assert_eq!(removed, 1);
        assert!(kept.exists());
        assert!(!drop.exists());

        // Cleanup the kept file too.
        let _ = fs::remove_file(&kept);
    }
}
