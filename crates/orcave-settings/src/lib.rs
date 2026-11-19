//! Orcave Settings - Persistent configuration management with encryption
//!
//! Provides typed settings access with:
//! - Tauri plugin-store backend for persistence
//! - DPAPI-protected key + AES-256-GCM encryption for sensitive values
//! - Change notifications via broadcast channel
//! - Schema validation and migration

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use dashmap::DashMap;
use parking_lot::RwLock;
use rand::RngCore;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tauri::Wry;
use tauri_plugin_store::Store;
use tokio::sync::{broadcast, Mutex};
use tracing::{info, instrument};
use zeroize::{Zeroize, Zeroizing};

// Session expiry grace: treat a session as expired this many seconds before
// the recorded expiry so callers reconnect proactively.
const SESSION_EXPIRY_GRACE_SECS: u64 = 300;

// Reserved encrypted setting keys used by the native session service.
const SESSION_MATERIAL_KEY: &str = "native.session.material";
const SESSION_STATE_KEY: &str = "native.session.state";

#[derive(thiserror::Error, Debug)]
pub enum SettingsError {
    #[error("Serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Encryption failed: {0}")]
    Encryption(String),
    #[error("Decryption failed: {0}")]
    Decryption(String),
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("Key derivation failed: {0}")]
    KeyDerivation(String),
    #[error("Setting not found: {0}")]
    NotFound(String),
    #[error("Validation failed: {0}")]
    Validation(String),
    #[error("Migration failed: {0}")]
    Migration(String),
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for SettingsError {
    fn from(e: anyhow::Error) -> Self {
        SettingsError::Internal(e.to_string())
    }
}

// ===== Native session handling (secure, DPAPI-backed) =====

/// Redacted session error. Never leaks handles, tokens, or account data.
#[derive(thiserror::Error, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "code", content = "message")]
pub enum SessionError {
    #[error("Session not connected")]
    NotConnected,
    #[error("Session expired")]
    Expired,
    #[error("Session validation failed: {0}")]
    Validation(String),
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("Encryption failed: {0}")]
    Encryption(String),
    #[error("Decryption failed: {0}")]
    Decryption(String),
    #[error("Browser session info rejected")]
    Rejected,
    #[error("Browser session connection unavailable pending authorized native validation")]
    Unsupported,
    #[error("Already connected")]
    AlreadyConnected,
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        SessionError::Internal(e.to_string())
    }
}

impl From<SettingsError> for SessionError {
    fn from(e: SettingsError) -> Self {
        SessionError::Internal(e.to_string())
    }
}

/// Native session lifecycle state. Carries no secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Disconnected,
    Connecting,
    Connected,
    Unverified,
    Expired,
    Error,
}

/// Minimal credential material derived from an existing browser session.
/// Stored only in DPAPI-encrypted form at rest. Never written plaintext.
#[derive(Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct SessionMaterial {
    /// Opaque handle bound to the browser session. Not a cookie or raw auth.
    pub session_handle: String,
    /// Windows account the session is bound to (account isolation).
    pub bound_account: String,
    /// Unix timestamp when the session expires.
    pub expires_at: u64,
    /// Unix timestamp when the session was connected.
    pub connected_at: u64,
}

impl std::fmt::Debug for SessionMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionMaterial")
            .field("session_handle", &"[REDACTED]")
            .field("bound_account", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("connected_at", &self.connected_at)
            .finish()
    }
}

impl SessionMaterial {
    /// Detect expiry with proactive grace window.
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now + SESSION_EXPIRY_GRACE_SECS >= self.expires_at
    }

    /// Seconds remaining until expiry (0 if expired).
    pub fn ttl_secs(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.expires_at.saturating_sub(now)
    }
}

/// Legacy browser request info. Not an IPC contract or proof of provider access.
#[derive(Zeroize)]
#[zeroize(drop)]
pub struct BrowserSessionInfo {
    /// Opaque browser-derived request payload supplied by the user.
    pub request_payload: String,
    /// Account identifier the user asserts the session is bound to.
    pub account_hint: Option<String>,
}

impl std::fmt::Debug for BrowserSessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserSessionInfo")
            .field("request_payload", &"[REDACTED]")
            .field("account_hint", &"[REDACTED]")
            .finish()
    }
}

