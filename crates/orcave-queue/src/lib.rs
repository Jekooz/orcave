//! Orcave Queue - Playback queue management
//!
//! Features:
//! - Queue with add, remove, reorder, clear
//! - History for previous tracks
//! - Repeat modes (off, all, one)
//! - Shuffle with deterministic ordering
//! - Gapless playback coordination
//! - Queue persistence

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tracing::{info, instrument, warn};
use ulid::Ulid;

use orcave_audio::TrackHandle;
use orcave_catalogue::{MediaItem, MediaType};
use orcave_core::TauriResult;
use orcave_settings::RepeatMode;
use std::sync::atomic::{AtomicUsize, Ordering};

pub use orcave_core::SeedType;

#[derive(thiserror::Error, Debug)]
pub enum QueueError {
    #[error("Queue empty")]
    Empty,
    #[error("Index out of bounds: {0}")]
    IndexOutOfBounds(usize),
    #[error("Item not found: {0}")]
    NotFound(String),
    #[error("Audio error: {0}")]
    Audio(#[from] orcave_audio::AudioError),
    #[error("Catalogue error: {0}")]
    Catalogue(#[from] orcave_catalogue::CatalogueError),
    #[error("Settings error: {0}")]
    Settings(#[from] orcave_settings::SettingsError),
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for QueueError {
    fn from(e: anyhow::Error) -> Self {
        QueueError::Internal(e.to_string())
    }
}

/// Queue operation modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    Normal,
    Radio,    // Auto-generated from seed
    Playlist, // Fixed playlist
    Album,    // Album tracks
    Artist,   // Artist top tracks
}

/// Queue item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub queue_id: Ulid,
    pub media_item: MediaItem,
    pub track_handle: TrackHandle,
    pub added_at: chrono::DateTime<chrono::Utc>,
    pub source: QueueSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSource {
    UserAdded,
    Radio,
    Playlist,
    Album,
    Artist,
    Recommendation,
    History,
}

/// Queue event for UI updates
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueueEvent {
    ItemAdded { index: usize, item: QueueItem },
    ItemRemoved { index: usize, item_id: String },
    ItemsReordered { from: usize, to: usize },
    Cleared,
    CurrentIndexChanged { index: usize },
    ModeChanged(QueueMode),
    RepeatModeChanged(RepeatMode),
    ShuffleChanged(bool),
    HistoryAdded(QueueItem),
}

/// Audio playback backend abstraction.
///
/// `QueueService` depends on this trait so tests can inject a mock
/// instead of the real (rodio-backed) `AudioEngine`.
#[async_trait]
pub trait AudioBackend: Send + Sync {
    async fn play(&self, track_handle: TrackHandle) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    fn subscribe(&self) -> broadcast::Receiver<orcave_audio::AudioEvent>;
    fn register_stream_url(&self, track_id: String, stream_url: String);
}

#[async_trait]
impl AudioBackend for orcave_audio::AudioEngine {
    async fn play(&self, track_handle: TrackHandle) -> Result<()> {
        self.play(track_handle).await
    }

    async fn stop(&self) -> Result<()> {
        self.stop().await
    }

    fn subscribe(&self) -> broadcast::Receiver<orcave_audio::AudioEvent> {
        self.subscribe()
    }

    fn register_stream_url(&self, track_id: String, stream_url: String) {
        self.register_stream_url(track_id, stream_url)
    }
}

/// Settings backend abstraction.
///
/// `QueueService` depends on this trait so tests can inject a mock
/// instead of the real (tauri-store-backed) `SettingsService`.
#[async_trait]
pub trait SettingsBackend: Send + Sync {
    async fn get_all(&self) -> Result<orcave_settings::Settings>;
    async fn update_field(&self, field: &str, value: serde_json::Value) -> Result<()>;
}

#[async_trait]
impl SettingsBackend for orcave_settings::SettingsService {
    async fn get_all(&self) -> Result<orcave_settings::Settings> {
        self.get_all().await
    }

    async fn update_field(&self, field: &str, value: serde_json::Value) -> Result<()> {
        self.update_field(field, value).await
    }
}

/// Catalogue backend abstraction.
///
/// `QueueService` depends on this trait so tests can inject a mock
/// instead of the real (network-backed) `CatalogueService`.
#[async_trait]
pub trait CatalogueBackend: Send + Sync {
    async fn get_recommendations(
        &self,
        seed_type: orcave_core::SeedType,
        seed_id: &str,
        limit: usize,
    ) -> Result<Vec<orcave_catalogue::MediaItem>>;
    async fn get_stream_url(
        &self,
        video_id: &str,
        quality: orcave_settings::AudioQuality,
    ) -> Result<String>;
}

#[async_trait]
impl CatalogueBackend for orcave_catalogue::CatalogueService {
    async fn get_recommendations(
        &self,
        seed_type: orcave_core::SeedType,
        seed_id: &str,
        limit: usize,
    ) -> Result<Vec<orcave_catalogue::MediaItem>> {
        self.get_recommendations(seed_type, seed_id, limit).await
    }

    async fn get_stream_url(
        &self,
        video_id: &str,
        quality: orcave_settings::AudioQuality,
    ) -> Result<String> {
        self.get_stream_url(video_id, quality).await
    }
}

/// Queue service
pub struct QueueService {
    audio: Arc<dyn AudioBackend>,
    catalogue: Arc<dyn CatalogueBackend>,
    settings: Arc<dyn SettingsBackend>,

    // Queue state
    queue: Arc<RwLock<VecDeque<QueueItem>>>,
    history: Arc<RwLock<VecDeque<QueueItem>>>,
    current_index: Arc<RwLock<usize>>,
    mode: Arc<RwLock<QueueMode>>,
    repeat_mode: Arc<RwLock<RepeatMode>>,
    shuffle: Arc<RwLock<bool>>,
    shuffle_order: Arc<RwLock<Vec<usize>>>,

    // Radio/auto-play state
    radio_seed: Arc<RwLock<Option<RadioSeed>>>,
    auto_play: Arc<RwLock<bool>>,

    // Cancellation-aware advance: generation id that increments on every
    // advance() call so a stale in-flight advance cannot clobber a newer one.
    pending_advance: Arc<AtomicUsize>,

    // Event bus
    event_tx: broadcast::Sender<QueueEvent>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct RadioSeed {
    seed_type: SeedType,
    seed_id: String,
}

impl QueueService {
    /// Create a new queue service
    #[instrument(skip(audio, catalogue, settings))]
    pub async fn new(
        audio: Arc<dyn AudioBackend>,
        catalogue: Arc<dyn CatalogueBackend>,
        settings: Arc<dyn SettingsBackend>,
    ) -> Result<Self> {
        let settings_data = settings.get_all().await?;
        let (event_tx, _) = broadcast::channel(128);

        let service = Self {
            audio,
            catalogue,
            settings,
            queue: Arc::new(RwLock::new(VecDeque::new())),
            history: Arc::new(RwLock::new(VecDeque::new())),
            current_index: Arc::new(RwLock::new(0)),
            mode: Arc::new(RwLock::new(QueueMode::Normal)),
            repeat_mode: Arc::new(RwLock::new(settings_data.playback_repeat_mode)),
            shuffle: Arc::new(RwLock::new(settings_data.playback_shuffle)),
            shuffle_order: Arc::new(RwLock::new(Vec::new())),
            radio_seed: Arc::new(RwLock::new(None)),
            auto_play: Arc::new(RwLock::new(true)),
            pending_advance: Arc::new(AtomicUsize::new(0)),
            event_tx,
        };

        // Subscribe to audio events for auto-advance
        service.subscribe_to_audio().await;

        info!("Queue service initialized");
        Ok(service)
    }

    /// Subscribe to audio events for auto-advance
    async fn subscribe_to_audio(&self) {
        let mut rx = self.audio.subscribe();
        let service = self.clone();

        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if let orcave_audio::AudioEvent::TrackEnded(_) = event {
                    if let Err(e) = service.advance().await {
                        tracing::warn!("Auto-advance failed: {e}");
                    }
                }
            }
        });
    }

    /// Get current queue snapshot
    pub async fn get_queue(&self) -> Vec<QueueItem> {
        self.queue.read().await.iter().cloned().collect()
    }

    /// Get current history
    pub async fn get_history(&self) -> Vec<QueueItem> {
        self.history.read().await.iter().cloned().collect()
    }

    /// Get current index
    pub async fn current_index(&self) -> usize {
        *self.current_index.read().await
    }

    /// Get current item
    pub async fn current_item(&self) -> Option<QueueItem> {
        let queue = self.queue.read().await;
        let index = *self.current_index.read().await;
        queue.get(index).cloned()
    }

    /// Get queue mode
    pub async fn mode(&self) -> QueueMode {
        *self.mode.read().await
    }

    /// Get repeat mode
    pub async fn repeat_mode(&self) -> RepeatMode {
        *self.repeat_mode.read().await
    }

    /// Get shuffle state
    pub async fn shuffle(&self) -> bool {
        *self.shuffle.read().await
    }

    /// Add item to queue. Resolves and registers the signed stream URL internally
    /// (never in TrackHandle sent to frontend, never across the Tauri boundary).
    #[instrument(skip(self, media_item, track_handle))]
    pub async fn add(
        &self,
        media_item: MediaItem,
        track_handle: TrackHandle,
        source: QueueSource,
    ) -> Result<usize> {
        // Resolve signed stream URL from catalogue and register internally.
        let quality = self.settings.get_all().await?.audio_quality;
        let stream_url = self
            .catalogue
            .get_stream_url(&media_item.id, quality)
            .await
            .with_context(|| format!("Failed to resolve stream URL for track {}", media_item.id))?;
        self.audio
            .register_stream_url(track_handle.id.clone(), stream_url);

        let item = QueueItem {
            id: media_item.id.clone(),
            queue_id: Ulid::new(),
            media_item,
            track_handle,
            added_at: chrono::Utc::now(),
            source,
        };

        let mut queue = self.queue.write().await;
        let index = queue.len();
        queue.push_back(item.clone());

        // Update shuffle order if shuffle is enabled
        if *self.shuffle.read().await {
            self.regenerate_shuffle_order_for_len(queue.len()).await;
        }

        self.emit(QueueEvent::ItemAdded { index, item });
        self.emit_diagnostic("add", queue.len());

        // Auto-play if queue was empty
        let should_play = queue.len() == 1 && *self.auto_play.read().await;
        drop(queue);
        if should_play {
            self.play_index(0, None).await?;
        }

        Ok(index)
    }

    /// Add multiple items. Resolves and registers signed stream URLs internally.
    #[instrument(skip(self, items))]
    pub async fn add_many(&self, items: Vec<(MediaItem, TrackHandle, QueueSource)>) -> Result<()> {
        let mut queue = self.queue.write().await;
        let start_index = queue.len();

        let quality = self.settings.get_all().await?.audio_quality;

        for (i, (media_item, track_handle, source)) in items.into_iter().enumerate() {
            // Resolve signed stream URL from catalogue and register internally.
            let stream_url = self
                .catalogue
                .get_stream_url(&media_item.id, quality)
                .await
                .with_context(|| {
                    format!("Failed to resolve stream URL for track {}", media_item.id)
                })?;
            self.audio
                .register_stream_url(track_handle.id.clone(), stream_url);

            let item = QueueItem {
                id: media_item.id.clone(),
                queue_id: Ulid::new(),
                media_item,
                track_handle,
                added_at: chrono::Utc::now(),
                source,
            };
            queue.push_back(item.clone());
            self.emit(QueueEvent::ItemAdded {
                index: start_index + i,
                item,
            });
        }

        if *self.shuffle.read().await {
            self.regenerate_shuffle_order_for_len(queue.len()).await;
        }

        self.emit_diagnostic("add_many", queue.len());
        Ok(())
    }

    /// Remove item at index
    #[instrument(skip(self))]
    pub async fn remove(&self, index: usize) -> Result<QueueItem> {
        let mut queue = self.queue.write().await;
        if index >= queue.len() {
            return Err(QueueError::IndexOutOfBounds(index).into());
        }

        let item = queue.remove(index).context("Item removed")?;

        // Adjust current index
        let mut current = self.current_index.write().await;
        if index < *current && *current > 0 {
            *current -= 1;
        } else if index == *current && *current >= queue.len() && *current > 0 {
            *current -= 1;
        }

        // Regenerate shuffle order
        if *self.shuffle.read().await {
            self.regenerate_shuffle_order_for_len(queue.len()).await;
        }

        self.emit(QueueEvent::ItemRemoved {
            index,
            item_id: item.id.clone(),
        });
        self.emit_diagnostic("remove", queue.len());

        Ok(item)
    }

    /// Move item from one index to another
    #[instrument(skip(self))]
    pub async fn move_item(&self, from: usize, to: usize) -> Result<()> {
        let mut queue = self.queue.write().await;
        if from >= queue.len() || to >= queue.len() {
            return Err(QueueError::IndexOutOfBounds(from.max(to)).into());
        }

        let item = queue.remove(from).context("Item removed")?;
        queue.insert(to, item);

        if *self.shuffle.read().await {
            self.regenerate_shuffle_order_for_len(queue.len()).await;
        }

        self.emit(QueueEvent::ItemsReordered { from, to });
        Ok(())
    }

    /// Clear queue. Stops any active playback so an orphaned track cannot
    /// keep playing after its queue entry is gone.
    #[instrument(skip(self))]
    pub async fn clear(&self) -> Result<()> {
        self.stop().await.ok();
        self.queue.write().await.clear();
        *self.current_index.write().await = 0;
        self.regenerate_shuffle_order().await;
        self.emit(QueueEvent::Cleared);
        self.emit_diagnostic("clear", 0);
        Ok(())
    }

    /// Play item at index.
    ///
    /// `advance_generation` is `Some(ticket)` when called from `advance()`
    /// (ticket from `fetch_add`), in which case we abort if a newer
    /// advance/stop has started since (`counter != ticket + 1`). It is `None`
    /// for direct calls, in which case we snapshot the counter and abort if it
    /// changes before committing to audio playback.
    #[instrument(skip(self))]
    pub async fn play_index(&self, index: usize, advance_generation: Option<usize>) -> Result<()> {
        let item = {
            let queue = self.queue.read().await;
            if index >= queue.len() {
                return Err(QueueError::IndexOutOfBounds(index).into());
            }
            queue.get(index).context("Item not found")?.clone()
        };

        let generation = match advance_generation {
            Some(ticket) => {
                // Called from advance(): we hold ticket, counter is ticket+1
                // while we are the current advance.
                if self.pending_advance.load(Ordering::Acquire) != ticket + 1 {
                    return Ok(());
                }
                ticket
            }
            None => {
                // Direct call: snapshot current counter; abort if an advance
                // or stop started while we were computing.
                self.pending_advance.load(Ordering::Acquire)
            }
        };

        *self.current_index.write().await = index;

        // Add current to history
        self.add_to_history(item.clone()).await;

        // Re-check generation right before committing to audio playback.
        let current = self.pending_advance.load(Ordering::Acquire);
        let stale = match advance_generation {
            Some(ticket) => current != ticket + 1,
            None => current != generation,
        };
        if stale {
            return Ok(());
        }

        // Play via audio engine
        self.audio.play(item.track_handle.clone()).await?;

        self.emit(QueueEvent::CurrentIndexChanged { index });
        Ok(())
    }

    /// Play current item
    pub async fn play_current(&self) -> Result<()> {
        let index = *self.current_index.read().await;
        self.play_index(index, None).await
    }

    /// Stop playback and invalidate any in-flight advances.
    ///
    /// Increments the advance generation so any stale in-flight `advance()`
    /// aborts before mutating queue or audio state. This is the single point
    /// where audio is stopped and the generation is bumped, making `advance()`
    /// linearizable: one TrackEnded → exactly one advance().
    pub async fn stop(&self) -> Result<()> {
        self.pending_advance.fetch_add(1, Ordering::SeqCst);
        self.audio.stop().await?;
        Ok(())
    }

    /// Advance to next track.
    ///
    /// Generation-aware: if a newer advance() starts while this one is in
    /// flight, the stale call aborts before mutating queue/audio state. This
    /// makes manual Next (while a track is buffering) race-free and guarantees
    /// one TrackEnded event advances exactly once.
    #[instrument(skip(self))]
    pub async fn advance(&self) -> Result<()> {
        // Take a unique, monotonically increasing ticket. After this, the
        // counter is `generation + 1` while we are the current advance.
        let generation = self.pending_advance.fetch_add(1, Ordering::SeqCst);

        let repeat = *self.repeat_mode.read().await;
        let shuffle = *self.shuffle.read().await;

        let next_index = self.calculate_next_index(repeat, shuffle).await?;

        // Abort if a newer advance() or stop() superseded this one before we
        // committed. We are current iff the counter is still generation + 1.
        if self.pending_advance.load(Ordering::Acquire) != generation + 1 {
            return Ok(());
        }

        if let Some(next) = next_index {
            self.play_index(next, Some(generation)).await?;
        } else {
            // Queue ended: only reached when calculate_next_index found no
            // next track (empty queue, or end of shuffle order).
            match repeat {
                RepeatMode::All => {
                    // Loop to beginning (shuffle-aware).
                    let next = if shuffle {
                        self.get_shuffle_index(0).await
                    } else {
                        Some(0)
                    };
                    if let Some(idx) = next {
                        if self.pending_advance.load(Ordering::Acquire) == generation + 1 {
                            self.play_index(idx, Some(generation)).await?;
                        }
                    }
                }
                RepeatMode::One => {
                    // Replay current track.
                    let index = *self.current_index.read().await;
                    let queue = self.queue.read().await;
                    if queue.get(index).is_some() {
                        drop(queue);
                        if self.pending_advance.load(Ordering::Acquire) == generation + 1 {
                            self.play_index(index, Some(generation)).await?;
                        }
                    }
                }
                RepeatMode::Off => {
                    // Stop
                    if self.pending_advance.load(Ordering::Acquire) == generation + 1 {
                        self.stop().await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Go to previous track
    #[instrument(skip(self))]
    pub async fn previous(&self) -> Result<()> {
        let shuffle = *self.shuffle.read().await;
        let current = *self.current_index.read().await;

        let prev_index = if shuffle {
            self.get_shuffle_prev(current).await
        } else if current > 0 {
            Some(current - 1)
        } else {
            None
        };

        if let Some(idx) = prev_index {
            self.play_index(idx, None).await?;
        }

        Ok(())
    }

    /// Calculate next index based on repeat/shuffle mode
    async fn calculate_next_index(
        &self,
        repeat: RepeatMode,
        shuffle: bool,
    ) -> Result<Option<usize>> {
        let queue = self.queue.read().await;
        let current = *self.current_index.read().await;

        if queue.is_empty() {
            return Ok(None);
        }

        if shuffle {
            Ok(self.get_shuffle_next(current).await)
        } else if current + 1 < queue.len() {
            Ok(Some(current + 1))
        } else {
            match repeat {
                RepeatMode::All => Ok(Some(0)),
                RepeatMode::One => Ok(Some(current)),
                RepeatMode::Off => Ok(None),
            }
        }
    }

    /// Add item to history
    async fn add_to_history(&self, item: QueueItem) {
        let mut history = self.history.write().await;
        history.push_front(item.clone());

        // Limit history size
        if history.len() > 100 {
            history.pop_back();
        }

        self.emit(QueueEvent::HistoryAdded(item));
    }

    /// Set repeat mode
    #[instrument(skip(self))]
    pub async fn set_repeat_mode(&self, mode: RepeatMode) -> Result<()> {
        *self.repeat_mode.write().await = mode;
        self.settings
            .update_field("playback_repeat_mode", serde_json::to_value(mode)?)
            .await?;
        self.emit(QueueEvent::RepeatModeChanged(mode));
        Ok(())
    }

    /// Set shuffle
    #[instrument(skip(self))]
    pub async fn set_shuffle(&self, enabled: bool) -> Result<()> {
        *self.shuffle.write().await = enabled;
        self.settings
            .update_field("playback_shuffle", serde_json::to_value(enabled)?)
            .await?;
        self.regenerate_shuffle_order().await;
        self.emit(QueueEvent::ShuffleChanged(enabled));
        Ok(())
    }

    /// Set queue mode
    #[instrument(skip(self))]
    pub async fn set_mode(&self, mode: QueueMode) -> Result<()> {
        *self.mode.write().await = mode;
        self.emit(QueueEvent::ModeChanged(mode));
        Ok(())
    }

    /// Set auto-play
    pub async fn set_auto_play(&self, enabled: bool) {
        *self.auto_play.write().await = enabled;
    }

    /// Regenerate shuffle order
    async fn regenerate_shuffle_order(&self) {
        let queue_len = self.queue.read().await.len();
        self.regenerate_shuffle_order_for_len(queue_len).await;
    }

    async fn regenerate_shuffle_order_for_len(&self, queue_len: usize) {
        let mut order: Vec<usize> = (0..queue_len).collect();

        if queue_len > 1 {
            use rand::seq::SliceRandom;
            order.shuffle(&mut rand::thread_rng());
        }

        *self.shuffle_order.write().await = order;
    }

    /// Get next index in shuffle order
    async fn get_shuffle_next(&self, current: usize) -> Option<usize> {
        let order = self.shuffle_order.read().await;
        let pos = order.iter().position(|&i| i == current)?;
        if pos + 1 < order.len() {
            Some(order[pos + 1])
        } else {
            None
        }
    }

    /// Get previous index in shuffle order
    async fn get_shuffle_prev(&self, current: usize) -> Option<usize> {
        let order = self.shuffle_order.read().await;
        let pos = order.iter().position(|&i| i == current)?;
        if pos > 0 {
            Some(order[pos - 1])
        } else {
            None
        }
    }

    /// Get shuffle index by position
    async fn get_shuffle_index(&self, position: usize) -> Option<usize> {
        let order = self.shuffle_order.read().await;
        order.get(position).copied()
    }

    /// Start radio from seed
    #[instrument(skip(self))]
    pub async fn start_radio(&self, seed_type: SeedType, seed_id: String) -> Result<()> {
        // Get recommendations based on seed
        let recommendations = self
            .catalogue
            .get_recommendations(seed_type, &seed_id, 20)
            .await?;

        // Convert to queue items
        let mut items = Vec::new();
        for media_item in recommendations {
            if media_item.media_type != MediaType::Track {
                warn!(item_id = %media_item.id, media_type = ?media_item.media_type, "Skipping non-track radio recommendation");
                continue;
            }
            let track_handle = TrackHandle {
                id: media_item.id.clone(),
                title: media_item.title.clone(),
                artist: media_item
                    .artists
                    .iter()
                    .map(|artist| artist.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                duration: media_item.duration,
            };
            items.push((media_item, track_handle, QueueSource::Radio));
        }

        self.clear().await?;
        self.add_many(items).await?;
        *self.mode.write().await = QueueMode::Radio;
        *self.radio_seed.write().await = Some(RadioSeed { seed_type, seed_id });

        self.emit(QueueEvent::ModeChanged(QueueMode::Radio));
        Ok(())
    }

    /// Subscribe to queue events
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.event_tx.subscribe()
    }

    fn emit(&self, event: QueueEvent) {
        let _ = self.event_tx.send(event);
    }

    fn emit_diagnostic(&self, _operation: &str, _item_count: usize) {
        // Deliberately no-op: queue events must reflect real queue/audio state.
        // Diagnostic operations are surfaced via DiagnosticEvent::QueueOperation
        // on the app event bus (tauri main), not as fake queue items.
    }

    /// Shutdown queue service
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        self.queue.write().await.clear();
        self.history.write().await.clear();
        Ok(())
    }
}

impl Clone for QueueService {
    fn clone(&self) -> Self {
        Self {
            audio: self.audio.clone(),
            catalogue: self.catalogue.clone(),
            settings: self.settings.clone(),
            queue: self.queue.clone(),
            history: self.history.clone(),
            current_index: self.current_index.clone(),
            mode: self.mode.clone(),
            repeat_mode: self.repeat_mode.clone(),
            shuffle: self.shuffle.clone(),
            shuffle_order: self.shuffle_order.clone(),
            radio_seed: self.radio_seed.clone(),
            auto_play: self.auto_play.clone(),
            pending_advance: self.pending_advance.clone(),
            event_tx: self.event_tx.clone(),
        }
    }
}

/// Tauri command handlers
pub mod commands {
    use super::*;
    use orcave_core::TauriError;

    #[tauri::command]
    pub async fn queue_get(
        queue: tauri::State<'_, Arc<QueueService>>,
    ) -> TauriResult<Vec<QueueItem>> {
        Ok(queue.get_queue().await)
    }

    #[tauri::command]
    pub async fn queue_get_history(
        queue: tauri::State<'_, Arc<QueueService>>,
    ) -> TauriResult<Vec<QueueItem>> {
        Ok(queue.get_history().await)
    }

    #[tauri::command]
    pub async fn queue_add(
        queue: tauri::State<'_, Arc<QueueService>>,
        media_item: orcave_catalogue::MediaItem,
        track_handle: TrackHandle,
        source: QueueSource,
    ) -> TauriResult<usize> {
        queue
            .add(media_item, track_handle, source)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_remove(
        queue: tauri::State<'_, Arc<QueueService>>,
        index: usize,
    ) -> TauriResult<QueueItem> {
        queue
            .remove(index)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_move(
        queue: tauri::State<'_, Arc<QueueService>>,
        from: usize,
        to: usize,
    ) -> TauriResult<()> {
        queue
            .move_item(from, to)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_clear(queue: tauri::State<'_, Arc<QueueService>>) -> TauriResult<()> {
        queue
            .clear()
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_play_index(
        queue: tauri::State<'_, Arc<QueueService>>,
        index: usize,
    ) -> TauriResult<()> {
        queue
            .play_index(index, None)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_next(queue: tauri::State<'_, Arc<QueueService>>) -> TauriResult<()> {
        queue
            .advance()
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_previous(queue: tauri::State<'_, Arc<QueueService>>) -> TauriResult<()> {
        queue
            .previous()
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_set_repeat(
        queue: tauri::State<'_, Arc<QueueService>>,
        mode: RepeatMode,
    ) -> TauriResult<()> {
        queue
            .set_repeat_mode(mode)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_set_shuffle(
        queue: tauri::State<'_, Arc<QueueService>>,
        enabled: bool,
    ) -> TauriResult<()> {
        queue
            .set_shuffle(enabled)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }

    #[tauri::command]
    pub async fn queue_start_radio(
        queue: tauri::State<'_, Arc<QueueService>>,
        seed_type: SeedType,
        seed_id: String,
    ) -> TauriResult<()> {
        queue
            .start_radio(seed_type, seed_id)
            .await
            .map_err(|e| TauriError::Queue(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_queue_item_serialization() {
        let item = QueueItem {
            id: "test".to_string(),
            queue_id: Ulid::new(),
            media_item: MediaItem {
                id: "test".to_string(),
                browse_id: None,
                media_type: MediaType::Track,
                title: "Test".to_string(),
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
            },
            track_handle: TrackHandle {
                id: "test".to_string(),
                title: "Test".to_string(),
                artist: "Test".to_string(),
                duration: None,
            },
            added_at: chrono::Utc::now(),
            source: QueueSource::UserAdded,
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("test"));
    }

    #[test]
    fn test_queue_mode_serialization() {
        for (mode, expected) in [
            (QueueMode::Normal, "\"normal\""),
            (QueueMode::Radio, "\"radio\""),
            (QueueMode::Playlist, "\"playlist\""),
            (QueueMode::Album, "\"album\""),
            (QueueMode::Artist, "\"artist\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), expected);
            assert_eq!(serde_json::from_str::<QueueMode>(expected).unwrap(), mode);
        }
    }

    #[test]
    fn test_repeat_mode_serialization() {
        for (mode, expected) in [
            (RepeatMode::Off, "\"off\""),
            (RepeatMode::All, "\"all\""),
            (RepeatMode::One, "\"one\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), expected);
            assert_eq!(serde_json::from_str::<RepeatMode>(expected).unwrap(), mode);
        }
    }

    #[test]
    fn test_queue_error_display() {
        assert_eq!(
            QueueError::IndexOutOfBounds(5).to_string(),
            "Index out of bounds: 5"
        );
        assert_eq!(QueueError::Empty.to_string(), "Queue empty");
        assert_eq!(
            QueueError::NotFound("track-x".to_string()).to_string(),
            "Item not found: track-x"
        );
    }

    #[test]
    fn test_queue_source_serialization() {
        for (source, expected) in [
            (QueueSource::UserAdded, "\"user_added\""),
            (QueueSource::Radio, "\"radio\""),
            (QueueSource::Recommendation, "\"recommendation\""),
            (QueueSource::History, "\"history\""),
        ] {
            assert_eq!(serde_json::to_string(&source).unwrap(), expected);
            assert_eq!(
                serde_json::from_str::<QueueSource>(expected).unwrap(),
                source
            );
        }
    }

    // --- Stream URL leak prevention tests ---

    #[test]
    fn test_queue_item_serialization_excludes_stream_url() {
        let item = QueueItem {
            id: "test".to_string(),
            queue_id: Ulid::new(),
            media_item: MediaItem {
                id: "test".to_string(),
                browse_id: None,
                media_type: MediaType::Track,
                title: "Test".to_string(),
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
            },
            track_handle: TrackHandle {
                id: "test".to_string(),
                title: "Test".to_string(),
                artist: "Test".to_string(),
                duration: None,
            },
            added_at: chrono::Utc::now(),
            source: QueueSource::UserAdded,
        };

        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("test"));
        assert!(
            !json.contains("stream_url"),
            "stream_url must not appear in QueueItem: {json}"
        );
        assert!(
            !json.contains("youtube.com"),
            "signed URL host must not appear: {json}"
        );
    }

    #[test]
    fn test_queue_item_serialization_rejects_secret_url() {
        // Inject a fake secret-bearing stream URL, verify it never appears in serialized QueueItem.
        let item = QueueItem {
            id: "item-xyz".to_string(),
            queue_id: Ulid::new(),
            media_item: MediaItem {
                id: "item-xyz".to_string(),
                browse_id: None,
                media_type: MediaType::Track,
                title: "Track Title".to_string(),
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
            },
            track_handle: TrackHandle {
                id: "track-xyz".to_string(),
                title: "Track Title".to_string(),
                artist: "Track Artist".to_string(),
                duration: None,
            },
            added_at: chrono::Utc::now(),
            source: QueueSource::UserAdded,
        };
        let _secret_url = "https://youtube.com/watch?v=secret&signature=abc123&token=supersecret";
        let json = serde_json::to_string(&item).unwrap();
        assert!(
            !json.contains("supersecret"),
            "secret token must not appear: {json}"
        );
        assert!(
            !json.contains("stream_url"),
            "stream_url key must not appear: {json}"
        );
        assert!(
            !json.contains("youtube.com"),
            "signed URL host must not appear: {json}"
        );
    }

    #[test]
    fn test_queue_event_item_added_excludes_stream_url() {
        let item = QueueItem {
            id: "event-item".to_string(),
            queue_id: Ulid::new(),
            media_item: MediaItem {
                id: "event-item".to_string(),
                browse_id: None,
                media_type: MediaType::Track,
                title: "Event Track".to_string(),
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
            },
            track_handle: TrackHandle {
                id: "event-track".to_string(),
                title: "Event Track".to_string(),
                artist: "Event Artist".to_string(),
                duration: None,
            },
            added_at: chrono::Utc::now(),
            source: QueueSource::UserAdded,
        };
        let event = QueueEvent::ItemAdded { index: 0, item };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("stream_url"),
            "stream_url must not appear in ItemAdded: {json}"
        );
        assert!(
            !json.contains("youtube.com"),
            "signed URL host must not appear in ItemAdded: {json}"
        );
    }

    // --- Mock backends for generation-aware advance tests ---

    struct MockAudio {
        played: std::sync::Mutex<Vec<String>>,
        stopped: std::sync::atomic::AtomicUsize,
    }

    impl MockAudio {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                played: std::sync::Mutex::new(Vec::new()),
                stopped: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl AudioBackend for MockAudio {
        async fn play(&self, track_handle: TrackHandle) -> Result<()> {
            self.played.lock().unwrap().push(track_handle.id.clone());
            Ok(())
        }

        async fn stop(&self) -> Result<()> {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<orcave_audio::AudioEvent> {
            let (_, rx) = broadcast::channel(16);
            rx
        }

        fn register_stream_url(&self, _track_id: String, _stream_url: String) {}
    }

    struct MockCatalogue;

    #[async_trait]
    impl CatalogueBackend for MockCatalogue {
        async fn get_recommendations(
            &self,
            _seed_type: orcave_core::SeedType,
            _seed_id: &str,
            _limit: usize,
        ) -> Result<Vec<orcave_catalogue::MediaItem>> {
            Ok(vec![])
        }

        async fn get_stream_url(
            &self,
            _video_id: &str,
            _quality: orcave_settings::AudioQuality,
        ) -> Result<String> {
            Ok("http://mock.stream".to_string())
        }
    }

    struct MockSettings;

    #[async_trait]
    impl SettingsBackend for MockSettings {
        async fn get_all(&self) -> Result<orcave_settings::Settings> {
            Ok(orcave_settings::Settings::default())
        }

        async fn update_field(&self, _field: &str, _value: serde_json::Value) -> Result<()> {
            Ok(())
        }
    }

    fn make_queue_item(id: &str) -> QueueItem {
        QueueItem {
            id: id.to_string(),
            queue_id: Ulid::new(),
            media_item: MediaItem {
                id: id.to_string(),
                browse_id: None,
                media_type: MediaType::Track,
                title: id.to_string(),
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
            },
            track_handle: TrackHandle {
                id: id.to_string(),
                title: id.to_string(),
                artist: "Artist".to_string(),
                duration: None,
            },
            added_at: chrono::Utc::now(),
            source: QueueSource::UserAdded,
        }
    }

    async fn new_test_service() -> (Arc<QueueService>, Arc<MockAudio>) {
        let audio = MockAudio::new();
        let audio_clone = audio.clone();
        let service = QueueService::new(audio, Arc::new(MockCatalogue), Arc::new(MockSettings))
            .await
            .unwrap();
        let service = Arc::new(service);
        (service, audio_clone)
    }

    async fn add_tracks(service: &QueueService, count: usize) {
        for i in 0..count {
            let item = make_queue_item(&format!("track-{i}"));
            service
                .add(
                    item.media_item.clone(),
                    item.track_handle.clone(),
                    QueueSource::UserAdded,
                )
                .await
                .unwrap();
        }
    }

    // Test 1: generation-aware advance — a stale advance aborts when a newer one superseded it.
    #[tokio::test]
    async fn test_advance_generation_aware_stale_aborts() {
        let (service, audio) = new_test_service().await;
        add_tracks(&service, 3).await;

        // add_tracks auto-played track 0 (queue was empty on first add).
        assert_eq!(*audio.played.lock().unwrap(), vec!["track-0"]);
        assert_eq!(service.current_index().await, 0);

        service.advance().await.unwrap();
        assert_eq!(*audio.played.lock().unwrap(), vec!["track-0", "track-1"]);
    }

    // Test 2: TrackEnded advances exactly once.
    #[tokio::test]
    async fn test_track_ended_advances_once() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 2).await;

        service.play_index(0, None).await.unwrap();

        // Simulate a single TrackEnded → advance() once.
        service.advance().await.unwrap();
        assert_eq!(service.current_index().await, 1);

        // A second advance should still move to the next track (track 1 has no next since repeat=Off).
        // With repeat Off and only 2 tracks, advancing from track 1 stops.
        service.advance().await.unwrap();
        // Index stays at 1 (no next; RepeatMode::Off stops).
        assert_eq!(service.current_index().await, 1);
    }

    // Test 3: Manual Next while buffering — repeated advance() calls don't
    // double-commit past the queue end; the second advance stops at the last track.
    #[tokio::test]
    async fn test_manual_next_while_buffering_race_free() {
        let (service, audio) = new_test_service().await;
        add_tracks(&service, 2).await;

        // add_tracks auto-played track 0.
        assert_eq!(*audio.played.lock().unwrap(), vec!["track-0"]);
        assert_eq!(service.current_index().await, 0);

        // Fire two advances back-to-back. First advances to track 1; second
        // reaches queue end (repeat Off) and stops, leaving index at 1.
        service.advance().await.unwrap();
        service.advance().await.unwrap();

        assert_eq!(*audio.played.lock().unwrap(), vec!["track-0", "track-1"]);
        assert_eq!(service.current_index().await, 1);
    }

    // Test 4: Repeat All loops.
    #[tokio::test]
    async fn test_repeat_all_loops() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 2).await;

        service.set_repeat_mode(RepeatMode::All).await.unwrap();
        service.play_index(0, None).await.unwrap();

        service.advance().await.unwrap();
        assert_eq!(service.current_index().await, 1);

        service.advance().await.unwrap();
        assert_eq!(service.current_index().await, 0);
    }

    // Test 5: Repeat One replays current.
    #[tokio::test]
    async fn test_repeat_one_replays_current() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 2).await;

        service.set_repeat_mode(RepeatMode::One).await.unwrap();
        service.play_index(1, None).await.unwrap();

        service.advance().await.unwrap();
        assert_eq!(service.current_index().await, 1);

        service.advance().await.unwrap();
        assert_eq!(service.current_index().await, 1);
    }

    // Test 6: stop() increments generation and stops audio.
    #[tokio::test]
    async fn test_stop_increments_generation() {
        let (service, audio) = new_test_service().await;
        add_tracks(&service, 1).await;

        service.play_index(0, None).await.unwrap();

        let stops_before = audio.stopped.load(Ordering::SeqCst);
        service.stop().await.unwrap();
        let stops_after = audio.stopped.load(Ordering::SeqCst);
        assert_eq!(stops_after, stops_before + 1);
    }

    // Test 7: shuffle advances through all tracks.
    #[tokio::test]
    async fn test_shuffle_advances() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 5).await;

        service.set_shuffle(true).await.unwrap();

        // Pick a starting index that is not at the end of the shuffle order.
        let order = service.shuffle_order.read().await.clone();
        let start_idx = if order.len() > 1 { order[0] } else { 0 };

        service.play_index(start_idx, None).await.unwrap();

        let start = service.current_index().await;
        let mut seen = std::collections::HashSet::new();
        seen.insert(start);
        let mut advanced = false;
        for _ in 0..20 {
            let before = service.current_index().await;
            service.advance().await.unwrap();
            let after = service.current_index().await;
            if after != before {
                advanced = true;
                seen.insert(after);
            } else {
                break; // reached end of shuffle order
            }
        }
        assert!(advanced, "shuffle advance must move at least once");
        assert!(seen.len() > 1, "shuffle should visit multiple tracks");
    }

    // --- Constraint tests: queue invariants ---

    /// currentIndex must always be within bounds [0, queue.len()).
    /// After any operation, current_index <= queue.len() - 1 when queue non-empty,
    /// and current_index == 0 when queue empty.
    #[tokio::test]
    async fn test_constraint_current_index_bounds() {
        let (service, _audio) = new_test_service().await;

        // Empty queue: index must be 0.
        assert_eq!(service.current_index().await, 0);
        assert!(service.current_item().await.is_none());

        // Add 5 tracks.
        add_tracks(&service, 5).await;
        let queue = service.get_queue().await;
        assert_eq!(queue.len(), 5);

        // Index must be < len.
        let idx = service.current_index().await;
        assert!(
            idx < queue.len(),
            "current_index {idx} must be < queue len {}",
            queue.len()
        );

        // Remove all tracks one by one; after each removal index must stay valid.
        while !service.get_queue().await.is_empty() {
            service.remove(0).await.unwrap();
            let q = service.get_queue().await;
            let idx = service.current_index().await;
            if q.is_empty() {
                assert_eq!(idx, 0, "empty queue index must be 0, got {idx}");
            } else {
                assert!(
                    idx < q.len(),
                    "current_index {idx} must be < queue len {}",
                    q.len()
                );
            }
        }
    }

    /// No duplicate queue_ids may exist in the queue at any time.
    /// Each add() generates a fresh Ulid, so queue_ids are unique by construction.
    #[tokio::test]
    async fn test_constraint_no_duplicate_queue_ids() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 10).await;

        let queue = service.get_queue().await;
        let ids: Vec<_> = queue.iter().map(|i| i.queue_id).collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(*id), "duplicate queue_id {id} found in queue");
        }
        assert_eq!(ids.len(), 10, "queue should contain 10 items");
    }

    /// RepeatMode::Off: advancing past the last track stops at the last track.
    /// RepeatMode::All: advancing past the last track loops to index 0.
    /// RepeatMode::One: advancing replays the current track.
    #[tokio::test]
    async fn test_constraint_repeat_mode_behavior() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 3).await;

        // Repeat Off: advance from last track stops.
        service.set_repeat_mode(RepeatMode::Off).await.unwrap();
        service.play_index(2, None).await.unwrap();
        assert_eq!(service.current_index().await, 2);
        service.advance().await.unwrap();
        assert_eq!(
            service.current_index().await,
            2,
            "Repeat Off must stop at last track"
        );

        // Repeat All: advance from last track loops to 0.
        service.set_repeat_mode(RepeatMode::All).await.unwrap();
        service.play_index(2, None).await.unwrap();
        assert_eq!(service.current_index().await, 2);
        service.advance().await.unwrap();
        assert_eq!(
            service.current_index().await,
            0,
            "Repeat All must loop to 0"
        );

        // Repeat One: advance replays current track (only applies at queue end).
        service.set_repeat_mode(RepeatMode::One).await.unwrap();
        service.play_index(2, None).await.unwrap(); // last track
        assert_eq!(service.current_index().await, 2);
        service.advance().await.unwrap();
        assert_eq!(
            service.current_index().await,
            2,
            "Repeat One must replay current"
        );
    }

    /// Shuffle must preserve all tracks: after adding N tracks and enabling
    /// shuffle, the shuffle order must contain exactly N unique indices covering [0, N).
    #[tokio::test]
    async fn test_constraint_shuffle_preserves_all_tracks() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 7).await;

        service.set_shuffle(true).await.unwrap();

        let order = service.shuffle_order.read().await.clone();
        assert_eq!(
            order.len(),
            7,
            "shuffle order length must equal queue length"
        );

        let mut seen = std::collections::HashSet::new();
        for &idx in &order {
            assert!(
                seen.insert(idx),
                "shuffle order contains duplicate index {idx}"
            );
            assert!(idx < 7, "shuffle index {idx} out of range [0, 7)");
        }
        assert_eq!(seen.len(), 7, "shuffle order must cover all 7 indices");
    }

    /// Adding to an empty queue auto-plays track 0.
    #[tokio::test]
    async fn test_constraint_auto_play_on_empty_queue() {
        let (service, audio) = new_test_service().await;

        // Queue starts empty, no auto-play yet.
        assert_eq!(service.get_queue().await.len(), 0);

        // First add auto-plays track 0.
        let item = make_queue_item("track-first");
        service
            .add(
                item.media_item.clone(),
                item.track_handle.clone(),
                QueueSource::UserAdded,
            )
            .await
            .unwrap();
        assert_eq!(service.current_index().await, 0);
        assert_eq!(*audio.played.lock().unwrap(), vec!["track-first"]);

        // Second add does NOT auto-play (queue already has items).
        let item = make_queue_item("track-second");
        service
            .add(
                item.media_item.clone(),
                item.track_handle.clone(),
                QueueSource::UserAdded,
            )
            .await
            .unwrap();
        assert_eq!(*audio.played.lock().unwrap(), vec!["track-first"]);
        assert_eq!(service.current_index().await, 0);
    }

    /// Clear() resets current_index to 0 and empties the queue.
    #[tokio::test]
    async fn test_constraint_clear_resets_state() {
        let (service, _audio) = new_test_service().await;
        add_tracks(&service, 5).await;
        service.play_index(3, None).await.unwrap();
        assert_eq!(service.current_index().await, 3);
        assert!(!service.get_queue().await.is_empty());

        service.clear().await.unwrap();
        assert!(service.get_queue().await.is_empty());
        assert_eq!(
            service.current_index().await,
            0,
            "clear must reset index to 0"
        );
    }
}
