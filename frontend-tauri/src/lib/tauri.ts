// Tauri API type definitions and wrappers

// Auth types
//
// Legacy OAuth states remain in the shared store. They are not evidence of a
// usable browser session and must never unlock the connected UI.
export interface AuthState {
  type:
    | 'unauthenticated'      // YouTube Music browser session not connected
    | 'connecting'           // Connecting to existing browser session
    | 'companion_required'   // Browser companion permission required
    | 'validating'           // Validating session
    | 'authenticated'        // Legacy OAuth state; not browser-session proof
    | 'session_expired'      // Session expired
    | 'browser_unsupported'  // Browser unsupported
    | 'provider_unavailable' // Provider access unavailable
    | 'error';
}

// Legacy OAuth account metadata; never use this as browser-session proof.
export interface UserInfo {
  id: string;
  email: string;
  name: string;
  picture?: string;
  verified_email: boolean;
}

// `native_session_get_status` reports local native session material, not
// verified YouTube Music access. Never treat an OAuth state, browser-opened
// event, or auth_connect_browser_session response as provider validation.
export interface NativeSessionStatus {
  state: 'disconnected' | 'connecting' | 'connected' | 'unverified' | 'expired' | 'error';
  connected: boolean;
  expired: boolean;
  ttl_secs: number;
  handle: string | null;
}

// Browser-based authentication (Python sidecar)
export interface BrowserAuthStatus {
  state: 'idle' | 'waiting' | 'success' | 'failed' | 'cancelled' | 'error';
  browser: string;
  error?: string;
}

export interface BrowserInfo {
  browser: string;
}

// Catalogue types
export interface MediaItem {
  id: string;
  browse_id?: string;
  media_type: 'track' | 'album' | 'artist' | 'playlist' | 'station' | 'video' | 'podcast' | 'podcast_episode';
  title: string;
  subtitle?: string;
  description?: string;
  thumbnails: Thumbnail[];
  duration?: number; // milliseconds
  duration_text?: string;
  artists: ArtistRef[];
  album?: AlbumRef;
  playlist?: PlaylistRef;
  year?: number;
  view_count?: number;
  is_explicit: boolean;
  is_liked: boolean;
  is_in_library: boolean;
  playback_url?: string;
  metadata: Record<string, unknown>;
}

export interface Thumbnail {
  url: string;
  width: number;
  height: number;
}

export interface ArtistRef {
  id: string;
  name: string;
  browse_id?: string;
}

export interface AlbumRef {
  id: string;
  name: string;
  browse_id?: string;
}

export interface PlaylistRef {
  id: string;
  name: string;
  browse_id?: string;
}

export interface BrowseResponse {
  node_id: string;
  title: string;
  sections: BrowseSection[];
  continuation?: string;
  metadata: Record<string, unknown>;
}

export interface BrowseSection {
  title?: string;
  items: MediaItem[];
  continuation?: string;
  section_type: string;
}

export interface SearchResponse {
  query: string;
  results: MediaItem[];
  filters: SearchFilter[];
  continuation?: string;
}

export interface SearchFilter {
  key: string;
  label: string;
  selected: boolean;
}

// Local library types
// Backed by Agent B Tauri commands: library_add_folder, library_get_tracks,
// library_get_artists, library_get_albums, library_rescan.
// Shapes mirror crates/orcave-library/src/models.rs (serialized over IPC).
export interface LibraryTrack {
  id: string;
  path: string;
  content_hash: number[] | null;
  title: string;
  artist: string;
  album: string;
  duration: number | null; // milliseconds
  artwork_path: string | null;
  added_at: string;
  modified_at: string | null;
  size: number;
  folder_id: string;
}

export interface LibraryArtist {
  id: string;
  name: string;
  track_ids: string[];
}

export interface LibraryAlbum {
  id: string;
  title: string;
  artist: string;
  track_ids: string[];
  artwork_path: string | null;
}

export type LibraryFolderSource =
  | { kind: 'local' }
  | { kind: 'smb'; unc_path: string; credential_target: string | null };

