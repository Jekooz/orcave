//! Orcave Catalogue - YouTube Music browsing, search, and metadata
//!
//! Provides:
//! - InnerTube API client for YouTube Music
//! - Browse nodes (home, library, explore, search)
//! - Media item types (tracks, albums, artists, playlists)
//! - Caching and offline support
//! - Continuation-based pagination

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use dashmap::DashMap;
use orcave_auth::{AuthService, AuthenticatedClient};
use orcave_core::{DiagnosticEvent, SeedType, TauriResult};
use orcave_settings::{AudioQuality, SettingsService};
use reqwest::{header::RETRY_AFTER, Client, StatusCode};
use serde::{Deserialize, Serialize, Serializer};
use tokio::sync::{broadcast, Mutex};
use tracing::instrument;

#[derive(thiserror::Error, Debug)]
pub enum CatalogueError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Authentication required")]
    AuthRequired,
    #[error("API error: {0}")]
    Api(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("Invalid continuation")]
    InvalidContinuation,
    #[error("Settings error: {0}")]
    Settings(#[from] orcave_settings::SettingsError),
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for CatalogueError {
    fn from(e: anyhow::Error) -> Self {
        CatalogueError::Internal(e.to_string())
    }
}

/// Typed failure taxonomy for stream URL resolution and playback coordination.
///
/// Every variant maps to a deterministic recovery action or a frontend-visible
/// error code. No variant exposes secrets, tokens, or raw URLs.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum CatalogueStreamError {
    #[error("Expired or invalid streaming descriptor")]
    Expired,
    #[error("HTTP 403: access forbidden")]
    Forbidden,
    #[error("HTTP 429: rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("HTTP 401: authentication failed")]
    AuthFailure,
    #[error("DNS/connectivity failure: {0}")]
    Connectivity(String),
    #[error("Network timeout")]
    Timeout,
    #[error("Unsupported audio codec")]
    UnsupportedCodec,
    #[error("Corrupt or incomplete response")]
    CorruptResponse,
    #[error("Cancelled operation")]
    Cancelled,
    #[error("Stale account or queue generation")]
    StaleAccount,
}

impl CatalogueStreamError {
    /// Whether this error should trigger a retry attempt.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            CatalogueStreamError::RateLimited { .. }
                | CatalogueStreamError::Timeout
                | CatalogueStreamError::Connectivity(_)
        )
    }

    /// Suggested delay before the next retry, derived from `Retry-After` or
    /// exponential backoff. Returns `None` for non-retryable errors.
    pub fn retry_delay(&self, attempt: u32) -> Option<Duration> {
        if !self.is_retryable() {
            return None;
        }
        match self {
            CatalogueStreamError::RateLimited { retry_after } => *retry_after,
            _ => {
                // Exponential backoff with jitter: base 500ms * 2^attempt
                let base = Duration::from_millis(500);
                let backoff = base.saturating_mul(2u32.saturating_pow(attempt));
                // Jitter: random 0-25% of backoff
                let jitter_ms =
                    (rand::random::<u32>() % (backoff.as_millis() as u32 / 4)).min(1000);
                Some(backoff + Duration::from_millis(jitter_ms as u64))
            }
        }
    }

    /// Map to a frontend-visible error code (redacted, no secrets).
    pub fn frontend_code(&self) -> StreamErrorCode {
        match self {
            CatalogueStreamError::Forbidden
            | CatalogueStreamError::RateLimited { .. }
            | CatalogueStreamError::AuthFailure => StreamErrorCode::ProviderError,
            CatalogueStreamError::Timeout | CatalogueStreamError::Connectivity(_) => {
                StreamErrorCode::NetworkError
            }
            CatalogueStreamError::UnsupportedCodec | CatalogueStreamError::CorruptResponse => {
                StreamErrorCode::DecoderError
            }
            CatalogueStreamError::Expired | CatalogueStreamError::StaleAccount => {
                StreamErrorCode::AccountError
            }
            CatalogueStreamError::Cancelled => StreamErrorCode::AccountError,
        }
    }

    /// Redacted user-facing message, safe to display.
    pub fn redacted_message(&self) -> String {
        match self {
            CatalogueStreamError::Expired => "Streaming link expired. Retrying...".to_string(),
            CatalogueStreamError::Forbidden => "Access to this track is forbidden.".to_string(),
            CatalogueStreamError::RateLimited { retry_after } => match retry_after {
                Some(d) => format!("Rate limited. Try again in {} seconds.", d.as_secs()),
                None => "Rate limited. Retrying with backoff.".to_string(),
            },
            CatalogueStreamError::AuthFailure => {
                "Authentication failed. Please sign in again.".to_string()
            }
            CatalogueStreamError::Connectivity(_) => {
                "Network connection lost. Retrying...".to_string()
            }
            CatalogueStreamError::Timeout => "Network request timed out. Retrying...".to_string(),
            CatalogueStreamError::UnsupportedCodec => "Audio codec not supported.".to_string(),
            CatalogueStreamError::CorruptResponse => {
                "Received corrupt response from server.".to_string()
            }
            CatalogueStreamError::Cancelled => "Playback cancelled.".to_string(),
            CatalogueStreamError::StaleAccount => {
                "Account session is stale. Please refresh.".to_string()
            }
        }
    }
}

/// Frontend-visible stream error code (redacted, no secrets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamErrorCode {
    ProviderError,
    NetworkError,
    DecoderError,
    AccountError,
}

/// Redacted stream error info for frontend consumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StreamErrorInfo {
    pub error_code: StreamErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl From<&CatalogueStreamError> for StreamErrorInfo {
    fn from(e: &CatalogueStreamError) -> Self {
        StreamErrorInfo {
            error_code: e.frontend_code(),
            message: e.redacted_message(),
            retryable: e.is_retryable(),
        }
    }
}

/// Media item types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Track,
    Album,
    Artist,
    Playlist,
    Station,
    Video,
    Podcast,
    PodcastEpisode,
}

/// Browse node identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrowseId(pub String);

impl BrowseId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn home() -> Self {
        Self("FEmusic_home".to_string())
    }

    pub fn library() -> Self {
        Self("FEmusic_library".to_string())
    }

    pub fn explore() -> Self {
        Self("FEmusic_explore".to_string())
    }

    pub fn liked_music() -> Self {
        Self("FEmusic_liked_music".to_string())
    }

    pub fn history() -> Self {
        Self("FEmusic_history".to_string())
    }

    pub fn downloads() -> Self {
        Self("FEmusic_downloads".to_string())
    }
}

/// Media item from YouTube Music
#[derive(Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: String,
    pub browse_id: Option<BrowseId>,
    pub media_type: MediaType,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub thumbnails: Vec<Thumbnail>,
    pub duration: Option<Duration>,
    pub duration_text: Option<String>,
    pub artists: Vec<ArtistRef>,
    pub album: Option<AlbumRef>,
    pub playlist: Option<PlaylistRef>,
    pub year: Option<i32>,
    pub view_count: Option<u64>,
    pub is_explicit: bool,
    pub is_liked: bool,
    pub is_in_library: bool,
    #[serde(skip_serializing)]
    pub playback_url: Option<String>,
    #[serde(serialize_with = "serialize_public_metadata")]
    pub metadata: serde_json::Value,
}

impl std::fmt::Debug for MediaItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaItem")
            .field("id", &self.id)
            .field("browse_id", &self.browse_id)
            .field("media_type", &self.media_type)
            .field("title", &self.title)
            .field("subtitle", &self.subtitle)
            .field("description", &self.description)
            .field("thumbnails", &self.thumbnails)
            .field("duration", &self.duration)
            .field("duration_text", &self.duration_text)
            .field("artists", &self.artists)
            .field("album", &self.album)
            .field("playlist", &self.playlist)
            .field("year", &self.year)
            .field("view_count", &self.view_count)
            .field("is_explicit", &self.is_explicit)
            .field("is_liked", &self.is_liked)
            .field("is_in_library", &self.is_in_library)
            .field("playback_url", &"[REDACTED]")
            .field("metadata", &"[REDACTED]")
            .finish()
    }
}