impl BrowserSessionInfo {
    /// Immediate validation: shape + length bounds only. No secrets logged.
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.request_payload.is_empty() || self.request_payload.len() > 8192 {
            return Err(SessionError::Rejected);
        }
        if let Some(hint) = &self.account_hint {
            if hint.len() > 256 {
                return Err(SessionError::Rejected);
            }
        }
        Ok(())
    }
}

/// Native session service. Encrypts session material at rest via DPAPI
/// (Windows account-scoped). No cookies, no raw authorization, no plaintext
/// session files, no secrets in React/Zustand.
pub struct NativeSessionService {
    settings: Arc<SettingsService>,
    state: RwLock<SessionState>,
    material: RwLock<Option<Zeroizing<SessionMaterial>>>,
}

impl NativeSessionService {
    /// Create a new session service and restore any persisted session.
    pub async fn new(settings: Arc<SettingsService>) -> Result<Self> {
        let service = Self {
            settings,
            state: RwLock::new(SessionState::Disconnected),
            material: RwLock::new(None),
        };
        service.restore().await?;
        Ok(service)
    }

    /// Restore legacy material without treating its stored state as provider validation.
    async fn restore(&self) -> Result<()> {
        let stored: Option<SessionMaterial> = self
            .settings
            .get_encrypted(SESSION_MATERIAL_KEY)
            .await
            .ok()
            .flatten();
        match stored {
            Some(material) if !material.is_expired() => {
                *self.material.write() = Some(Zeroizing::new(material));
                *self.state.write() = SessionState::Unverified;
            }
            Some(_) => {
                // Preserve legacy persisted material until an explicit local disconnect.
                *self.state.write() = SessionState::Expired;
            }
            None => {
                *self.state.write() = SessionState::Disconnected;
            }
        }
        Ok(())
    }

    /// Clear persisted session material and state.
    async fn clear_persisted(&self) -> Result<()> {
        self.settings.remove(SESSION_MATERIAL_KEY).await?;
        self.settings.remove(SESSION_STATE_KEY).await?;
        Ok(())
    }

    /// Current session state (no secrets).
    pub fn state(&self) -> SessionState {
        *self.state.read()
    }

    /// No production provider validation is available; legacy material is never connected.
    pub fn is_connected(&self) -> bool {
        false
    }

    /// Whether the session is expired (explicit expiry detection).
    /// A disconnected service with no material is not "expired"; only
    /// expired material or the explicit `Expired` state counts.
    pub fn is_expired(&self) -> bool {
        matches!(*self.state.read(), SessionState::Expired)
            || self
                .material
                .read()
                .as_ref()
                .is_some_and(|m| m.is_expired())
    }

    /// Seconds until expiry (0 if none/expired).
    pub fn ttl_secs(&self) -> u64 {
        self.material.read().as_ref().map_or(0, |m| m.ttl_secs())
    }

    /// Redacted handle for diagnostics (never a raw secret).
    pub fn handle_prefix(&self) -> Option<String> {
        let m = self.material.read();
        let m = m.as_ref()?;
        let h = &m.session_handle;
        if h.len() <= 8 {
            return Some("[REDACTED]".to_string());
        }
        Some(format!("{}…{}", &h[..4], &h[h.len() - 4..]))
    }

    /// Reject legacy browser payloads until an authorized native validator exists.
    pub async fn connect(
        &self,
        _info: &BrowserSessionInfo,
        _account_id: String,
    ) -> Result<(), SessionError> {
        Err(SessionError::Unsupported)
    }

    /// Reject reconnect without deleting legacy local material.
    pub async fn reconnect(
        &self,
        info: &BrowserSessionInfo,
        account_id: String,
    ) -> Result<(), SessionError> {
        self.connect(info, account_id).await
    }

    /// Explicit disconnect: clears local session only. Does NOT log the user
    /// out of the website/browser session.
    pub async fn disconnect(&self) -> Result<(), SessionError> {
        self.clear_persisted().await?;
        *self.material.write() = None;
        *self.state.write() = SessionState::Disconnected;
        Ok(())
    }

    /// Shutdown: persist state then clear in-memory material.
    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.settings.save().await;
        *self.material.write() = None;
        Ok(())
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn session_error_debug_shows_variant_not_secret() {
        let err = SessionError::Internal("generic failure".into());
        let s = format!("{err:?}");
        assert!(s.contains("Internal"));
        assert!(!s.contains("access_token"));
        assert!(!s.contains("refresh_token"));
    }