export interface LibraryFolder {
  id: string;
  path: string;
  source_type: LibraryFolderSource;
  added_at: string;
  last_scanned: string | null;
}

export interface LibraryScanResult {
  tracks_added: number;
  tracks_updated: number;
  duplicates_skipped: number;
  errors: Array<{ path: string; message: string }>;
}

// Audio output device types
// Backed by Agent C device selection commands (not yet registered on the
// Rust side). Frontend wiring below calls them and degrades gracefully until
// they exist.
export interface AudioOutputDevice {
  id: string;
  name: string;
  is_default: boolean;
  is_active: boolean;
}

// Audio types
export interface TrackHandle {
  id: string;
  title: string;
  artist: string;
  stream_url?: string;
  duration?: number;
}

export type PlaybackState = 'stopped' | 'playing' | 'paused' | 'buffering' | 'ended' | 'error';

export interface AudioEvent {
  type: 'state_changed' | 'track_changed' | 'position_changed' | 'volume_changed' | 'buffering_update' | 'error' | 'track_ended' | 'crossfade_started' | 'crossfade_ended';
  data: unknown;
}

// Queue types
export interface QueueItem {
  id: string;
  queue_id: string;
  media_item: MediaItem;
  track_handle: TrackHandle;
  added_at: string;
  source: 'user_added' | 'radio' | 'playlist' | 'album' | 'artist' | 'recommendation' | 'history';
}

export type QueueMode = 'normal' | 'radio' | 'playlist' | 'album' | 'artist';
export type RepeatMode = 'off' | 'all' | 'one';
export type SeedType = 'track' | 'album' | 'artist' | 'playlist' | 'genre';

export interface QueueEvent {
  type: 'item_added' | 'item_removed' | 'items_reordered' | 'cleared' | 'current_index_changed' | 'mode_changed' | 'repeat_mode_changed' | 'shuffle_changed' | 'history_added';
  data: unknown;
}

// Recommendations types
export interface RecommendationSet {
  id: string;
  title: string;
  subtitle?: string;
  seed_type?: SeedType;
  seed_id?: string;
  items: MediaItem[];
  generated_at: string;
  expires_at: string;
  algorithm: 'collaborative_filtering' | 'content_based' | 'hybrid' | 'seed_based' | 'personalized_mix' | 'mood_station';
}

export type MixType = 'your_mix' | 'discover_mix' | 'new_release_mix' | 'liked_songs_mix' | 'supermix';
export type StationType = 'focus' | 'workout' | 'relax' | 'party' | 'commute' | 'sleep' | { genre: string };

// Settings types
export interface Settings {
  version: number;
  auth_auto_sign_in: boolean;
  auth_remember_credentials: boolean;
  audio_quality: 'low' | 'normal' | 'high' | 'lossless';
  audio_normalize_volume: boolean;
  audio_crossfade: { enabled: boolean; duration_ms: number };
  audio_gapless: { enabled: boolean; preload_next: boolean };
  audio_output_device?: string;
  playback_repeat_mode: RepeatMode;
  playback_shuffle: boolean;
  theme: 'system' | 'light' | 'dark';
  ui_compact_mode: boolean;
  ui_animations: boolean;
  ui_language: string;
  network_proxy?: string;
  network_timeout_secs: number;
  cache_max_size_mb: number;
  cache_artwork: boolean;
  cache_audio_preload: boolean;
  diagnostics_enabled: boolean;
  diagnostics_log_level: string;
  diagnostics_crash_reporting: boolean;
  advanced_experimental_features: boolean;
  advanced_developer_mode: boolean;
}

// Diagnostics types
export interface DiagnosticEvent {
  type: string;
  data: Record<string, unknown>;
  timestamp: string;
}

// Tauri command wrappers
export async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke: tauriInvoke } = await import('@tauri-apps/api/core');
  return tauriInvoke<T>(command, args);
}

// Event listener
export async function listen<T>(event: string, handler: (event: { payload: T }) => void): Promise<() => void> {
  const { listen: tauriListen } = await import('@tauri-apps/api/event');
  return tauriListen(event, handler);
}