// Raw provider JSON can contain signed URLs and tokens. Keep it native-side only.
fn serialize_public_metadata<S: Serializer>(
    _: &serde_json::Value,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde_json::Map::<String, serde_json::Value>::new().serialize(serializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
    pub browse_id: Option<BrowseId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumRef {
    pub id: String,
    pub name: String,
    pub browse_id: Option<BrowseId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistRef {
    pub id: String,
    pub name: String,
    pub browse_id: Option<BrowseId>,
}

/// Browse response with continuation
#[derive(Clone, Serialize, Deserialize)]
pub struct BrowseResponse {
    pub node_id: BrowseId,
    pub title: String,
    pub sections: Vec<BrowseSection>,
    pub continuation: Option<String>,
    #[serde(serialize_with = "serialize_public_metadata")]
    pub metadata: serde_json::Value,
}

impl std::fmt::Debug for BrowseResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseResponse")
            .field("node_id", &self.node_id)
            .field("title", &self.title)
            .field("sections", &self.sections)
            .field("continuation", &self.continuation)
            .field("metadata", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseSection {
    pub title: Option<String>,
    pub items: Vec<MediaItem>,
    pub continuation: Option<String>,
    pub section_type: String,
}

/// Search response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<MediaItem>,
    pub filters: Vec<SearchFilter>,
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchFilter {
    pub key: String,
    pub label: String,
    pub selected: bool,
}

/// Catalogue service
pub struct CatalogueService {
    auth: Arc<AuthService>,
    settings: Arc<SettingsService>,

    // InnerTube client
    client: Client,
    api_base: String,
    context: InnerTubeContext,

    // Caches
    browse_cache: DashMap<String, CachedBrowse>,
    search_cache: DashMap<String, CachedSearch>,
    item_cache: DashMap<String, MediaItem>,

    // Pending requests deduplication
    pending_browse: DashMap<String, Arc<Mutex<()>>>,
    pending_search: DashMap<String, Arc<Mutex<()>>>,

    // Event bus
    event_tx: broadcast::Sender<CatalogueEvent>,

    // Cancellation flag for in-flight stream resolution
    cancelled: Arc<AtomicBool>,
}

/// Internal catalogue events
#[derive(Debug, Clone)]
pub enum CatalogueEvent {
    BrowseCompleted {
        node_id: BrowseId,
        item_count: usize,
        cached: bool,
    },
    SearchCompleted {
        query: String,
        result_count: usize,
        cached: bool,
    },
    ItemResolved {
        item_id: String,
    },
    CacheEvicted {
        count: usize,
    },
    /// Emitted when stream resolution fails after all recovery attempts.
    /// Frontend can display `error_code` and `message` to the user.
    StreamError {
        video_id: String,
        error_code: StreamErrorCode,
        redacted_message: String,
        retryable: bool,
    },
    Diagnostic(DiagnosticEvent),
}

/// Cached browse response
#[derive(Debug, Clone)]
struct CachedBrowse {
    response: BrowseResponse,
    expires_at: u64,
}

/// Cached search response
#[derive(Debug, Clone)]
struct CachedSearch {
    response: SearchResponse,
    expires_at: u64,
}

/// InnerTube API context
#[derive(Debug, Clone, Serialize)]
struct InnerTubeContext {
    client: ClientContext,
    user: UserContext,
    request: RequestContext,
}

#[derive(Debug, Clone, Serialize)]
struct ClientContext {
    client_name: String,
    client_version: String,
    os_name: String,
    os_version: String,
    platform: String,
    user_agent: String,
}

#[derive(Debug, Clone, Serialize)]
struct UserContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RequestContext {
    use_sse: bool,
}

impl CatalogueService {
    /// Create a new catalogue service
    #[instrument(skip(auth, settings))]
    pub async fn new(auth: Arc<AuthService>, settings: Arc<SettingsService>) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .timeout(Duration::from_secs(30))
            .build()?;

        let context = InnerTubeContext {
            client: ClientContext {
                client_name: "WEB_REMIX".to_string(),
                client_version: "1.20241113.01.00".to_string(),
                os_name: "Windows".to_string(),
                os_version: "10.0".to_string(),
                platform: "DESKTOP".to_string(),
                user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
                    .to_string(),
            },
            user: UserContext { access_token: None },
            request: RequestContext { use_sse: false },
        };

        let (event_tx, _) = broadcast::channel(128);

        Ok(Self {
            auth,
            settings,
            client,
            api_base: "https://music.youtube.com/youtubei/v1".to_string(),
            context,
            browse_cache: DashMap::new(),
            search_cache: DashMap::new(),
            item_cache: DashMap::new(),
            pending_browse: DashMap::new(),
            pending_search: DashMap::new(),
            event_tx,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Get authenticated InnerTube client
    async fn get_client(&self) -> Result<AuthenticatedClient> {
        self.auth
            .authenticated_client()
            .ok_or(CatalogueError::AuthRequired.into())
    }

    /// Update context with current access token
    // ===== Browse API =====

    /// Browse a node (home, library, album, artist, playlist, etc.)
    #[instrument(skip(self))]
    pub async fn browse(&self, browse_id: BrowseId) -> Result<BrowseResponse> {
        let cache_key = browse_id.0.clone();
        let start = std::time::Instant::now();

        // Check cache
        if let Some(cached) = self.browse_cache.get(&cache_key) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                self.emit(CatalogueEvent::BrowseCompleted {
                    node_id: browse_id.clone(),
                    item_count: cached.response.sections.iter().map(|s| s.items.len()).sum(),
                    cached: true,
                });
                return Ok(cached.response.clone());
            }
        }

        // Deduplicate concurrent requests
        let mutex = self
            .pending_browse
            .entry(cache_key.clone())
            .or_default()
            .clone();
        let _guard = mutex.lock().await;

        // Double-check cache after acquiring lock
        if let Some(cached) = self.browse_cache.get(&cache_key) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                self.emit(CatalogueEvent::BrowseCompleted {
                    node_id: browse_id.clone(),
                    item_count: cached.response.sections.iter().map(|s| s.items.len()).sum(),
                    cached: true,
                });
                return Ok(cached.response.clone());
            }
        }

        let auth_client = self.get_client().await?;
        let url = format!("{}/browse", self.api_base);

        let request = BrowseRequest {
            context: self.context.clone(),
            browse_id: browse_id.0.clone(),
        };

        let response: serde_json::Value = auth_client.post_json(&url, &request).await?;
        let browse_response: BrowseResponse = self.parse_browse_response(response).await?;

        // Cache for 5 minutes
        let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 300;
        self.browse_cache.insert(
            cache_key,
            CachedBrowse {
                response: browse_response.clone(),
                expires_at,
            },
        );

        let latency = start.elapsed().as_millis() as u64;
        self.emit(CatalogueEvent::Diagnostic(
            DiagnosticEvent::CatalogueRequest {
                endpoint: "browse".to_string(),
                latency_ms: latency,
                cached: false,
            },
        ));

        self.emit(CatalogueEvent::BrowseCompleted {
            node_id: browse_id,
            item_count: browse_response.sections.iter().map(|s| s.items.len()).sum(),
            cached: false,
        });

        Ok(browse_response)
    }

    /// Browse with continuation
    #[instrument(skip(self))]
    pub async fn browse_continuation(&self, continuation: String) -> Result<BrowseResponse> {
        let auth_client = self.get_client().await?;
        let url = format!("{}/browse", self.api_base);

        let request = ContinuationRequest {
            context: self.context.clone(),
            continuation,
        };

        let response: serde_json::Value = auth_client.post_json(&url, &request).await?;
        Ok(self.parse_browse_response(response).await?)
    }

    /// Parse browse response from InnerTube
    async fn parse_browse_response(&self, value: serde_json::Value) -> Result<BrowseResponse> {
        // This is a simplified parser - real implementation would handle
        // the complex InnerTube response structure
        let contents = value
            .get("contents")
            .or_else(|| value.get("onResponseReceivedActions"))
            .context("No contents in response")?;

        // Extract sections from various possible locations
        let sections = self.extract_sections(contents).await?;

        Ok(BrowseResponse {
            node_id: BrowseId("unknown".to_string()),
            title: "Browse".to_string(),
            sections,
            continuation: self.extract_continuation(&value),
            metadata: value,
        })
    }

    async fn extract_sections(&self, value: &serde_json::Value) -> Result<Vec<BrowseSection>> {
        let mut sections = Vec::new();

        // Try various paths in the response
        if let Some(tabs) = value.get("tabs").and_then(|v| v.as_array()) {
            for tab in tabs {
                if let Some(tab_content) = tab.get("tabRenderer").and_then(|r| r.get("content")) {
                    sections.extend(Box::pin(self.extract_sections(tab_content)).await?);
                }
            }
        }

        if let Some(sections_json) = value.get("sections").and_then(|v| v.as_array()) {
            for section in sections_json {
                if let Some(items) = self.extract_items(section).await? {
                    sections.push(BrowseSection {
                        title: section
                            .get("title")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                        items,
                        continuation: self.extract_continuation(section),
                        section_type: section
                            .get("sectionType")
                            .and_then(|t| t.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                    });
                }
            }
        }

        if let Some(items_json) = value.get("items").and_then(|v| v.as_array()) {
            let items = self.parse_media_items(items_json).await?;
            if !items.is_empty() {
                sections.push(BrowseSection {
                    title: None,
                    items,
                    continuation: self.extract_continuation(value),
                    section_type: "grid".to_string(),
                });
            }
        }

        Ok(sections)
    }

    async fn extract_items(&self, section: &serde_json::Value) -> Result<Option<Vec<MediaItem>>> {
        if let Some(content) = section
            .get("musicCarouselShelfRenderer")
            .or_else(|| section.get("musicShelfRenderer"))
            .or_else(|| section.get("musicItemSectionRenderer"))
        {
            if let Some(items) = content.get("contents").and_then(|c| c.as_array()) {
                return Ok(Some(self.parse_media_items(items).await?));
            }
        }
        Ok(None)
    }

    async fn parse_media_items(&self, items: &[serde_json::Value]) -> Result<Vec<MediaItem>> {
        let mut result = Vec::new();
        for item in items {
            if let Some(parsed) = self.parse_media_item(item).await? {
                result.push(parsed);
            }
        }
        Ok(result)
    }

    async fn parse_media_item(&self, item: &serde_json::Value) -> Result<Option<MediaItem>> {
        // Parse various item types: musicTwoRowItemRenderer, musicResponsiveListItemRenderer, etc.
        let Some(renderer) = item
            .get("musicTwoRowItemRenderer")
            .or_else(|| item.get("musicResponsiveListItemRenderer"))
            .or_else(|| item.get("musicOneRowItemRenderer"))
        else {
            return Ok(None);
        };
        Self::parse_media_item_value(renderer)
    }

    /// Parse a single renderer into a `MediaItem`. Pure function: no HTTP, no auth,
    /// no self state — safe to call directly from tests.
    fn parse_media_item_value(renderer: &serde_json::Value) -> Result<Option<MediaItem>> {
        let title = renderer
            .get("title")
            .and_then(|t| t.get("runs"))
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .and_then(|f| f.get("text"))
            .and_then(|t| t.as_str())
            .or_else(|| responsive_title(renderer))
            .unwrap_or("Unknown")
            .to_string();

        // Prefer the real watch/video id; fall back to browse ids. Never fabricate ids:
        // items without any identifier are skipped.
        let id = track_video_id(renderer)
            .or_else(|| {
                renderer
                    .get("navigationEndpoint")
                    .and_then(|e| e.get("browseEndpoint"))
                    .and_then(|b| b.get("browseId"))
                    .and_then(|b| b.as_str())
            })
            .ok_or_else(|| CatalogueError::Parse("media item has no id".into()))?
            .to_string();

        let media_type = classify_media_type(&id, renderer);

        let thumbnails = renderer
            .get("thumbnail")
            .and_then(|t| t.get("thumbnails"))
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(Thumbnail {
                            url: t.get("url")?.as_str()?.to_string(),
                            width: t.get("width")?.as_u64()? as u32,
                            height: t.get("height")?.as_u64()? as u32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let duration_text = extract_duration_text(renderer);
        let duration = duration_text.as_deref().and_then(parse_duration);

        return Ok(Some(MediaItem {
            id: id.clone(),
            browse_id: Some(BrowseId(id)),
            media_type,
            title,
            subtitle: subtitle_text(renderer),
            description: None,
            thumbnails,
            duration,
            duration_text: duration_text.clone(),
            artists: extract_artists(renderer),
            album: None,
            playlist: None,
            year: None,
            view_count: None,
            is_explicit: false,
            is_liked: false,
            is_in_library: false,
            playback_url: None,
            metadata: renderer.clone(),
        }));
    }

    fn extract_continuation(&self, value: &serde_json::Value) -> Option<String> {
        value
            .get("continuationContents")
            .and_then(|c| c.get("continuations"))
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("nextContinuationData"))
            .and_then(|d| d.get("continuation"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
    }

    /// Fetch playable tracks related to a real InnerTube seed.
    pub async fn get_recommendations(
        &self,
        seed_type: SeedType,
        seed_id: &str,
        limit: usize,
    ) -> Result<Vec<MediaItem>> {
        if seed_id.is_empty() {
            return Err(CatalogueError::Api("Recommendation seed is empty".into()).into());
        }
        if seed_type == SeedType::Genre {
            return Err(CatalogueError::Api(
                "Genre seeds require a browse ID; an arbitrary genre name is unsupported".into(),
            )
            .into());
        }
        if limit == 0 {
            return Ok(Vec::new());
        }

        let auth_client = self.get_client().await?;
        let response: serde_json::Value = match seed_type {
            SeedType::Track => {
                auth_client
                    .post_json(
                        &format!("{}/next", self.api_base),
                        &NextRequest {
                            context: self.context.clone(),
                            video_id: seed_id.to_string(),
                            playlist_id: None,
                            params: None,
                        },
                    )
                    .await?
            }
            SeedType::Album | SeedType::Artist | SeedType::Playlist => {
                auth_client
                    .post_json(
                        &format!("{}/browse", self.api_base),
                        &BrowseRequest {
                            context: self.context.clone(),
                            browse_id: seed_id.to_string(),
                        },
                    )
                    .await?
            }
            SeedType::Genre => unreachable!("genre seeds are rejected above"),
        };

        let mut candidates = Vec::new();
        collect_track_candidates(&response, &mut candidates);
        let mut seen = HashSet::new();
        let mut tracks = Vec::new();
        for candidate in candidates {
            let renderer = candidate
                .get("musicTwoRowItemRenderer")
                .or_else(|| candidate.get("musicResponsiveListItemRenderer"))
                .or_else(|| candidate.get("musicOneRowItemRenderer"));
            let Some(video_id) = renderer.and_then(track_video_id) else {
                continue;
            };
            if renderer.and_then(recommendation_title).is_none() {
                continue;
            }
            if video_id == seed_id || !seen.insert(video_id.to_string()) {
                continue;
            }
            if let Some(item) = self.parse_media_item(candidate).await? {
                tracks.push(item);
                if tracks.len() == limit {
                    break;
                }
            }
        }
        Ok(tracks)
    }

    // ===== Search API =====

    /// Search YouTube Music
    #[instrument(skip(self))]
    pub async fn search(&self, query: &str, filter: Option<&str>) -> Result<SearchResponse> {
        let cache_key = format!("{}:{}", query, filter.unwrap_or(""));
        let start = std::time::Instant::now();

        // Check cache
        if let Some(cached) = self.search_cache.get(&cache_key) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                self.emit(CatalogueEvent::SearchCompleted {
                    query: query.to_string(),
                    result_count: cached.response.results.len(),
                    cached: true,
                });
                return Ok(cached.response.clone());
            }
        }

        // Deduplicate
        let mutex = self
            .pending_search
            .entry(cache_key.clone())
            .or_default()
            .clone();
        let _guard = mutex.lock().await;

        // Double-check
        if let Some(cached) = self.search_cache.get(&cache_key) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                self.emit(CatalogueEvent::SearchCompleted {
                    query: query.to_string(),
                    result_count: cached.response.results.len(),
                    cached: true,
                });
                return Ok(cached.response.clone());
            }
        }

        let auth_client = self.get_client().await?;
        let url = format!("{}/search", self.api_base);

        let request = SearchRequest {
            context: self.context.clone(),
            query: query.to_string(),
            params: filter.map(|f| f.to_string()),
        };

        let response: serde_json::Value = auth_client.post_json(&url, &request).await?;
        let search_response = self.parse_search_response(response, query).await?;

        // Cache for 10 minutes
        let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 600;
        self.search_cache.insert(
            cache_key,
            CachedSearch {
                response: search_response.clone(),
                expires_at,
            },
        );

        let latency = start.elapsed().as_millis() as u64;
        self.emit(CatalogueEvent::Diagnostic(
            DiagnosticEvent::CatalogueRequest {
                endpoint: "search".to_string(),
                latency_ms: latency,
                cached: false,
            },
        ));

        self.emit(CatalogueEvent::SearchCompleted {
            query: query.to_string(),
            result_count: search_response.results.len(),
            cached: false,
        });

        Ok(search_response)
    }

    async fn parse_search_response(
        &self,
        value: serde_json::Value,
        query: &str,
    ) -> Result<SearchResponse> {
        let mut results = Vec::new();
        let mut filters = Vec::new();

        // Parse tabs for filters
        if let Some(tabs) = value
            .get("contents")
            .and_then(|c| c.get("tabbedSearchResultsRenderer"))
            .and_then(|t| t.get("tabs"))
            .and_then(|t| t.as_array())
        {
            for tab in tabs {
                if let Some(tab_renderer) = tab.get("tabRenderer") {
                    filters.push(SearchFilter {
                        key: tab_renderer
                            .get("endpoint")
                            .and_then(|e| e.get("searchEndpoint"))
                            .and_then(|s| s.get("searchParams"))
                            .and_then(|p| p.as_str())
                            .unwrap_or("")
                            .to_string(),
                        label: tab_renderer
                            .get("title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        selected: tab_renderer
                            .get("selected")
                            .and_then(|s| s.as_bool())
                            .unwrap_or(false),
                    });
                }
            }
        }

        // Parse results from first tab
        if let Some(contents) = value
            .get("contents")
            .and_then(|c| c.get("tabbedSearchResultsRenderer"))
            .and_then(|t| t.get("tabs"))
            .and_then(|t| t.as_array())
            .and_then(|a| a.first())
            .and_then(|t| t.get("tabRenderer"))
            .and_then(|r| r.get("content"))
            .and_then(|c| c.get("sectionListRenderer"))
            .and_then(|s| s.get("contents"))
            .and_then(|c| c.as_array())
        {
            for section in contents {
                if let Some(items) = section
                    .get("musicShelfRenderer")
                    .and_then(|m| m.get("contents"))
                    .and_then(|c| c.as_array())
                {
                    results.extend(self.parse_media_items(items).await?);
                }
            }
        }

        Ok(SearchResponse {
            query: query.to_string(),
            results,
            filters,
            continuation: self.extract_continuation(&value),
        })
    }

    // ===== Item Resolution =====

    /// Get detailed item by ID
    #[instrument(skip(self))]
    pub async fn get_item(&self, item_id: &str) -> Result<MediaItem> {
        // Check cache
        if let Some(item) = self.item_cache.get(item_id) {
            return Ok(item.clone());
        }

        // Fetch based on ID type
        let item = if item_id.len() == 11 {
            self.get_track(item_id).await?
        } else if item_id.starts_with("MPREb_") {
            self.get_playlist(item_id).await?
        } else if item_id.starts_with("UC") {
            self.get_artist(item_id).await?
        } else {
            self.get_album(item_id).await?
        };

        self.item_cache.insert(item_id.to_string(), item.clone());
        self.emit(CatalogueEvent::ItemResolved {
            item_id: item_id.to_string(),
        });
        Ok(item)
    }

    async fn get_track(&self, video_id: &str) -> Result<MediaItem> {
        let auth_client = self.get_client().await?;
        let url = format!("{}/next", self.api_base);

        let request = NextRequest {
            context: self.context.clone(),
            video_id: video_id.to_string(),
            playlist_id: None,
            params: None,
        };

        let response: serde_json::Value = auth_client.post_json(&url, &request).await?;

        // Real metadata: locate the renderer for this video inside the watch-next
        // response and parse it like a shelf item (title, artists, thumbnails, duration).
        let mut candidates = Vec::new();
        collect_track_candidates(&response, &mut candidates);
        for candidate in candidates {
            let renderer = candidate
                .get("musicTwoRowItemRenderer")
                .or_else(|| candidate.get("musicResponsiveListItemRenderer"))
                .or_else(|| candidate.get("musicOneRowItemRenderer"));
            let Some(renderer) = renderer else {
                continue;
            };
            if track_video_id(renderer) != Some(video_id) {
                continue;
            }
            if let Some(item) = self.parse_media_item(candidate).await? {
                return Ok(item);
            }
        }

        // Fallback: minimal stub when the response carries no parseable item.
        Ok(MediaItem {
            id: video_id.to_string(),
            browse_id: Some(BrowseId(video_id.to_string())),
            media_type: MediaType::Track,
            title: "Track".to_string(),
            subtitle: None,
            description: None,
            thumbnails: vec![],
            duration: None,
            duration_text: None,
            artists: vec![],
            album: None,
            playlist: None,
            year: None,
            view_count: None,
            is_explicit: false,
            is_liked: false,
            is_in_library: false,
            playback_url: None,
            metadata: response,
        })
    }

    async fn get_album(&self, _browse_id: &str) -> Result<MediaItem> {
        Ok(MediaItem {
            id: _browse_id.to_string(),
            browse_id: Some(BrowseId(_browse_id.to_string())),
            media_type: MediaType::Album,
            title: "Album".to_string(),
            subtitle: None,
            description: None,
            thumbnails: vec![],
            duration: None,
            duration_text: None,
            artists: vec![],
            album: None,
            playlist: None,
            year: None,
            view_count: None,
            is_explicit: false,
            is_liked: false,
            is_in_library: false,
            playback_url: None,
            metadata: serde_json::json!({}),
        })
    }

    async fn get_artist(&self, _browse_id: &str) -> Result<MediaItem> {
        Ok(MediaItem {
            id: _browse_id.to_string(),
            browse_id: Some(BrowseId(_browse_id.to_string())),
            media_type: MediaType::Artist,
            title: "Artist".to_string(),
            subtitle: None,
            description: None,
            thumbnails: vec![],
            duration: None,
            duration_text: None,
            artists: vec![],
            album: None,
            playlist: None,
            year: None,
            view_count: None,
            is_explicit: false,
            is_liked: false,
            is_in_library: false,
            playback_url: None,
            metadata: serde_json::json!({}),
        })
    }

    async fn get_playlist(&self, _browse_id: &str) -> Result<MediaItem> {
        Ok(MediaItem {
            id: _browse_id.to_string(),
            browse_id: Some(BrowseId(_browse_id.to_string())),
            media_type: MediaType::Playlist,
            title: "Playlist".to_string(),
            subtitle: None,
            description: None,
            thumbnails: vec![],
            duration: None,
            duration_text: None,
            artists: vec![],
            album: None,
            playlist: None,
            year: None,
            view_count: None,
            is_explicit: false,
            is_liked: false,
            is_in_library: false,
            playback_url: None,
            metadata: serde_json::json!({}),
        })
    }

    // ===== Streaming URL Resolution =====

    /// Get streaming URL for a track with bounded retry recovery.
    #[instrument(skip(self))]
    pub async fn get_stream_url(&self, video_id: &str, quality: AudioQuality) -> Result<String> {
        self.get_stream_url_with_retry(video_id, quality)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Re-resolve a fresh signed streaming URL when the current one is suspected expired.
    #[instrument(skip(self))]
    pub async fn re_resolve_stream_url(
        &self,
        video_id: &str,
        quality: AudioQuality,
    ) -> Result<String> {
        self.get_stream_url(video_id, quality).await
    }

    /// Cancel any in-flight stream resolution. Checked between retries.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Reset cancellation flag so new resolutions can proceed.
    pub fn reset_cancellation(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Sleep that wakes early if cancellation is requested.
    async fn sleep_cancellation_aware(
        &self,
        duration: Duration,
    ) -> Result<(), CatalogueStreamError> {
        if self.is_cancelled() {
            return Err(CatalogueStreamError::Cancelled);
        }
        let sleep = tokio::time::sleep(duration);
        let cancelled = self.cancelled.clone();
        tokio::select! {
            _ = sleep => {},
            _ = async {
                // Poll cancellation flag during sleep
                let mut interval = tokio::time::interval(Duration::from_millis(50));
                loop {
                    interval.tick().await;
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                }
            } => {
                return Err(CatalogueStreamError::Cancelled);
            }
        }
        if self.is_cancelled() {
            return Err(CatalogueStreamError::Cancelled);
        }
        Ok(())
    }

    /// Bounded retry loop for stream URL resolution.
    ///
    /// - Max 3 retries for transient errors (429, 500-series via Connectivity/Timeout, timeouts, connectivity)
    /// - Respects `Retry-After` header on 429
    /// - No retries for 401, 403 (auth/account issues)
    /// - Cancellation-aware: checks cancellation flag between retries
    async fn get_stream_url_with_retry(
        &self,
        video_id: &str,
        quality: AudioQuality,
    ) -> Result<String, CatalogueStreamError> {
        const MAX_RETRIES: u32 = 3;
        let mut attempt: u32 = 0;

        loop {
            if self.is_cancelled() {
                self.emit_stream_error(video_id, CatalogueStreamError::Cancelled);
                return Err(CatalogueStreamError::Cancelled);
            }

            match self.get_stream_url_inner(video_id, quality).await {
                Ok(url) => {
                    // Reset cancellation on success
                    self.reset_cancellation();
                    return Ok(url);
                }
                Err(err) => {
                    if !err.is_retryable() || attempt >= MAX_RETRIES {
                        self.emit_stream_error(video_id, err.clone());
                        return Err(err);
                    }

                    attempt += 1;
                    let delay = err.retry_delay(attempt).unwrap_or(Duration::from_secs(1));
                    self.sleep_cancellation_aware(delay).await?;
                }
            }
        }
    }

    /// Single attempt at resolving a streaming URL, with HTTP status checking.
    async fn get_stream_url_inner(
        &self,
        video_id: &str,
        quality: AudioQuality,
    ) -> Result<String, CatalogueStreamError> {
        let auth_client = self
            .get_client()
            .await
            .map_err(|_| CatalogueStreamError::AuthFailure)?;
        let url = format!("{}/player", self.api_base);

        let request = PlayerRequest {
            context: self.context.clone(),
            video_id: video_id.to_string(),
            playback_context: PlaybackContext {
                content_playback_context: ContentPlaybackContext {
                    html5_preference: "HTML5_PREF_WANTS".to_string(),
                    reference_key: "music".to_string(),
                    signature_timestamp: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| CatalogueStreamError::Connectivity("clock error".to_string()))?
                        .as_millis() as u64,
                },
            },
            params: quality_param(quality).to_string(),
        };

        let (status, response) = auth_client
            .post_json_with_status(&url, &request)
            .await
            .map_err(Self::map_reqwest_error_to_stream_error)?;

        match status {
            StatusCode::OK => {
                let value: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|_| CatalogueStreamError::CorruptResponse)?;

                // Extract streaming URL from streamingData
                let streaming_data = value
                    .get("streamingData")
                    .ok_or(CatalogueStreamError::CorruptResponse)?;

                // Prefer adaptive formats
                if let Some(formats) = streaming_data
                    .get("adaptiveFormats")
                    .and_then(|f| f.as_array())
                {
                    for format in formats {
                        let mime = format
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("");
                        if mime.contains("audio") {
                            if let Some(url) = format.get("url").and_then(|u| u.as_str()) {
                                return Ok(url.to_string());
                            }
                        }
                    }
                }

                // Fallback to standard formats
                if let Some(formats) = streaming_data.get("formats").and_then(|f| f.as_array()) {
                    for format in formats {
                        let mime = format
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .unwrap_or("");
                        if mime.contains("audio") {
                            if let Some(url) = format.get("url").and_then(|u| u.as_str()) {
                                return Ok(url.to_string());
                            }
                        }
                    }
                }

                Err(CatalogueStreamError::CorruptResponse)
            }
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = Self::parse_retry_after(&response);
                Err(CatalogueStreamError::RateLimited { retry_after })
            }
            StatusCode::FORBIDDEN => Err(CatalogueStreamError::Forbidden),
            StatusCode::UNAUTHORIZED => Err(CatalogueStreamError::AuthFailure),
            StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT => Err(CatalogueStreamError::Connectivity(format!(
                "server error: {}",
                status
            ))),
            _ => Err(CatalogueStreamError::CorruptResponse),
        }
    }

    /// Map a `reqwest::Error` to a `CatalogueStreamError`.
    fn map_reqwest_error_to_stream_error(e: reqwest::Error) -> CatalogueStreamError {
        if e.is_timeout() {
            return CatalogueStreamError::Timeout;
        }
        if e.is_connect() {
            return CatalogueStreamError::Connectivity(e.to_string());
        }
        CatalogueStreamError::Connectivity(e.to_string())
    }

    /// Parse `Retry-After` header as seconds or HTTP-date.
    fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
        let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;

        // Try as seconds first
        if let Ok(secs) = value.trim().parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }

        // Try as HTTP-date (RFC 1123 / RFC 2822 format)
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(value.trim()) {
            let now = SystemTime::now();
            let retry_at: SystemTime = dt.into();
            return retry_at.duration_since(now).ok();
        }

        None
    }

    /// Emit a `CatalogueEvent::StreamError` so the frontend can display a diagnostic.
    fn emit_stream_error(&self, video_id: &str, error: CatalogueStreamError) {
        let info: StreamErrorInfo = (&error).into();
        self.emit(CatalogueEvent::StreamError {
            video_id: video_id.to_string(),
            error_code: info.error_code,
            redacted_message: info.message,
            retryable: info.retryable,
        });
    }

    // ===== Cache Management =====

    /// Clear expired cache entries
    pub async fn cleanup_cache(&self) -> usize {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut count = 0;

        self.browse_cache.retain(|_, v| {
            if v.expires_at < now {
                count += 1;
                false
            } else {
                true
            }
        });

        self.search_cache.retain(|_, v| {
            if v.expires_at < now {
                count += 1;
                false
            } else {
                true
            }
        });

        if count > 0 {
            self.emit(CatalogueEvent::CacheEvicted { count });
        }

        count
    }

    /// Subscribe to catalogue events
    pub fn subscribe(&self) -> broadcast::Receiver<CatalogueEvent> {
        self.event_tx.subscribe()
    }

    fn emit(&self, event: CatalogueEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Shutdown catalogue service
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        self.browse_cache.clear();
        self.search_cache.clear();
        self.item_cache.clear();
        Ok(())
    }
}