    #[test]
    fn material_debug_redacts_handle_and_account() {
        let m = SessionMaterial {
            session_handle: "super-secret-handle-value".to_string(),
            bound_account: "user@example.com".to_string(),
            expires_at: 9999999999,
            connected_at: 1000,
        };
        let s = format!("{m:?}");
        assert!(!s.contains("super-secret-handle-value"));
        assert!(!s.contains("user@example.com"));
        assert_eq!(s.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn browser_info_debug_redacts_request_and_hint() {
        let info = BrowserSessionInfo {
            request_payload: "synthetic-sensitive-payload".into(),
            account_hint: Some("synthetic-account".into()),
        };
        let output = format!("{info:?}");
        assert!(!output.contains("synthetic-sensitive-payload"));
        assert!(!output.contains("synthetic-account"));
        assert_eq!(output.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn legacy_connected_state_is_not_accepted_as_verified() {
        assert_ne!(SessionState::Unverified, SessionState::Connected);
        assert_eq!(
            serde_json::to_string(&SessionState::Unverified).unwrap(),
            "\"unverified\""
        );
    }

    #[test]
    fn expiry_grace_boundary_treats_near_expiry_as_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut m = SessionMaterial {
            session_handle: "h".into(),
            bound_account: "a".into(),
            expires_at: now + SESSION_EXPIRY_GRACE_SECS,
            connected_at: now,
        };
        assert!(
            m.is_expired(),
            "at the grace boundary the session is expired"
        );
        assert_eq!(
            m.ttl_secs(),
            SESSION_EXPIRY_GRACE_SECS,
            "ttl is raw time remaining, not grace-adjusted"
        );

        m.expires_at = now + SESSION_EXPIRY_GRACE_SECS + 1;
        assert!(!m.is_expired(), "one second past the grace window is valid");
        assert!(m.ttl_secs() >= SESSION_EXPIRY_GRACE_SECS + 1);

        m.expires_at = now.saturating_sub(1);
        assert!(m.is_expired());
        assert_eq!(m.ttl_secs(), 0);
    }

    #[test]
    fn session_error_codes_are_generic_and_secret_free() {
        let err = SessionError::Validation("account id is required".into());
        let s = err.to_string();
        assert!(s.contains("account id is required"));
        assert!(!s.contains("access_token"));
        assert!(!s.contains("refresh_token"));
        assert!(!s.contains("cookie"));
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Validation"));
    }

    #[test]
    fn expiry_detection_and_ttl() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut m = SessionMaterial {
            session_handle: "h".into(),
            bound_account: "a".into(),
            expires_at: now + 1000,
            connected_at: now,
        };
        assert!(!m.is_expired());
        assert!(m.ttl_secs() >= 999);
        m.expires_at = now - 100;
        assert!(m.is_expired());
        assert_eq!(m.ttl_secs(), 0);
    }

    #[test]
    fn browser_info_rejects_empty_and_oversized() {
        let info = BrowserSessionInfo {
            request_payload: String::new(),
            account_hint: None,
        };
        assert!(info.validate().is_err());
        let info = BrowserSessionInfo {
            request_payload: "x".repeat(9000),
            account_hint: None,
        };
        assert!(info.validate().is_err());
        let info = BrowserSessionInfo {
            request_payload: "valid".into(),
            account_hint: Some("a".repeat(300).into()),
        };
        assert!(info.validate().is_err());
        let info = BrowserSessionInfo {
            request_payload: "valid".into(),
            account_hint: None,
        };
        assert!(info.validate().is_ok());
    }
}

/// Audio quality settings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioQuality {
    Low = 0,      // 48kbps AAC
    Normal = 1,   // 128kbps AAC
    High = 2,     // 256kbps AAC
    Lossless = 3, // FLAC (when available)
}

impl Default for AudioQuality {
    fn default() -> Self {
        AudioQuality::High
    }
}

/// UI theme settings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    System,
    Light,
    Dark,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::System
    }
}

/// Crossfade settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossfadeSettings {
    pub enabled: bool,
    pub duration_ms: u32,
}

impl Default for CrossfadeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: 5000,
        }
    }
}

/// Gapless playback settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaplessSettings {
    pub enabled: bool,
    pub preload_next: bool,
}

