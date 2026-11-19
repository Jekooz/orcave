//! Orcave Library - Folder scanning and file watching.
//!
//! Walks library folders with walkdir, extracts metadata, detects duplicates,
//! and watches for file changes with notify. Read-only: source files are only
//! ever read, never modified or deleted.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use dashmap::DashMap;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::artwork::cache_artwork;
use crate::dedup::{hash_file, DedupIndex};
use crate::metadata::{extract_metadata, is_audio_file};
use crate::models::{FolderSource, LibraryFolder, LibraryTrack, ScanError, ScanResult};

/// Events emitted by the scanner watcher.
#[derive(Debug, Clone)]
pub enum ScanEvent {
    ScanStarted {
        folder_id: String,
    },
    ScanCompleted {
        folder_id: String,
        result: ScanResult,
    },
    FileChanged {
        path: PathBuf,
    },
    FileDeleted {
        path: PathBuf,
    },
    FileCreated {
        path: PathBuf,
    },
}

/// Walks a folder and discovers audio files.
pub struct Scanner {
    /// Dedup index shared across scans (parking_lot for sync walkdir iteration).
    dedup: Arc<Mutex<DedupIndex>>,
    /// Existing tracks keyed by relative path, for update detection.
    tracks_by_path: Arc<DashMap<String, LibraryTrack>>,
    /// Public track index owned by LibraryService; scanned tracks are inserted
    /// here so `library_get_tracks` can return them.
    tracks: Arc<DashMap<String, LibraryTrack>>,
    event_tx: mpsc::UnboundedSender<ScanEvent>,
}

impl Scanner {
    pub fn new(
        event_tx: mpsc::UnboundedSender<ScanEvent>,
        tracks: Arc<DashMap<String, LibraryTrack>>,
    ) -> Self {
        Self {
            dedup: Arc::new(Mutex::new(DedupIndex::new())),
            tracks_by_path: Arc::new(DashMap::new()),
            tracks,
            event_tx,
        }
    }

    /// Scan a folder recursively. Safe incremental: only new/changed files are
    /// re-processed; unchanged files are skipped.
    pub async fn scan_folder(&self, folder: &LibraryFolder) -> Result<ScanResult> {
        let root = match &folder.source_type {
            FolderSource::Local => folder.path.clone(),
            FolderSource::Smb { unc_path, .. } => PathBuf::from(unc_path),
        };

        if !root.exists() {
            return Ok(ScanResult {
                errors: vec![ScanError {
                    path: root.clone(),
                    message: "folder path does not exist".to_string(),
                }],
                ..Default::default()
            });
        }

        let mut result = ScanResult::default();
        let dedup = Arc::clone(&self.dedup);
        let tracks_by_path = Arc::clone(&self.tracks_by_path);

        let walk = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok());