fn responsive_title(renderer: &serde_json::Value) -> Option<&str> {
    renderer
        .get("flexColumns")?
        .as_array()?
        .first()?
        .get("musicResponsiveListItemFlexColumnRenderer")?
        .get("text")?
        .get("runs")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()
}

fn recommendation_title(renderer: &serde_json::Value) -> Option<&str> {
    renderer
        .get("title")
        .and_then(|title| title.get("runs"))
        .and_then(|runs| runs.as_array())
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("text"))
        .and_then(|text| text.as_str())
        .or_else(|| responsive_title(renderer))
        .filter(|title| !title.is_empty())
}

fn track_video_id(renderer: &serde_json::Value) -> Option<&str> {
    renderer
        .get("navigationEndpoint")
        .and_then(|e| e.get("watchEndpoint"))
        .and_then(|e| e.get("videoId"))
        .and_then(|id| id.as_str())
        .or_else(|| {
            renderer
                .get("flexColumns")?
                .as_array()?
                .first()?
                .get("musicResponsiveListItemFlexColumnRenderer")?
                .get("text")?
                .get("runs")?
                .as_array()?
                .iter()
                .find_map(|run| {
                    run.get("navigationEndpoint")?
                        .get("watchEndpoint")?
                        .get("videoId")?
                        .as_str()
                })
        })
}