impl Default for GaplessSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            preload_next: true,
        }
    }
}

/// Complete application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    // Version for migration
    pub version: u32,

    // Authentication
    pub auth_auto_sign_in: bool,
    pub auth_remember_credentials: bool,

    // Audio
    pub audio_quality: AudioQuality,
    pub audio_normalize_volume: bool,
    pub audio_crossfade: CrossfadeSettings,
    pub audio_gapless: GaplessSettings,
    pub audio_output_device: Option<String>,

    // Playback
    pub playback_repeat_mode: RepeatMode,
    pub playback_shuffle: bool,

    // UI
    pub theme: Theme,
    pub ui_compact_mode: bool,
    pub ui_animations: bool,
    pub ui_language: String,

    // Network
    pub network_proxy: Option<String>,
    pub network_timeout_secs: u32,

    // Cache
    pub cache_max_size_mb: u32,
    pub cache_artwork: bool,
    pub cache_audio_preload: bool,

    // Diagnostics
    pub diagnostics_enabled: bool,
    pub diagnostics_log_level: String,
    pub diagnostics_crash_reporting: bool,

    // Advanced
    pub advanced_experimental_features: bool,
    pub advanced_developer_mode: bool,

    // Last modified
    #[serde(skip)]
    pub last_modified: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl Default for RepeatMode {
    fn default() -> Self {
        RepeatMode::Off
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            auth_auto_sign_in: true,
            auth_remember_credentials: true,
            audio_quality: AudioQuality::default(),
            audio_normalize_volume: true,
            audio_crossfade: CrossfadeSettings::default(),
            audio_gapless: GaplessSettings::default(),
            audio_output_device: None,
            playback_repeat_mode: RepeatMode::default(),
            playback_shuffle: false,
            theme: Theme::default(),
            ui_compact_mode: false,
            ui_animations: true,
            ui_language: "en-US".to_string(),
            network_proxy: None,
            network_timeout_secs: 30,
            cache_max_size_mb: 500,
            cache_artwork: true,
            cache_audio_preload: true,
            diagnostics_enabled: true,
            diagnostics_log_level: "info".to_string(),
            diagnostics_crash_reporting: true,
            advanced_experimental_features: false,
            advanced_developer_mode: false,
            last_modified: None,
        }
    }
}

const CURRENT_VERSION: u32 = 1;
const SETTINGS_KEY: &str = "orcave.settings";
const ENCRYPTED_PREFIX: &str = "orcave.encrypted.";
const PROTECTED_KEY: &str = "orcave.protected_key";
const LEGACY_KEY: &str = "encryption_key";

fn is_reserved_key(key: &str) -> bool {
    key == LEGACY_KEY || key == PROTECTED_KEY || key.starts_with(ENCRYPTED_PREFIX)
}

fn validate_setting_key(key: &str) -> Result<()> {
    if key.is_empty() || is_reserved_key(key) {
        return Err(SettingsError::Validation("Reserved or empty settings key".into()).into());
    }
    Ok(())
}

/// DPAPI-protected master key for encrypted settings and session material.
///
/// `CryptProtectData` without entropy is scoped to the calling Windows user
/// account: only the same account (same logon session context) can unprotect
/// it. This is the per-account key scope for session material; the master key
/// never leaves this process unprotected and no browser cookies or raw
/// authorization data are ever involved.
///
/// `crates/orcave-windows` is read-only to this crate and is not depended on
/// here (the dependency direction is `orcave-windows` → `orcave-settings`;
/// a reverse dependency would be a cycle). Its `get_encryption_key` /
/// `store_credentials` / `retrieve_credentials` / `clear_credentials` DPAPI
/// and Credential Manager APIs exist in the Windows layer; this crate applies
/// the same account-scoped DPAPI primitive directly.
#[cfg(windows)]
fn protect_key(key: &[u8; 32]) -> Result<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let mut input = CRYPT_INTEGER_BLOB {
        cbData: 32,
        pbData: key.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &mut input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|e| SettingsError::KeyDerivation(e.to_string()))?;
        let protected = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(protected)
    }
}

