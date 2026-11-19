//! Orcave Library - Tauri command handlers.
//!
//! Commands:
//!   library_add_folder, library_remove_folder, library_get_tracks,
//!   library_get_artists, library_get_albums, library_get_artwork,
//!   library_rescan, library_register_track, library_state

use std::path::PathBuf;
use std::sync::Arc;

use crate::service::LibraryService;
use crate::FolderSource;
use orcave_core::TauriResult;
use serde::{Deserialize, Serialize};
use tauri::State;

/// Public track resource safe for IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackResource {
    pub id: String,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: Option<u64>,
    pub artwork_available: bool,
    pub folder_id: String,
}

/// Public artist resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistResource {
    pub id: String,
    pub name: String,
    pub track_count: usize,
}

/// Public album resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumResource {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub track_count: usize,
    pub artwork_available: bool,
}

/// Convert a LibraryTrack to a TrackResource.
pub fn track_to_resource(track: &crate::models::LibraryTrack) -> TrackResource {
    TrackResource {
        id: track.id.clone(),
        path: track.path.to_string_lossy().to_string(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration_ms: track.duration_ms,
        artwork_available: track.artwork_path.is_some(),
        folder_id: track.folder_id.clone(),
    }
}

pub fn artist_to_resource(artist: &crate::models::LibraryArtist) -> ArtistResource {
    ArtistResource {
        id: artist.id.clone(),
        name: artist.name.clone(),
        track_count: artist.track_ids.len(),
    }
}

pub fn album_to_resource(album: &crate::models::LibraryAlbum) -> AlbumResource {
    AlbumResource {
        id: album.id.clone(),
        title: album.title.clone(),
        artist: album.artist.clone(),
        track_count: album.track_ids.len(),
        artwork_available: album.artwork_path.is_some(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddFolderRequest {
    pub path: String,
    pub source: String,
    pub unc_path: Option<String>,
    pub credential_target: Option<String>,
}

#[tauri::command]
pub async fn library_add_folder(
    library: State<'_, Arc<LibraryService>>,
    path: String,
) -> TauriResult<crate::models::LibraryFolder> {
    let path = PathBuf::from(&path);
    library.add_folder(&path, FolderSource::Local).await
}

#[tauri::command]
pub async fn library_remove_folder(
    library: State<'_, Arc<LibraryService>>,
    folder_id: String,
) -> TauriResult<()> {
    library.remove_folder(&folder_id).await
}

#[tauri::command]
pub async fn library_get_tracks(
    library: State<'_, Arc<LibraryService>>,
) -> TauriResult<Vec<TrackResource>> {
    Ok(library.get_tracks())
}

#[tauri::command]
pub async fn library_get_artists(
    library: State<'_, Arc<LibraryService>>,
) -> TauriResult<Vec<ArtistResource>> {
    Ok(library.get_artists())
}

#[tauri::command]
pub async fn library_get_albums(
    library: State<'_, Arc<LibraryService>>,
) -> TauriResult<Vec<AlbumResource>> {
    Ok(library.get_albums())
}

#[tauri::command]
pub async fn library_get_folders(
    library: State<'_, Arc<LibraryService>>,
) -> TauriResult<Vec<crate::models::LibraryFolder>> {
    Ok(library.get_folders())
}

#[tauri::command]
pub async fn library_get_artwork(
    library: State<'_, Arc<LibraryService>>,
    track_id: String,
) -> TauriResult<Option<String>> {
    Ok(library
        .get_artwork_path(&track_id)
        .map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
pub async fn library_rescan(
    library: State<'_, Arc<LibraryService>>,
) -> TauriResult<Vec<(String, String)>> {
    let results: Vec<(String, String)> = library
        .rescan_all()
        .await?
        .into_iter()
        .map(|(folder_id, result)| {
            (
                folder_id,
                format!(
                    "added={} updated={} dup={} err={}",
                    result.tracks_added,
                    result.tracks_updated,
                    result.duplicates_skipped,
                    result.errors.len()
                ),
            )
        })
        .collect();
    Ok(results)
}

#[tauri::command]
pub async fn library_register_track(
    library: State<'_, Arc<LibraryService>>,
    track_id: String,
) -> TauriResult<()> {
    library.register_track_for_playback(&track_id)
}

#[tauri::command]
pub async fn library_state(
    library: State<'_, Arc<LibraryService>>,
) -> TauriResult<crate::service::LibraryState> {
    Ok(library.state())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Tauri 2 deserializes each command parameter from the top-level JSON key
    /// matching the parameter name. With `library_add_folder(library, path: String)`
    /// the expected args shape is flat `{"path": "/music/folder"}`, NOT
    /// `{"req": { ... }}`. This test locks in the fixed contract: the command's
    /// `path: String` parameter is populated from the top-level `"path"` key.
    #[test]
    fn library_add_folder_deserializes_flat_path() {
        let args = json!({"path": "/music/folder"});
        let path: String = serde_json::from_value(args["path"].clone())
            .expect("'path' must deserialize as String");
        assert_eq!(path, "/music/folder");
    }

    /// The legacy nested shape `{"req": {"path": ...}}` must no longer be
    /// consumed: there is no top-level `"path"` key, so a flat `path: String`
    /// parameter cannot be populated from it (the original `missing required key`
    /// class of error, now inverted).
    #[test]
    fn library_add_folder_ignores_nested_req_shape() {
        let args = json!({"req": {"path": "/music/folder"}});
        let result: serde_json::Result<String> =
            serde_json::from_value(args.get("path").cloned().unwrap_or(serde_json::Value::Null));
        assert!(
            result.is_err(),
            "flat 'path' param must not read nested 'req'; got {:?}",
            result
        );
    }
}
