//! Orcave Library - Local music library service.
//!
//! Provides:
//! - User-selected music folders (local + SMB via UNC)
//! - Metadata extraction via symphonia (title, artist, album, duration, artwork)
//! - Track, artist, album identities
//! - Artwork extraction and on-disk caching
//! - Library persistence via settings store
//! - File disappearance/recovery via notify watcher + incremental rescan
//! - SMB network shares with authentication boundaries (no plaintext creds in IPC)
//! - Duplicate detection by blake3 content hash
//! - Safe incremental rescans
//!
//! Tauri commands: library_add_folder, library_get_tracks, library_get_artists,
//! library_get_albums, library_get_artwork, library_rescan, library_remove_folder,
//! library_register_track, library_state

pub mod artwork;
pub mod commands;
pub mod dedup;
pub mod metadata;
pub mod models;
pub mod scanner;
pub mod service;
pub mod smb;

pub use artwork::cache_artwork;
pub use commands::{track_to_resource, AlbumResource, ArtistResource, TrackResource};
pub use dedup::{hash_bytes, hash_file, DedupIndex};
pub use metadata::extract_metadata;
pub use models::FolderSource;
pub use models::{
    LibraryAlbum, LibraryArtist, LibraryFolder, LibraryTrack, ScanError, ScanResult, TrackMetadata,
};
pub use service::LibraryService;
pub use smb::{validate_smb_share, SmbShare};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_exports_are_public() {
        // Compile-time check that key types are exported.
        let _folder = LibraryFolder {
            id: "f".to_string(),
            path: std::path::PathBuf::from("/music"),
            source_type: FolderSource::Local,
            added_at: chrono::Utc::now(),
            last_scanned: None,
        };
        let _ = _folder;
    }
}