        for entry in walk {
            let path = entry.path();
            if !path.is_file() || !is_audio_file(path) {
                continue;
            }

            // Skip hidden files/dirs and common non-audio system dirs.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') || name.starts_with("$RECYCLE.BIN") {
                    continue;
                }
            }

            let rel = path
                .strip_prefix(&root)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| path.to_path_buf());
            let path_key = rel.to_string_lossy().to_string();

            let metadata = match path.metadata() {
                Ok(m) => m,
                Err(e) => {
                    result.errors.push(ScanError {
                        path: path.to_path_buf(),
                        message: format!("metadata read failed: {e}"),
                    });
                    continue;
                }
            };
            let modified = metadata.modified().ok();
            let size = metadata.len();

            // Skip unchanged files already indexed (incremental scan).
            if let Some(existing) = tracks_by_path.get(&path_key) {
                if existing.modified_at == modified && existing.size == size {
                    continue;
                }
                result.tracks_updated += 1;
            }

            // Extract metadata (read-only).
            let meta = match extract_metadata(path) {
                Ok(m) => m,
                Err(e) => {
                    result.errors.push(ScanError {
                        path: path.to_path_buf(),
                        message: format!("metadata extraction failed: {e}"),
                    });
                    continue;
                }
            };

            // Content hash for dedup.
            let content_hash = match hash_file(path) {
                Ok(h) => Some(h),
                Err(e) => {
                    result.errors.push(ScanError {
                        path: path.to_path_buf(),
                        message: format!("content hash failed: {e}"),
                    });
                    None
                }
            };

            // Duplicate detection: same content under a different track id.
            let is_duplicate = content_hash.as_ref().is_some_and(|h| {
                let d = dedup.lock();
                d.owner_of(h).map_or(false, |owner| !owner.is_empty())
            });

            if is_duplicate {
                result.duplicates_skipped += 1;
                continue;
            }

            let id = ulid::Ulid::new().to_string();
            let title = meta.title.unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            });

            let track = LibraryTrack {
                id: id.clone(),
                path: path.to_path_buf(),
                content_hash,
                title,
                artist: meta.artist.unwrap_or_else(|| "Unknown Artist".to_string()),
                album: meta.album.unwrap_or_else(|| "Unknown Album".to_string()),
                duration_ms: meta.duration.map(|d| d.as_millis() as u64),
                artwork_path: None,
                added_at: chrono::Utc::now(),
                modified_at: modified,
                size,
                folder_id: folder.id.clone(),
            };

            // Register hash in dedup index (first writer wins).
            if let Some(h) = &track.content_hash {
                dedup.lock().register(&id, h);
            }

            // Cache artwork to disk (read from source, write to cache dir).
            if meta.artwork_bytes.is_some() {
                if let Ok(Some(_art_path)) = cache_artwork(&id, path) {
                    debug!(track_id = %id, "artwork cached");
                }
            }

            tracks_by_path.insert(path_key, track.clone());
            self.tracks.insert(id, track);
            result.tracks_added += 1;
        }

        Ok(result)
    }

    /// Start watching a folder for changes (create, modify, delete, rename).
    pub async fn watch_folder(&self, path: &Path) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let event_tx = self.event_tx.clone();

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let _ = tx.send(res);
            },
            Config::default(),
        )
        .context("failed to create notify watcher")?;

        watcher
            .watch(path, RecursiveMode::Recursive)
            .context(format!("failed to watch folder: {}", path.display()))?;

        let event_tx_inner = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Ok(event) => {
                        for ev in event_to_scan_events(&event) {
                            let _ = event_tx_inner.send(ev);
                        }
                    }
                    Err(e) => error!(%e, "notify watcher error"),
                }
            }
        });

        info!(path = %path.display(), "started folder watcher");
        Ok(())
    }

    pub fn event_channel(&self) -> mpsc::UnboundedSender<ScanEvent> {
        self.event_tx.clone()
    }
}