#[cfg(windows)]
fn unprotect_key(protected: &[u8]) -> Result<[u8; 32]> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    if protected.is_empty() || protected.len() > u32::MAX as usize {
        return Err(SettingsError::KeyDerivation("Invalid protected key".into()).into());
    }
    let input = CRYPT_INTEGER_BLOB {
        cbData: protected.len() as u32,
        pbData: protected.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|e| SettingsError::KeyDerivation(e.to_string()))?;
        let result = if output.cbData == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(std::slice::from_raw_parts(output.pbData, 32));
            Ok(key)
        } else {
            Err(SettingsError::KeyDerivation("Invalid unprotected key length".into()).into())
        };
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        result
    }
}

#[cfg(not(windows))]
fn protect_key(_: &[u8; 32]) -> Result<Vec<u8>> {
    Err(SettingsError::KeyDerivation("DPAPI requires Windows".into()).into())
}

#[cfg(not(windows))]
fn unprotect_key(_: &[u8]) -> Result<[u8; 32]> {
    Err(SettingsError::KeyDerivation("DPAPI requires Windows".into()).into())
}

/// Encrypted payload structure
#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
struct EncryptedPayload {
    version: u32,
    salt: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Settings service
pub struct SettingsService {
    store: Arc<Store<Wry>>,

    // In-memory cache
    cache: DashMap<String, serde_json::Value>,
    encrypted_cache: DashMap<String, Zeroizing<Vec<u8>>>,

    // DPAPI-protected at rest; zeroized when service drops.
    master_key: RwLock<Option<Zeroizing<[u8; 32]>>>,

    // Change notifications
    change_tx: broadcast::Sender<(String, serde_json::Value)>,

    // Initialization guard
    initialized: Mutex<bool>,
}

impl SettingsService {
    /// Create a new settings service
    #[instrument(skip(store))]
    pub async fn new(store: Arc<Store<Wry>>) -> Result<Self> {
        let (change_tx, _) = broadcast::channel(128);

        let service = Self {
            store,
            cache: DashMap::new(),
            encrypted_cache: DashMap::new(),
            master_key: RwLock::new(None),
            change_tx,
            initialized: Mutex::new(false),
        };

        // Initialize encryption key
        service.init_encryption_key().await?;

        // Load existing settings
        service.load().await?;

        Ok(service)
    }

    /// Load the protected key, migrate a legacy raw key, or create a new random key.
    async fn init_encryption_key(&self) -> Result<()> {
        if let Some(value) = self.store.get(PROTECTED_KEY) {
            let protected: Vec<u8> = serde_json::from_value(value)?;
            let key = unprotect_key(&protected)?;
            *self.master_key.write() = Some(Zeroizing::new(key));
            if self.store.get(LEGACY_KEY).is_some() {
                self.store.delete(LEGACY_KEY);
                self.store
                    .save()
                    .map_err(|e| SettingsError::Storage(e.to_string()))?;
            }
            return Ok(());
        }

        let key = if let Some(value) = self.store.get(LEGACY_KEY) {
            let mut raw = Zeroizing::new(serde_json::from_value::<Vec<u8>>(value)?);
            if raw.len() != 32 {
                return Err(
                    SettingsError::KeyDerivation("Invalid legacy key length".into()).into(),
                );
            }
            let mut key = Zeroizing::new([0u8; 32]);
            key.copy_from_slice(&raw);
            raw.zeroize();
            key
        } else {
            if self
                .store
                .keys()
                .iter()
                .any(|key| key.starts_with(ENCRYPTED_PREFIX))
            {
                return Err(SettingsError::KeyDerivation(
                    "Encrypted values exist without a key".into(),
                )
                .into());
            }
            let mut key = Zeroizing::new([0u8; 32]);
            rand::rngs::OsRng.fill_bytes(&mut *key);
            key
        };
        let protected = protect_key(&key)?;
        self.store
            .set(PROTECTED_KEY, serde_json::to_value(protected)?);
        self.store.delete(LEGACY_KEY);
        self.store
            .save()
            .map_err(|e| SettingsError::Storage(e.to_string()))?;
        *self.master_key.write() = Some(key);
        Ok(())
    }

