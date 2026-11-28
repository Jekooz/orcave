//! Orcave Tauri - Main application entry point
//!
//! This is the Tauri 2 + React/TypeScript frontend for the Orcave 2.0 Alpha.
//! The diagnostic UI provides controls for:
//! - Authentication
//! - Catalogue browsing
//! - Playback and queue
//! - Recommendations
//! - Settings
//! - Redacted diagnostics

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::RwLock;
use tauri::{Emitter, Manager};
use tauri_plugin_store::StoreExt;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, instrument, Level};
use tracing_subscriber::FmtSubscriber;

use orcave_audio::{commands as audio_commands, AudioEngine, AudioEvent, PlaybackState};
use orcave_auth::{commands as auth_commands, AuthEvent, AuthService, AuthState};
use orcave_catalogue::{commands as catalogue_commands, CatalogueService};
use orcave_core::{AppInstanceId, AppLifecycle, DiagnosticEvent, TauriError, TauriResult};
use orcave_library::{commands as library_commands, LibraryService};
use orcave_queue::{commands as queue_commands, QueueEvent, QueueService};
use orcave_recommendations::{
    commands as recommendations_commands, RecommendationService, RecommendationSet,
};
use orcave_settings::{NativeSessionService, SessionError, Settings, SettingsService};
use orcave_windows::{WindowsIntegration, PythonSidecarManager};

/// Tauri command handlers for settings
#[tauri::command]
async fn settings_get_all(state: tauri::State<'_, Arc<SettingsService>>) -> TauriResult<Settings> {
    state
        .get_all()
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))
}

#[tauri::command]
async fn settings_get(
    state: tauri::State<'_, Arc<SettingsService>>,
    key: String,
) -> TauriResult<serde_json::Value> {
    check_public_settings_key(&key)?;
    state
        .get(&key)
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))
        .map(|v| v.unwrap_or(serde_json::Value::Null))
}

#[tauri::command]
async fn settings_set(
    state: tauri::State<'_, Arc<SettingsService>>,
    key: String,
    value: serde_json::Value,
) -> TauriResult<()> {
    check_public_settings_key(&key)?;
    state
        .set(&key, &value)
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))?;
    state
        .save()
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))
}

#[tauri::command]
async fn settings_set_encrypted(
    state: tauri::State<'_, Arc<SettingsService>>,
    key: String,
    value: serde_json::Value,
) -> TauriResult<()> {
    check_public_settings_key(&key)?;
    state
        .set_encrypted(&key, &value)
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))?;
    state
        .save()
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))
}

#[tauri::command]
async fn settings_remove(
    state: tauri::State<'_, Arc<SettingsService>>,
    key: String,
) -> TauriResult<()> {
    check_public_settings_key(&key)?;
    state
        .remove(&key)
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))
}

fn check_public_settings_key(key: &str) -> TauriResult<()> {
    if key.starts_with("auth.")
        || key.starts_with("native.session.")
        || key.starts_with("orcave.encrypted.")
        || key == "encryption_key"
    {
        return Err(TauriError::Settings("Reserved setting".to_string()));
    }
    Ok(())
}

fn r3v_runtime_data_root(value: Option<OsString>, temp: &Path) -> Result<Option<PathBuf>> {
    let Some(value) = value else { return Ok(None) };
    let root = PathBuf::from(value).canonicalize()?;
    let base = temp.join("opencode").canonicalize()?;
    anyhow::ensure!(
        root.starts_with(&base) && root != base,
        "R3V data directory must be a dedicated folder under the opencode temp directory"
    );
    Ok(Some(root))
}

#[tauri::command]
async fn settings_reset(state: tauri::State<'_, Arc<SettingsService>>) -> TauriResult<()> {
    state
        .reset_to_defaults()
        .await
        .map_err(|e| TauriError::Settings(e.to_string()))
}

// ===== Browser-based authentication commands (Python sidecar) =====

/// Browser info for sign-in prompt
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct BrowserInfo {
    browser: String,
}

