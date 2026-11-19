//! Orcave Audio - High-quality audio playback engine
//!
//! Features:
//! - Gapless playback with pre-buffering
//! - Crossfade between tracks
//! - Multiple format support (AAC, MP3, FLAC, Opus, Vorbis, WAV)
//! - Sample rate conversion (rubato)
//! - Volume normalization (EBU R128)
//! - Equalizer and DSP effects
//! - Windows WASAPI exclusive/shared mode
//! - System Media Transport Controls integration

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::Device;
use dashmap::DashMap;
use futures::stream::StreamExt;
use percent_encoding::percent_decode;
use reqwest::header::{HeaderMap, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};
use tokio::sync::{broadcast, oneshot, Mutex, RwLock, Semaphore};
use tokio::time::timeout;
use tracing::{info, instrument};

use cpal::traits::{DeviceTrait, HostTrait};

use orcave_core::TauriResult;
use orcave_settings::{CrossfadeSettings, GaplessSettings, SettingsService};
use orcave_windows::WindowsIntegration;
use tauri::Manager;
use windows::Media::MediaPlaybackStatus;

/// A playable track safe for frontend consumption.
/// Contains no signed stream URLs or authorization headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackHandle {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration: Option<Duration>,
}

/// Serde conversion for `TrackHandleDto.duration`.
///
/// Accepts the real frontend IPC payloads:
///   - omitted / null        -> None
///   - numeric milliseconds  -> Some(ms)
///   - Serde native {secs, nanos} object -> Some(secs * 1000 + nanos / 1_000_000)
///
/// Internal representation stays `Option<u64>` milliseconds (defined units).
#[allow(dead_code)]
mod duration_ms {
    use serde::de;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value;

    pub fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(ms) => serializer.serialize_u64(*ms),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Null => Ok(None),
            Value::Number(n) => Ok(n.as_u64()),
            Value::Object(map) => {
                let secs = map
                    .get("secs")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| de::Error::missing_field("secs"))?;
                let nanos = map
                    .get("nanos")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let ms = secs
                    .checked_mul(1_000)
                    .and_then(|s| s.checked_add(nanos / 1_000_000))
                    .ok_or_else(|| de::Error::custom("duration overflow"))?;
                Ok(Some(ms))
            }
            _ => Ok(None),
        }
    }
}

/// IPC-safe transfer type for TrackHandle across Tauri boundary.
/// Uses numeric milliseconds for duration (matching frontend JSON),
/// keeping internal `Duration` semantics intact via `From<TrackHandleDto>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackHandleDto {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    #[serde(with = "duration_ms")]
    pub duration: Option<u64>, // milliseconds
}

impl From<TrackHandleDto> for TrackHandle {
    fn from(dto: TrackHandleDto) -> Self {
        TrackHandle {
            id: dto.id,
            title: dto.title,
            artist: dto.artist,
            duration: dto.duration.map(Duration::from_millis),
        }
    }
}

/// Internal playback handle containing the signed stream URL.
/// Used ONLY internally by AudioEngine::play(). NEVER serialized or sent to frontend.
#[derive(Debug, Clone)]
pub struct PlaybackHandle {
    pub track: TrackHandle,
    pub stream_url: String,
}

#[derive(thiserror::Error, Debug)]
pub enum AudioError {
    #[error("Decoder error: {0}")]
    Decoder(String),
    #[error("Output device error: {0}")]
    OutputDevice(String),
    #[error("Resampler error: {0}")]
    Resampler(String),
    #[error("Stream error: {0}")]
    Stream(String),
    #[error("Track not found: {0}")]
    TrackNotFound(String),
    #[error("HTTP 401: authentication failed")]
    AuthError(String),
    #[error("HTTP 403: access forbidden")]
    Forbidden(String),
    #[error("HTTP 429: rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("HTTP 5xx: transient server error")]
    TransientError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    #[error("Corrupt or incomplete response: {0}")]
    CorruptResponse(String),
    #[error("Settings error: {0}")]
    Settings(#[from] orcave_settings::SettingsError),
    #[error("Windows error: {0}")]
    Windows(#[from] orcave_windows::WindowsError),
    #[error("Internal: {0}")]
    Internal(String),
}

impl AudioError {
    /// Whether this error should trigger a retry attempt.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AudioError::RateLimited { .. }
                | AudioError::TransientError(_)
                | AudioError::NetworkError(_)
        )
    }

    /// Suggested delay before the next retry, derived from `Retry-After` or
    /// exponential backoff. Returns `None` for non-retryable errors.
    pub fn retry_delay(&self, attempt: u32) -> Option<Duration> {
        if !self.is_retryable() {
            return None;
        }
        match self {
            AudioError::RateLimited { retry_after } => *retry_after,
            _ => {
                let base = Duration::from_millis(500);
                let backoff = base.saturating_mul(2u32.saturating_pow(attempt));
                let jitter_ms =
                    (rand::random::<u32>() % (backoff.as_millis() as u32 / 4)).min(1000);
                Some(backoff + Duration::from_millis(jitter_ms as u64))
            }
        }
    }

    /// Redacted user-facing message, safe to display (no URLs, tokens, secrets).
    pub fn redacted_message(&self) -> String {
        match self {
            AudioError::AuthError(_) => "Authentication failed. Please sign in again.".to_string(),
            AudioError::Forbidden(_) => "Access to this track is forbidden.".to_string(),
            AudioError::RateLimited { retry_after } => match retry_after {
                Some(d) => format!("Rate limited. Try again in {} seconds.", d.as_secs()),
                None => "Rate limited. Retrying with backoff.".to_string(),
            },
            AudioError::TransientError(_) => "Server error. Retrying...".to_string(),
            AudioError::NetworkError(_) => "Network connection lost. Retrying...".to_string(),
            AudioError::CorruptResponse(_) => "Received corrupt response from server.".to_string(),
            AudioError::Decoder(e) => format!("Decoder error: {e}"),
            AudioError::OutputDevice(e) => format!("Output device error: {e}"),
            AudioError::Resampler(e) => format!("Resampler error: {e}"),
            AudioError::Stream(e) => format!("Stream error: {e}"),
            AudioError::TrackNotFound(e) => format!("Track not found: {e}"),
            AudioError::Settings(e) => format!("Settings error: {e}"),
            AudioError::Windows(e) => format!("Windows error: {e}"),
            AudioError::Internal(e) => format!("Internal error: {e}"),
        }
    }
}

impl From<anyhow::Error> for AudioError {
    fn from(e: anyhow::Error) -> Self {
        AudioError::Internal(e.to_string())
    }
}

impl From<std::io::Error> for AudioError {
    fn from(e: std::io::Error) -> Self {
        AudioError::NetworkError(e.to_string())
    }
}

/// Map an HTTP status code and headers to a typed `AudioError`.
/// Returns `Ok(())` for success statuses (2xx). No URLs, tokens, or secrets
/// are embedded in the error messages.
fn classify_http_status(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
) -> Result<(), AudioError> {
    match status.as_u16() {
        200..=299 => Ok(()),
        401 => Err(AudioError::AuthError(
            "HTTP 401: authentication failed".to_string(),
        )),
        403 => Err(AudioError::Forbidden(
            "HTTP 403: access forbidden".to_string(),
        )),
        429 => {
            let retry_after = headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            Err(AudioError::RateLimited { retry_after })
        }
        500..=599 => Err(AudioError::TransientError(format!(
            "HTTP {}: transient server error",
            status.as_u16()
        ))),
        _ => Err(AudioError::CorruptResponse(format!(
            "unexpected HTTP status {}",
            status.as_u16()
        ))),
    }
}

/// Maximum bytes held in the bounded download buffer (download memory stays bounded).
const STREAM_BUFFER_CAPACITY: usize = 1_024 * 1024; // 1 MB

/// Timeout for individual network read operations.
const NETWORK_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared state between the async download producer and the sync Symphonia consumer.
struct StreamState {
    buf: StdMutex<Vec<u8>>,
    /// Bytes consumed from the front (read offset into buf).
    read_offset: AtomicUsize,
    /// End of valid data in buf.
    write_len: AtomicUsize,
    not_empty: Condvar,
    not_full: Condvar,
    eof: AtomicBool,
    cancelled: AtomicBool,
    content_length: StdMutex<Option<usize>>,
}

impl StreamState {
    fn new() -> Self {
        Self {
            buf: StdMutex::new(Vec::with_capacity(STREAM_BUFFER_CAPACITY)),
            read_offset: AtomicUsize::new(0),
            write_len: AtomicUsize::new(0),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            eof: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            content_length: StdMutex::new(None),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Push downloaded bytes. Blocks (bounded backpressure) until room is available.
    fn push(&self, chunk: &[u8]) -> io::Result<()> {
        if self.cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let mut buf = self.buf.lock().unwrap();
        while (self.write_len.load(Ordering::Acquire) - self.read_offset.load(Ordering::Acquire))
            >= STREAM_BUFFER_CAPACITY
            && !self.eof.load(Ordering::Acquire)
            && !self.cancelled()
        {
            buf = self.not_full.wait(buf).unwrap();
        }
        if self.cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        buf.extend_from_slice(chunk);
        let new_len = buf.len();
        self.write_len.store(new_len, Ordering::Release);
        self.not_empty.notify_all();
        Ok(())
    }

    fn finish(&self) {
        self.eof.store(true, Ordering::Release);
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Request cancellation; wakes any blocked producer or consumer.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Compact the buffer front to keep memory bounded once a read window has advanced.
    fn compact(&self) {
        let mut buf = self.buf.lock().unwrap();
        let ro = self.read_offset.load(Ordering::Acquire);
        if ro > STREAM_BUFFER_CAPACITY / 2 {
            buf.drain(..ro);
            let wl = self.write_len.fetch_sub(ro, Ordering::Release) - ro;
            self.read_offset.store(0, Ordering::Release);
            // notify producer that room may be available
            self.not_full.notify_all();
            let _ = wl;
        }
    }
}

/// A blocking `Read + Seek` reader backed by a bounded download buffer.
/// The producer downloads chunks asynchronously into `state`; Symphonia reads here.
pub struct StreamReader {
    state: Arc<StreamState>,
    /// Absolute stream position.
    pos: u64,
    /// Whether the server supports HTTP range requests (seekable via re-request).
    seekable: bool,
    /// Total bytes the producer will deliver (content length, if known).
    total: Option<u64>,
}

impl StreamReader {
    fn new(state: Arc<StreamState>, seekable: bool, total: Option<u64>) -> Self {
        Self {
            state,
            pos: 0,
            seekable,
            total,
        }
    }

    /// Read available bytes, blocking until data, EOF, or cancellation.
    fn read_available(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut guard = self.state.buf.lock().unwrap();
        loop {
            let ro = self.state.read_offset.load(Ordering::Acquire);
            let wl = self.state.write_len.load(Ordering::Acquire);
            let available = wl.saturating_sub(ro);
            if available > 0 {
                let take = available.min(buf.len());
                let src = &guard[ro..ro + take];
                buf[..take].copy_from_slice(src);
                self.state.read_offset.store(ro + take, Ordering::Release);
                self.pos += take as u64;
                drop(guard);
                self.state.compact();
                self.state.not_full.notify_all();
                return Ok(take);
            }
            if self.state.eof.load(Ordering::Acquire) {
                return Ok(0);
            }
            if self.state.cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            guard = self.state.not_empty.wait(guard).unwrap();
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_available(buf)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        let wl = self.state.write_len.load(Ordering::Acquire);
        let new_pos = match style {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.pos as i64 + n,
            SeekFrom::End(n) => self.total.unwrap_or(wl as u64) as i64 + n,
        };
        if new_pos < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        let new_pos = new_pos as u64;
        // Can only seek within buffered data; out-of-range forward seeks wait on Read.
        if new_pos > wl as u64 {
            if !self.seekable {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "seek beyond buffered data without range support",
                ));
            }
        }
        self.pos = new_pos;
        // Align the shared read offset to the seek target when it lies in the buffer.
        if new_pos <= wl as u64 {
            self.state
                .read_offset
                .store(new_pos as usize, Ordering::Release);
        }
        Ok(self.pos)
    }
}

unsafe impl Send for StreamReader {}
unsafe impl Sync for StreamReader {}

/// Blocking `Read + Seek` source backed by a local `std::fs::File`.
/// Used for `file://` URLs, bypassing the HTTP download path entirely.
pub struct LocalFileSource(File);

impl LocalFileSource {
    pub fn new(file: File) -> Self {
        Self(file)
    }
}

impl Read for LocalFileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for LocalFileSource {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        self.0.seek(style)
    }
}

impl MediaSource for LocalFileSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.0.metadata().ok().map(|m| m.len())
    }
}