/// Classify a media item from its id and the renderer endpoint that produced it.
///
/// Ids from `watchEndpoint.videoId` are tracks. Browse ids follow YouTube Music
/// id-space conventions: `MPREb_`/`VL`/`RD` playlists, `UC` artists, `OLAK5`
/// albums, 11-char ids are otherwise treated as tracks.
fn classify_media_type(id: &str, renderer: &serde_json::Value) -> MediaType {
    let from_watch = renderer
        .get("navigationEndpoint")
        .and_then(|e| e.get("watchEndpoint"))
        .and_then(|e| e.get("videoId"))
        .and_then(|v| v.as_str())
        .map_or(false, |v| v == id);

    if from_watch || id.len() == 11 {
        return MediaType::Track;
    }
    if id.starts_with("MPREb_") || id.starts_with("VL") || id.starts_with("RD") {
        return MediaType::Playlist;
    }
    if id.starts_with("UC") {
        return MediaType::Artist;
    }
    if id.starts_with("OLAK5") {
        return MediaType::Album;
    }
    MediaType::Track
}

/// Flatten subtitle runs into a display string, e.g. "The Artist - Topic".
fn subtitle_text(renderer: &serde_json::Value) -> Option<String> {
    renderer
        .get("subtitle")
        .and_then(|s| s.get("runs"))
        .and_then(|r| r.as_array())
        .map(|runs| {
            runs.iter()
                .filter_map(|run| {
                    run.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.trim().is_empty())
}

/// Extract artist refs from subtitle runs, falling back to the flex-columns
/// artist column used by `musicResponsiveListItemRenderer`.
fn extract_artists(renderer: &serde_json::Value) -> Vec<ArtistRef> {
    let mut artists: Vec<ArtistRef> = Vec::new();

    let collect_run = |run: &serde_json::Value, out: &mut Vec<ArtistRef>| -> Option<()> {
        let text = run.get("text")?.as_str()?;
        if text.trim().is_empty() || text.trim() == "·" {
            return None;
        }
        let browse_id = run
            .get("navigationEndpoint")
            .and_then(|e| e.get("browseEndpoint"))
            .and_then(|b| b.get("browseId"))
            .and_then(|b| b.as_str())
            .map(BrowseId::new);
        out.push(ArtistRef {
            id: browse_id
                .as_ref()
                .map(|b| b.0.clone())
                .unwrap_or_else(|| text.to_string()),
            name: text.to_string(),
            browse_id,
        });
        Some(())
    };

    if let Some(runs) = renderer
        .get("subtitle")
        .and_then(|s| s.get("runs"))
        .and_then(|r| r.as_array())
    {
        for run in runs {
            let _ = collect_run(run, &mut artists);
        }
    }

    if artists.is_empty() {
        if let Some(columns) = renderer.get("flexColumns").and_then(|c| c.as_array()) {
            for column in columns.iter().skip(1) {
                if let Some(runs) = column
                    .get("musicResponsiveListItemFlexColumnRenderer")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.get("runs"))
                    .and_then(|r| r.as_array())
                {
                    for run in runs {
                        let _ = collect_run(run, &mut artists);
                    }
                    if !artists.is_empty() {
                        break;
                    }
                }
            }
        }
    }

    artists
}

/// Duration text from the thumbnail overlay (`3:45`), `durationText`, or
/// `lengthSeconds`.
fn extract_duration_text(renderer: &serde_json::Value) -> Option<String> {
    renderer
        .get("thumbnail")
        .and_then(|t| t.get("thumbnailOverlay"))
        .and_then(|o| o.get("thumbnailOverlayTimeStatusRenderer"))
        .and_then(|r| r.get("text"))
        .and_then(|t| t.get("simpleText"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            renderer
                .get("durationText")
                .and_then(|t| t.get("simpleText"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            renderer
                .get("lengthSeconds")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
}

/// Parse `H:MM:SS`, `M:SS`, or a seconds string into a `Duration`.
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(secs) = text.parse::<f64>() {
        return Some(Duration::from_secs_f64(secs));
    }
    let parts = text.split(':');
    let vals: Vec<u32> = parts
        .map(|p| p.trim().parse::<u32>().ok())
        .collect::<Option<Vec<u32>>>()?;
    let millis = match vals.as_slice() {
        [m, s] => (*m as u64) * 60 + (*s as u64),
        [h, m, s] => ((*h as u64) * 60 + *m as u64) * 60 + (*s as u64),
        _ => return None,
    };
    Some(Duration::from_millis(millis * 1000))
}

fn collect_track_candidates<'a>(
    value: &'a serde_json::Value,
    candidates: &mut Vec<&'a serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("musicTwoRowItemRenderer")
                || map.contains_key("musicResponsiveListItemRenderer")
                || map.contains_key("musicOneRowItemRenderer")
            {
                candidates.push(value);
                return;
            }
            for child in map.values() {
                collect_track_candidates(child, candidates);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_track_candidates(child, candidates);
            }
        }
        _ => {}
    }
}

// ===== Request/Response Types =====

#[derive(Debug, Serialize)]
struct BrowseRequest {
    context: InnerTubeContext,
    browse_id: String,
}

#[derive(Debug, Serialize)]
struct ContinuationRequest {
    context: InnerTubeContext,
    continuation: String,
}

#[derive(Debug, Serialize)]
struct SearchRequest {
    context: InnerTubeContext,
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<String>,
}

#[derive(Debug, Serialize)]
struct NextRequest {
    context: InnerTubeContext,
    video_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    playlist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<String>,
}

#[derive(Debug, Serialize)]
struct PlayerRequest {
    context: InnerTubeContext,
    video_id: String,
    playback_context: PlaybackContext,
    params: String,
}

#[derive(Debug, Serialize)]
struct PlaybackContext {
    content_playback_context: ContentPlaybackContext,
}

#[derive(Debug, Serialize)]
struct ContentPlaybackContext {
    html5_preference: String,
    reference_key: String,
    signature_timestamp: u64,
}

fn quality_param(quality: AudioQuality) -> &'static str {
    match quality {
        AudioQuality::Low => "CAE=",
        AudioQuality::Normal => "CAI=",
        AudioQuality::High => "CAM=",
        AudioQuality::Lossless => "CAQ=",
    }
}

/// Tauri command handlers
pub mod commands {
    use super::*;

    #[tauri::command]
    pub async fn catalogue_browse(
        catalogue: tauri::State<'_, Arc<CatalogueService>>,
        browse_id: String,
    ) -> TauriResult<BrowseResponse> {
        Ok(catalogue.browse(BrowseId::new(browse_id)).await?)
    }

    #[tauri::command]
    pub async fn catalogue_search(
        catalogue: tauri::State<'_, Arc<CatalogueService>>,
        query: String,
        filter: Option<String>,
    ) -> TauriResult<SearchResponse> {
        Ok(catalogue.search(&query, filter.as_deref()).await?)
    }

    #[tauri::command]
    pub async fn catalogue_get_item(
        catalogue: tauri::State<'_, Arc<CatalogueService>>,
        item_id: String,
    ) -> TauriResult<MediaItem> {
        Ok(catalogue.get_item(&item_id).await?)
    }

    #[tauri::command]
    pub async fn catalogue_get_stream_url(
        catalogue: tauri::State<'_, Arc<CatalogueService>>,
        video_id: String,
        quality: orcave_settings::AudioQuality,
    ) -> TauriResult<String> {
        Ok(catalogue.get_stream_url(&video_id, quality).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browse_id_helpers() {
        assert_eq!(BrowseId::home().0, "FEmusic_home");
        assert_eq!(BrowseId::library().0, "FEmusic_library");
    }

    #[test]
    fn test_media_type_serialization() {
        let track = MediaType::Track;
        let json = serde_json::to_string(&track).unwrap();
        assert_eq!(json, "\"track\"");
    }

    #[test]
    fn provider_metadata_and_playback_url_do_not_cross_serialization_boundary() {
        let renderer = serde_json::json!({
            "title": {"runs": [{"text": "Safe title"}]},
            "navigationEndpoint": {"watchEndpoint": {"videoId": "AbCdEfGhIjK"}},
            "providerData": {
                "accessToken": "CANARY_PROVIDER_ACCESS_TOKEN",
                "streamingData": {"adaptiveFormats": [{"url": "https://example.invalid/audio?sig=CANARY_SIGNED_URL"}]}
            }
        });
        let mut item = CatalogueService::parse_media_item_value(&renderer)
            .unwrap()
            .unwrap();
        assert_eq!(
            item.metadata["providerData"]["accessToken"],
            "CANARY_PROVIDER_ACCESS_TOKEN"
        );
        item.playback_url = Some("https://example.invalid/audio?sig=CANARY_PLAYBACK_URL".into());
        let serialized_item = serde_json::to_string(&item).unwrap();
        assert!(serialized_item.contains("Safe title"));
        assert!(serialized_item.contains("AbCdEfGhIjK"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serialized_item).unwrap()["metadata"],
            serde_json::json!({})
        );
        for canary in [
            "CANARY_PROVIDER_ACCESS_TOKEN",
            "CANARY_SIGNED_URL",
            "CANARY_PLAYBACK_URL",
        ] {
            assert!(!serialized_item.contains(canary), "item leaked {canary}");
        }
        let debug_item = format!("{item:?}");
        assert!(debug_item.contains("Safe title"));
        assert!(debug_item.contains("AbCdEfGhIjK"));
        for canary in [
            "CANARY_PROVIDER_ACCESS_TOKEN",
            "CANARY_SIGNED_URL",
            "CANARY_PLAYBACK_URL",
        ] {
            assert!(!debug_item.contains(canary), "item Debug leaked {canary}");
        }

        let response = BrowseResponse {
            node_id: BrowseId::home(),
            title: "Browse".into(),
            sections: vec![BrowseSection {
                title: Some("Shelf".into()),
                items: vec![item],
                continuation: None,
                section_type: "grid".into(),
            }],
            continuation: None,
            metadata: serde_json::json!({
                "responseContext": {"accessToken": "CANARY_BROWSE_TOKEN"},
                "streamingData": {"url": "https://example.invalid/audio?sig=CANARY_BROWSE_URL"}
            }),
        };
        let serialized_browse = serde_json::to_string(&response).unwrap();
        assert!(serialized_browse.contains("Safe title"));
        assert!(serialized_browse.contains("Shelf"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serialized_browse).unwrap()["metadata"],
            serde_json::json!({})
        );
        for canary in [
            "CANARY_PROVIDER_ACCESS_TOKEN",
            "CANARY_SIGNED_URL",
            "CANARY_PLAYBACK_URL",
            "CANARY_BROWSE_TOKEN",
            "CANARY_BROWSE_URL",
        ] {
            assert!(
                !serialized_browse.contains(canary),
                "browse leaked {canary}"
            );
        }
        let debug_browse = format!("{response:?}");
        assert!(debug_browse.contains("Safe title"));
        assert!(debug_browse.contains("Shelf"));
        for canary in [
            "CANARY_PROVIDER_ACCESS_TOKEN",
            "CANARY_SIGNED_URL",
            "CANARY_PLAYBACK_URL",
            "CANARY_BROWSE_TOKEN",
            "CANARY_BROWSE_URL",
        ] {
            assert!(
                !debug_browse.contains(canary),
                "browse Debug leaked {canary}"
            );
        }
    }

    #[test]
    fn recommendation_candidates_require_real_video_ids_and_titles() {
        let response = serde_json::json!({
            "contents": {
                "singleColumnMusicWatchNextResultsRenderer": {
                    "tabbedRenderer": {
                        "watchNextTabbedResultsRenderer": {
                            "tabs": [{
                                "tabRenderer": {
                                    "content": {
                                        "musicQueueRenderer": {
                                            "content": {
                                                "playlistPanelRenderer": {
                                                    "contents": [
                                                        {"playlistPanelVideoRenderer": {"videoId": "not-a-music-item"}},
                                                        {"musicResponsiveListItemRenderer": {
                                                            "flexColumns": [{
                                                                "musicResponsiveListItemFlexColumnRenderer": {
                                                                    "text": {"runs": [{
                                                                        "text": "Real track",
                                                                        "navigationEndpoint": {
                                                                            "watchEndpoint": {"videoId": "AbCdEfGhIjK"}
                                                                        }
                                                                    }]}
                                                                }
                                                            }]
                                                        }},
                                                        {"musicTwoRowItemRenderer": {
                                                            "title": {"runs": [{"text": "Another track"}]},
                                                            "navigationEndpoint": {
                                                                "watchEndpoint": {"videoId": "12345678901"}
                                                            }
                                                        }}
                                                    ]
                                                }
                                            }
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        });
        let mut candidates = Vec::new();
        collect_track_candidates(&response, &mut candidates);
        assert_eq!(candidates.len(), 2);
        let renderers: Vec<_> = candidates
            .iter()
            .map(|item| {
                item.get("musicResponsiveListItemRenderer")
                    .or_else(|| item.get("musicTwoRowItemRenderer"))
                    .unwrap()
            })
            .collect();
        assert_eq!(track_video_id(renderers[0]), Some("AbCdEfGhIjK"));
        assert_eq!(recommendation_title(renderers[0]), Some("Real track"));
        assert_eq!(track_video_id(renderers[1]), Some("12345678901"));
        assert_eq!(recommendation_title(renderers[1]), Some("Another track"));
        assert_eq!(track_video_id(&serde_json::json!({})), None);
    }

    // ===== CatalogueStreamError taxonomy tests =====

    #[test]
    fn stream_error_is_retryable() {
        assert!(CatalogueStreamError::RateLimited { retry_after: None }.is_retryable());
        assert!(CatalogueStreamError::RateLimited {
            retry_after: Some(Duration::from_secs(5))
        }
        .is_retryable());
        assert!(CatalogueStreamError::Timeout.is_retryable());
        assert!(CatalogueStreamError::Connectivity("dns fail".to_string()).is_retryable());
        assert!(!CatalogueStreamError::Forbidden.is_retryable());
        assert!(!CatalogueStreamError::AuthFailure.is_retryable());
        assert!(!CatalogueStreamError::Expired.is_retryable());
        assert!(!CatalogueStreamError::StaleAccount.is_retryable());
        assert!(!CatalogueStreamError::UnsupportedCodec.is_retryable());
        assert!(!CatalogueStreamError::CorruptResponse.is_retryable());
        assert!(!CatalogueStreamError::Cancelled.is_retryable());
    }

    #[test]
    fn stream_error_retry_delay() {
        // RateLimited with explicit Retry-After
        let e = CatalogueStreamError::RateLimited {
            retry_after: Some(Duration::from_secs(10)),
        };
        assert_eq!(e.retry_delay(0), Some(Duration::from_secs(10)));

        // Non-retryable returns None
        assert_eq!(CatalogueStreamError::Forbidden.retry_delay(0), None);
        assert_eq!(CatalogueStreamError::AuthFailure.retry_delay(0), None);
        assert_eq!(CatalogueStreamError::Expired.retry_delay(0), None);

        // Timeout: exponential backoff, attempt 0 -> 500ms + jitter (max 125ms jitter)
        let d = CatalogueStreamError::Timeout.retry_delay(0).unwrap();
        assert!(d >= Duration::from_millis(500));
        assert!(d <= Duration::from_millis(625));

        // Attempt 2 -> 500ms * 4 = 2000ms + jitter (max 500ms jitter, capped at 1000ms)
        let d2 = CatalogueStreamError::Timeout.retry_delay(2).unwrap();
        assert!(d2 >= Duration::from_millis(2000));
    }

    #[test]
    fn stream_error_frontend_code() {
        assert_eq!(
            CatalogueStreamError::Forbidden.frontend_code(),
            StreamErrorCode::ProviderError
        );
        assert_eq!(
            CatalogueStreamError::RateLimited { retry_after: None }.frontend_code(),
            StreamErrorCode::ProviderError
        );
        assert_eq!(
            CatalogueStreamError::AuthFailure.frontend_code(),
            StreamErrorCode::ProviderError
        );
        assert_eq!(
            CatalogueStreamError::Timeout.frontend_code(),
            StreamErrorCode::NetworkError
        );
        assert_eq!(
            CatalogueStreamError::Connectivity("x".to_string()).frontend_code(),
            StreamErrorCode::NetworkError
        );
        assert_eq!(
            CatalogueStreamError::UnsupportedCodec.frontend_code(),
            StreamErrorCode::DecoderError
        );
        assert_eq!(
            CatalogueStreamError::CorruptResponse.frontend_code(),
            StreamErrorCode::DecoderError
        );
        assert_eq!(
            CatalogueStreamError::Expired.frontend_code(),
            StreamErrorCode::AccountError
        );
        assert_eq!(
            CatalogueStreamError::StaleAccount.frontend_code(),
            StreamErrorCode::AccountError
        );
        assert_eq!(
            CatalogueStreamError::Cancelled.frontend_code(),
            StreamErrorCode::AccountError
        );
    }

    #[test]
    fn stream_error_redacted_message_no_secrets() {
        let msg = CatalogueStreamError::Expired.redacted_message();
        assert!(!msg.contains("http"));
        assert!(!msg.contains("token"));
        assert!(!msg.contains("signature"));

        let msg = CatalogueStreamError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
        }
        .redacted_message();
        assert!(msg.contains("5"));

        let msg = CatalogueStreamError::Connectivity("dns.example.com leak".to_string())
            .redacted_message();
        assert!(!msg.contains("dns.example.com leak"));
        assert!(msg.contains("Network connection lost"));
    }

    #[test]
    fn stream_error_info_conversion() {
        let e = CatalogueStreamError::Forbidden;
        let info: StreamErrorInfo = (&e).into();
        assert_eq!(info.error_code, StreamErrorCode::ProviderError);
        assert!(!info.retryable);
        assert!(!info.message.is_empty());

        let e = CatalogueStreamError::Timeout;
        let info: StreamErrorInfo = (&e).into();
        assert_eq!(info.error_code, StreamErrorCode::NetworkError);
        assert!(info.retryable);
    }

    #[test]
    fn stream_error_code_serialization() {
        assert_eq!(
            serde_json::to_string(&StreamErrorCode::ProviderError).unwrap(),
            "\"provider_error\""
        );
        assert_eq!(
            serde_json::to_string(&StreamErrorCode::NetworkError).unwrap(),
            "\"network_error\""
        );
        assert_eq!(
            serde_json::to_string(&StreamErrorCode::DecoderError).unwrap(),
            "\"decoder_error\""
        );
        assert_eq!(
            serde_json::to_string(&StreamErrorCode::AccountError).unwrap(),
            "\"account_error\""
        );
    }

    // ===== Retry-After parsing tests (via wiremock) =====

    #[tokio::test]
    async fn parse_retry_after_seconds_header() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/retry-seconds"))
            .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "10"))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let response = client
            .get(&format!("{}/retry-seconds", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let delay = CatalogueService::parse_retry_after(&response);
        assert_eq!(delay, Some(Duration::from_secs(10)));
    }

    #[tokio::test]
    async fn parse_retry_after_no_header() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/no-retry-header"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let response = client
            .get(&format!("{}/no-retry-header", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let delay = CatalogueService::parse_retry_after(&response);
        assert_eq!(delay, None);
    }

    #[tokio::test]
    async fn parse_retry_after_invalid_header() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/invalid-retry"))
            .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "not-a-number"))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let response = client
            .get(&format!("{}/invalid-retry", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let delay = CatalogueService::parse_retry_after(&response);
        assert_eq!(delay, None);
    }

    #[tokio::test]
    async fn parse_retry_after_http_date() {
        // Use a hardcoded HTTP-date (RFC 1123) about 30 seconds in the future
        let now = SystemTime::now() + Duration::from_secs(30);
        let dt: chrono::DateTime<chrono::Utc> = now.into();
        let http_date = dt.to_rfc2822();

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/retry-date"))
            .respond_with(ResponseTemplate::new(429).append_header("Retry-After", http_date))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let response = client
            .get(&format!("{}/retry-date", mock_server.uri()))
            .send()
            .await
            .unwrap();

        let delay = CatalogueService::parse_retry_after(&response);
        assert!(delay.is_some());
        let d = delay.unwrap();
        assert!(d >= Duration::from_secs(25));
        assert!(d <= Duration::from_secs(35));
    }

    // ===== Error-to-frontend mapping tests =====

    #[test]
    fn stale_account_maps_to_account_error() {
        let e = CatalogueStreamError::StaleAccount;
        assert_eq!(e.frontend_code(), StreamErrorCode::AccountError);
        assert!(!e.is_retryable());
        let msg = e.redacted_message();
        assert!(msg.contains("stale") || msg.contains("account"));
    }

    #[test]
    fn expired_descriptor_maps_to_account_error() {
        let e = CatalogueStreamError::Expired;
        assert_eq!(e.frontend_code(), StreamErrorCode::AccountError);
        assert!(!e.is_retryable());
        let msg = e.redacted_message();
        assert!(msg.contains("expired"));
    }

    #[test]
    fn auth_failure_no_retry() {
        let e = CatalogueStreamError::AuthFailure;
        assert!(!e.is_retryable());
        assert_eq!(e.frontend_code(), StreamErrorCode::ProviderError);
    }

    #[test]
    fn forbidden_no_retry() {
        let e = CatalogueStreamError::Forbidden;
        assert!(!e.is_retryable());
        assert_eq!(e.frontend_code(), StreamErrorCode::ProviderError);
    }

    // ===== parse_media_item metadata extraction =====

    fn two_row_item(json: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"musicTwoRowItemRenderer": json})
    }

    #[test]
    fn parse_media_item_extracts_title_id_thumbnails_duration_artists() {
        let item = two_row_item(serde_json::json!({
            "title": {"runs": [{"text": "Midnight Train"}]},
            "subtitle": {"runs": [
                {"text": "The Artist", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCartist123"}}},
                {"text": " · Topic"}
            ]},
            "navigationEndpoint": {"watchEndpoint": {"videoId": "AbCdEfGhIjK"}},
            "thumbnail": {
                "thumbnails": [
                    {"url": "https://i.ytimg.com/vi/AbCdEfGhIjK/hqdefault.jpg", "width": 480, "height": 360}
                ],
                "thumbnailOverlay": {
                    "thumbnailOverlayTimeStatusRenderer": {"text": {"simpleText": "3:45"}}
                }
            }
        }));
        let renderer = item.get("musicTwoRowItemRenderer").unwrap();
        let media = CatalogueService::parse_media_item_value(renderer)
            .expect("parse")
            .expect("item present");
        assert_eq!(media.id, "AbCdEfGhIjK");
        assert_eq!(media.title, "Midnight Train");
        assert_eq!(media.media_type, MediaType::Track);
        assert_eq!(media.duration, Some(Duration::from_secs(225)));
        assert_eq!(media.duration_text.as_deref(), Some("3:45"));
        assert_eq!(media.thumbnails.len(), 1);
        assert_eq!(
            media.thumbnails[0].url,
            "https://i.ytimg.com/vi/AbCdEfGhIjK/hqdefault.jpg"
        );
        assert!(media
            .artists
            .iter()
            .any(|a| a.name == "The Artist" && a.id == "UCartist123" && a.browse_id.is_some()));
    }

    #[test]
    fn parse_media_item_media_type_from_browse_id() {
        let artist = two_row_item(serde_json::json!({
            "title": {"runs": [{"text": "The Artist"}]},
            "navigationEndpoint": {"browseEndpoint": {"browseId": "UCartistId"}},
        }));
        let playlist = two_row_item(serde_json::json!({
            "title": {"runs": [{"text": "Mix"}]},
            "navigationEndpoint": {"browseEndpoint": {"browseId": "VLplId"}},
        }));
        let album = two_row_item(serde_json::json!({
            "title": {"runs": [{"text": "Album"}]},
            "navigationEndpoint": {"browseEndpoint": {"browseId": "OLAK5albumId"}},
        }));

        let mtype = |item: &serde_json::Value| {
            CatalogueService::parse_media_item_value(item.get("musicTwoRowItemRenderer").unwrap())
                .expect("parse")
                .expect("item present")
                .media_type
        };

        assert_eq!(mtype(&artist), MediaType::Artist);
        assert_eq!(mtype(&playlist), MediaType::Playlist);
        assert_eq!(mtype(&album), MediaType::Album);
    }

    #[test]
    fn parse_media_item_skips_item_without_id() {
        let item = two_row_item(serde_json::json!({
            "title": {"runs": [{"text": "No id"}]}
        }));
        let result =
            CatalogueService::parse_media_item_value(item.get("musicTwoRowItemRenderer").unwrap());
        assert!(
            result.is_err(),
            "expected Parse error for item without id, got {result:?}"
        );
    }

    #[test]
    fn parse_duration_handles_variants() {
        assert_eq!(parse_duration("3:45"), Some(Duration::from_secs(225)));
        assert_eq!(parse_duration("1:02:15"), Some(Duration::from_secs(3735)));
        assert_eq!(parse_duration("225"), Some(Duration::from_secs(225)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("not-a-duration"), None);
    }

    #[test]
    fn extract_artists_from_subtitle_runs() {
        let renderer = serde_json::json!({
            "subtitle": {"runs": [
                {"text": "Main Artist", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCmain"}}},
                {"text": " · Guest", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCguest"}}}
            ]}
        });
        let artists = extract_artists(&renderer);
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].name, "Main Artist");
        assert_eq!(artists[0].id, "UCmain");
        assert_eq!(
            artists[0].browse_id.as_ref().map(|b| b.0.as_str()),
            Some("UCmain")
        );
        assert_eq!(artists[1].name, " · Guest");
    }

    #[test]
    fn extract_artists_skips_separator_only_runs() {
        let renderer = serde_json::json!({
            "subtitle": {"runs": [
                {"text": "Solo Artist"},
                {"text": "·"}
            ]}
        });
        let artists = extract_artists(&renderer);
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, "Solo Artist");
    }
}