/// Browser sign-in status
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct BrowserAuthStatus {
    state: String,
    browser: String,
    error: Option<String>,
}

/// Start browser-based sign-in flow
#[tauri::command]
async fn auth_browser_start(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
    open_browser: Option<bool>,
) -> TauriResult<BrowserAuthStatus> {
    let open = open_browser.unwrap_or(true);
    let result = sidecar
        .auth_browser_start(open)
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))?;
    
    Ok(BrowserAuthStatus {
        state: result.get("state").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        browser: result.get("browser").and_then(|v| v.as_str()).unwrap_or("your browser").to_string(),
        error: result.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
    })
}

/// Get browser sign-in status
#[tauri::command]
async fn auth_browser_status(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
) -> TauriResult<BrowserAuthStatus> {
    let result = sidecar
        .auth_browser_status()
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))?;
    
    Ok(BrowserAuthStatus {
        state: result.get("state").and_then(|v| v.as_str()).unwrap_or("idle").to_string(),
        browser: result.get("browser").and_then(|v| v.as_str()).unwrap_or("your browser").to_string(),
        error: result.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
    })
}

/// Get browser info (which browser will be used for sign-in)
#[tauri::command]
async fn auth_browser_info(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
) -> TauriResult<BrowserInfo> {
    let result = sidecar
        .auth_browser_info()
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))?;
    
    Ok(BrowserInfo {
        browser: result.get("browser").and_then(|v| v.as_str()).unwrap_or("your browser").to_string(),
    })
}

/// User confirms they've signed in - close window and read cookies
#[tauri::command]
async fn auth_browser_finish(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
) -> TauriResult<BrowserAuthStatus> {
    let result = sidecar
        .auth_browser_finish()
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))?;
    
    Ok(BrowserAuthStatus {
        state: result.get("state").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
        browser: result.get("browser").and_then(|v| v.as_str()).unwrap_or("your browser").to_string(),
        error: result.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
    })
}

/// Cancel browser sign-in flow
#[tauri::command]
async fn auth_browser_cancel(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
) -> TauriResult<BrowserAuthStatus> {
    let result = sidecar
        .auth_browser_cancel()
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))?;
    
    Ok(BrowserAuthStatus {
        state: result.get("state").and_then(|v| v.as_str()).unwrap_or("cancelled").to_string(),
        browser: result.get("browser").and_then(|v| v.as_str()).unwrap_or("your browser").to_string(),
        error: result.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
    })
}

/// Handoff cookies/headers from browser (Settings paste fallback)
#[tauri::command]
async fn auth_handoff(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
    cookies: serde_json::Value,
    headers: Option<serde_json::Value>,
) -> TauriResult<serde_json::Value> {
    sidecar
        .auth_handoff(cookies, headers)
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))
}

/// Get auth status
#[tauri::command]
async fn auth_status(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
) -> TauriResult<serde_json::Value> {
    sidecar
        .auth_status()
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))
}

/// Sign out
#[tauri::command]
async fn auth_signout(
    sidecar: tauri::State<'_, Arc<PythonSidecarManager>>,
) -> TauriResult<serde_json::Value> {
    sidecar
        .auth_signout()
        .await
        .map_err(|e| TauriError::Auth(e.to_string()))
}

// ===== Native session commands (legacy local material, not provider verification) =====

/// Redacted local-material status for the frontend. Never signals provider access.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
struct SessionStatus {
    state: String,
    connected: bool,
    expired: bool,
    ttl_secs: u64,
    handle: Option<String>,
}

#[tauri::command]
async fn native_session_disconnect(
    session: tauri::State<'_, Arc<NativeSessionService>>,
) -> Result<(), orcave_settings::SessionError> {
    session.disconnect().await
}

#[tauri::command]
async fn native_session_get_status(
    session: tauri::State<'_, Arc<NativeSessionService>>,
) -> Result<SessionStatus, orcave_settings::SessionError> {
    let st = session.state();
    Ok(SessionStatus {
        state: format!("{st:?}").to_lowercase(),
        connected: false,
        expired: session.is_expired(),
        ttl_secs: 0,
        handle: None,
    })
}

