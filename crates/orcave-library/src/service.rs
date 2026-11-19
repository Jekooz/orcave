//! Orcave Library - LibraryService: the central orchestration service.
//!
//! Manages library folders, tracks, artists, albums, artwork cache, and the
//! scanner. Persists folder list to settings. Bridges to AudioEngine for
//! playback registration. Safe incremental rescans via modify-time tracking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::artwork::cached_artwork_path;
use crate::models::{LibraryAlbum, LibraryArtist, LibraryFolder, LibraryTrack, ScanResult};
use crate::scanner::{ScanEvent, Scanner};
use crate::smb::validate_smb_share;
use crate::{FolderSource, SmbShare};
use orcave_audio::AudioEngine;
use orcave_core::TauriResult;
use orcave_settings::SettingsService;

/// Settings keys used by the library service.
const SETTINGS_FOLDERS_KEY: &str = "orcave.library.folders";

/// LibraryService state snapshot for frontend consumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryState {
    pub folders: Vec<LibraryFolder>,
    pub track_count: usize,
    pub artist_count: usize,
    pub album_count: usize,
    pub last_scan: Option<String>,
}

/// LibraryService: coordinates scanning, metadata, dedup, artwork, persistence.
pub struct LibraryService {
    settings: Arc<SettingsService>,
    audio: Arc<AudioEngine>,

    folders: Arc<DashMap<String, LibraryFolder>>,
    tracks: Arc<DashMap<String, LibraryTrack>>, // id -> track
    tracks_by_path: Arc<DashMap<String, String>>, // path key -> track id
    artists: Arc<DashMap<String, LibraryArtist>>, // name -> artist
    albums: Arc<DashMap<String, LibraryAlbum>>, // (artist|title) -> album

    scanner: Arc<Mutex<Option<Scanner>>>,
    scan_event_tx: mpsc::UnboundedSender<ScanEvent>,

    initialized: Arc<Mutex<bool>>,
}

impl LibraryService {
    /// Create a new LibraryService. Folders are restored from settings.
    pub async fn new(settings: Arc<SettingsService>, audio: Arc<AudioEngine>) -> Result<Self> {
        let (scan_event_tx, mut scan_event_rx) = mpsc::unbounded_channel::<ScanEvent>();

        let service = Self {
            settings,
            audio,
            folders: Arc::new(DashMap::new()),
            tracks: Arc::new(DashMap::new()),
            tracks_by_path: Arc::new(DashMap::new()),
            artists: Arc::new(DashMap::new()),
            albums: Arc::new(DashMap::new()),
            scanner: Arc::new(Mutex::new(None)),
            scan_event_tx,
            initialized: Arc::new(Mutex::new(false)),
        };

        // Restore folders from settings.
        service.restore_folders().await?;

        // Bridge scan events to internal handlers.
        let folders = Arc::clone(&service.folders);
        let tracks = Arc::clone(&service.tracks);
        let tracks_by_path = Arc::clone(&service.tracks_by_path);
        let artists = Arc::clone(&service.artists);
        let albums = Arc::clone(&service.albums);
        tokio::spawn(async move {
            while let Some(event) = scan_event_rx.recv().await {
                match event {
                    ScanEvent::ScanCompleted { folder_id, result } => {
                        info!(
                            folder_id,
                            tracks_added = result.tracks_added,
                            "scan completed"
                        );
                    }
                    ScanEvent::FileCreated { path } | ScanEvent::FileChanged { path } => {
                        debug!(path = %path.display(), "file change detected");
                    }
                    ScanEvent::FileDeleted { path } => {
                        debug!(path = %path.display(), "file deleted");
                        let _ = folders;
                        let _ = tracks;
                        let _ = tracks_by_path;
                        let _ = artists;
                        let _ = albums;
                    }
                    ScanEvent::ScanStarted { .. } => {}
                }
            }
        });

        Ok(service)
    }

    /// Restore persisted folders from settings.
    async fn restore_folders(&self) -> Result<()> {
        let folders: Vec<LibraryFolder> = self
            .settings
            .get(SETTINGS_FOLDERS_KEY)
            .await?
            .unwrap_or_default();
        for folder in folders {
            self.folders.insert(folder.id.clone(), folder);
        }
        Ok(())
    }

    /// Persist the folder list to settings.
    async fn persist_folders(&self) -> Result<()> {
        let list: Vec<LibraryFolder> = self.folders.iter().map(|e| e.value().clone()).collect();
        self.settings.set(SETTINGS_FOLDERS_KEY, &list).await?;
        self.settings.save().await?;
        Ok(())
    }