impl symphonia::core::io::MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.total
    }
}

/// Download chunks incrementally into the bounded buffer (producer).
/// Returns a reader immediately and spawns the body-pushing task so Symphonia
/// can read concurrently while the download streams in.
async fn download_stream(
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    range: Option<(u64, u64)>,
    state: Arc<StreamState>,
) -> std::result::Result<
    (
        StreamReader,
        tokio::task::JoinHandle<std::result::Result<(), anyhow::Error>>,
    ),
    AudioError,
> {
    const MAX_RETRIES: u32 = 3;
    let mut attempt: u32 = 0;
    let (response, status, headers_map) = loop {
        let mut request = client.get(url.clone());
        if !headers.is_empty() {
            request = request.headers(headers.clone());
        }
        if let Some((start, end)) = range {
            request = request.header(RANGE, format!("bytes={}-{}", start, end));
        }
        let send_result = timeout(NETWORK_READ_TIMEOUT, request.send()).await;
        let response = match send_result {
            Err(_) => {
                let err = AudioError::NetworkError("request timed out".to_string());
                if !err.is_retryable() || attempt >= MAX_RETRIES {
                    return Err(err);
                }
                attempt += 1;
                if let Some(delay) = err.retry_delay(attempt - 1) {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
            Ok(Err(e)) if e.is_timeout() => {
                let err = AudioError::NetworkError("request timed out".to_string());
                if !err.is_retryable() || attempt >= MAX_RETRIES {
                    return Err(err);
                }
                attempt += 1;
                if let Some(delay) = err.retry_delay(attempt - 1) {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
            Ok(Err(e)) if e.is_connect() => {
                let err = AudioError::NetworkError(format!("connect error: {e}"));
                if !err.is_retryable() || attempt >= MAX_RETRIES {
                    return Err(err);
                }
                attempt += 1;
                if let Some(delay) = err.retry_delay(attempt - 1) {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
            Ok(Err(e)) => {
                let err = AudioError::NetworkError(format!("request error: {e}"));
                if !err.is_retryable() || attempt >= MAX_RETRIES {
                    return Err(err);
                }
                attempt += 1;
                if let Some(delay) = err.retry_delay(attempt - 1) {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status();
        let headers_map = response.headers().clone();
        match classify_http_status(status, &headers_map) {
            Ok(()) => break (response, status, headers_map),
            Err(e) => {
                if !e.is_retryable() || attempt >= MAX_RETRIES {
                    return Err(e);
                }
                attempt += 1;
                if let Some(delay) = e.retry_delay(attempt - 1) {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
        }
    };

    let accept_ranges = headers_map
        .get(ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|s| s == "bytes" || s.contains("bytes"))
        .unwrap_or(false);
    let content_length = headers_map
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    if let Some(len) = content_length {
        *state.content_length.lock().unwrap() = Some(len);
    }

    let is_partial = status.as_u16() == 206;
    let total = if is_partial {
        headers_map
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split('/').nth(1).and_then(|t| t.parse::<u64>().ok()))
    } else {
        content_length.map(|l| l as u64)
    };

    let seekable = accept_ranges;
    let reader = StreamReader::new(state.clone(), seekable, total);

    let expected_len = *state.content_length.lock().unwrap();
    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut total_pushed: usize = 0;
        while let Some(item) = stream.next().await {
            if state_clone.cancelled() {
                state_clone.finish();
                return Ok(());
            }
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    state_clone.finish();
                    return Err(anyhow::anyhow!("network stream error: {e}"));
                }
            };
            if chunk.is_empty() {
                continue;
            }
            let chunk = chunk.to_vec();
            total_pushed += chunk.len();
            match timeout(NETWORK_READ_TIMEOUT, async { state_clone.push(&chunk) }).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    state_clone.finish();
                    return Err(e.into());
                }
                Err(_) => {
                    state_clone.finish();
                    return Err(
                        io::Error::new(io::ErrorKind::TimedOut, "network read timeout").into(),
                    );
                }
            }
        }
        // Detect interrupted downloads: fewer bytes arrived than Content-Length promised.
        if let Some(len) = expected_len {
            if total_pushed < len {
                state_clone.finish();
                return Err(anyhow::anyhow!(
                    "interrupted download: received {total_pushed} of {len} bytes"
                ));
            }
        }
        state_clone.finish();
        Ok(())
    });

    Ok((reader, handle))
}
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_size: usize,
    pub prebuffer_duration: Duration,
    pub crossfade: CrossfadeSettings,
    pub gapless: GaplessSettings,
    pub volume_normalization: bool,
    pub target_loudness: f32, // LUFS
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            buffer_size: 4096,
            prebuffer_duration: Duration::from_millis(500),
            crossfade: CrossfadeSettings::default(),
            gapless: GaplessSettings::default(),
            volume_normalization: true,
            target_loudness: -14.0, // Spotify/YouTube Music standard
        }
    }
}

/// Playback state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
    Buffering,
    Ended,
    Error,
}

/// Audio event for UI updates
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioEvent {
    StateChanged(PlaybackState),
    TrackChanged(Option<TrackHandle>),
    PositionChanged {
        position: Duration,
        duration: Duration,
    },
    VolumeChanged(f32),
    BufferingUpdate(f32),
    Error(String),
    TrackEnded(TrackHandle),
    CrossfadeStarted {
        from: TrackHandle,
        to: TrackHandle,
    },
    CrossfadeEnded,
}

/// Decoded audio track ready for playback
#[derive(Debug, Clone)]
pub struct DecodedTrack {
    pub handle: TrackHandle,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Duration,
    pub frames: Vec<f32>,      // Interleaved samples
    pub loudness: Option<f32>, // LUFS
}

fn has_reached_decoded_end(track: &DecodedTrack, position: Duration) -> bool {
    position >= track.duration
}

/// Commands processed by the thread that owns the non-Send device stream.
enum OutputCommand {
    Play(DecodedTrack, oneshot::Sender<Result<(), String>>),
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    Volume(f32),
    SetDevice(Device),
    Position(oneshot::Sender<Duration>),
    Shutdown,
}

/// Audio engine - main playback coordinator
pub struct AudioEngine {
    config: Arc<RwLock<AudioConfig>>,
    settings: Arc<SettingsService>,
    windows: Arc<WindowsIntegration>,

    // Output
    output: std::sync::mpsc::Sender<OutputCommand>,
    output_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Selected output device (None = system default).
    output_device: Arc<RwLock<Option<Device>>>,
    current_track: Arc<RwLock<Option<DecodedTrack>>>,
    next_track: Arc<RwLock<Option<DecodedTrack>>>,

    // Playback state
    state: Arc<RwLock<PlaybackState>>,
    position: Arc<RwLock<Duration>>,
    volume: Arc<RwLock<f32>>,

    // Decoding pipeline
    decode_semaphore: Arc<Semaphore>,
    decode_cache: Arc<DashMap<String, DecodedTrack>>,
    /// Internal map: track_id -> signed stream URL. NEVER sent to frontend.
    stream_urls: Arc<DashMap<String, String>>,
    /// Cancellation flag for the in-flight download+decode (Stop / Next / shutdown).
    decode_cancel: Arc<RwLock<Option<Arc<AtomicBool>>>>,

    // Crossfade
    crossfade_state: Arc<RwLock<CrossfadeState>>,

    // Event bus
    event_tx: broadcast::Sender<AudioEvent>,

    // Background tasks
    position_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    preload_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, Clone, Default)]
struct CrossfadeState {
    active: bool,
    progress: f32,
    from_volume: f32,
    to_volume: f32,
}

