//! Orcave Library - Data models for local music library.
//!
//! Defines Track, Artist, Album, Folder identities and related value types.
//! No filesystem or network access here: pure data + validation.

use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// A single audio track in the local library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTrack {
    /// Stable track id (ULID). Not the file path; path can change, id persists.
    pub id: String,
    /// Absolute file path (local or UNC). Read-only: never written during scan.
    pub path: PathBuf,
    /// Content hash (blake3) for duplicate detection. None until computed.
    pub content_hash: Option<[u8; 32]>,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Path to cached extracted artwork (inside app cache dir), if any.
    pub artwork_path: Option<PathBuf>,
    pub added_at: DateTime<Utc>,
    /// Last modification time observed at scan time.
    pub modified_at: Option<SystemTime>,
    pub size: u64,
    /// Source folder id this track was discovered under.
    pub folder_id: String,
}

impl LibraryTrack {
    pub fn new(
        id: String,
        path: PathBuf,
        title: String,
        artist: String,
        album: String,
        duration_ms: Option<u64>,
        folder_id: String,
    ) -> Self {
        Self {
            id,
            path,
            content_hash: None,
            title,
            artist,
            album,
            duration_ms,
            artwork_path: None,
            added_at: Utc::now(),
            modified_at: None,
            size: 0,
            folder_id,
        }
    }

    /// Duration as `std::time::Duration`, derived from `duration_ms`.
    pub fn duration(&self) -> Option<std::time::Duration> {
        self.duration_ms.map(std::time::Duration::from_millis)
    }
}

/// An artist identity aggregated from tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryArtist {
    pub id: String,
    pub name: String,
    pub track_ids: Vec<String>,
}

/// An album identity aggregated from tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub track_ids: Vec<String>,
    /// Path to cached artwork for the album (representative track's art).
    pub artwork_path: Option<PathBuf>,
}

/// A user-selected library folder (music root).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryFolder {
    pub id: String,
    pub path: PathBuf,
    pub source_type: FolderSource,
    pub added_at: DateTime<Utc>,
    pub last_scanned: Option<DateTime<Utc>>,
}

/// Source kind for a library folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FolderSource {
    Local,
    /// SMB share accessed via UNC path. `credential_target` names a Windows
    /// Credential Manager entry holding the password; the password itself is
    /// never stored in settings or crossed the IPC boundary.
    Smb {
        unc_path: String,
        credential_target: Option<String>,
    },
}

/// Extracted metadata from a single audio file. Pure result of symphonia parse.
#[derive(Debug, Clone)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<std::time::Duration>,
    /// Raw embedded artwork bytes (e.g. ID3 APIC / FLAC picture), if present.
    pub artwork_bytes: Option<Vec<u8>>,
}

/// Result of scanning one folder: tracks discovered and duplicates skipped.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub tracks_added: usize,
    pub tracks_updated: usize,
    pub duplicates_skipped: usize,
    pub errors: Vec<ScanError>,
}

/// A non-fatal scan error for one file.
#[derive(Debug, Clone)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}