/// Convert a notify Event into ScanEvents.
fn event_to_scan_events(event: &Event) -> Vec<ScanEvent> {
    let mut out = Vec::new();
    for path in event.paths.iter() {
        match event.kind {
            EventKind::Create(_) => out.push(ScanEvent::FileCreated { path: path.clone() }),
            EventKind::Modify(_) => out.push(ScanEvent::FileChanged { path: path.clone() }),
            EventKind::Remove(_) => out.push(ScanEvent::FileDeleted { path: path.clone() }),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_wav_in(dir: &TempDir, name: &str, freq: f32) -> PathBuf {
        let wav = crate::metadata::make_wav(freq, 100, 48000);
        let path = dir.path().join(name);
        std::fs::write(&path, &wav).unwrap();
        path
    }

    #[tokio::test]
    async fn scan_folder_discover_audio_files() {
        let dir = TempDir::new().unwrap();
        make_wav_in(&dir, "song1.wav", 440.0);
        make_wav_in(&dir, "song2.wav", 880.0);
        std::fs::write(dir.path().join("readme.txt"), b"not audio").unwrap();

        // Verify files exist on disk before scanning.
        let disk_files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            disk_files.iter().any(|f| f == "song1.wav"),
            "song1.wav missing from disk: {:?}",
            disk_files
        );
        assert!(
            disk_files.iter().any(|f| f == "song2.wav"),
            "song2.wav missing from disk: {:?}",
            disk_files
        );

        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-1".to_string(),
            path: dir.path().to_path_buf(),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert_eq!(result.tracks_added, 2);
        assert_eq!(result.duplicates_skipped, 0);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn scan_duplicate_content_skipped() {
        let dir = TempDir::new().unwrap();
        let wav = crate::metadata::make_wav(440.0, 100, 48000);
        std::fs::write(dir.path().join("a.wav"), &wav).unwrap();
        std::fs::write(dir.path().join("b.wav"), &wav).unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-1".to_string(),
            path: dir.path().to_path_buf(),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert_eq!(result.tracks_added, 1);
        assert_eq!(result.duplicates_skipped, 1);
    }

    #[tokio::test]
    async fn scan_nonexistent_folder_returns_error() {
        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-x".to_string(),
            path: PathBuf::from("\\\\nonexistent\\path"),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert!(!result.errors.is_empty());
        assert_eq!(result.tracks_added, 0);
    }

    #[test]
    fn is_audio_file_used_by_scanner() {
        assert!(is_audio_file(Path::new("x.mp3")));
        assert!(!is_audio_file(Path::new("x.txt")));
    }

    #[tokio::test]
    async fn scan_folder_with_spaces_in_path() {
        let dir = TempDir::new().unwrap();
        let music_dir = dir.path().join("My Music Folder");
        std::fs::create_dir_all(&music_dir).unwrap();
        let wav = crate::metadata::make_wav(440.0, 100, 48000);
        std::fs::write(music_dir.join("song one.wav"), &wav).unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-spaces".to_string(),
            path: music_dir,
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert_eq!(result.tracks_added, 1);
        assert_eq!(result.duplicates_skipped, 0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    }

    #[tokio::test]
    async fn scan_empty_folder() {
        let dir = TempDir::new().unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-empty".to_string(),
            path: dir.path().to_path_buf(),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert_eq!(result.tracks_added, 0);
        assert_eq!(result.duplicates_skipped, 0);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn scan_folder_source_files_unchanged_after_scan() {
        let dir = TempDir::new().unwrap();
        let wav = crate::metadata::make_wav(440.0, 100, 48000);
        let path = dir.path().join("song.wav");
        std::fs::write(&path, &wav).unwrap();

        // Record source file state before scan.
        let hash_before = crate::dedup::hash_file(&path).unwrap();
        let meta_before = path.metadata().unwrap();
        let size_before = meta_before.len();
        let mtime_before = meta_before.modified().ok();

        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-readonly".to_string(),
            path: dir.path().to_path_buf(),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert_eq!(result.tracks_added, 1);
        assert!(result.errors.is_empty());

        // Source file must be unmodified: same content, size, mtime; still exists.
        assert!(path.exists(), "source file was deleted by scan");
        let hash_after = crate::dedup::hash_file(&path).unwrap();
        assert_eq!(hash_before, hash_after, "source file content changed");
        let meta_after = path.metadata().unwrap();
        assert_eq!(size_before, meta_after.len(), "source file size changed");
        assert_eq!(
            mtime_before,
            meta_after.modified().ok(),
            "source file mtime changed"
        );
    }

    #[tokio::test]
    async fn scan_folder_mixed_unsupported_and_audio() {
        let dir = TempDir::new().unwrap();
        make_wav_in(&dir, "song1.wav", 440.0);
        make_wav_in(&dir, "song2.wav", 880.0);
        std::fs::write(dir.path().join("notes.txt"), b"not audio").unwrap();
        std::fs::write(dir.path().join("cover.png"), b"PNG").unwrap();
        std::fs::write(dir.path().join("data.json"), b"{}").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<ScanEvent>();
        let tracks = Arc::new(DashMap::new());
        let scanner = Scanner::new(tx, Arc::clone(&tracks));
        let folder = LibraryFolder {
            id: "folder-mixed".to_string(),
            path: dir.path().to_path_buf(),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        let result = scanner.scan_folder(&folder).await.unwrap();
        assert_eq!(result.tracks_added, 2);
        assert_eq!(result.duplicates_skipped, 0);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    }
}