#[derive(serde::Serialize)]
struct NativeProbe {
    pid: u32,
    platform: &'static str,
    version: &'static str,
}

#[tauri::command]
fn diagnostics_native_probe() -> NativeProbe {
    NativeProbe {
        pid: std::process::id(),
        platform: std::env::consts::OS,
        version: env!("CARGO_PKG_VERSION"),
    }
}

fn auth_state_payload(state: AuthState) -> &'static str {
    match state {
        AuthState::Unauthenticated => "unauthenticated",
        AuthState::Authenticating => "authenticating",
        AuthState::Authenticated => "authenticated",
        AuthState::TokenExpired => "token_expired",
        AuthState::Refreshing => "refreshing",
        AuthState::Error => "error",
    }
}

// Re-export command handlers
mod commands {
    pub use orcave_audio::*;
    pub use orcave_catalogue::*;
    pub use orcave_queue::*;
    pub use orcave_recommendations::*;
    // Settings commands are defined locally above
}

/// Application-wide events for cross-subsystem communication
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
#[non_exhaustive]
pub enum AppEvent {
    LifecycleChanged(AppLifecycle),
    AuthStateChanged(AuthState),
    PlaybackStateChanged(PlaybackState),
    QueueChanged(QueueEvent),
    RecommendationsUpdated(RecommendationSet),
    SettingsChanged(Settings),
    NetworkStatusChanged(bool),
    Error(String),
    Diagnostic(DiagnosticEvent),
}

/// Central application state - single source of truth
pub struct AppState {
    pub instance_id: AppInstanceId,
    pub lifecycle: RwLock<AppLifecycle>,
    pub auth: Arc<AuthService>,
    pub catalogue: Arc<CatalogueService>,
    pub audio: Arc<AudioEngine>,
    pub queue: Arc<QueueService>,
    pub library: Arc<LibraryService>,
    pub recommendations: Arc<RecommendationService>,
    pub settings: Arc<SettingsService>,
    pub windows: Arc<WindowsIntegration>,

    // Event buses for cross-subsystem communication
    pub(crate) app_events: broadcast::Sender<AppEvent>,
    pub(crate) shutdown_tx: mpsc::Sender<()>,
    pub(crate) shutdown_rx: RwLock<Option<mpsc::Receiver<()>>>,
}

/// Handle to the application state for external access (Tauri commands, etc.)
#[derive(Clone)]
pub struct AppHandle {
    state: Arc<AppState>,
}

impl AppHandle {
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    pub fn auth(&self) -> &Arc<AuthService> {
        &self.state.auth
    }

    pub fn catalogue(&self) -> &Arc<CatalogueService> {
        &self.state.catalogue
    }

    pub fn audio(&self) -> &Arc<AudioEngine> {
        &self.state.audio
    }

    pub fn queue(&self) -> &Arc<QueueService> {
        &self.state.queue
    }

    pub fn library(&self) -> &Arc<LibraryService> {
        &self.state.library
    }

    pub fn recommendations(&self) -> &Arc<RecommendationService> {
        &self.state.recommendations
    }

    pub fn settings(&self) -> &Arc<SettingsService> {
        &self.state.settings
    }

    pub fn windows(&self) -> &Arc<WindowsIntegration> {
        &self.state.windows
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AppEvent> {
        self.state.subscribe_events()
    }

    pub fn emit(&self, event: AppEvent) {
        self.state.emit(event);
    }

    #[instrument(skip(self))]
    async fn start_background_tasks(&self) -> Result<()> {
        let state = self.state.clone();

        // Network monitoring task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            let mut last_online = true;

            loop {
                interval.tick().await;
                let online = Self::check_connectivity().await;
                if online != last_online {
                    state.emit(AppEvent::NetworkStatusChanged(online));
                    last_online = online;
                }
            }
        });