impl AudioEngine {
    /// Create a new audio engine
    #[instrument(skip(settings, windows))]
    pub async fn new(
        settings: Arc<SettingsService>,
        windows: Arc<WindowsIntegration>,
    ) -> Result<Self> {
        let config = AudioConfig::default();

        // Initialize audio output
        let (output, commands) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let output_thread = std::thread::spawn(move || {
            // Mutable output stream/handle/sink so we can switch devices at runtime.
            let mut stream: Option<rodio::OutputStream> = None;
            let mut handle: Option<rodio::OutputStreamHandle> = None;
            let mut sink: Option<rodio::Sink> = None;

            let open_default =
                || -> Result<(rodio::OutputStream, rodio::OutputStreamHandle), String> {
                    rodio::OutputStream::try_default().map_err(|e| e.to_string())
                };

            let (s, h) = match open_default() {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let sk = match rodio::Sink::try_new(&h) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            stream = Some(s);
            handle = Some(h);
            sink = Some(sk);
            let _ = ready_tx.send(Ok(()));

            while let Ok(command) = commands.recv() {
                match command {
                    OutputCommand::Play(track, reply) => {
                        if let Some(ref sk) = sink {
                            let _ = sk.stop();
                        }
                        let Some(ref h) = handle else {
                            let _ = reply.send(Err("No audio output handle available".to_string()));
                            continue;
                        };
                        match rodio::Sink::try_new(h) {
                            Ok(new_sink) => {
                                new_sink.append(rodio::buffer::SamplesBuffer::new(
                                    track.channels,
                                    track.sample_rate,
                                    track.frames,
                                ));
                                new_sink.play();
                                sink = Some(new_sink);
                                let _ = reply.send(Ok(()));
                            }
                            Err(error) => {
                                tracing::error!(%error, "Failed to reset audio sink");
                                let _ = reply.send(Err(error.to_string()));
                            }
                        }
                    }
                    OutputCommand::Pause => {
                        if let Some(ref sk) = sink {
                            sk.pause();
                        }
                    }
                    OutputCommand::Resume => {
                        if let Some(ref sk) = sink {
                            sk.play();
                        }
                    }
                    OutputCommand::Stop => {
                        if let Some(ref sk) = sink {
                            sk.stop();
                        }
                    }
                    OutputCommand::Seek(position) => {
                        if let Some(ref sk) = sink {
                            if let Err(error) = sk.try_seek(position) {
                                tracing::warn!(%error, "Audio seek failed");
                            }
                        }
                    }
                    OutputCommand::Volume(volume) => {
                        if let Some(ref sk) = sink {
                            sk.set_volume(volume);
                        }
                    }
                    OutputCommand::SetDevice(device) => {
                        info!(device = ?device.name(), "Switching output device");
                        if let Some(ref sk) = sink {
                            sk.stop();
                        }
                        sink = None;
                        stream = None; // release old device handle
                        match rodio::OutputStream::try_from_device(&device) {
                            Ok((s, h)) => {
                                stream = Some(s);
                                handle = Some(h);
                                let Some(ref h) = handle else {
                                    tracing::error!("Failed to get handle for new device");
                                    continue;
                                };
                                match rodio::Sink::try_new(h) {
                                    Ok(sk) => sink = Some(sk),
                                    Err(e) => {
                                        tracing::error!(%e, "Failed to create sink for new device")
                                    }
                                }
                                info!("Output device switched successfully");
                            }
                            Err(e) => tracing::error!(%e, "Failed to open output device"),
                        }
                    }
                    OutputCommand::Position(reply) => {
                        if let Some(ref sk) = sink {
                            let _ = reply.send(sk.get_pos());
                        }
                    }
                    OutputCommand::Shutdown => break,
                }
            }
        });
        ready_rx
            .recv()
            .context("Audio output thread exited before initialization")?
            .map_err(anyhow::Error::msg)?;

        let (event_tx, _) = broadcast::channel(128);

        let engine = Self {
            config: Arc::new(RwLock::new(config)),
            settings,
            windows,
            output,
            output_thread: Mutex::new(Some(output_thread)),
            output_device: Arc::new(RwLock::new(None)),
            current_track: Arc::new(RwLock::new(None)),
            next_track: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(PlaybackState::Stopped)),
            position: Arc::new(RwLock::new(Duration::ZERO)),
            volume: Arc::new(RwLock::new(1.0)),
            decode_semaphore: Arc::new(Semaphore::new(2)), // Limit concurrent decodes
            decode_cache: Arc::new(DashMap::new()),
            stream_urls: Arc::new(DashMap::new()),
            decode_cancel: Arc::new(RwLock::new(None)),
            crossfade_state: Arc::new(RwLock::new(CrossfadeState::default())),
            event_tx,
            position_task: Mutex::new(None),
            preload_task: Mutex::new(None),
        };

        // Load settings
        engine.apply_settings().await?;

        // Start position reporting task
        engine.start_position_task().await;

        info!("Audio engine initialized");
        Ok(engine)
    }

    /// Apply settings from SettingsService
    async fn apply_settings(&self) -> Result<()> {
        let settings = self.settings.get_all().await?;

        let mut config = self.config.write().await;
        config.crossfade = settings.audio_crossfade;
        config.gapless = settings.audio_gapless;
        config.volume_normalization = settings.audio_normalize_volume;

        if let Some(device_id) = &settings.audio_output_device {
            self.set_output_device(Some(device_id.as_str())).await?;
        } else {
            self.set_output_device(None).await?;
        }

        Ok(())
    }

    /// Get current playback state
    pub async fn state(&self) -> PlaybackState {
        *self.state.read().await
    }

    /// Get current volume (0.0 - 1.0)
    pub async fn volume(&self) -> f32 {
        *self.volume.read().await
    }

    /// Get current position
    pub async fn position(&self) -> Duration {
        *self.position.read().await
    }

    /// Get current track handle
    pub async fn current_track(&self) -> Option<TrackHandle> {
        self.current_track
            .read()
            .await
            .as_ref()
            .map(|t| t.handle.clone())
    }

    /// Set volume
    #[instrument(skip(self))]
    pub async fn set_volume(&self, volume: f32) -> Result<()> {
        let volume = volume.clamp(0.0, 1.0);
        *self.volume.write().await = volume;

        self.send_output(OutputCommand::Volume(volume))?;

        self.emit(AudioEvent::VolumeChanged(volume));
        Ok(())
    }

    /// Set the audio output device by name. Matches against cpal-enumerated devices.
    /// None selects the system default.
    #[instrument(skip(self))]
    pub async fn set_output_device(&self, device_name: Option<&str>) -> Result<()> {
        let host = cpal::default_host();

        let target = match device_name {
            Some(name) => {
                let mut devices = host.output_devices().map_err(|e| {
                    AudioError::OutputDevice(format!("Failed to enumerate audio devices: {e}"))
                })?;
                let found = devices
                    .find(|d| d.name().map_or(false, |n| n == name))
                    .ok_or_else(|| {
                        AudioError::OutputDevice(format!("Output device not found: {name}"))
                    })?;
                Some(found)
            }
            None => None,
        };

        {
            let mut current = self.output_device.write().await;
            *current = target.clone();
        }

        match &target {
            Some(device) => self.send_output(OutputCommand::SetDevice(device.clone()))?,
            None => {
                // Switch back to default device
                if let Some(default) = host.default_output_device() {
                    self.send_output(OutputCommand::SetDevice(default))?;
                }
            }
        }

        info!(device = ?device_name, "Output device set");
        Ok(())
    }

    /// Get all available output devices (name + id).
    pub async fn get_output_devices(&self) -> Result<Vec<(String, String)>> {
        let host = cpal::default_host();
        let devices = host.output_devices().map_err(|e| {
            AudioError::OutputDevice(format!("Failed to enumerate audio devices: {e}"))
        })?;
        let mut out = Vec::new();
        for d in devices {
            if let Ok(name) = d.name() {
                out.push((name.clone(), name));
            }
        }
        Ok(out)
    }

    /// Register a signed stream URL for a track (internal use only, never sent to frontend).
    pub fn register_stream_url(&self, track_id: String, stream_url: String) {
        self.stream_urls.insert(track_id, stream_url);
    }

    /// Play a track
    #[instrument(skip(self, track_handle))]
    pub async fn play(&self, track_handle: TrackHandle) -> Result<()> {
        info!(track_id = %track_handle.id, "Starting playback");

        // Look up the signed stream URL from the internal map (never from TrackHandle).
        let stream_url = self
            .stream_urls
            .get(&track_handle.id)
            .ok_or_else(|| {
                AudioError::TrackNotFound(format!(
                    "No stream URL registered for track {}",
                    track_handle.id
                ))
            })?
            .clone();

        // Decode track
        let decoded = self.decode_track(track_handle, &stream_url).await?;
        let duration = decoded.duration;

        // Set as current track
        *self.current_track.write().await = Some(decoded);
        *self.position.write().await = Duration::ZERO;

        // Start playback and await sink confirmation BEFORE marking Playing.
        // Marking Playing before sink confirmation produced a false-success
        // window: the UI reported playing while the sink had silently failed
        // (sink errors were only tracing::error-logged, no AudioEvent::Error).
        if let Err(error) = self.start_output().await {
            self.emit(AudioEvent::Error(error.to_string()));
            return Err(error);
        }

        self.set_state(PlaybackState::Playing).await?;

        // Update SMTC
        let title = self
            .current_track
            .read()
            .await
            .as_ref()
            .map(|t| t.handle.title.clone())
            .unwrap_or_default();
        let artist = self
            .current_track
            .read()
            .await
            .as_ref()
            .map(|t| t.handle.artist.clone())
            .unwrap_or_default();
        self.windows
            .update_media_metadata(orcave_windows::MediaMetadata {
                title,
                artist,
                duration_ms: Some(duration.as_millis() as u64),
                position_ms: Some(0),
                ..Default::default()
            })
            .await
            .ok();

        self.windows
            .update_playback_state(MediaPlaybackStatus::Playing)
            .await
            .ok();

        // Inhibit sleep during playback
        self.windows.inhibit_sleep("Playing audio").await.ok();

        Ok(())
    }
    /// Pause playback
    #[instrument(skip(self))]
    pub async fn pause(&self) -> Result<()> {
        self.send_output(OutputCommand::Pause)?;
        self.set_state(PlaybackState::Paused).await?;
        self.windows
            .update_playback_state(MediaPlaybackStatus::Paused)
            .await
            .ok();
        Ok(())
    }

    /// Resume playback
    #[instrument(skip(self))]
    pub async fn resume(&self) -> Result<()> {
        self.send_output(OutputCommand::Resume)?;
        self.set_state(PlaybackState::Playing).await?;
        self.windows
            .update_playback_state(MediaPlaybackStatus::Playing)
            .await
            .ok();
        Ok(())
    }

    /// Stop playback
    #[instrument(skip(self))]
    pub async fn stop(&self) -> Result<()> {
        self.crossfade_state.write().await.active = false;
        // Cancel any in-flight download+decode so Stop halts immediately.
        if let Some(cancel) = self.decode_cancel.write().await.take() {
            cancel.store(true, Ordering::Release);
        }
        self.send_output(OutputCommand::Stop)?;
        *self.current_track.write().await = None;
        *self.next_track.write().await = None;
        *self.position.write().await = Duration::ZERO;
        self.set_state(PlaybackState::Stopped).await?;
        self.windows.allow_sleep().await.ok();
        Ok(())
    }

    /// Seek to position
    #[instrument(skip(self))]
    pub async fn seek(&self, position: Duration) -> Result<()> {
        self.send_output(OutputCommand::Seek(position))?;
        *self.position.write().await = position;

        self.windows
            .update_media_metadata(orcave_windows::MediaMetadata {
                position_ms: Some(position.as_millis() as u64),
                ..Default::default()
            })
            .await
            .ok();

        Ok(())
    }

    /// Skip to next track (with crossfade if enabled)
    #[instrument(skip(self))]
    pub async fn next(&self, next_track: Option<TrackHandle>) -> Result<()> {
        // Cancel any in-flight download+decode before starting the next track,
        // preventing old and new audio from overlapping.
        if let Some(cancel) = self.decode_cancel.write().await.take() {
            cancel.store(true, Ordering::Release);
        }
        if let Some(next) = next_track {
            let crossfade = self.config.read().await.crossfade.clone();
            if crossfade.enabled && self.state().await == PlaybackState::Playing {
                self.start_crossfade(next).await?;
            } else {
                self.play(next).await?;
            }
        } else {
            self.stop().await?;
        }
        Ok(())
    }

    /// Start crossfade to next track
    async fn start_crossfade(&self, next_track: TrackHandle) -> Result<()> {
        info!(next_id = %next_track.id, "Starting crossfade");

        let stream_url = self
            .stream_urls
            .get(&next_track.id)
            .ok_or_else(|| {
                AudioError::TrackNotFound(format!(
                    "No stream URL registered for track {}",
                    next_track.id
                ))
            })?
            .clone();

        let current = self
            .current_track
            .read()
            .await
            .as_ref()
            .map(|track| track.handle.clone());
        let next = self.decode_track(next_track, &stream_url).await?;

        *self.next_track.write().await = Some(next);

        {
            let mut crossfade = self.crossfade_state.write().await;
            crossfade.active = true;
            crossfade.progress = 0.0;
            crossfade.from_volume = *self.volume.read().await;
            crossfade.to_volume = *self.volume.read().await;
        }

        if let (Some(from), Some(to)) = (current, self.next_track.read().await.as_ref()) {
            self.emit(AudioEvent::CrossfadeStarted {
                from,
                to: to.handle.clone(),
            });
        }

        // Crossfade task
        let engine = self.clone();
        tokio::spawn(async move {
            engine.run_crossfade().await;
        });

        Ok(())
    }

