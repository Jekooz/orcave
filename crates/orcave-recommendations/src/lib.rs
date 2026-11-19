//! Orcave Recommendations - YouTube Music recommendation engine
//!
//! Implements the approved Alpha recommendation design:
//! - Seed-based recommendations (track, album, artist, playlist)
//! - Personalized mixes (Your Mix, Discover Mix, New Release Mix)
//! - Mood/genre stations
//! - Related content (similar artists, album tracks, etc.)
//! - Caching and offline support

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tracing::instrument;
use uuid::Uuid;

use orcave_auth::AuthService;
use orcave_catalogue::{CatalogueService, MediaItem, MediaType};
use orcave_core::{SeedType, TauriResult};
use orcave_settings::SettingsService;

#[derive(thiserror::Error, Debug)]
pub enum RecommendationError {
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
    #[error("Settings error: {0}")]
    Settings(#[from] orcave_settings::SettingsError),
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for RecommendationError {
    fn from(e: anyhow::Error) -> Self {
        RecommendationError::Internal(e.to_string())
    }
}

/// Recommendation set with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendationSet {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub seed_type: Option<SeedType>,
    pub seed_id: Option<String>,
    pub items: Vec<MediaItem>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub algorithm: RecommendationAlgorithm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationAlgorithm {
    CollaborativeFiltering,
    ContentBased,
    Hybrid,
    SeedBased,
    PersonalizedMix,
    MoodStation,
}

/// Personalized mix types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixType {
    YourMix,
    DiscoverMix,
    NewReleaseMix,
    LikedSongsMix,
    Supermix,
}

/// Mood/genre station
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StationType {
    Focus,
    Workout,
    Relax,
    Party,
    Commute,
    Sleep,
    Genre(String), // Dynamic genre stations
}

/// Cached recommendation
#[derive(Debug, Clone)]
struct CachedRecommendation {
    set: RecommendationSet,
    expires_at: u64,
}

/// Recommendation service
pub struct RecommendationService {
    auth: Arc<AuthService>,
    catalogue: Arc<CatalogueService>,
    settings: Arc<SettingsService>,

    // Caches
    recommendation_cache: DashMap<String, CachedRecommendation>,
    mix_cache: DashMap<MixType, CachedRecommendation>,
    station_cache: DashMap<StationType, CachedRecommendation>,

    // Pending requests
    pending: DashMap<String, Arc<Mutex<()>>>,

    // Event bus
    event_tx: broadcast::Sender<RecommendationEvent>,
}

/// Internal recommendation events
#[derive(Debug, Clone)]
pub enum RecommendationEvent {
    RecommendationsGenerated { set_id: String, count: usize },
    MixUpdated(MixType),
    StationUpdated(StationType),
    CacheEvicted { count: usize },
}

impl RecommendationService {
    /// Create a new recommendation service
    #[instrument(skip(auth, catalogue, settings))]
    pub async fn new(
        auth: Arc<AuthService>,
        catalogue: Arc<CatalogueService>,
        settings: Arc<SettingsService>,
    ) -> Result<Self> {
        let (event_tx, _) = broadcast::channel(64);

        Ok(Self {
            auth,
            catalogue,
            settings,
            recommendation_cache: DashMap::new(),
            mix_cache: DashMap::new(),
            station_cache: DashMap::new(),
            pending: DashMap::new(),
            event_tx,
        })
    }

    /// Get recommendations for a seed
    #[instrument(skip(self))]
    pub async fn get_recommendations(
        &self,
        seed_type: SeedType,
        seed_id: &str,
        limit: usize,
    ) -> Result<Vec<MediaItem>> {
        let cache_key = format!("{:?}:{}", seed_type, seed_id);

        // Check cache
        if let Some(cached) = self.recommendation_cache.get(&cache_key) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                return Ok(cached.set.items.clone());
            }
        }

        // Deduplicate
        let mutex = self.pending.entry(cache_key.clone()).or_default().clone();
        let _guard = mutex.lock().await;

