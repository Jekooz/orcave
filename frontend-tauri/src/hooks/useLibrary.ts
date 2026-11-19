import { useCallback, useState, useEffect } from 'react'
import { invoke } from '@/lib/tauri'
import type { LibraryTrack, LibraryArtist, LibraryAlbum, LibraryFolder, LibraryScanResult, TrackHandle } from '@/lib/tauri'

// ---------------------------------------------------------------------------
// Temporary sanitized stage diagnostics (playback pipeline observation).
// Logging only. No raw media, file paths, stream URLs, tokens, credentials,
// or content hashes are logged. Track identifiers are shortened where shown.
// ---------------------------------------------------------------------------
function diagNow(): number {
  return typeof performance !== 'undefined' && typeof performance.now === 'function'
    ? performance.now()
    : Date.now()
}

function shortId(id: string): string {
  return id.length > 8 ? `...${id.slice(-8)}` : id
}

export function useLibrary() {
  const [tracks, setTracks] = useState<LibraryTrack[]>([])
  const [artists, setArtists] = useState<LibraryArtist[]>([])
  const [albums, setAlbums] = useState<LibraryAlbum[]>([])
  const [folders, setFolders] = useState<LibraryFolder[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const loadAll = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const [trackData, artistData, albumData, folderData] = await Promise.all([
        invoke<LibraryTrack[]>('library_get_tracks'),
        invoke<LibraryArtist[]>('library_get_artists'),
        invoke<LibraryAlbum[]>('library_get_albums'),
        invoke<LibraryFolder[]>('library_get_folders'),
      ])
      setTracks(trackData)
      setArtists(artistData)
      setAlbums(albumData)
      setFolders(folderData)
    } catch (reason) {
      const message = String(reason)
      console.error('Load library failed:', reason)
      setError(message)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void loadAll()
  }, [loadAll])

  const refresh = useCallback(async () => {
    await loadAll()
  }, [loadAll])

  const addFolder = useCallback(async (folderPath: string): Promise<LibraryFolder> => {
    setError(null)
    try {
      const folder = await invoke<LibraryFolder>('library_add_folder', {
        path: folderPath,
      })
      await loadAll()
      return folder
    } catch (reason) {
      const message = String(reason)
      console.error('Add folder failed:', reason)
      setError(message)
      throw reason
    }
  }, [loadAll])

  const rescan = useCallback(async (): Promise<LibraryScanResult> => {
    setError(null)
    try {
      const result = await invoke<LibraryScanResult>('library_rescan')
      await loadAll()
      return result
    } catch (reason) {
      const message = String(reason)
      console.error('Rescan failed:', reason)
      setError(message)
      throw reason
    }
  }, [loadAll])

  const playTrack = useCallback(async (track: LibraryTrack): Promise<unknown> => {
    setError(null)
    const handle: TrackHandle = {
      id: track.id,
      title: track.title,
      artist: track.artist,
      duration: track.duration ?? undefined,
    }
    const started = diagNow()
    console.info(
      `[audio-diag] stage=library_play_track_invoke track_id=${shortId(
        track.id
      )} event=start`
    )
    // Register the local file:// stream URL on the Rust AudioEngine before
    // playback. Without this, stream_urls[id] is empty and play() returns
    // TrackNotFound before any decode happens (silent playback).
    // Register-track failures are surfaced to the UI (setError) instead of
    // being swallowed.
    try {
      await invoke('library_register_track', { trackId: track.id })
    } catch (reason) {
      const elapsed = diagNow() - started
      const message = String(reason)
      console.error(
        `[audio-diag] stage=library_register_track track_id=${shortId(
          track.id
        )} event=end success=false elapsed_ms=${elapsed.toFixed(1)} error=${message}`
      )
      setError(message)
      throw reason
    }
    return invoke('audio_play', { trackHandle: handle }).then(
      (result) => {
        const elapsed = diagNow() - started
        console.info(
          `[audio-diag] stage=library_play_track_invoke track_id=${shortId(
            track.id
          )} event=end success=true elapsed_ms=${elapsed.toFixed(1)}`
        )
        return result
      },
      (error) => {
        const elapsed = diagNow() - started
        const message = String(error)
        console.error(
          `[audio-diag] stage=library_play_track_invoke track_id=${shortId(
            track.id
          )} event=end success=false elapsed_ms=${elapsed.toFixed(1)} error=${message}`
        )
        setError(message)
        throw error
      }
    )
  }, [setError])

  return {
    tracks,
    artists,
    albums,
    folders,
    loading,
    error,
    refresh,
    addFolder,
    rescan,
    playTrack,
  }
}