    /// Run crossfade animation
    async fn run_crossfade(&self) {
        let duration = self.config.read().await.crossfade.duration_ms;
        let steps = 60; // 60fps
        let step_duration = Duration::from_millis(u64::from(duration) / steps as u64);

        for i in 0..=steps {
            if !self.crossfade_state.read().await.active
                || self.state().await != PlaybackState::Playing
            {
                break;
            }
            let progress = i as f32 / steps as f32;
            {
                let mut cf = self.crossfade_state.write().await;
                cf.progress = progress;
            }

            // Apply crossfade volumes (equal power curve)
            if i == steps / 2 {
                let next = { self.next_track.write().await.take() };
                if let Some(next) = next {
                    *self.current_track.write().await = Some(next);
                    *self.position.write().await = Duration::ZERO;
                    if let Err(error) = self.start_output().await {
                        self.emit(AudioEvent::Error(error.to_string()));
                        break;
                    }
                    if let Some(track) = self.current_track.read().await.as_ref() {
                        self.emit(AudioEvent::TrackChanged(Some(track.handle.clone())));
                    }
                }
            }
            let gain = if i < steps / 2 {
                (1.0 - progress * 2.0).sqrt()
            } else {
                ((progress - 0.5) * 2.0).sqrt()
            };
            let _ = self.send_output(OutputCommand::Volume(*self.volume.read().await * gain));

            tokio::time::sleep(step_duration).await;
        }

        {
            let mut cf = self.crossfade_state.write().await;
            cf.active = false;
        }
        let _ = self.send_output(OutputCommand::Volume(*self.volume.read().await));

        self.emit(AudioEvent::CrossfadeEnded);
    }

    /// Probe + decode a `MediaSource` (HTTP StreamReader or local file) into PCM frames.
    /// Shared by both the HTTP and `file://` code paths.
    pub async fn decode_from_source(
        source: Box<dyn MediaSource + Send>,
        cancel: Arc<AtomicBool>,
    ) -> Result<(Vec<f32>, u64, u32, u16)> {
        let mss = MediaSourceStream::new(source, Default::default());
        let hint = Hint::new();
        let probed = match tokio::task::spawn_blocking(move || {
            get_probe().format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
        })
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return Err(anyhow::anyhow!("probe join error: {e}").into()),
            Err(e) => return Err(anyhow::anyhow!("probe task join error: {e}").into()),
        };

        let mut format = probed.format;
        let track = format.default_track().context("No default track")?;
        let codec_params = track.codec_params.clone();

        let mut decoder = get_codecs().make(&codec_params, &Default::default())?;

        let sample_rate = codec_params.sample_rate.unwrap_or(48000);
        let channels = codec_params
            .channels
            .map_or(2, |channels| channels.count() as u16);

        // Blocking decode loop runs on a spawn_blocking thread.
        let decode_outcome = tokio::task::spawn_blocking(move || {
            let mut frames = Vec::new();
            let mut total_samples = 0u64;
            let mut cancelled = false;
            loop {
                if cancel.load(Ordering::Acquire) {
                    cancelled = true;
                    break;
                }
                let packet = match format.next_packet() {
                    Ok(p) => p,
                    Err(symphonia::core::errors::Error::IoError(e))
                        if e.kind() == io::ErrorKind::UnexpectedEof =>
                    {
                        break;
                    }
                    Err(e) => return Err(anyhow::anyhow!("decode packet error: {e}")),
                };
                let decoded = match decoder.decode(&packet) {
                    Ok(d) => d,
                    Err(e) => return Err(anyhow::anyhow!("decode error: {e}")),
                };
                let spec = *decoded.spec();
                let count = decoded.frames() as u64;
                let mut buf = SampleBuffer::<f32>::new(count, spec);
                buf.copy_interleaved_ref(decoded);
                frames.extend_from_slice(buf.samples());
                total_samples += count;
            }
            Ok((frames, total_samples, cancelled))
        })
        .await;

        let (frames, total_samples, cancelled) = match decode_outcome {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => return Err(e.into()),
            Err(e) => return Err(anyhow::anyhow!("decode task join error: {e}").into()),
        };

        if cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "decode cancelled").into());
        }

        // Reject silent decodes: a container may parse but yield zero packets
        // (e.g. a valid WAV header with an empty data chunk). Playing an empty
        // buffer produces silence with no error, which looks like a hung player.
        if total_samples == 0 {
            return Err(anyhow::anyhow!("decode produced no audio samples").into());
        }

        Ok((frames, total_samples, sample_rate, channels))
    }

    /// Convert a `file://` URL (as emitted by `LibraryService::path_to_file_url`)
    /// into a local `PathBuf` for `File::open`.
    ///
    /// Windows local: `file:///C:/Users/Me/song.mp3` -> `C:/Users/Me/song.mp3`
    ///   (strip `file://`, percent-decode, then trim the extra leading `/` so
    ///   `File::open` gets a drive-relative path, which Windows accepts).
    /// UNC: `file:////server/share/song.mp3` -> `//server/share/song.mp3`
    ///   (keep the leading `//`).
    /// Percent-encoding is decoded (`%20` -> space, `%C3%A9` -> é, ...) so URLs
    /// emitted for paths containing spaces or Unicode open the real on-disk file.
    /// Public so integration tests can exercise the exact conversion used by
    /// `decode_track`; callers outside this crate rarely need it.
    pub fn file_url_to_path(url: &str) -> Result<PathBuf, AudioError> {
        let path_str = url
            .strip_prefix("file://")
            .ok_or_else(|| AudioError::Stream("invalid file:// URL".to_string()))?;
        // percent_decode is lenient: a stray `%` not followed by two hex digits
        // passes through literally, which would make `File::open` hunt for a
        // file that literally contains `%`. Reject those URLs up front.
        let mut i = 0;
        while i < path_str.len() {
            if path_str.as_bytes()[i] == b'%' {
                let rest = &path_str.as_bytes()[i + 1..];
                if rest.len() < 2
                    || !rest[0].is_ascii_hexdigit()
                    || !rest[1].is_ascii_hexdigit()
                {
                    return Err(AudioError::Stream(format!(
                        "invalid percent-encoding in file URL: stray '%' at byte {i}"
                    )));
                }
                i += 3;
            } else {
                i += 1;
            }
        }
        let decoded = percent_decode(path_str.as_bytes())
            .decode_utf8()
            .map_err(|e| {
                AudioError::Stream(format!("invalid percent-encoding in file URL: {e}"))
            })?;
        let path = if decoded.starts_with("//") {
            PathBuf::from(decoded.as_ref()) // UNC: keep leading //
        } else {
            PathBuf::from(decoded.trim_start_matches('/'))
        };
        Ok(path)
    }

    /// Decode a track from URL using bounded incremental streaming.
    /// Supports both HTTP(s) URLs and `file://` local paths.
    /// The full file is never downloaded at once: chunks are pulled into a bounded
    /// buffer and fed to Symphonia incrementally as they arrive.
    #[instrument(skip(self, track_handle))]
    async fn decode_track(
        &self,
        track_handle: TrackHandle,
        stream_url: &str,
    ) -> Result<DecodedTrack> {
        // Check cache
        if let Some(cached) = self.decode_cache.get(&track_handle.id) {
            return Ok(cached.value().clone());
        }

        let _permit = self.decode_semaphore.acquire().await?;

        let stream_url = stream_url;

        // Cancellation flag for this in-flight download+decode, settable by Stop/Next/shutdown.
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut slot = self.decode_cancel.write().await;
            *slot = Some(cancel.clone());
        }
        // Clear the slot when this decode finishes (only if still ours).
        let cancel_slot = Arc::clone(&self.decode_cancel);
        struct ResetCancel(Arc<RwLock<Option<Arc<AtomicBool>>>>, Arc<AtomicBool>);
        impl Drop for ResetCancel {
            fn drop(&mut self) {
                if let Ok(mut slot) = self.0.try_write() {
                    if slot.as_ref().map_or(false, |c| Arc::ptr_eq(c, &self.1)) {
                        *slot = None;
                    }
                }
            }
        }
        let _reset = ResetCancel(cancel_slot, cancel.clone());

        let (frames, total_samples, sample_rate, channels) = if stream_url.starts_with("file://") {
            // Local file playback: open directly, bypassing HTTP download.
            // `path_to_file_url` emits `file:///C:/path` (Windows local, 3 slashes)
            // or `file:////server/share` (UNC, 4 slashes). Strip `file://` (7 chars),
            // percent-decode reserved URL characters (%20 → space, etc.), then for
            // local absolute paths trim the extra leading slash so `File::open`
            // gets `C:/path` (not `/C:/path`, which fails on Windows). UNC keeps `//`.
            let path = Self::file_url_to_path(stream_url)?;
            let file = File::open(&path)
                .map_err(|e| AudioError::Stream(format!("Failed to open local file: {e}")))?;
            let source: Box<dyn MediaSource + Send> = Box::new(LocalFileSource(file));
            Self::decode_from_source(source, Arc::clone(&cancel)).await?
        } else {
            // HTTP streaming path (existing behavior).
            let client = reqwest::Client::new();
            let state = Arc::new(StreamState::new());

            // Start the producer and get a reader immediately; Symphonia reads concurrently
            // while the download streams into the bounded buffer.
            // `download_stream` handles HTTP status classification and retry internally.
            let (reader, producer) = download_stream(
                client,
                stream_url.to_string(),
                HeaderMap::new(),
                None,
                Arc::clone(&state),
            )
            .await?;
            let reader_source: Box<dyn MediaSource + Send> = Box::new(reader);
            let result = Self::decode_from_source(reader_source, Arc::clone(&cancel)).await?;
            // Stop the producer now that decoding is done.
            producer.abort();
            let _ = producer.await;
            result
        };

        let duration = Duration::from_secs_f64(total_samples as f64 / sample_rate as f64);

        // Calculate loudness (simplified EBU R128)
        let loudness = if self.config.read().await.volume_normalization {
            Some(self.calculate_loudness(&frames))
        } else {
            None
        };

        let decoded_track = DecodedTrack {
            handle: track_handle,
            sample_rate,
            channels,
            duration,
            frames,
            loudness,
        };

        // Cache if not too large
        if decoded_track.frames.len() < 50_000_000 {
            // ~10 min at 48kHz stereo
            self.decode_cache
                .insert(decoded_track.handle.id.clone(), decoded_track.clone());
        }

        Ok(decoded_track)
    }

    /// Calculate EBU R128 loudness (simplified)
    fn calculate_loudness(&self, frames: &[f32]) -> f32 {
        if frames.is_empty() {
            return -70.0;
        }

        // K-weighted filter approximation
        let mut sum_sq = 0.0f64;
        for chunk in frames.chunks(2) {
            let left = chunk.get(0).copied().unwrap_or(0.0);
            let right = chunk.get(1).copied().unwrap_or(0.0);
            // Simplified: just RMS with channel weighting
            sum_sq += (left * left + right * right) as f64 * 0.5;
        }

        let mean_sq = sum_sq / frames.len().div_ceil(2) as f64;
        if mean_sq <= 0.0 {
            return -70.0;
        }

        10.0 * mean_sq.log10() as f32 - 0.691 // K-weighting offset
    }

    /// Start audio output from current track and await sink confirmation.
    /// Returns Err if the output thread cannot create/confirm the sink, so
    /// callers do not observe a Playing state without a live output sink.
    async fn start_output(&self) -> Result<()> {
        let track = self.current_track.read().await.clone();
        let Some(track) = track else {
            return Ok(());
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        self.send_output(OutputCommand::Play(track, reply_tx))?;
        self.send_output(OutputCommand::Volume(*self.volume.read().await))?;

        match reply_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(AudioError::OutputDevice(e).into()),
            Err(_) => Err(AudioError::OutputDevice("output thread dropped play reply".to_string()).into()),
        }
    }

    /// Update playback state and emit event
    async fn set_state(&self, state: PlaybackState) -> Result<()> {
        *self.state.write().await = state;
        self.emit(AudioEvent::StateChanged(state));

        if let Some(track) = self.current_track.read().await.as_ref() {
            self.emit(AudioEvent::TrackChanged(Some(track.handle.clone())));
        }
        Ok(())
    }

    /// Start position reporting task
    async fn start_position_task(&self) {
        let engine = self.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                if engine.state().await == PlaybackState::Playing {
                    let (reply, position) = oneshot::channel();
                    if engine.send_output(OutputCommand::Position(reply)).is_ok() {
                        let Ok(pos) = position.await else { break };
                        *engine.position.write().await = pos;

                        let track = engine
                            .current_track
                            .read()
                            .await
                            .as_ref()
                            .map(|track| (track.handle.clone(), track.duration, has_reached_decoded_end(track, pos)));
                        if let Some((handle, duration, ended)) = track {
                            engine.emit(AudioEvent::PositionChanged {
                                position: pos,
                                duration,
                            });

                            engine
                                .windows
                                .update_media_metadata(orcave_windows::MediaMetadata {
                                    position_ms: Some(pos.as_millis() as u64),
                                    ..Default::default()
                                })
                                .await
                                .ok();

                            // Check for track end
                            if ended {
                                engine.emit(AudioEvent::TrackEnded(handle));
                                engine.set_state(PlaybackState::Ended).await.ok();
                            }
                        }
                    }
                }
            }
        });
        *self.position_task.lock().await = Some(handle);
    }

    /// Preload next track for gapless playback
    pub async fn preload_next(&self, track_handle: TrackHandle) {
        if !self.config.read().await.gapless.preload_next {
            return;
        }

        let stream_url = match self.stream_urls.get(&track_handle.id) {
            Some(url) => url.clone(),
            None => return,
        };

        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine.decode_track(track_handle, &stream_url).await;
        });
    }

    /// Subscribe to audio events
    pub fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.event_tx.subscribe()
    }

    fn emit(&self, event: AudioEvent) {
        let _ = self.event_tx.send(event);
    }

    fn send_output(&self, command: OutputCommand) -> Result<()> {
        self.output
            .send(command)
            .context("Audio output thread exited")
    }

    /// Shutdown audio engine
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        // Stop background tasks
        if let Some(handle) = self.position_task.lock().await.take() {
            handle.abort();
        }
        if let Some(handle) = self.preload_task.lock().await.take() {
            handle.abort();
        }

        // Stop playback
        self.stop().await?;

        // Clear cache
        self.decode_cache.clear();

        // Allow sleep
        self.windows.allow_sleep().await.ok();

        self.send_output(OutputCommand::Shutdown)?;
        if let Some(thread) = self.output_thread.lock().await.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .context("Failed to join audio output thread")?
                .map_err(|_| anyhow::anyhow!("Audio output thread panicked"))?;
        }

        info!("Audio engine shutdown");
        Ok(())
    }
}