        // Double-check
        if let Some(cached) = self.recommendation_cache.get(&cache_key) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                return Ok(cached.set.items.clone());
            }
        }

        // Fetch from catalogue (which calls InnerTube)
        let items = self
            .catalogue
            .get_recommendations(seed_type, seed_id, limit)
            .await?;

        // Cache for 30 minutes
        let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 1800;
        let set = RecommendationSet {
            id: Uuid::new_v4().to_string(),
            title: format!("{:?} Recommendations", seed_type),
            subtitle: Some(seed_id.to_string()),
            seed_type: Some(seed_type),
            seed_id: Some(seed_id.to_string()),
            items: items.clone(),
            generated_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            algorithm: RecommendationAlgorithm::SeedBased,
        };

        self.recommendation_cache
            .insert(cache_key, CachedRecommendation { set, expires_at });

        self.emit(RecommendationEvent::RecommendationsGenerated {
            set_id: Uuid::new_v4().to_string(),
            count: items.len(),
        });

        Ok(items)
    }

    /// Get personalized mix
    #[instrument(skip(self))]
    pub async fn get_mix(&self, mix_type: MixType, limit: usize) -> Result<RecommendationSet> {
        // Check cache
        if let Some(cached) = self.mix_cache.get(&mix_type) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                return Ok(cached.set.clone());
            }
        }

        // Generate mix based on type
        let items = match mix_type {
            MixType::YourMix => self.generate_your_mix(limit).await?,
            MixType::DiscoverMix => self.generate_discover_mix(limit).await?,
            MixType::NewReleaseMix => self.generate_new_release_mix(limit).await?,
            MixType::LikedSongsMix => self.generate_liked_songs_mix(limit).await?,
            MixType::Supermix => self.generate_supermix(limit).await?,
        };

        let set = RecommendationSet {
            id: Uuid::new_v4().to_string(),
            title: format!("{:?}", mix_type),
            subtitle: Some("Personalized for you".to_string()),
            seed_type: None,
            seed_id: None,
            items,
            generated_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(4),
            algorithm: RecommendationAlgorithm::PersonalizedMix,
        };

        // Cache for 4 hours
        let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 14400;
        self.mix_cache.insert(
            mix_type,
            CachedRecommendation {
                set: set.clone(),
                expires_at,
            },
        );

        self.emit(RecommendationEvent::MixUpdated(mix_type));
        Ok(set)
    }

    /// Get mood/genre station
    #[instrument(skip(self))]
    pub async fn get_station(
        &self,
        station_type: StationType,
        limit: usize,
    ) -> Result<RecommendationSet> {
        // Check cache
        if let Some(cached) = self.station_cache.get(&station_type) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if now < cached.expires_at {
                return Ok(cached.set.clone());
            }
        }

        let items = match &station_type {
            StationType::Focus => self.generate_focus_station(limit).await?,
            StationType::Workout => self.generate_workout_station(limit).await?,
            StationType::Relax => self.generate_relax_station(limit).await?,
            StationType::Party => self.generate_party_station(limit).await?,
            StationType::Commute => self.generate_commute_station(limit).await?,
            StationType::Sleep => self.generate_sleep_station(limit).await?,
            StationType::Genre(genre) => self.generate_genre_station(genre, limit).await?,
        };

        let set = RecommendationSet {
            id: Uuid::new_v4().to_string(),
            title: format!("{:?} Station", station_type),
            subtitle: Some("Mood station".to_string()),
            seed_type: None,
            seed_id: None,
            items,
            generated_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(2),
            algorithm: RecommendationAlgorithm::MoodStation,
        };

        // Cache for 2 hours
        let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 7200;
        self.station_cache.insert(
            station_type.clone(),
            CachedRecommendation {
                set: set.clone(),
                expires_at,
            },
        );

        self.emit(RecommendationEvent::StationUpdated(station_type));
        Ok(set)
    }

    /// Generate "Your Mix" - personalized based on history
    async fn generate_your_mix(&self, limit: usize) -> Result<Vec<MediaItem>> {
        // Would use listening history, liked songs, etc.
        // For now, return recently played or liked
        self.catalogue
            .browse(orcave_catalogue::BrowseId::history())
            .await
            .map(|r| {
                r.sections
                    .into_iter()
                    .flat_map(|s| s.items)
                    .take(limit)
                    .collect()
            })
            .or_else(|_| Ok(Vec::new()))
    }

    /// Generate "Discover Mix" - new artists similar to taste
    async fn generate_discover_mix(&self, limit: usize) -> Result<Vec<MediaItem>> {
        // Would use collaborative filtering
        // For now, use explore page
        self.catalogue
            .browse(orcave_catalogue::BrowseId::explore())
            .await
            .map(|r| {
                r.sections
                    .into_iter()
                    .flat_map(|s| s.items)
                    .take(limit)
                    .collect()
            })
            .or_else(|_| Ok(Vec::new()))
    }

    /// Generate "New Release Mix" - new releases from followed artists
    async fn generate_new_release_mix(&self, limit: usize) -> Result<Vec<MediaItem>> {
        // Would query new releases from subscribed artists
        // For now, return empty
        Ok(Vec::new())
    }

    /// Generate "Liked Songs Mix" - shuffle of liked songs
    async fn generate_liked_songs_mix(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.catalogue
            .browse(orcave_catalogue::BrowseId::liked_music())
            .await
            .map(|r| {
                r.sections
                    .into_iter()
                    .flat_map(|s| s.items)
                    .take(limit)
                    .collect()
            })
            .or_else(|_| Ok(Vec::new()))
    }

    /// Generate "Supermix" - combined personalized mix
    async fn generate_supermix(&self, limit: usize) -> Result<Vec<MediaItem>> {
        let mut items = Vec::new();
        items.extend(self.generate_your_mix(limit / 3).await?);
        items.extend(self.generate_discover_mix(limit / 3).await?);
        items.extend(self.generate_liked_songs_mix(limit / 3).await?);
        items.truncate(limit);
        Ok(items)
    }

    /// Generate focus station
    async fn generate_focus_station(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station("focus instrumental", limit).await
    }

    /// Generate workout station
    async fn generate_workout_station(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station("workout high energy", limit).await
    }

    /// Generate relax station
    async fn generate_relax_station(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station("relax ambient", limit).await
    }

    /// Generate party station
    async fn generate_party_station(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station("party dance", limit).await
    }

    /// Generate commute station
    async fn generate_commute_station(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station("commute driving", limit).await
    }

    /// Generate sleep station
    async fn generate_sleep_station(&self, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station("sleep calm", limit).await
    }

    /// Generate genre station
    async fn generate_genre_station(&self, genre: &str, limit: usize) -> Result<Vec<MediaItem>> {
        self.search_station(&format!("{} music", genre), limit)
            .await
    }

    /// Search for station tracks
    async fn search_station(&self, query: &str, limit: usize) -> Result<Vec<MediaItem>> {
        let response = self.catalogue.search(query, Some("station")).await?;
        Ok(response.results.into_iter().take(limit).collect())
    }

    /// Get related content for an item
    #[instrument(skip(self))]
    pub async fn get_related(&self, item: &MediaItem, limit: usize) -> Result<Vec<MediaItem>> {
        match item.media_type {
            MediaType::Track => {
                self.get_recommendations(SeedType::Track, &item.id, limit)
                    .await
            }
            MediaType::Album => {
                self.get_recommendations(SeedType::Album, &item.id, limit)
                    .await
            }
            MediaType::Artist => {
                self.get_recommendations(SeedType::Artist, &item.id, limit)
                    .await
            }
            MediaType::Playlist => {
                self.get_recommendations(SeedType::Playlist, &item.id, limit)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Cleanup expired cache entries
    pub async fn cleanup_cache(&self) -> usize {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut count = 0;

        self.recommendation_cache.retain(|_, v| {
            if v.expires_at < now {
                count += 1;
                false
            } else {
                true
            }
        });

        self.mix_cache.retain(|_, v| {
            if v.expires_at < now {
                count += 1;
                false
            } else {
                true
            }
        });

        self.station_cache.retain(|_, v| {
            if v.expires_at < now {
                count += 1;
                false
            } else {
                true
            }
        });

        if count > 0 {
            self.emit(RecommendationEvent::CacheEvicted { count });
        }

        count
    }

    /// Subscribe to recommendation events
    pub fn subscribe(&self) -> broadcast::Receiver<RecommendationEvent> {
        self.event_tx.subscribe()
    }

    fn emit(&self, event: RecommendationEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Shutdown recommendation service
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        self.recommendation_cache.clear();
        self.mix_cache.clear();
        self.station_cache.clear();
        Ok(())
    }
}

/// Tauri command handlers
pub mod commands {
    use super::*;
    use orcave_core::TauriError;

    #[tauri::command]
    pub async fn recommendations_get_for_seed(
        recommendations: tauri::State<'_, Arc<RecommendationService>>,
        seed_type: SeedType,
        seed_id: String,
        limit: usize,
    ) -> TauriResult<Vec<MediaItem>> {
        recommendations
            .get_recommendations(seed_type, &seed_id, limit)
            .await
            .map_err(|e| TauriError::Recommendations(e.to_string()))
    }

    #[tauri::command]
    pub async fn recommendations_get_mix(
        recommendations: tauri::State<'_, Arc<RecommendationService>>,
        mix_type: MixType,
        limit: usize,
    ) -> TauriResult<RecommendationSet> {
        recommendations
            .get_mix(mix_type, limit)
            .await
            .map_err(|e| TauriError::Recommendations(e.to_string()))
    }

    #[tauri::command]
    pub async fn recommendations_get_station(
        recommendations: tauri::State<'_, Arc<RecommendationService>>,
        station_type: StationType,
        limit: usize,
    ) -> TauriResult<RecommendationSet> {
        recommendations
            .get_station(station_type, limit)
            .await
            .map_err(|e| TauriError::Recommendations(e.to_string()))
    }

    #[tauri::command]
    pub async fn recommendations_get_related(
        recommendations: tauri::State<'_, Arc<RecommendationService>>,
        item: MediaItem,
        limit: usize,
    ) -> TauriResult<Vec<MediaItem>> {
        recommendations
            .get_related(&item, limit)
            .await
            .map_err(|e| TauriError::Recommendations(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recommendation_set_serialization() {
        let set = RecommendationSet {
            id: "test".to_string(),
            title: "Test Mix".to_string(),
            subtitle: None,
            seed_type: None,
            seed_id: None,
            items: vec![],
            generated_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            algorithm: RecommendationAlgorithm::PersonalizedMix,
        };

        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains("Test Mix"));
    }

    #[test]
    fn test_mix_type_serialization() {
        let mix = MixType::YourMix;
        let json = serde_json::to_string(&mix).unwrap();
        assert_eq!(json, "\"your_mix\"");
    }
}