    /// Load settings from storage
    #[instrument(skip(self))]
    async fn load(&self) -> Result<()> {
        // Load main settings (synchronous API returns Option)
        if let Some(value) = self.store.get(SETTINGS_KEY) {
            if let Ok(settings) = serde_json::from_value::<Settings>(value.clone()) {
                self.apply_settings(settings).await?;
            }
        }

        // Load encrypted values (synchronous API returns Vec)
        let keys = self.store.keys();
        let encrypted_keys: Vec<String> = keys
            .into_iter()
            .filter(|k| k.starts_with(ENCRYPTED_PREFIX))
            .collect();

        for key in encrypted_keys {
            if let Some(value) = self.store.get(&key) {
                let payload = serde_json::from_value::<EncryptedPayload>(value)?;
                let mut decrypted = Zeroizing::new(self.decrypt_value(&payload).await?);
                // Older saves encrypted an already-encrypted payload. Recover it once,
                // then the next save writes the canonical single-layer format.
                if let Ok(inner) = serde_json::from_slice::<EncryptedPayload>(&decrypted) {
                    let recovered = self.decrypt_value(&inner).await?;
                    decrypted.zeroize();
                    decrypted = Zeroizing::new(recovered);
                }
                let plain_key = key.strip_prefix(ENCRYPTED_PREFIX).unwrap_or(&key);
                self.encrypted_cache
                    .insert(plain_key.to_string(), decrypted);
            }
        }

        *self.initialized.lock().await = true;
        Ok(())
    }

    /// Apply settings to cache
    async fn apply_settings(&self, mut settings: Settings) -> Result<()> {
        settings.last_modified = Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());

        // Migrate if needed
        if settings.version < CURRENT_VERSION {
            settings = self.migrate_settings(settings).await?;
        }

        let value = serde_json::to_value(&settings)?;
        self.cache.insert(SETTINGS_KEY.to_string(), value);
        Ok(())
    }

    /// Migrate settings from older version
    async fn migrate_settings(&self, mut settings: Settings) -> Result<Settings> {
        info!(
            "Migrating settings from version {} to {}",
            settings.version, CURRENT_VERSION
        );

        // Version 1 is current, no migration needed yet
        settings.version = CURRENT_VERSION;
        Ok(settings)
    }

    /// Save all settings to storage
    #[instrument(skip(self))]
    pub async fn save(&self) -> Result<()> {
        let initialized = *self.initialized.lock().await;
        if !initialized {
            return Ok(());
        }

        // Save main settings (synchronous API returns ())
        if let Some(value) = self.cache.get(SETTINGS_KEY) {
            self.store.set(SETTINGS_KEY, value.value().clone());
        }

        // Save all other cache keys (library folders, etc.) that survive restarts.
        for entry in self.cache.iter() {
            let key = entry.key();
            if key == SETTINGS_KEY {
                continue;
            }
            let value = entry.value().clone();
            self.store.set(key, value);
        }

        // Save encrypted values
        for entry in self.encrypted_cache.iter() {
            let key = format!("{}{}", ENCRYPTED_PREFIX, entry.key());
            let payload = self.encrypt_value(entry.value()).await?;
            let value: serde_json::Value = serde_json::from_slice(&payload)?;
            self.store.set(&key, value);
        }

        self.store
            .save()
            .map_err(|e| SettingsError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Get a typed setting value
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        if self.encrypted_cache.contains_key(key) || is_reserved_key(key) {
            return Ok(None);
        }
        if key == SETTINGS_KEY {
            if let Some(value) = self.cache.get(key) {
                return Ok(Some(serde_json::from_value(value.value().clone())?));
            }
            return Ok(None);
        }

        // Check regular cache first.
        if let Some(value) = self.cache.get(key) {
            return Ok(Some(serde_json::from_value(value.value().clone())?));
        }

        // Fall back to the persistent store so keys written by other services
        // (e.g. library folders) survive restarts.
        if let Some(value) = self.store.get(key) {
            return Ok(Some(serde_json::from_value(value)?));
        }

        Ok(None)
    }

    /// Get a setting with default
    pub async fn get_or<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        Ok(self.get(key).await?.unwrap_or_default())
    }

    /// Get the complete settings object
    pub async fn get_all(&self) -> Result<Settings> {
        self.get_or(SETTINGS_KEY).await
    }