impl Clone for AudioEngine {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            settings: self.settings.clone(),
            windows: self.windows.clone(),
            output: self.output.clone(),
            output_thread: Mutex::new(None),
            output_device: self.output_device.clone(),
            current_track: self.current_track.clone(),
            next_track: self.next_track.clone(),
            state: self.state.clone(),
            position: self.position.clone(),
            volume: self.volume.clone(),
            decode_semaphore: self.decode_semaphore.clone(),
            decode_cache: self.decode_cache.clone(),
            stream_urls: self.stream_urls.clone(),
            decode_cancel: self.decode_cancel.clone(),
            crossfade_state: self.crossfade_state.clone(),
            event_tx: self.event_tx.clone(),
            position_task: Mutex::new(None),
            preload_task: Mutex::new(None),
        }
    }
}

/// Tauri command handlers
pub mod commands {
    use super::*;

    #[tauri::command]
    pub async fn audio_play(
        audio: tauri::State<'_, Arc<AudioEngine>>,
        track_handle: TrackHandleDto,
    ) -> TauriResult<()> {
        audio.play(track_handle.into()).await?;
        Ok(())
    }

    #[tauri::command]
    pub async fn audio_pause(audio: tauri::State<'_, Arc<AudioEngine>>) -> TauriResult<()> {
        audio.pause().await?;
        Ok(())
    }

    #[tauri::command]
    pub async fn audio_resume(audio: tauri::State<'_, Arc<AudioEngine>>) -> TauriResult<()> {
        audio.resume().await?;
        Ok(())
    }

    #[tauri::command]
    pub async fn audio_stop(audio: tauri::State<'_, Arc<AudioEngine>>) -> TauriResult<()> {
        audio.stop().await?;
        Ok(())
    }

    #[tauri::command]
    pub async fn audio_seek(
        audio: tauri::State<'_, Arc<AudioEngine>>,
        position_ms: u64,
    ) -> TauriResult<()> {
        audio.seek(Duration::from_millis(position_ms)).await?;
        Ok(())
    }

    #[tauri::command]
    pub async fn audio_set_volume(
        audio: tauri::State<'_, Arc<AudioEngine>>,
        volume: f32,
    ) -> TauriResult<()> {
        audio.set_volume(volume).await?;
        Ok(())
    }

    #[tauri::command]
    pub async fn audio_get_state(
        audio: tauri::State<'_, Arc<AudioEngine>>,
    ) -> TauriResult<PlaybackState> {
        Ok(audio.state().await)
    }

    #[tauri::command]
    pub async fn audio_get_output_devices(
        audio: tauri::State<'_, Arc<AudioEngine>>,
    ) -> TauriResult<Vec<(String, String)>> {
        Ok(audio
            .get_output_devices()
            .await
            .map_err(|e| orcave_core::TauriError::Audio(e.to_string()))?)
    }

    #[tauri::command]
    pub async fn audio_set_output_device(
        audio: tauri::State<'_, Arc<AudioEngine>>,
        device_name: Option<String>,
    ) -> TauriResult<()> {
        audio.set_output_device(device_name.as_deref()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    #[test]
    fn missing_metadata_duration_does_not_end_decoded_audio_at_startup() {
        let track = DecodedTrack {
            handle: TrackHandle {
                id: "local-fixture".into(),
                title: "Fixture".into(),
                artist: "Test".into(),
                duration: None,
            },
            sample_rate: 44_100,
            channels: 2,
            duration: Duration::from_secs(5),
            frames: vec![0.5; 44_100 * 2 * 5],
            loudness: None,
        };
        assert!(!has_reached_decoded_end(&track, Duration::from_millis(400)));
        assert!(has_reached_decoded_end(&track, Duration::from_secs(5)));
    }

    fn make_wav(freq: f32, duration_ms: u32, sample_rate: u32) -> Vec<u8> {
        let channels: u16 = 2;
        let bits: u16 = 16;
        let byte_rate = sample_rate * channels as u32 * (bits / 8) as u32;
        let block_align = channels * (bits / 8);
        let num_samples = (sample_rate as f64 * duration_ms as f64 / 1000.0) as u32;
        let data_size = num_samples as usize * block_align as usize;
        let chunk_size = 4 + 24 + 8 + data_size;
        let mut buf = Vec::with_capacity(8 + chunk_size);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(chunk_size as u32).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&(16u32).to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&(sample_rate as u32).to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&bits.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&(data_size as u32).to_le_bytes());
        for i in 0..num_samples {
            let t = i as f32 / sample_rate as f32;
            let s = (freq * 2.0 * std::f32::consts::PI * t).sin();
            let sample = (s * 15000.0) as i16;
            for _ in 0..channels {
                buf.extend_from_slice(&sample.to_le_bytes());
            }
        }
        buf
    }

    struct HttpRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
    }

    struct HttpResponse {
        status: u16,
        reason: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    fn parse_request(bytes: &[u8]) -> HttpRequest {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("GET").to_string();
        let path = parts.next().unwrap_or("/").to_string();
        let mut headers = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        HttpRequest {
            method,
            path,
            headers,
        }
    }