    /// Add a user-selected folder to the library and scan it.
    pub async fn add_folder(
        &self,
        path: &Path,
        source_type: FolderSource,
    ) -> TauriResult<LibraryFolder> {
        // Validate path exists and is readable.
        if !path.exists() {
            return Err(orcave_core::TauriError::Internal(
                "folder path does not exist".to_string(),
            ));
        }

        // SMB auth boundary: validate reachability without credentials.
        if let FolderSource::Smb { unc_path, .. } = &source_type {
            let validation = validate_smb_share(unc_path);
            if !validation.accessible {
                return Err(orcave_core::TauriError::Internal(format!(
                    "SMB share not accessible: {}",
                    validation.reason.unwrap_or_default()
                )));
            }
        }

        let id = ulid::Ulid::new().to_string();
        let folder = LibraryFolder {
            id: id.clone(),
            path: path.to_path_buf(),
            source_type,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };

        self.folders.insert(id.clone(), folder.clone());
        self.persist_folders()
            .await
            .map_err(|e| orcave_core::TauriError::Internal(e.to_string()))?;

        // Kick off a scan.
        let scanner = self.scanner.lock().await;
        let scanner = scanner.as_ref().ok_or_else(|| {
            orcave_core::TauriError::Internal("scanner not initialized".to_string())
        })?;
        let result = scanner
            .scan_folder(&folder)
            .await
            .map_err(|e| orcave_core::TauriError::Internal(e.to_string()))?;
        for error in &result.errors {
            warn!(folder_id = %folder.id, path = %error.path.display(), reason = %error.message, "library scan error");
        }
        drop(scanner);
        self.rebuild_indexes();

        Ok(folder)
    }

    /// Remove a folder from the library and drop its tracks.
    pub async fn remove_folder(&self, folder_id: &str) -> TauriResult<()> {
        let folder = self.folders.remove(folder_id).map(|(_, f)| f);
        let _folder = folder
            .ok_or_else(|| orcave_core::TauriError::Internal("folder not found".to_string()))?;

        // Remove tracks belonging to this folder.
        let track_ids: Vec<String> = self
            .tracks
            .iter()
            .filter(|e| e.value().folder_id == folder_id)
            .map(|e| e.key().clone())
            .collect();
        for id in &track_ids {
            self.tracks.remove(id);
        }

        // Rebuild path index and aggregates.
        self.rebuild_indexes();

        self.persist_folders()
            .await
            .map_err(|e| orcave_core::TauriError::Internal(e.to_string()))?;

        Ok(())
    }

    /// Rescan all folders (incremental: skips unchanged files).
    pub async fn rescan_all(&self) -> TauriResult<Vec<(String, ScanResult)>> {
        let scanner = self.scanner.lock().await;
        let scanner = scanner.as_ref().ok_or_else(|| {
            orcave_core::TauriError::Internal("scanner not initialized".to_string())
        })?;
        drop(scanner);

        let mut results = Vec::new();
        for entry in self.folders.iter() {
            let folder = entry.value().clone();
            let result = scanner
                .scan_folder(&folder)
                .await
                .map_err(|e| orcave_core::TauriError::Internal(e.to_string()))?;
            // Update last_scanned.
            if let Some(mut f) = self.folders.get_mut(&folder.id) {
                f.last_scanned = Some(chrono::Utc::now());
            }
            results.push((folder.id.clone(), result));
        }

        // Persist updated last_scanned timestamps.
        self.persist_folders()
            .await
            .map_err(|e| orcave_core::TauriError::Internal(e.to_string()))?;

        // Rebuild aggregates from current track index.
        self.rebuild_indexes();

        Ok(results)
    }

    /// Get all tracks (public, safe for IPC).
    pub fn get_tracks(&self) -> Vec<crate::commands::TrackResource> {
        self.tracks
            .iter()
            .map(|e| crate::commands::track_to_resource(e.value()))
            .collect()
    }

    /// Get all artists (aggregated from tracks).
    pub fn get_artists(&self) -> Vec<crate::commands::ArtistResource> {
        self.artists
            .iter()
            .map(|e| crate::commands::artist_to_resource(e.value()))
            .collect()
    }

    /// Get all albums (aggregated from tracks).
    pub fn get_albums(&self) -> Vec<crate::commands::AlbumResource> {
        self.albums
            .iter()
            .map(|e| crate::commands::album_to_resource(e.value()))
            .collect()
    }

    /// Get all library folders.
    pub fn get_folders(&self) -> Vec<crate::models::LibraryFolder> {
        self.folders.iter().map(|e| e.value().clone()).collect()
    }

    /// Get cached artwork path for a track id.
    pub fn get_artwork_path(&self, track_id: &str) -> Option<PathBuf> {
        cached_artwork_path(track_id)
    }

    /// Register a track's local file path as a stream URL on AudioEngine so it
    /// can be played. The path is converted to a file:// URL.
    pub fn register_track_for_playback(&self, track_id: &str) -> TauriResult<()> {
        let track = self
            .tracks
            .get(track_id)
            .ok_or_else(|| orcave_core::TauriError::Internal("track not found".to_string()))?;

        let file_url = path_to_file_url(&track.path);
        self.audio
            .register_stream_url(track_id.to_string(), file_url);
        Ok(())
    }