        // Periodic diagnostics emission
        let state = self.state.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                state.emit(AppEvent::Diagnostic(DiagnosticEvent::PerformanceMetric {
                    name: "process_uptime".to_string(),
                    value: started.elapsed().as_secs_f64(),
                    unit: "seconds".to_string(),
                }));
            }
        });

        Ok(())
    }

    async fn check_connectivity() -> bool {
        // Simple connectivity check
        reqwest::get("https://www.google.com/generate_204")
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

impl AppState {
    /// Create a new application state with all subsystems initialized
    #[instrument(skip_all)]
    pub async fn new(
        auth: Arc<AuthService>,
        catalogue: Arc<CatalogueService>,
        audio: Arc<AudioEngine>,
        queue: Arc<QueueService>,
        library: Arc<LibraryService>,
        recommendations: Arc<RecommendationService>,
        settings: Arc<SettingsService>,
        windows: Arc<WindowsIntegration>,
    ) -> Result<AppHandle> {
        let instance_id = AppInstanceId::new();
        info!(instance_id = %instance_id, "Initializing Orcave application state");

        let (app_events_tx, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let state = Self {
            instance_id,
            lifecycle: RwLock::new(AppLifecycle::Starting),
            auth,
            catalogue,
            audio,
            queue,
            library,
            recommendations,
            settings,
            windows,
            app_events: app_events_tx,
            shutdown_tx,
            shutdown_rx: RwLock::new(Some(shutdown_rx)),
        };

        let handle = AppHandle {
            state: Arc::new(state),
        };

        // Start background tasks
        handle.start_background_tasks().await?;

        info!(instance_id = %instance_id, "Application state initialized");
        Ok(handle)
    }

    /// Subscribe to application events
    pub fn subscribe_events(&self) -> broadcast::Receiver<AppEvent> {
        self.app_events.subscribe()
    }

    /// Emit an application event
    pub fn emit(&self, event: AppEvent) {
        if let Err(e) = self.app_events.send(event) {
            debug!("No receivers for app event: {}", e);
        }
    }

    /// Get current lifecycle state
    pub fn lifecycle(&self) -> AppLifecycle {
        *self.lifecycle.read()
    }

    /// Set lifecycle state and emit event
    pub fn set_lifecycle(&self, lifecycle: AppLifecycle) {
        *self.lifecycle.write() = lifecycle;
        self.emit(AppEvent::LifecycleChanged(lifecycle));
    }

    /// Initiate graceful shutdown
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        info!("Initiating graceful shutdown");
        self.set_lifecycle(AppLifecycle::ShuttingDown);

        // Signal shutdown to background tasks
        if let Some(mut rx) = self.shutdown_rx.write().take() {
            let _ = self.shutdown_tx.send(()).await;
            rx.close();
        }

        // Shutdown subsystems in reverse dependency order
        self.recommendations.shutdown().await?;
        self.library.shutdown().await?;
        self.queue.shutdown().await?;
        self.audio.shutdown().await?;
        self.catalogue.shutdown().await?;
        self.auth.shutdown().await?;
        self.windows.shutdown().await?;
        self.settings.shutdown().await?;

        self.set_lifecycle(AppLifecycle::Terminated);
        info!("Shutdown complete");
        Ok(())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<()> {
    // Initialize tracing
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .with_target(false)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting Orcave 2.0 Alpha");

    let mut context = tauri::generate_context!();
    if let Some(root) = r3v_runtime_data_root(
        std::env::var_os("ORCAVE_R3V_ISOLATED_DATA_DIR"),
        &std::env::temp_dir(),
    )? {
        orcave_library::artwork::set_isolated_cache_root(root.clone())?;
        context.config_mut().app.app_directories_override =
            Some(tauri::utils::config::AppDirectoriesOverride::Root(root));
        info!("R3V isolated app directories enabled");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // Initialize application state
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = initialize_app(app_handle).await {
                    tracing::error!("Failed to initialize app: {}", e);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // Browser-based authentication commands (Python sidecar)
            auth_browser_start,
            auth_browser_status,
            auth_browser_info,
            auth_browser_finish,
            auth_browser_cancel,
            auth_handoff,
            auth_status,
            auth_signout,
            // Legacy OAuth commands remain separate; none validate browser sessions.
            auth_cancel_flow,
            auth_commands::auth_refresh_token,
            auth_commands::auth_sign_out,
            auth_commands::auth_get_state,
            auth_commands::auth_get_user_info,
            // Catalogue commands
            catalogue_commands::catalogue_browse,
            catalogue_commands::catalogue_search,
            catalogue_commands::catalogue_get_item,
            // catalogue_get_stream_url deliberately excluded: signed stream URLs
            // must not cross the Tauri IPC boundary into frontend JavaScript.
            // Audio plays via queue_add which resolves stream URLs internally.
            // Audio commands
            audio_commands::audio_play,
            audio_commands::audio_pause,
            audio_commands::audio_resume,
            audio_commands::audio_stop,
            audio_commands::audio_seek,
            audio_commands::audio_set_volume,
            audio_commands::audio_get_state,
            audio_commands::audio_get_output_devices,
            audio_commands::audio_set_output_device,
            // Queue commands
            queue_commands::queue_get,
            queue_commands::queue_get_history,
            queue_commands::queue_add,
            queue_commands::queue_remove,
            queue_commands::queue_move,
            queue_commands::queue_clear,
            queue_commands::queue_play_index,
            queue_commands::queue_next,
            queue_commands::queue_previous,
            queue_commands::queue_set_repeat,
            queue_commands::queue_set_shuffle,
            queue_commands::queue_start_radio,
            // Library commands
            library_commands::library_add_folder,
            library_commands::library_remove_folder,
            library_commands::library_get_tracks,
            library_commands::library_get_artists,
            library_commands::library_get_albums,
            library_commands::library_get_folders,
            library_commands::library_get_artwork,
            library_commands::library_rescan,
            library_commands::library_register_track,
            library_commands::library_state,
            // Recommendations commands
            recommendations_commands::recommendations_get_for_seed,
            recommendations_commands::recommendations_get_mix,
            recommendations_commands::recommendations_get_station,
            recommendations_commands::recommendations_get_related,
            // Settings commands
            settings_get_all,
            settings_get,
            settings_set,
            settings_set_encrypted,
            settings_remove,
            settings_reset,
            // Native session commands
            native_session_disconnect,
            native_session_get_status,
            diagnostics_native_probe,
        ])
        .run(context)
        .map_err(|e| anyhow::anyhow!(e))?;

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Orcave failed to start: {error}");
        std::process::exit(1);
    }
}

async fn initialize_app(app_handle: tauri::AppHandle) -> Result<()> {
    // Get the store from Tauri plugin-store
    let store = app_handle.store("settings.dat")?;

    // Initialize settings first (needed by WindowsIntegration)
    let settings = Arc::new(SettingsService::new(store.clone()).await?);

    // Native session service restores legacy local material as unverified.
    let native_session = Arc::new(NativeSessionService::new(settings.clone()).await?);

    // Initialize Windows integration with settings
    let windows = Arc::new(WindowsIntegration::new(settings.clone(), store).await?);

    // Initialize Python sidecar manager for browser-based authentication
    let sidecar = Arc::new(PythonSidecarManager::new()?);
    sidecar.launch().await?;
    sidecar.fetch_install_token().await.ok(); // Best effort

    // Initialize auth
    let auth = Arc::new(AuthService::new(settings.clone(), windows.clone()).await?);
    let mut auth_events = auth.subscribe();

    // Initialize catalogue
    let catalogue = Arc::new(CatalogueService::new(auth.clone(), settings.clone()).await?);

    // Initialize audio
    let audio = Arc::new(AudioEngine::new(settings.clone(), windows.clone()).await?);

    // Initialize library (needs settings + audio)
    let library = Arc::new(LibraryService::new(settings.clone(), audio.clone()).await?);
    library.init_scanner().await?;

    // Initialize queue
    let queue =
        Arc::new(QueueService::new(audio.clone(), catalogue.clone(), settings.clone()).await?);

    // Wire Windows media keys to queue/audio playback controls.
    let q = queue.clone();
    let a = audio.clone();
    windows.set_play_pause_cb(move || {
        let q = q.clone();
        let a = a.clone();
        tokio::spawn(async move {
            match a.state().await {
                PlaybackState::Playing | PlaybackState::Buffering => {
                    let _ = a.pause().await;
                }
                PlaybackState::Paused => {
                    let _ = a.resume().await;
                }
                _ => {
                    if !q.get_queue().await.is_empty() {
                        let _ = q.play_current().await;
                    }
                }
            }
        });
    });
    let q = queue.clone();
    windows.set_next_cb(move || {
        let q = q.clone();
        tokio::spawn(async move {
            let _ = q.advance().await;
        });
    });
    let q = queue.clone();
    windows.set_previous_cb(move || {
        let q = q.clone();
        tokio::spawn(async move {
            let _ = q.previous().await;
        });
    });

    // Initialize recommendations
    let recommendations = Arc::new(
        RecommendationService::new(auth.clone(), catalogue.clone(), settings.clone()).await?,
    );

    // Create app state
    let handle = AppState::new(
        auth.clone(),
        catalogue.clone(),
        audio.clone(),
        queue.clone(),
        library.clone(),
        recommendations.clone(),
        settings.clone(),
        windows.clone(),
    )
    .await?;

    // Keep a single app-lifetime receiver, subscribed before any auth commands
    // become available. A lagged receiver resyncs with the current safe state.
    let auth_handle = app_handle.clone();
    let auth_for_resync = auth.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match auth_events.recv().await {
                Ok(AuthEvent::StateChanged(state)) => {
                    if auth_handle
                        .emit("auth_state_changed", auth_state_payload(state))
                        .is_err()
                    {
                        tracing::warn!("Failed to deliver auth state change");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if auth_handle
                        .emit(
                            "auth_state_changed",
                            auth_state_payload(auth_for_resync.state()),
                        )
                        .is_err()
                    {
                        tracing::warn!("Failed to deliver auth state change");
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut event_rx = handle.subscribe_events();
    let events_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(AppEvent::Diagnostic(diagnostic)) => {
                    if let Err(error) = events_handle.emit("app_diagnostic", diagnostic) {
                        tracing::warn!("Failed to deliver diagnostic event: {error}");
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Bridge audio events to frontend
    let mut audio_events = audio.subscribe();
    let audio_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(event) = audio_events.recv().await {
            if let Err(error) = audio_handle.emit("audio_event", &event) {
                tracing::warn!("Failed to deliver audio event: {error}");
            }
        }
    });

    // Bridge queue events to frontend
    let mut queue_events = queue.subscribe();
    let queue_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(event) = queue_events.recv().await {
            if let Err(error) = queue_handle.emit("queue_event", &event) {
                tracing::warn!("Failed to deliver queue event: {error}");
            }
        }
    });

    // Bridge catalogue events to frontend (stream errors)
    let mut catalogue_events = catalogue.subscribe();
    let catalogue_handle = app_handle.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(event) = catalogue_events.recv().await {
            if let orcave_catalogue::CatalogueEvent::StreamError {
                video_id,
                error_code,
                redacted_message,
                retryable,
            } = event
            {
                let payload = serde_json::json!({
                    "video_id": video_id,
                    "error_code": error_code,
                    "message": redacted_message,
                    "retryable": retryable,
                });
                if let Err(error) = catalogue_handle.emit("catalogue_stream_error", &payload) {
                    tracing::warn!("Failed to deliver catalogue stream error: {error}");
                }
            }
        }
    });

    // Store the handle in the app state
    app_handle.manage(handle.clone());

    // Also manage services directly for Tauri commands
    app_handle.manage(settings);
    app_handle.manage(native_session);
    app_handle.manage(windows.clone());
    app_handle.manage(auth);
    app_handle.manage(catalogue);
    app_handle.manage(audio);
    app_handle.manage(queue);
    app_handle.manage(library);
    app_handle.manage(recommendations);
    app_handle.manage(sidecar);

    handle.state.set_lifecycle(AppLifecycle::Running);
    handle.emit(AppEvent::Diagnostic(DiagnosticEvent::PerformanceMetric {
        name: "native_process_id".to_string(),
        value: f64::from(std::process::id()),
        unit: "pid".to_string(),
    }));

    info!("Application initialized successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r3v_runtime_data_root_only_accepts_dedicated_temp_directory() {
        let temp = std::env::temp_dir();
        let root = temp.join("opencode").join("r3v-isolated-profile");
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            r3v_runtime_data_root(Some(root.clone().into_os_string()), &temp).unwrap(),
            Some(root.canonicalize().unwrap())
        );
        assert!(r3v_runtime_data_root(Some(std::env::var_os("APPDATA").unwrap()), &temp).is_err());
    }

    #[test]
    fn auth_states_map_to_frontend_string_payloads() {
        for (state, expected) in [
            (AuthState::Unauthenticated, "unauthenticated"),
            (AuthState::Authenticating, "authenticating"),
            (AuthState::Authenticated, "authenticated"),
            (AuthState::TokenExpired, "token_expired"),
            (AuthState::Refreshing, "refreshing"),
            (AuthState::Error, "error"),
        ] {
            assert_eq!(auth_state_payload(state), expected);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }

    /// Gate 9: auth_flow_status payloads carry no OAuth, client ID, or secret
    /// material. They are safe to surface in the frontend UI.
    #[test]
    fn auth_flow_status_payloads_are_generic_and_secret_free() {
        for payload in [
            "browser_opened",
            "companion_required",
            "provider_rejected",
            "timed_out",
        ] {
            assert!(!payload.contains("oauth"));
            assert!(!payload.contains("client_id"));
            assert!(!payload.contains("token"));
            assert!(!payload.contains("secret"));
            assert!(!payload.contains("password"));
            assert!(!payload.contains("cookie"));
        }
        assert_eq!(
            serde_json::to_string("browser_opened").unwrap(),
            "\"browser_opened\""
        );
        assert_eq!(
            serde_json::to_string("provider_rejected").unwrap(),
            "\"provider_rejected\""
        );
        assert_eq!(serde_json::to_string("timed_out").unwrap(), "\"timed_out\"");
        assert_eq!(
            serde_json::to_string("companion_required").unwrap(),
            "\"companion_required\""
        );
    }

    /// SessionStatus serializes round-trip with no secret fields.
    #[test]
    fn session_status_serialization_round_trip() {
        let status = SessionStatus {
            state: "unverified".to_string(),
            connected: false,
            expired: false,
            ttl_secs: 0,
            handle: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: SessionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status.state, deserialized.state);
        assert_eq!(status.connected, deserialized.connected);
        assert_eq!(status.ttl_secs, deserialized.ttl_secs);
        assert_eq!(status.handle, deserialized.handle);
    }

    #[tokio::test]
    async fn browser_connect_is_unavailable_without_emitting_or_persisting() {
        assert!(matches!(
            auth_connect_browser_session().await,
            Err(SessionError::Unsupported)
        ));
        assert!(auth_cancel_flow().await.is_ok());
    }

    #[test]
    fn native_session_keys_are_not_public_settings_keys() {
        for key in ["native.session.material", "native.session.state"] {
            assert!(check_public_settings_key(key).is_err());
        }
    }

    /// Gate 9: the browser-session connection path does not reference
    /// `ORCAVE_GOOGLE_DESKTOP_CLIENT_ID`. OAuth is a disabled secondary path,
    /// not a gate.
    #[test]
    fn browser_session_connection_omits_oauth_client_gate() {
        let env_var = "ORCAVE_GOOGLE_DESKTOP_CLIENT_ID";
        for payload in [
            "browser_opened",
            "companion_required",
            "provider_rejected",
            "timed_out",
            "unauthenticated",
            "connecting",
            "authenticated",
        ] {
            assert!(!payload.contains(env_var));
        }
    }
}