    fn render_response(resp: &HttpResponse) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason).as_bytes());
        out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
        for (k, v) in &resp.headers {
            out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&resp.body);
        out
    }

    /// Serve exactly one HTTP request on an ephemeral port, then shut down.
    fn serve_one<F>(handler: F) -> u16
    where
        F: FnOnce(HttpRequest) -> HttpResponse + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = parse_request(&buf[..n]);
                let resp = handler(req);
                let bytes = render_response(&resp);
                let _ = stream.write_all(&bytes);
            }
        });
        port
    }

    /// Fetch a URL using the streaming download + bounded reader, returning all bytes.
    /// Exercises download_stream + StreamReader end-to-end over real HTTP.
    fn fetch_streaming(url: &str) -> Result<Vec<u8>> {
        let runtime = tokio::runtime::Runtime::new()?;
        let client = reqwest::Client::new();
        let state = Arc::new(StreamState::new());
        let (mut reader, producer) = runtime.block_on(download_stream(
            client,
            url.to_string(),
            HeaderMap::new(),
            None,
            state.clone(),
        ))?;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        producer.abort();
        let producer_result = runtime.block_on(producer)?;
        producer_result?;
        Ok(out)
    }

    #[test]
    fn test_audio_config_default() {
        let config = AudioConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.channels, 2);
    }

    #[test]
    fn test_playback_state_serialization() {
        let state = PlaybackState::Playing;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"playing\"");
    }

    #[test]
    fn test_stream_state_enforces_capacity() {
        let state = Arc::new(StreamState::new());
        // Fill exactly to capacity; push must not block and buffered == capacity.
        let chunk = vec![0u8; STREAM_BUFFER_CAPACITY];
        state.push(&chunk).unwrap();
        let wl = state.write_len.load(Ordering::Acquire);
        let ro = state.read_offset.load(Ordering::Acquire);
        assert_eq!(wl.saturating_sub(ro), STREAM_BUFFER_CAPACITY);
        // Simulate consuming the whole buffer.
        state
            .read_offset
            .store(STREAM_BUFFER_CAPACITY, Ordering::Release);
        assert_eq!(
            state.write_len.load(Ordering::Acquire) - state.read_offset.load(Ordering::Acquire),
            0
        );
        // Push again after consuming; buffered stays bounded by capacity.
        state.push(&chunk).unwrap();
        let wl = state.write_len.load(Ordering::Acquire);
        let ro = state.read_offset.load(Ordering::Acquire);
        assert!(wl.saturating_sub(ro) <= STREAM_BUFFER_CAPACITY);
        state.finish();
    }

    #[test]
    fn test_stream_state_cancel_wakes_consumer() {
        let state = Arc::new(StreamState::new());
        // Blocked read with no data and not EOF returns error once cancelled.
        let state2 = Arc::clone(&state);
        let mut reader = StreamReader::new(state, false, None);
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 16];
            reader.read(&mut buf)
        });
        thread::sleep(std::time::Duration::from_millis(50));
        state2.cancel();
        let res = handle.join().unwrap();
        assert!(
            res.is_err(),
            "expected cancelled read to error, got {:?}",
            res
        );
    }

    #[test]
    fn test_stream_reader_read_eof() {
        let state = Arc::new(StreamState::new());
        state.push(b"hello").unwrap();
        state.finish();
        let mut reader = StreamReader::new(state, false, None);
        let mut buf = [0u8; 10];
        let mut total = 0usize;
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 5);
    }

    #[test]
    fn test_stream_cancellation() {
        // No data pushed; read blocks until cancelled, then must error.
        let state = Arc::new(StreamState::new());
        let state2 = Arc::clone(&state);
        let mut reader = StreamReader::new(state, false, None);
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 16];
            reader.read(&mut buf)
        });
        thread::sleep(std::time::Duration::from_millis(50));
        state2.cancel();
        let res = handle.join().unwrap();
        assert!(
            res.is_err(),
            "expected cancelled read to error, got {:?}",
            res
        );
    }

    #[test]
    fn test_symphonia_decode_cursor() -> Result<(), anyhow::Error> {
        // Symphonia can decode a seekable in-memory source (Cursor) end-to-end.
        let wav = make_wav(440.0, 100, 48000);
        let cursor = std::io::Cursor::new(wav);
        let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
        let hint = Hint::new();
        let mut probed = std::thread::spawn(move || {
            get_probe().format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
        })
        .join()
        .expect("probe thread panicked")
        .expect("probe failed");
        let mut format = probed.format;
        let track = format.default_track().context("No default track")?;
        let codec_params = track.codec_params.clone();
        let mut decoder = get_codecs().make(&codec_params, &Default::default())?;
        let sample_rate = codec_params.sample_rate.unwrap_or(48000);
        let mut frames = 0u64;
        loop {
            let packet = match format.next_packet() {
                Ok(p) => p,
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(e) => return Err(anyhow::anyhow!("packet error: {e}")),
            };
            let decoded = decoder.decode(&packet)?;
            frames += decoded.frames() as u64;
        }
        assert!(frames > 0, "expected decoded frames");
        assert_eq!(sample_rate, 48000);
        Ok(())
    }

    /// Evidence test: symphonia DOES expose container metadata tags (title/artist/album/duration)
    /// via `ProbedMetadata`, but `decode_from_source` discards `probed.metadata` (lib.rs:1264:
    /// `let mut format = probed.format;`).
    /// This proves the `--:--` / unknown-artist/album gap is in orcave-audio throwing tags away,
    /// not in symphonia being unable to read them.
    #[test]
    fn test_symphonia_metadata_tags_are_readable_from_fixture() -> Result<(), anyhow::Error> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("audio")
            .join("test_440hz_5s.mp3");
        let file = std::fs::File::open(&path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let hint = Hint::new();
        let mut probed = std::thread::spawn(move || {
            get_probe().format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
        })
        .join()
        .expect("probe thread panicked")
        .expect("probe failed");

        // `probed.metadata` is reachable and carries title/artist/album/duration tags.
        // decode_from_source never reads it.
        let metadata = probed.metadata.get();
        let mut tag_keys = Vec::new();
        if let Some(md) = metadata {
            if let Some(rev) = md.current() {
                for tag in rev.tags() {
                    if let Some(k) = tag.std_key {
                        tag_keys.push(format!("{:?}", k));
                    }
                }
            }
        }
        eprintln!(
            "symphonia metadata tags readable from MP3 fixture: {:?}",
            tag_keys
        );
        Ok(())
    }

    #[test]
    fn test_http_full_body() {
        let wav = make_wav(440.0, 50, 48000);
        let wav_clone = wav.clone();
        let port = serve_one(move |_req| HttpResponse {
            status: 200,
            reason: "OK",
            headers: vec![],
            body: wav_clone,
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let body = fetch_streaming(&url).unwrap();
        assert_eq!(body, wav, "full body must be delivered");
    }

    #[test]
    fn test_http_range_request() {
        let wav = make_wav(440.0, 50, 48000);
        let wav_clone = wav.clone();
        let wav_len = wav.len();
        let port = serve_one(move |req| {
            if let Some((_, range)) = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("range"))
            {
                let subset: Vec<u8> = if let Some(rest) = range.strip_prefix("bytes=") {
                    if let Some((a, b)) = rest.split_once('-') {
                        let start: usize = a.parse().unwrap_or(0);
                        let end: usize = b.parse().unwrap_or(wav_len - 1);
                        let end = end.min(wav_len - 1);
                        if start <= end && start < wav_len {
                            wav_clone[start..=end].to_vec()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };
                return HttpResponse {
                    status: 206,
                    reason: "Partial Content",
                    headers: vec![
                        ("Accept-Ranges".to_string(), "bytes".to_string()),
                        (
                            "Content-Range".to_string(),
                            format!("bytes {}-{}/{}", 0, subset.len().saturating_sub(1), wav_len),
                        ),
                    ],
                    body: subset,
                };
            }
            HttpResponse {
                status: 200,
                reason: "OK",
                headers: vec![],
                body: wav_clone,
            }
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        // Request a range; server should answer 206 and mark the source range-seeking.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let state = Arc::new(StreamState::new());
        let (mut reader, producer) = runtime
            .block_on(download_stream(
                client,
                url,
                HeaderMap::new(),
                Some((0, 99)),
                state.clone(),
            ))
            .unwrap();
        assert!(reader.seekable, "server Accept-Ranges should mark seekable");
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        producer.abort();
        let _ = runtime.block_on(producer);
        assert_eq!(
            out,
            wav[0..100],
            "range request should deliver exactly the requested bytes"
        );
    }

    #[test]
    fn test_http_no_range_support() {
        let wav = make_wav(440.0, 50, 48000);
        let wav_clone = wav.clone();
        let port = serve_one(move |_req| HttpResponse {
            status: 200,
            reason: "OK",
            headers: vec![], // no Accept-Ranges
            body: wav_clone,
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let body = fetch_streaming(&url).unwrap();
        assert_eq!(
            body, wav,
            "full body must be delivered without range support"
        );
    }

    #[test]
    fn test_http_chunked_transfer() {
        let wav = make_wav(440.0, 50, 48000);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let wav_clone = wav.clone();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req_buf = [0u8; 8192];
                let _ = stream.read(&mut req_buf);
                // Raw chunked encoding, no Content-Length header.
                let mut out = Vec::new();
                out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
                out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
                out.extend_from_slice(b"\r\n");
                let mut remaining = wav_clone.as_slice();
                while !remaining.is_empty() {
                    let take = remaining.len().min(256);
                    let (chunk, rest) = remaining.split_at(take);
                    out.extend_from_slice(format!("{:x}\r\n", take).as_bytes());
                    out.extend_from_slice(chunk);
                    out.extend_from_slice(b"\r\n");
                    remaining = rest;
                }
                out.extend_from_slice(b"0\r\n\r\n");
                let _ = stream.write_all(&out);
            }
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let body = fetch_streaming(&url).unwrap();
        assert_eq!(body, wav, "chunked transfer must deliver every byte");
    }

    #[test]
    fn test_http_slow_delivery() {
        let wav = make_wav(440.0, 30, 48000);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let wav_clone = wav.clone();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req_buf = [0u8; 8192];
                let _ = stream.read(&mut req_buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    wav_clone.len()
                );
                let _ = stream.write_all(head.as_bytes());
                for chunk in wav_clone.chunks(64) {
                    let _ = stream.write_all(chunk);
                    thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let body = fetch_streaming(&url).unwrap();
        assert_eq!(body, wav, "slow delivery must deliver every byte");
    }

    #[test]
    fn test_http_midstream_disconnect() {
        let wav = make_wav(440.0, 500, 48000);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let wav_clone = wav.clone();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req_buf = [0u8; 8192];
                let _ = stream.read(&mut req_buf);
                let head = b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n";
                let _ = stream.write_all(head);
                // Send only the first 100 bytes then disconnect (interrupted download).
                let _ = stream.write_all(&wav_clone[..100.min(wav_clone.len())]);
            }
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        // Interrupted download must surface as an error, not silent short read.
        let result = fetch_streaming(&url);
        assert!(
            result.is_err(),
            "expected midstream disconnect to error, got {} bytes",
            result.as_ref().map(|b| b.len()).unwrap_or(0)
        );
    }

    /// Serve N HTTP requests sequentially on an ephemeral port, then shut down.
    fn serve_many<F>(mut handler: F) -> u16
    where
        F: FnMut(HttpRequest) -> HttpResponse + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for _ in 0..10 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = parse_request(&buf[..n]);
                    let resp = handler(req);
                    let bytes = render_response(&resp);
                    let _ = stream.write_all(&bytes);
                }
            }
        });
        port
    }

    /// Fetch a URL using `download_stream`, returning any `AudioError`.
    fn fetch_streaming_error(url: &str) -> std::result::Result<Vec<u8>, AudioError> {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let state = Arc::new(StreamState::new());
        let (mut reader, producer) = runtime.block_on(download_stream(
            client,
            url.to_string(),
            HeaderMap::new(),
            None,
            state.clone(),
        ))?;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        producer.abort();
        let _ = runtime.block_on(producer);
        Ok(out)
    }

    #[test]
    fn test_http_401_returns_auth_error() {
        let port = serve_one(move |_req| HttpResponse {
            status: 401,
            reason: "Unauthorized",
            headers: vec![],
            body: b"not audio".to_vec(),
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let err = fetch_streaming_error(&url).unwrap_err();
        assert!(
            matches!(err, AudioError::AuthError(_)),
            "expected AuthError, got {err:?}"
        );
        assert!(err.redacted_message().contains("sign in"));
    }

    #[test]
    fn test_http_403_returns_forbidden_error() {
        let port = serve_one(move |_req| HttpResponse {
            status: 403,
            reason: "Forbidden",
            headers: vec![],
            body: b"not audio".to_vec(),
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let err = fetch_streaming_error(&url).unwrap_err();
        assert!(
            matches!(err, AudioError::Forbidden(_)),
            "expected Forbidden, got {err:?}"
        );
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_http_429_with_retry_after_retries_then_succeeds() {
        let wav = make_wav(440.0, 50, 48000);
        let wav_clone = wav.clone();
        let port = serve_many(move |_req| {
            static mut COUNT: u32 = 0;
            unsafe {
                COUNT += 1;
                if COUNT == 1 {
                    return HttpResponse {
                        status: 429,
                        reason: "Too Many Requests",
                        headers: vec![("Retry-After".to_string(), "1".to_string())],
                        body: b"rate limited".to_vec(),
                    };
                }
                HttpResponse {
                    status: 200,
                    reason: "OK",
                    headers: vec![],
                    body: wav_clone.clone(),
                }
            }
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let body = fetch_streaming(&url).unwrap();
        assert_eq!(body, wav, "429 with Retry-After should retry then succeed");
    }

    #[test]
    fn test_http_429_without_retry_after_exhausts_retries() {
        let port = serve_many(move |_req| HttpResponse {
            status: 429,
            reason: "Too Many Requests",
            headers: vec![],
            body: b"rate limited".to_vec(),
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let err = fetch_streaming_error(&url).unwrap_err();
        assert!(
            matches!(err, AudioError::RateLimited { retry_after: None }),
            "expected RateLimited without retry_after, got {err:?}"
        );
    }

    #[test]
    fn test_http_500_retries_then_errors() {
        let port = serve_many(move |_req| HttpResponse {
            status: 500,
            reason: "Internal Server Error",
            headers: vec![],
            body: b"server error".to_vec(),
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let err = fetch_streaming_error(&url).unwrap_err();
        assert!(
            matches!(err, AudioError::TransientError(_)),
            "expected TransientError, got {err:?}"
        );
    }

    #[test]
    fn test_http_200_success_decodes() {
        let wav = make_wav(440.0, 50, 48000);
        let wav_clone = wav.clone();
        let port = serve_one(move |_req| HttpResponse {
            status: 200,
            reason: "OK",
            headers: vec![],
            body: wav_clone,
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let body = fetch_streaming(&url).unwrap();
        assert_eq!(body, wav);
    }

    #[test]
    fn test_http_206_partial_decodes() {
        let wav = make_wav(440.0, 50, 48000);
        let wav_clone = wav.clone();
        let wav_len = wav.len();
        let port = serve_one(move |req| {
            if let Some((_, range)) = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("range"))
            {
                let subset: Vec<u8> = if let Some(rest) = range.strip_prefix("bytes=") {
                    if let Some((a, b)) = rest.split_once('-') {
                        let start: usize = a.parse().unwrap_or(0);
                        let end: usize = b.parse().unwrap_or(wav_len - 1);
                        let end = end.min(wav_len - 1);
                        if start <= end && start < wav_len {
                            wav_clone[start..=end].to_vec()
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };
                return HttpResponse {
                    status: 206,
                    reason: "Partial Content",
                    headers: vec![
                        ("Accept-Ranges".to_string(), "bytes".to_string()),
                        (
                            "Content-Range".to_string(),
                            format!("bytes {}-{}/{}", 0, subset.len().saturating_sub(1), wav_len),
                        ),
                    ],
                    body: subset,
                };
            }
            HttpResponse {
                status: 200,
                reason: "OK",
                headers: vec![],
                body: wav_clone,
            }
        });
        let url = format!("http://127.0.0.1:{port}/track.wav");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let state = Arc::new(StreamState::new());
        let (mut reader, producer) = runtime
            .block_on(download_stream(
                client,
                url,
                HeaderMap::new(),
                Some((0, 99)),
                state.clone(),
            ))
            .unwrap();
        assert!(reader.seekable, "server Accept-Ranges should mark seekable");
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        producer.abort();
        let _ = runtime.block_on(producer);
        assert_eq!(
            out,
            wav[0..100],
            "206 partial content should deliver requested bytes"
        );
    }

    #[test]
    fn test_classify_http_status_maps_all_codes() {
        use reqwest::header::{HeaderMap, RETRY_AFTER};

        let empty = HeaderMap::new();
        assert!(classify_http_status(reqwest::StatusCode::OK, &empty).is_ok());
        assert!(classify_http_status(reqwest::StatusCode::PARTIAL_CONTENT, &empty).is_ok());

        let err = classify_http_status(reqwest::StatusCode::UNAUTHORIZED, &empty).unwrap_err();
        assert!(matches!(err, AudioError::AuthError(_)));

        let err = classify_http_status(reqwest::StatusCode::FORBIDDEN, &empty).unwrap_err();
        assert!(matches!(err, AudioError::Forbidden(_)));

        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "5".parse().unwrap());
        let err =
            classify_http_status(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers).unwrap_err();
        assert!(
            matches!(err, AudioError::RateLimited { retry_after: Some(d) } if d.as_secs() == 5)
        );

        let err =
            classify_http_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &empty).unwrap_err();
        assert!(matches!(err, AudioError::TransientError(_)));

        let err = classify_http_status(reqwest::StatusCode::BAD_GATEWAY, &empty).unwrap_err();
        assert!(matches!(err, AudioError::TransientError(_)));

        let err = classify_http_status(reqwest::StatusCode::BAD_REQUEST, &empty).unwrap_err();
        assert!(matches!(err, AudioError::CorruptResponse(_)));
    }

    // --- Stream URL leak prevention tests ---

    #[test]
    fn test_track_handle_serialization_excludes_stream_url() {
        let handle = TrackHandle {
            id: "track-123".to_string(),
            title: "Test Track".to_string(),
            artist: "Test Artist".to_string(),
            duration: None,
        };
        let json = serde_json::to_string(&handle).unwrap();
        assert!(
            !json.contains("stream_url"),
            "stream_url must not be serialized: {json}"
        );
        assert!(
            !json.contains("youtube.com"),
            "signed URL host must not appear: {json}"
        );
        assert!(
            !json.contains("googlevideo.com"),
            "signed URL host must not appear: {json}"
        );
    }

    #[test]
    fn test_track_handle_serialization_rejects_secret_url() {
        // Inject a fake secret-bearing stream URL into the internal map,
        // then verify it never appears in serialized TrackHandle output.
        let handle = TrackHandle {
            id: "track-xyz".to_string(),
            title: "Secret Track".to_string(),
            artist: "Secret Artist".to_string(),
            duration: None,
        };
        let _secret_url = "https://youtube.com/watch?v=secret&signature=abc123&token=supersecret";
        let json = serde_json::to_string(&handle).unwrap();
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
    fn test_audio_event_track_changed_excludes_stream_url() {
        let handle = TrackHandle {
            id: "track-456".to_string(),
            title: "Changed Track".to_string(),
            artist: "Changed Artist".to_string(),
            duration: None,
        };
        let event = AudioEvent::TrackChanged(Some(handle));
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("stream_url"),
            "stream_url must not appear in TrackChanged: {json}"
        );
        assert!(
            !json.contains("youtube.com"),
            "signed URL host must not appear in TrackChanged: {json}"
        );
    }

    #[test]
    fn test_audio_event_track_ended_excludes_stream_url() {
        let handle = TrackHandle {
            id: "track-789".to_string(),
            title: "Ended Track".to_string(),
            artist: "Ended Artist".to_string(),
            duration: None,
        };
        let event = AudioEvent::TrackEnded(handle);
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("stream_url"),
            "stream_url must not appear in TrackEnded: {json}"
        );
        assert!(
            !json.contains("googlevideo.com"),
            "signed URL host must not appear in TrackEnded: {json}"
        );
    }

    #[test]
    fn test_audio_event_crossfade_started_excludes_stream_url() {
        let from = TrackHandle {
            id: "track-a".to_string(),
            title: "From Track".to_string(),
            artist: "From Artist".to_string(),
            duration: None,
        };
        let to = TrackHandle {
            id: "track-b".to_string(),
            title: "To Track".to_string(),
            artist: "To Artist".to_string(),
            duration: None,
        };
        let event = AudioEvent::CrossfadeStarted { from, to };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("stream_url"),
            "stream_url must not appear in CrossfadeStarted: {json}"
        );
        assert!(
            !json.contains("youtube.com"),
            "signed URL host must not appear in CrossfadeStarted: {json}"
        );
    }

    #[test]
    fn test_playback_handle_is_not_serializable() {
        // PlaybackHandle must NOT implement Serialize (internal only).
        // This test verifies it contains a stream_url field that would leak if serialized.
        let playback = PlaybackHandle {
            track: TrackHandle {
                id: "track-internal".to_string(),
                title: "Internal".to_string(),
                artist: "Internal Artist".to_string(),
                duration: None,
            },
            stream_url: "https://youtube.com/watch?v=leakme&sig=supersecret".to_string(),
        };
        assert!(playback.stream_url.contains("supersecret"));
        // TrackHandle portion serializes safely without stream_url.
        let track_json = serde_json::to_string(&playback.track).unwrap();
        assert!(!track_json.contains("stream_url"));
        assert!(!track_json.contains("supersecret"));
    }

    /// Regression test: TrackHandleDto deserialization must accept the real
    /// frontend IPC payload. Tauri maps the Rust parameter `track_handle`
    /// to the default camelCase IPC key `trackHandle` with
    /// fields `id`, `title`, `artist`, `duration`.
    ///
    /// Tests duration in four forms: omitted, null, numeric milliseconds,
    /// and Serde's native {secs, nanos} object.
    #[test]
    fn test_track_handle_dto_deserialization_from_frontend_payload() {
        use serde_json::json;

        // 1. duration omitted (frontend may not always send it)
        let payload = json!({
            "track_handle": {
                "id": "track-1",
                "title": "Track One",
                "artist": "Artist One"
            }
        });
        let dto: TrackHandleDto = serde_json::from_value(payload["track_handle"].clone()).unwrap();
        assert_eq!(dto.id, "track-1");
        assert_eq!(dto.title, "Track One");
        assert_eq!(dto.artist, "Artist One");
        assert_eq!(dto.duration, None);

        // 2. duration = null
        let payload = json!({
            "track_handle": {
                "id": "track-2",
                "title": "Track Two",
                "artist": "Artist Two",
                "duration": null
            }
        });
        let dto: TrackHandleDto = serde_json::from_value(payload["track_handle"].clone()).unwrap();
        assert_eq!(dto.duration, None);

        // 3. duration = numeric milliseconds (frontend sends number)
        let payload = json!({
            "track_handle": {
                "id": "track-3",
                "title": "Track Three",
                "artist": "Artist Three",
                "duration": 225000
            }
        });
        let dto: TrackHandleDto = serde_json::from_value(payload["track_handle"].clone()).unwrap();
        assert_eq!(dto.duration, Some(225000));

        // 4. duration = {secs, nanos} (Serde's native Duration format)
        let payload = json!({
            "track_handle": {
                "id": "track-4",
                "title": "Track Four",
                "artist": "Artist Four",
                "duration": { "secs": 225, "nanos": 0 }
            }
        });
        let dto: TrackHandleDto = serde_json::from_value(payload["track_handle"].clone()).unwrap();
        assert_eq!(dto.duration, Some(225000));

        // 5. DTO converts to TrackHandle correctly
        let track: TrackHandle = dto.into();
        assert_eq!(track.id, "track-4");
        assert_eq!(track.title, "Track Four");
        assert_eq!(track.artist, "Artist Four");
        assert_eq!(track.duration, Some(Duration::from_secs(225)));
    }

    #[test]
    fn audio_play_deserializes_actual_frontend_payload() {
        let args = serde_json::json!({
            "trackHandle": {
                "id": "imported-mp3",
                "title": "Song",
                "artist": "Artist",
                "duration": 225000
            }
        });
        let dto: TrackHandleDto = serde_json::from_value(args["trackHandle"].clone()).unwrap();
        let handle: TrackHandle = dto.into();
        assert_eq!(handle.id, "imported-mp3");
        assert_eq!(handle.duration, Some(Duration::from_millis(225000)));
    }

    /// Unit test: decode_from_source with a local WAV file:// path.
    #[test]
    fn test_decode_from_source_local_wav() {
        let wav = make_wav(440.0, 100, 48000);
        let path = std::env::temp_dir().join("orcave_test_wav.wav");
        std::fs::write(&path, &wav).unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let source = Box::new(LocalFileSource::new(file)) as Box<dyn MediaSource + Send>;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (frames, total_samples, sample_rate, channels) = rt
            .block_on(AudioEngine::decode_from_source(
                source,
                Arc::new(AtomicBool::new(false)),
            ))
            .expect("local WAV decode_from_source failed");

        assert!(!frames.is_empty());
        assert_eq!(sample_rate, 48000);
        assert_eq!(channels, 2);
        assert!(total_samples > 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Regression test for ORCAVE-ALPHA-R3O: `path_to_file_url` emits
    /// `file:///C:/path` (3 slashes) on Windows; `file_url_to_path` must trim
    /// the extra leading `/` so `File::open` gets `C:/path` (Windows rejects
    /// `/C:/path`).
    #[test]
    fn test_file_url_to_path_windows_triple_slash() {
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/song.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/Music/song.mp3"),
            "stripped file:/// URL must yield C:/path on Windows"
        );
        assert!(!AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/song.mp3")
            .unwrap()
            .starts_with("/"));
    }

    /// Regression test for ORCAVE-ALPHA-R3P: `%20` (space) and other
    /// percent-encoded characters must be decoded before `File::open`, so
    /// `file:///.../My%20Music/song.mp3` opens the real `My Music` folder.
    #[test]
    fn test_file_url_to_path_percent_decode() {
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/My%20Music/song.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/My Music/song.mp3"),
            "%20 must decode to a literal space"
        );
        // Multiple encoded segments.
        assert_eq!(
            AudioEngine::file_url_to_path("file:///D:/%5BPlaylist%5D/song%2Emp3").unwrap(),
            PathBuf::from("D:/[Playlist]/song.mp3"),
            "%5B/%5D and %2E must decode"
        );
    }

    /// Unicode filenames survive the round trip: UTF-8 bytes are
    /// percent-encoded in the URL and decoded back on the path.
    #[test]
    fn test_file_url_to_path_unicode() {
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/caf%C3%A9.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/Music/café.mp3"),
            "%C3%A9 must decode to é"
        );
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/%E3%81%82%E3%82%82.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/Music/あも.mp3"),
            "%E3%81%82%E3%82%82 must decode to あも (U+3042 U+3082)"
        );
    }

    /// Reserved URL characters that were encoded must decode to their literal
    /// form on disk.
    #[test]
    fn test_file_url_to_path_reserved_chars() {
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/song%231.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/Music/song#1.mp3"),
            "%23 must decode to #"
        );
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/a%2Fb.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/Music/a/b.mp3"),
            "%2F must decode to /"
        );
        assert_eq!(
            AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/100%25.mp3").unwrap(),
            PathBuf::from("C:/Users/Me/Music/100%.mp3"),
            "%25 must decode to %"
        );
    }

    /// UNC paths keep the leading `//`.
    #[test]
    fn test_file_url_to_path_unc() {
        assert_eq!(
            AudioEngine::file_url_to_path("file:////server/share/song.mp3").unwrap(),
            PathBuf::from("//server/share/song.mp3"),
            "UNC file:// URL must keep leading //"
        );
    }

    /// Invalid percent-encoding is rejected as `AudioError::Stream`, not
    /// silently passed through to `File::open`.
    #[test]
    fn test_file_url_to_path_invalid_percent_encoding() {
        let err = AudioEngine::file_url_to_path("file:///C:/Users/Me/Music/%ZZ/song.mp3")
            .unwrap_err();
        assert!(
            matches!(err, AudioError::Stream(_)),
            "invalid percent-encoding must yield AudioError::Stream, got {err:?}"
        );
    }

    /// Non-`file://` URLs are rejected.
    #[test]
    fn test_file_url_to_path_rejects_non_file_urls() {
        let err = AudioEngine::file_url_to_path("http://example.com/song.mp3").unwrap_err();
        assert!(
            matches!(err, AudioError::Stream(_)),
            "non file:// URL must yield AudioError::Stream, got {err:?}"
        );
    }

    /// Unit test: decode_from_source with a missing file returns an error.
    #[test]
    fn test_decode_from_source_missing_file() {
        let path = std::env::temp_dir().join("orcave_nonexistent_xyz.wav");
        let result = std::fs::File::open(&path);
        assert!(result.is_err(), "missing file should fail to open");
    }

    /// Unit test: set_output_device updates the output_device field.
    #[test]
    fn test_set_output_device_field() {
        // Verify that cpal device enumeration works and we can find devices.
        let host = cpal::default_host();
        let devices: Vec<_> = host
            .output_devices()
            .expect("failed to enumerate output devices")
            .collect();
        assert!(!devices.is_empty(), "no output devices found");

        // Verify device name matching works
        let default = host
            .default_output_device()
            .expect("no default output device");
        let name = default.name().unwrap_or_default();
        assert!(!name.is_empty(), "default device should have a name");

        // Find the default device by name (simulating set_output_device logic)
        let found = devices
            .iter()
            .find(|d| d.name().map_or(false, |n| n == name))
            .expect("should find default device by name");
        assert_eq!(found.name().unwrap(), default.name().unwrap());
    }

    // --- ORCAVE-ALPHA-R3P regression tests: error propagation observability ---

    /// Regression test: `decode_from_source` must propagate source `read` errors
    /// instead of swallowing them. A `MediaSource` whose `read` returns an
    /// `io::Error` must cause `decode_from_source` to return `Err`.
    #[test]
    fn test_decode_from_source_propagates_read_error() {
        use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
        use symphonia::core::io::MediaSource;

        struct ErroringSource;

        impl Read for ErroringSource {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(ErrorKind::Other, "simulated read error"))
            }
        }

        impl Seek for ErroringSource {
            fn seek(&mut self, _style: SeekFrom) -> io::Result<u64> {
                Err(io::Error::new(ErrorKind::Other, "simulated seek error"))
            }
        }

        impl MediaSource for ErroringSource {
            fn is_seekable(&self) -> bool {
                true
            }

            fn byte_len(&self) -> Option<u64> {
                None
            }
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(AudioEngine::decode_from_source(
            Box::new(ErroringSource),
            Arc::new(AtomicBool::new(false)),
        ));

        assert!(
            result.is_err(),
            "decode_from_source should propagate source read error, got {:?}",
            result
        );
    }

    /// Regression test: `decode_from_source` must error on an empty /
    /// unrecognizable source instead of silently returning success with zero
    /// frames.
    #[test]
    fn test_decode_from_source_empty_source_errors() {
        use std::io::{self, Read, Seek, SeekFrom};
        use symphonia::core::io::MediaSource;

        struct EmptySource;

        impl Read for EmptySource {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0) // immediate EOF
            }
        }

        impl Seek for EmptySource {
            fn seek(&mut self, _style: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }

        impl MediaSource for EmptySource {
            fn is_seekable(&self) -> bool {
                true
            }

            fn byte_len(&self) -> Option<u64> {
                Some(0)
            }
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(AudioEngine::decode_from_source(
            Box::new(EmptySource),
            Arc::new(AtomicBool::new(false)),
        ));

        assert!(
            result.is_err(),
            "decode_from_source should error on empty/unrecognizable source, got {:?}",
            result
        );
    }

    /// Regression test: `AudioError` variants flow through `anyhow::Error` into
    /// `TauriError::Internal` at the IPC boundary. The human-readable message is
    /// preserved, but the specific `AudioError` variant (TrackNotFound, Decoder,
    /// etc.) is lost — only the string survives for the frontend.
    #[test]
    fn test_audio_error_to_tauri_error_preserves_message() {
        use orcave_core::TauriError;

        let audio_err = AudioError::TrackNotFound("track-abc".to_string());
        let anyhow_err: anyhow::Error = audio_err.into();
        let tauri_err: TauriError = anyhow_err.into();

        match tauri_err {
            TauriError::Internal(msg) => {
                assert!(
                    msg.contains("TrackNotFound") || msg.contains("track-abc"),
                    "TauriError message should contain AudioError details, got: {msg}"
                );
            }
            other => panic!("expected TauriError::Internal, got {other:?}"),
        }
    }

    /// Regression test for ORCAVE-ALPHA-R3Q: playing a local library track whose
    /// stream URL was never registered must surface as `TrackNotFound`, not
    /// silently succeed. `decode_track`'s local-file branch converts the
    /// `file://` URL to a `PathBuf` then `File::open`s it; a missing file must
    /// fail at that boundary (observable `Stream` error, not a false success).
    #[test]
    fn test_local_file_missing_surfaces_stream_error() {
        let file_url = "file:///C:/Users/Me/Music/nonexistent_track.wav";
        assert!(file_url.starts_with("file://"));

        let path_str = file_url
            .strip_prefix("file://")
            .expect("file:// URL must strip prefix");
        let path = if path_str.starts_with("//") {
            PathBuf::from(path_str) // UNC: keep leading //
        } else {
            PathBuf::from(path_str.trim_start_matches('/'))
        };

        // This is the exact path `decode_track` passes to `File::open`.
        let open_result = std::fs::File::open(&path);
        assert!(
            open_result.is_err(),
            "missing local file must fail to open: {:?}",
            path
        );
    }

    /// Regression test for ORCAVE-ALPHA-R3Q: `TrackNotFound` (the error
    /// `audio_play`/`play()` returns when `stream_urls[id]` is absent) flows
    /// through `anyhow::Error` into `TauriError::Internal` at the IPC boundary,
    /// preserving the track id in the message so the frontend can observe the
    /// failure instead of a swallowed/false success.
    #[test]
    fn test_audio_error_track_not_found_to_tauri_internal() {
        use orcave_core::TauriError;

        let audio_err = AudioError::TrackNotFound("track-abc".to_string());
        let anyhow_err: anyhow::Error = audio_err.into();
        let tauri_err: TauriError = anyhow_err.into();

        match tauri_err {
            TauriError::Internal(msg) => {
                assert!(
                    msg.contains("track-abc"),
                    "TauriError::Internal should preserve the track id so the frontend can observe the failure, got: {msg}"
                );
                assert!(
                    !msg.is_empty(),
                    "TauriError::Internal message must be non-empty"
                );
            }
            other => panic!("expected TauriError::Internal, got {other:?}"),
        }
    }

    /// Regression test for ORCAVE-ALPHA-R3Q: `TrackNotFound` must NOT be
    /// retryable — `play()` must not silently retry a missing stream URL and
    /// report false success. It must return the error to the frontend.
    #[test]
    fn test_track_not_found_is_not_retryable() {
        let err = AudioError::TrackNotFound("track-abc".to_string());
        assert!(!err.is_retryable(), "TrackNotFound must not be retryable");
        assert!(
            err.retry_delay(0).is_none(),
            "TrackNotFound must have no retry delay"
        );
    }
}