    /// Initialize the scanner and register it with watchers for all folders.
    pub async fn init_scanner(&self) -> Result<()> {
        let mut scanner = Scanner::new(self.scan_event_tx.clone(), Arc::clone(&self.tracks));
        let restored_folders: Vec<_> = self.folders.iter().map(|e| e.value().clone()).collect();
        for folder in &restored_folders {
            let path = match &folder.source_type {
                FolderSource::Local => folder.path.clone(),
                FolderSource::Smb { unc_path, .. } => PathBuf::from(unc_path),
            };
            if path.exists() {
                if let Err(error) = scanner.watch_folder(&path).await {
                    warn!(folder_id = %folder.id, %error, "library watcher unavailable");
                }
            }
        }
        // Tracks are memory-only; watchers do not emit events for existing files.
        for folder in &restored_folders {
            match scanner.scan_folder(folder).await {
                Ok(result) => {
                    for error in &result.errors {
                        warn!(folder_id = %folder.id, path = %error.path.display(), reason = %error.message, "restored library scan error");
                    }
                    info!(folder_id = %folder.id, tracks_added = result.tracks_added, "restored library scanned");
                }
                Err(error) => warn!(folder_id = %folder.id, %error, "restored library scan failed"),
            }
        }
        self.rebuild_indexes();
        *self.scanner.lock().await = Some(scanner);
        *self.initialized.lock().await = true;
        Ok(())
    }

    /// Rebuild artist and album aggregates from the current track index.
    fn rebuild_indexes(&self) {
        self.artists.clear();
        self.albums.clear();

        let mut artist_map: HashMap<String, LibraryArtist> = HashMap::new();
        let mut album_map: HashMap<String, LibraryAlbum> = HashMap::new();

        for entry in self.tracks.iter() {
            let track = entry.value();

            // Artist aggregate.
            artist_map
                .entry(track.artist.clone())
                .or_insert_with(|| LibraryArtist {
                    id: format!("artist-{}", blake3_short(&track.artist)),
                    name: track.artist.clone(),
                    track_ids: Vec::new(),
                })
                .track_ids
                .push(track.id.clone());

            // Album aggregate keyed by artist|album.
            let album_key = format!("{}|{}", track.artist, track.album);
            album_map
                .entry(album_key.clone())
                .or_insert_with(|| LibraryAlbum {
                    id: format!("album-{}", blake3_short(&album_key)),
                    title: track.album.clone(),
                    artist: track.artist.clone(),
                    track_ids: Vec::new(),
                    artwork_path: None,
                })
                .track_ids
                .push(track.id.clone());
        }

        for (name, artist) in artist_map {
            self.artists.insert(name, artist);
        }
        for (key, album) in album_map {
            self.albums.insert(key, album);
        }
    }

    /// Shutdown: stop scanner watchers.
    pub async fn shutdown(&self) -> Result<()> {
        *self.scanner.lock().await = None;
        Ok(())
    }

    pub fn state(&self) -> LibraryState {
        LibraryState {
            folders: self.folders.iter().map(|e| e.value().clone()).collect(),
            track_count: self.tracks.len(),
            artist_count: self.artists.len(),
            album_count: self.albums.len(),
            last_scan: self
                .folders
                .iter()
                .filter_map(|e| e.value().last_scanned.map(|d| d.to_rfc3339()))
                .max(),
        }
    }
}

/// Convert a local path to a file:// URL for AudioEngine.
fn path_to_file_url(path: &Path) -> String {
    // reqwest / symphonia accept file:///C:/path or file:///host/share.
    // On Windows: file:///C:/Users/... (forward slashes).
    let display = path.display().to_string();
    let normalized = display.replace('\\', "/");
    if normalized.starts_with('/') {
        format!("file://{}", normalized)
    } else {
        format!("file:///{}", normalized)
    }
}

/// Short blake3-based id for aggregates.
fn blake3_short(s: &str) -> String {
    use blake3::Hasher;
    let mut h = Hasher::new();
    h.update(s.as_bytes());
    h.finalize().to_hex().to_string()[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_file_url_windows_style() {
        let url = path_to_file_url(Path::new("C:\\Users\\Me\\Music\\song.mp3"));
        assert!(url.starts_with("file:///C:/Users/Me/Music/song.mp3"));
        assert!(!url.contains('\\'));
    }

    #[test]
    fn path_to_file_url_unc_style() {
        let url = path_to_file_url(Path::new("\\\\server\\share\\song.mp3"));
        assert!(url.starts_with("file:////server/share/song.mp3"));
    }

    #[test]
    fn blake3_short_is_deterministic() {
        assert_eq!(blake3_short("artist-a"), blake3_short("artist-a"));
        assert_ne!(blake3_short("artist-a"), blake3_short("artist-b"));
        assert_eq!(blake3_short("artist-a").len(), 12);
    }
}