    /// Set a setting value
    #[instrument(skip(self, value))]
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        validate_setting_key(key)?;
        if self.encrypted_cache.contains_key(key)
            || self
                .store
                .get(&format!("{}{}", ENCRYPTED_PREFIX, key))
                .is_some()
        {
            return Err(SettingsError::Validation(
                "Cannot replace encrypted value with plain value".into(),
            )
            .into());
        }
        let json_value = serde_json::to_value(value)?;
        self.cache.insert(key.to_string(), json_value.clone());
        self.notify_change(key, json_value).await;
        Ok(())
    }

    /// Set an encrypted setting value
    #[instrument(skip(self, value))]
    pub async fn set_encrypted<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        validate_setting_key(key)?;
        if key == SETTINGS_KEY {
            return Err(
                SettingsError::Validation("Cannot encrypt main settings key".into()).into(),
            );
        }
        let json_bytes = serde_json::to_vec(value)?;
        self.cache.remove(key);
        self.encrypted_cache
            .insert(key.to_string(), Zeroizing::new(json_bytes));
        self.notify_change(key, serde_json::json!({"encrypted": true}))
            .await;
        Ok(())
    }

    /// Get an encrypted setting value
    pub async fn get_encrypted<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        if let Some(bytes) = self.encrypted_cache.get(key) {
            let json_str = std::str::from_utf8(bytes.as_ref())?;
            return Ok(Some(serde_json::from_str(json_str)?));
        }
        Ok(None)
    }

    /// Remove a setting
    pub async fn remove(&self, key: &str) -> Result<()> {
        validate_setting_key(key)?;
        self.cache.remove(key);
        self.encrypted_cache.remove(key);
        self.store.delete(key);
        self.store.delete(&format!("{}{}", ENCRYPTED_PREFIX, key));
        self.store
            .save()
            .map_err(|e| SettingsError::Storage(e.to_string()))?;
        self.notify_change(key, serde_json::Value::Null).await;
        Ok(())
    }

    /// Update a specific setting field
    #[instrument(skip(self))]
    pub async fn update_field(&self, field: &str, value: serde_json::Value) -> Result<()> {
        let mut settings = self.get_all().await?;

        // Use JSON pointer or manual field update
        match field {
            "audio_quality" => settings.audio_quality = serde_json::from_value(value)?,
            "theme" => settings.theme = serde_json::from_value(value)?,
            "playback_shuffle" => settings.playback_shuffle = serde_json::from_value(value)?,
            "playback_repeat_mode" => {
                settings.playback_repeat_mode = serde_json::from_value(value)?
            }
            "audio_crossfade" => settings.audio_crossfade = serde_json::from_value(value)?,
            "audio_gapless" => settings.audio_gapless = serde_json::from_value(value)?,
            "diagnostics_enabled" => settings.diagnostics_enabled = serde_json::from_value(value)?,
            _ => return Err(SettingsError::NotFound(field.to_string()).into()),
        }

        self.set(SETTINGS_KEY, &settings).await?;
        Ok(())
    }

    /// Subscribe to setting changes
    pub fn subscribe(&self) -> broadcast::Receiver<(String, serde_json::Value)> {
        self.change_tx.subscribe()
    }

    async fn notify_change(&self, key: &str, value: serde_json::Value) {
        let _ = self.change_tx.send((key.to_string(), value));
    }

    /// Encrypt a value using AES-256-GCM
    async fn encrypt_value(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut salt = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut salt);

        // Derive encryption key from master key + salt
        let derived_key = self.derive_key(&salt)?;

        // Generate nonce
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);

        // Encrypt with AES-256-GCM
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

        let cipher = Aes256Gcm::new_from_slice(&*derived_key)
            .map_err(|e| SettingsError::Encryption(e.to_string()))?;

        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|e| SettingsError::Encryption(e.to_string()))?;

        let payload = EncryptedPayload {
            version: 1,
            salt: salt.to_vec(),
            nonce: nonce.to_vec(),
            ciphertext,
        };

        Ok(serde_json::to_vec(&payload)?)
    }

    /// Decrypt a value
    async fn decrypt_value(&self, payload: &EncryptedPayload) -> Result<Vec<u8>> {
        if payload.version != 1
            || payload.salt.len() != 16
            || payload.nonce.len() != 12
            || payload.ciphertext.len() < 16
        {
            return Err(SettingsError::Decryption("Invalid encrypted payload".into()).into());
        }
        // Derive encryption key from master key + salt
        let derived_key = self.derive_key(&payload.salt)?;

        // Decrypt with AES-256-GCM
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

        let cipher = Aes256Gcm::new_from_slice(&*derived_key)
            .map_err(|e| SettingsError::Decryption(e.to_string()))?;

        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&payload.nonce),
                payload.ciphertext.as_ref(),
            )
            .map_err(|e| SettingsError::Decryption(e.to_string()))?;

        Ok(plaintext)
    }

    fn derive_key(&self, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        let guard = self.master_key.read();
        let master = guard
            .as_ref()
            .ok_or_else(|| SettingsError::KeyDerivation("Master key not initialized".into()))?;
        let mut derived = Zeroizing::new([0u8; 32]);
        // Keep the v1 derivation compatible with existing encrypted values.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&**master);
        hasher.update(salt);
        hasher.finalize_xof().fill(&mut *derived);
        Ok(derived)
    }

    /// Reset all settings to defaults
    #[instrument(skip(self))]
    pub async fn reset_to_defaults(&self) -> Result<()> {
        self.cache.clear();
        self.encrypted_cache.clear();
        self.store.clear();
        let protected = {
            let guard = self.master_key.read();
            let key = guard
                .as_ref()
                .ok_or_else(|| SettingsError::KeyDerivation("Master key not initialized".into()))?;
            protect_key(key)?
        };
        self.store
            .set(PROTECTED_KEY, serde_json::to_value(protected)?);
        self.store
            .save()
            .map_err(|e| SettingsError::Storage(e.to_string()))?;
        let defaults = Settings::default();
        self.apply_settings(defaults).await?;
        self.save().await?;
        Ok(())
    }

    /// Export settings (for backup)
    pub async fn export(&self) -> Result<serde_json::Value> {
        let mut export = serde_json::Map::new();

        for entry in self.cache.iter() {
            if !is_reserved_key(entry.key()) && !self.encrypted_cache.contains_key(entry.key()) {
                export.insert(entry.key().clone(), entry.value().clone());
            }
        }

        for entry in self.encrypted_cache.iter() {
            export.insert(
                format!("{}{}", ENCRYPTED_PREFIX, entry.key()),
                serde_json::json!({"encrypted": true}),
            );
        }

        Ok(serde_json::Value::Object(export))
    }

    /// Import settings (from backup)
    #[instrument(skip(self, data))]
    pub async fn import(&self, data: serde_json::Value) -> Result<()> {
        if let Some(obj) = data.as_object() {
            for (key, value) in obj {
                if key.starts_with(ENCRYPTED_PREFIX) {
                    // Encrypted values need special handling
                    // For import, we'd need the actual encrypted payload
                    continue;
                }
                validate_setting_key(key)?;
                if self.encrypted_cache.contains_key(key)
                    || self
                        .store
                        .get(&format!("{}{}", ENCRYPTED_PREFIX, key))
                        .is_some()
                {
                    return Err(SettingsError::Validation(
                        "Cannot import plain data over encrypted value".into(),
                    )
                    .into());
                }
                self.cache.insert(key.clone(), value.clone());
            }
        }
        self.save().await?;
        Ok(())
    }

    /// Shutdown the settings service
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        self.save().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_settings_keys_are_rejected() {
        for key in ["", LEGACY_KEY, PROTECTED_KEY, "orcave.encrypted.secret"] {
            assert!(validate_setting_key(key).is_err());
            assert!(is_reserved_key(key) || key.is_empty());
        }
        assert!(validate_setting_key("audio_quality").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn protected_key_round_trip_and_rejects_corruption() {
        let key = [42u8; 32];
        let mut protected = protect_key(&key).unwrap();
        assert_ne!(protected.as_slice(), key);
        assert_eq!(unprotect_key(&protected).unwrap(), key);
        protected[0] ^= 0xff;
        assert!(unprotect_key(&protected).is_err());
        assert!(unprotect_key(&[]).is_err());
    }

    #[tokio::test]
    async fn test_settings_default() {
        let settings = Settings::default();
        assert_eq!(settings.version, CURRENT_VERSION);
        assert_eq!(settings.audio_quality, AudioQuality::High);
        assert_eq!(settings.theme, Theme::System);
    }

    #[test]
    fn test_audio_quality_ordering() {
        assert!((AudioQuality::Low as u8) < AudioQuality::Normal as u8);
        assert!((AudioQuality::Normal as u8) < AudioQuality::High as u8);
        assert!((AudioQuality::High as u8) < AudioQuality::Lossless as u8);
    }
}
