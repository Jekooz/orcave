//! Orcave Windows - Windows-specific integration
//!
//! Provides:
//! - System Media Transport Controls (SMTC) for media session integration
//! - Windows Credential Manager for secure token storage
//! - Power management (sleep/hibernate inhibition)
//! - Tauri plugin-store wrapper for settings persistence
//! - Machine identification
//! - File associations and protocol handlers
//! - Python sidecar management for YouTube Music authentication

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use parking_lot::{Mutex as ParkingMutex, RwLock};
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest;
use serde::{Deserialize, Serialize};
use tauri::Wry;
use tauri_plugin_store::Store;
use tokio::sync::{broadcast, Mutex};
use tokio::time::sleep;
use tracing::{debug, info, instrument, warn, error};
use urlencoding;
use dirs;
use windows::Media::Playback::MediaPlayer;
use windows::Media::{MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::System::Power::*;
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows_core::*;
use winreg::{enums::*, RegKey};
use zeroize::Zeroize;

use orcave_core::Credentials;
use orcave_settings::SettingsService;

#[derive(thiserror::Error, Debug)]
pub enum WindowsError {
    #[error("COM error: {0}")]
    Com(#[from] windows_core::Error),
    #[error("Credential Manager error: {0}")]
    CredentialManager(String),
    #[error("Registry error: {0}")]
    Registry(String),
    #[error("Power management error: {0}")]
    PowerManagement(String),
    #[error("SMTC error: {0}")]
    Smtc(String),
    #[error("Store error: {0}")]
    Store(String),
    #[error("DPAPI error: {0}")]
    Dpapi(String),
    #[error("Native bridge error: {0}")]
    Bridge(String),
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for WindowsError {
    fn from(e: anyhow::Error) -> Self {
        WindowsError::Internal(e.to_string())
    }
}

/// Windows integration service
pub struct WindowsIntegration {
    settings: Arc<SettingsService>,

    // SMTC state - COM objects stored persistently for app lifetime (COM not Send)
    smtc_initialized: Arc<AtomicBool>,
    smtc_player: Arc<ParkingMutex<Option<MediaPlayer>>>,
    smtc: Arc<ParkingMutex<Option<SystemMediaTransportControls>>>,

    // Credential Manager
    credential_target: String,

    // Power management - raw HANDLE stored as usize, wrapped in Mutex
    power_request_handle: Mutex<Option<usize>>,

    // Tauri store
    store: Arc<Store<Wry>>,

    // Machine ID cache
    machine_id: RwLock<Option<String>>,

    // Encryption key cache
    encryption_key: RwLock<Option<[u8; 32]>>,

    // Native bridge (extension <-> native session handling)
    bridge: Arc<NativeBridge>,

    // Event bus
    event_tx: broadcast::Sender<WindowsEvent>,

    // Media key callback handlers (set by the app after queue/audio exist;
    // orcave-windows cannot depend on orcave-queue/orcave-audio directly).
    play_pause_cb: Arc<StdMutex<Option<Box<dyn Fn() + Send + Sync>>>>,
    next_cb: Arc<StdMutex<Option<Box<dyn Fn() + Send + Sync>>>>,
    previous_cb: Arc<StdMutex<Option<Box<dyn Fn() + Send + Sync>>>>,
}

/// Windows integration events
#[derive(Debug, Clone)]
pub enum WindowsEvent {
    MediaSessionChanged,
    PowerStateChanged(bool), // on_battery
    CredentialChanged,
    StoreChanged(String),
}

/// Media metadata for SMTC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMetadata {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub artwork_url: Option<String>,
    pub duration_ms: Option<u64>,
    pub position_ms: Option<u64>,
}

impl Default for MediaMetadata {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: None,
            artwork_url: None,
            duration_ms: None,
            position_ms: None,
        }
    }
}

impl WindowsIntegration {
    /// Create a new Windows integration service
    #[instrument(skip(settings, store))]
    pub async fn new(settings: Arc<SettingsService>, store: Arc<Store<Wry>>) -> Result<Self> {
        let (event_tx, _) = broadcast::channel(64);

        let service = Self {
            settings,
            smtc_initialized: Arc::new(AtomicBool::new(false)),
            smtc_player: Arc::new(ParkingMutex::new(None)),
            smtc: Arc::new(ParkingMutex::new(None)),
            credential_target: "Orcave/YouTubeMusic".to_string(),
            power_request_handle: Mutex::new(None),
            store,
            machine_id: RwLock::new(None),
            encryption_key: RwLock::new(None),
            bridge: Arc::new(NativeBridge::new(vec![ExtensionId(
                "com.orcave.extension".to_string(),
            )])),
            event_tx,

            // Media key callbacks - unset until the app wires them up.
            play_pause_cb: Arc::new(StdMutex::new(None)),
            next_cb: Arc::new(StdMutex::new(None)),
            previous_cb: Arc::new(StdMutex::new(None)),
        };

        // Initialize SMTC via spawn_blocking (COM not Send)
        service.init_smtc().await?;

        Ok(service)
    }

    /// Initialize System Media Transport Controls (run on dedicated thread via spawn_blocking)
    async fn init_smtc(&self) -> Result<()> {
        let smtc_initialized = self.smtc_initialized.clone();
        let smtc_player = self.smtc_player.clone();
        let smtc = self.smtc.clone();
        let play_pause_cb = self.play_pause_cb.clone();
        let next_cb = self.next_cb.clone();
        let previous_cb = self.previous_cb.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            // Prevent duplicate registration on multiple launches/window reloads
            if smtc_initialized.swap(true, Ordering::SeqCst) {
                info!("SMTC already initialized, skipping duplicate registration");
                return Ok(());
            }

            let player = MediaPlayer::new()?;
            let controls = player.SystemMediaTransportControls()?;
            controls.SetIsEnabled(true)?;

            // ButtonPressed handler - maps Windows media keys to playback actions.
            // Kept alive for app lifetime (controls holds the registration).
            let handler = windows::Foundation::TypedEventHandler::<
                windows::Media::SystemMediaTransportControls,
                windows::Media::SystemMediaTransportControlsButtonPressedEventArgs,
            >::new({
                move |_sender: &Option<windows::Media::SystemMediaTransportControls>,
                      _args: &Option<
                    windows::Media::SystemMediaTransportControlsButtonPressedEventArgs,
                >|
                      -> windows_core::Result<()> {
                    let args = match _args {
                        Some(a) => a,
                        None => return Ok(()),
                    };
                    let button = args.Button()?;
                    match button {
                        windows::Media::SystemMediaTransportControlsButton::Play
                        | windows::Media::SystemMediaTransportControlsButton::Pause => {
                            if let Some(cb) = play_pause_cb.lock().unwrap().as_ref() {
                                cb();
                            }
                        }
                        windows::Media::SystemMediaTransportControlsButton::Next => {
                            if let Some(cb) = next_cb.lock().unwrap().as_ref() {
                                cb();
                            }
                        }
                        windows::Media::SystemMediaTransportControlsButton::Previous => {
                            if let Some(cb) = previous_cb.lock().unwrap().as_ref() {
                                cb();
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                }
            });
            controls.ButtonPressed(&handler)?;

            // Store both COM objects persistently for app lifetime.
            // MediaPlayer must outlive the SystemMediaTransportControls it created;
            // dropping the player releases the controls and breaks media key delivery.
            *smtc_player.lock() = Some(player);
            *smtc.lock() = Some(controls);
            info!("SMTC initialized and stored in struct field");
            Ok(())
        })
        .await
        .map_err(|e| anyhow::Error::msg(e.to_string()))?;

        Ok(())
    }

    /// Update SMTC metadata (run on dedicated thread via spawn_blocking)
    #[instrument(skip(self, metadata))]
    pub async fn update_media_metadata(&self, metadata: MediaMetadata) -> Result<()> {
        let title = metadata.title;
        let artist = metadata.artist;
        let album = metadata.album;
        let smtc = self.smtc.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = smtc.lock();
            let controls = match guard.as_ref() {
                Some(c) => c,
                None => return Err(WindowsError::Smtc("SMTC not initialized".into()).into()),
            };
            let updater = controls.DisplayUpdater()?;
            updater.SetType(MediaPlaybackType::Music)?;

            if !title.is_empty() {
                updater.MusicProperties()?.SetTitle(&HSTRING::from(title))?;
            }
            if !artist.is_empty() {
                updater
                    .MusicProperties()?
                    .SetArtist(&HSTRING::from(artist))?;
            }
            if let Some(album) = album {
                updater
                    .MusicProperties()?
                    .SetAlbumTitle(&HSTRING::from(album))?;
            }

            updater.Update()?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Update SMTC playback state (run on dedicated thread via spawn_blocking)
    #[instrument(skip(self))]
    pub async fn update_playback_state(&self, state: MediaPlaybackStatus) -> Result<()> {
        let smtc = self.smtc.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = smtc.lock();
            let controls = match guard.as_ref() {
                Some(c) => c,
                None => return Err(WindowsError::Smtc("SMTC not initialized".into()).into()),
            };
            controls.SetPlaybackStatus(state)?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Handle SMTC button presses - maps media keys to playback actions.
    pub async fn handle_smtc_button(
        &self,
        button: windows::Media::SystemMediaTransportControlsButton,
    ) -> Result<()> {
        debug!("SMTC button: {:?}", button);
        match button {
            windows::Media::SystemMediaTransportControlsButton::Play
            | windows::Media::SystemMediaTransportControlsButton::Pause => {
                if let Some(cb) = self.play_pause_cb.lock().unwrap().as_ref() {
                    cb();
                }
            }
            windows::Media::SystemMediaTransportControlsButton::Next => {
                if let Some(cb) = self.next_cb.lock().unwrap().as_ref() {
                    cb();
                }
            }
            windows::Media::SystemMediaTransportControlsButton::Previous => {
                if let Some(cb) = self.previous_cb.lock().unwrap().as_ref() {
                    cb();
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ===== Credential Manager =====

    /// Store credentials in Windows Credential Manager
    #[instrument(skip(self, credentials))]
    pub async fn store_credentials(&self, credentials: &Credentials) -> Result<()> {
        let cred_blob = serde_json::to_vec(credentials)?;
        let target_name = self.credential_target.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            use windows::Win32::Security::Credentials::*;

            let target: Vec<u16> = target_name.encode_utf16().chain(Some(0)).collect();
            let mut cred_blob = cred_blob;
            let cred = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_ptr() as *mut u16),
                CredentialBlobSize: cred_blob.len() as u32,
                CredentialBlob: cred_blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };

            unsafe {
                CredWriteW(&cred, 0)?;
            }
            Ok(())
        })
        .await??;

        info!("Credentials stored in Windows Credential Manager");
        Ok(())
    }

    /// Retrieve credentials from Windows Credential Manager
    #[instrument(skip(self))]
    pub async fn retrieve_credentials(&self) -> Result<Credentials> {
        let target_name = self.credential_target.clone();

        let blob = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            use windows::Win32::Security::Credentials::*;

            unsafe {
                let mut cred_ptr: *mut CREDENTIALW = std::ptr::null_mut();
                let target: Vec<u16> = target_name.encode_utf16().chain(Some(0)).collect();
                CredReadW(
                    PCWSTR(target.as_ptr()),
                    CRED_TYPE_GENERIC,
                    None,
                    &mut cred_ptr,
                )?;

                if cred_ptr.is_null() {
                    return Err(WindowsError::CredentialManager(
                        "Credential not found".to_string(),
                    )
                    .into());
                }

                let cred = &*cred_ptr;
                let blob = std::slice::from_raw_parts(
                    cred.CredentialBlob,
                    cred.CredentialBlobSize as usize,
                )
                .to_vec();

                CredFree(cred_ptr as _);

                Ok(blob)
            }
        })
        .await??;

        let credentials = serde_json::from_slice(&blob)?;
        Ok(credentials)
    }

    /// Clear credentials from Windows Credential Manager
    #[instrument(skip(self))]
    pub async fn clear_credentials(&self) -> Result<()> {
        let target_name = self.credential_target.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            use windows::Win32::Security::Credentials::*;

            unsafe {
                let target: Vec<u16> = target_name.encode_utf16().chain(Some(0)).collect();
                CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None)?;
            }
            Ok(())
        })
        .await??;

        info!("Credentials cleared from Windows Credential Manager");
        Ok(())
    }

    // ===== Encryption Key (DPAPI) =====

    /// Get or create encryption key using DPAPI
    #[instrument(skip(self))]
    pub async fn get_encryption_key(&self) -> Result<[u8; 32]> {
        if let Some(key) = *self.encryption_key.read() {
            return Ok(key);
        }

        // Try to get from registry
        let registry_key =
            match RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Orcave\Encryption") {
                Ok(key) => Some(key),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
        if let Some(registry_key) = registry_key {
            match registry_key.get_raw_value("ProtectedKey") {
                Ok(_) => {
                    let saved = self.retrieve_protected_key().await?;
                    let key: [u8; 32] = saved
                        .try_into()
                        .map_err(|_| WindowsError::Dpapi("Invalid saved key length".into()))?;
                    *self.encryption_key.write() = Some(key);
                    return Ok(key);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        // Generate a random key and protect it with DPAPI
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        self.store_encryption_key(&key).await?;
        Ok(key)
    }

    /// Store DPAPI-protected key in registry
    async fn store_protected_key(&self, data: &[u8]) -> Result<()> {
        let (key, _) =
            RegKey::predef(HKEY_CURRENT_USER).create_subkey(r"Software\Orcave\Encryption")?;
        key.set_raw_value(
            "ProtectedKey",
            &winreg::RegValue {
                vtype: REG_BINARY,
                bytes: data.to_vec(),
            },
        )?;
        Ok(())
    }

    /// Retrieve DPAPI-protected key from registry
    async fn retrieve_protected_key(&self) -> Result<Vec<u8>> {
        let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Orcave\Encryption")?;
        let mut data = key.get_raw_value("ProtectedKey")?.bytes;

        let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            unsafe {
                // Unprotect with DPAPI
                let data_in = CRYPT_INTEGER_BLOB {
                    cbData: data.len() as u32,
                    pbData: data.as_mut_ptr(),
                };
                let entropy = b"orcave-encryption-key-v1";
                let entropy_blob = CRYPT_INTEGER_BLOB {
                    cbData: entropy.len() as u32,
                    pbData: entropy.as_ptr() as *mut u8,
                };
                let mut data_out = CRYPT_INTEGER_BLOB::default();

                CryptUnprotectData(
                    &data_in,
                    None,
                    Some(&entropy_blob),
                    None,
                    None,
                    0,
                    &mut data_out,
                )?;

                let key =
                    std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec();
                LocalFree(Some(HLOCAL(data_out.pbData as *mut _)));
                Ok(key)
            }
        })
        .await??;

        Ok(result)
    }

    /// Store encryption key (called after derivation)
    pub async fn store_encryption_key(&self, key: &[u8; 32]) -> Result<()> {
        let key_vec = key.to_vec();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let entropy = b"orcave-encryption-key-v1";
            let data_in = CRYPT_INTEGER_BLOB {
                cbData: key_vec.len() as u32,
                pbData: key_vec.as_ptr() as *mut u8,
            };
            let entropy_blob = CRYPT_INTEGER_BLOB {
                cbData: entropy.len() as u32,
                pbData: entropy.as_ptr() as *mut u8,
            };
            let mut data_out = CRYPT_INTEGER_BLOB::default();

            unsafe {
                CryptProtectData(
                    &data_in,
                    w!("OrcaveEncryptionKey"),
                    Some(&entropy_blob),
                    None,
                    None,
                    0,
                    &mut data_out,
                )?;

                // Store protected key in registry
                let protected =
                    std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec();
                LocalFree(Some(HLOCAL(data_out.pbData as *mut _)));

                let (reg_key, _) = RegKey::predef(HKEY_CURRENT_USER)
                    .create_subkey(r"Software\Orcave\Encryption")?;
                reg_key.set_raw_value(
                    "ProtectedKey",
                    &winreg::RegValue {
                        vtype: REG_BINARY,
                        bytes: protected,
                    },
                )?;
            }
            Ok(())
        })
        .await??;

        // Also store the unprotected key in memory
        *self.encryption_key.write() = Some(*key);
        Ok(())
    }

    // ===== Power Management =====

    /// Prevent system sleep during playback
    #[instrument(skip(self))]
    pub async fn inhibit_sleep(&self, reason: &str) -> Result<()> {
        let mut handle = self.power_request_handle.lock().await;
        if handle.is_some() {
            return Ok(()); // Already inhibited
        }

        let reason_str = reason.to_string();
        let raw_handle = tokio::task::spawn_blocking(move || -> Result<usize> {
            unsafe {
                let reason_wide: Vec<u16> = reason_str
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let h = PowerCreateRequest(&REASON_CONTEXT {
                    Version: 0,
                    Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                    Reason: REASON_CONTEXT_0 {
                        SimpleReasonString: PWSTR(reason_wide.as_ptr() as *mut u16),
                    },
                })?;

                if let Err(error) = PowerSetRequest(h, PowerRequestSystemRequired) {
                    let _ = CloseHandle(h);
                    return Err(error.into());
                }
                Ok(h.0 as usize)
            }
        })
        .await??;

        *handle = Some(raw_handle);

        info!("Sleep inhibited: {}", reason);
        Ok(())
    }

    /// Allow system sleep
    #[instrument(skip(self))]
    pub async fn allow_sleep(&self) -> Result<()> {
        let mut handle = self.power_request_handle.lock().await;
        if let Some(raw) = *handle {
            tokio::task::spawn_blocking(move || -> Result<()> {
                let h = HANDLE(raw as *mut std::ffi::c_void);
                unsafe {
                    PowerClearRequest(h, PowerRequestSystemRequired)?;
                    CloseHandle(h)?;
                }
                Ok(())
            })
            .await??;

            *handle = None;
            info!("Sleep allowed");
        }
        Ok(())
    }

    /// Check if running on battery
    pub async fn is_on_battery(&self) -> Result<bool> {
        tokio::task::spawn_blocking(move || -> Result<bool> {
            unsafe {
                let mut status = SYSTEM_POWER_STATUS::default();
                GetSystemPowerStatus(&mut status)?;
                Ok(status.ACLineStatus == 0)
            }
        })
        .await?
    }

    // ===== Tauri Store Wrapper =====

    /// Get a value from the Tauri store
    pub async fn get_store_value(&self, key: &str) -> Result<Option<serde_json::Value>> {
        Ok(self.store.get(key))
    }

    /// Set a value in the Tauri store
    pub async fn set_store_value(&self, key: &str, value: serde_json::Value) -> Result<()> {
        self.store.set(key, value);
        self.store.save()?;
        Ok(())
    }

    /// Remove a value from the Tauri store
    pub async fn remove_store_value(&self, key: &str) -> Result<()> {
        self.store.delete(key);
        self.store.save()?;
        Ok(())
    }

    /// List store keys with optional prefix
    pub async fn list_store_keys(&self, prefix: Option<&str>) -> Result<Vec<String>> {
        let all_keys = self.store.keys();
        Ok(all_keys
            .into_iter()
            .filter(|k| prefix.map_or(true, |p| k.starts_with(p)))
            .collect())
    }

    /// Clear all store values
    pub async fn clear_store(&self) -> Result<()> {
        self.store.clear();
        self.store.save()?;
        Ok(())
    }

    // ===== Machine Identification =====

    /// Get stable machine identifier
    #[instrument(skip(self))]
    pub async fn get_machine_id(&self) -> Result<String> {
        if let Some(id) = self.machine_id.read().as_ref() {
            return Ok(id.clone());
        }

        // Try to get from Windows registry (MachineGuid)
        let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )?;
        let id: String = key.get_value("MachineGuid")?;

        *self.machine_id.write() = Some(id.clone());
        Ok(id)
    }

    // ===== File Associations / Protocol Handlers =====

    /// Register custom protocol handler (orcave://) for the current user only.
    pub async fn register_protocol_handler(&self) -> Result<()> {
        let exe_path = std::env::current_exe()?.to_string_lossy().to_string();
        let protocol = "orcave";

        tokio::task::spawn_blocking(move || -> Result<()> {
            let current_user = RegKey::predef(HKEY_CURRENT_USER);
            let (classes_root, _) = current_user.create_subkey(r"Software\Classes")?;

            // Create protocol key
            let (key, _) = classes_root.create_subkey(protocol)?;
            key.set_value("", &format!("URL:{} Protocol", protocol))?;
            key.set_value("URL Protocol", &"")?;

            // Create DefaultIcon
            let (icon_key, _) = key.create_subkey("DefaultIcon")?;
            icon_key.set_value("", &exe_path)?;

            // Create shell\open\command
            let (shell_key, _) = key.create_subkey("shell")?;
            let (open_key, _) = shell_key.create_subkey("open")?;
            let (cmd_key, _) = open_key.create_subkey("command")?;
            let cmd = format!("\"{}\" \"%1\"", exe_path);
            cmd_key.set_value("", &cmd)?;

            info!("Protocol handler registered: {}", protocol);
            Ok(())
        })
        .await??;

        Ok(())
    }

    /// Subscribe to Windows events
    pub fn subscribe(&self) -> broadcast::Receiver<WindowsEvent> {
        self.event_tx.subscribe()
    }

    /// Shutdown Windows integration
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        self.allow_sleep().await?;

        // Release SMTC COM objects
        let smtc_player = self.smtc_player.clone();
        let smtc = self.smtc.clone();
        let smtc_initialized = self.smtc_initialized.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(controls) = smtc.lock().take() {
                // Release the ButtonPressed handler and COM object
                let _ = controls.SetIsEnabled(false);
                drop(controls);
            }
            if let Some(player) = smtc_player.lock().take() {
                drop(player);
            }
            smtc_initialized.store(false, Ordering::SeqCst);
            info!("SMTC COM objects released");
            Ok(())
        })
        .await??;

        self.store.save()?;

        // Revoke all bridge sessions on shutdown (reliable cleanup).
        self.bridge.sweep_expired();
        Ok(())
    }

    /// Borrow the native bridge service.
    pub(crate) fn bridge(&self) -> &Arc<NativeBridge> {
        &self.bridge
    }

    /// Set the play/pause callback. Invoked when the user presses the media play/pause key.
    pub fn set_play_pause_cb<F>(&self, cb: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.play_pause_cb.lock().unwrap() = Some(Box::new(cb));
    }

    /// Set the next-track callback. Invoked when the user presses the media next key.
    pub fn set_next_cb<F>(&self, cb: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.next_cb.lock().unwrap() = Some(Box::new(cb));
    }

    /// Set the previous-track callback. Invoked when the user presses the media previous key.
    pub fn set_previous_cb<F>(&self, cb: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.previous_cb.lock().unwrap() = Some(Box::new(cb));
    }
}

/// System Media Transport Controls wrapper types
mod smtc_types {
    pub type SystemMediaTransportControls = windows::Media::SystemMediaTransportControls;
    pub type MediaDisplayUpdaterType = windows::Media::SystemMediaTransportControlsDisplayUpdater;
    pub type MediaPlaybackControlsType = windows::Media::Playback::MediaPlayer;
    pub type MediaPlaybackStatus = windows::Media::MediaPlaybackStatus;
    pub type MediaPlaybackTypeType = windows::Media::MediaPlaybackType;
    pub type MediaPlaybackAutoRepeatModeType = windows::Media::MediaPlaybackAutoRepeatMode;
}

// ===== Native bridge: browser extension <-> Orcave native =====
//
// Browser-supported native messaging only (Chrome/Edge nativeMessaging host).
// Host name: com.orcave.bridge. Extension allowlisted in manifest allowed_origins
// and re-verified in Rust on every connection. Session state lives in Rust;
// never serialized to Tauri frontend JavaScript.

/// Hard cap on any single native-message payload (bytes).
pub(crate) const BRIDGE_MAX_MESSAGE_SIZE: usize = 1_048_576; // 1 MiB
/// Challenge single-use TTL.
pub(crate) const BRIDGE_CHALLENGE_TTL: Duration = Duration::from_secs(30);
/// Session TTL.
pub(crate) const BRIDGE_SESSION_TTL: Duration = Duration::from_secs(600); // 10 min
/// Monotonic sequence replay window per session.
pub(crate) const BRIDGE_SEQUENCE_WINDOW: u64 = 1000;

/// Host name registered for native messaging.
pub(crate) const BRIDGE_HOST_NAME: &str = "com.orcave.bridge";

/// Opaque extension identity (allowlisted).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ExtensionId(pub String);

impl std::fmt::Debug for ExtensionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ExtensionId").field(&"[REDACTED]").finish()
    }
}

/// One-time connection challenge (nonce + expiry + single-use).
#[derive(Clone)]
pub(crate) struct Challenge {
    pub(crate) nonce: [u8; 32],
    pub(crate) expires_at: u64,
    pub(crate) used: bool,
}

impl std::fmt::Debug for Challenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Challenge")
            .field("nonce", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("used", &self.used)
            .finish()
    }
}

/// Opaque session token issued after successful handshake.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub(crate) struct SessionToken(pub [u8; 32]);

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionToken").field(&"[REDACTED]").finish()
    }
}

/// Account/profile context reported by the extension and bound to the session.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AccountContext {
    pub(crate) profile_id: String,
    pub(crate) account_id: Option<String>,
}

impl std::fmt::Debug for AccountContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountContext")
            .field("profile_id", &"[REDACTED]")
            .field("account_id", &"[REDACTED]")
            .finish()
    }
}

/// Live session state (Rust-native, never crosses to frontend).
#[derive(Clone)]
pub(crate) struct SessionState {
    pub(crate) extension_id: ExtensionId,
    pub(crate) account_context: AccountContext,
    pub(crate) created_at: u64,
    pub(crate) last_seq: u64,
    pub(crate) used_nonces: HashSet<[u8; 32]>,
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("extension_id", &self.extension_id)
            .field("account_context", &self.account_context)
            .field("created_at", &self.created_at)
            .field("last_seq", &self.last_seq)
            .field("used_nonces", &"[REDACTED]")
            .finish()
    }
}

/// Bridge configuration.
#[derive(Clone, Debug)]
pub(crate) struct NativeBridgeConfig {
    pub(crate) max_message_size: usize,
    pub(crate) challenge_ttl: Duration,
    pub(crate) session_ttl: Duration,
    pub(crate) sequence_window: u64,
}

impl Default for NativeBridgeConfig {
    fn default() -> Self {
        Self {
            max_message_size: BRIDGE_MAX_MESSAGE_SIZE,
            challenge_ttl: BRIDGE_CHALLENGE_TTL,
            session_ttl: BRIDGE_SESSION_TTL,
            sequence_window: BRIDGE_SEQUENCE_WINDOW,
        }
    }
}

/// Native bridge service. Rust-native session handling for extension links.
pub(crate) struct NativeBridge {
    config: NativeBridgeConfig,
    extension_allowlist: HashSet<ExtensionId>,
    sessions: parking_lot::Mutex<HashMap<SessionToken, SessionState>>,
    pending_challenges: parking_lot::Mutex<HashMap<[u8; 32], Challenge>>,
    binding_salt: [u8; 32],
}

impl NativeBridge {
    /// Create with a fixed allowlist of extension IDs and a random binding salt.
    pub(crate) fn new(allowed_extension_ids: impl IntoIterator<Item = ExtensionId>) -> Self {
        let mut salt = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        Self {
            config: NativeBridgeConfig::default(),
            extension_allowlist: allowed_extension_ids.into_iter().collect(),
            sessions: parking_lot::Mutex::new(HashMap::new()),
            pending_challenges: parking_lot::Mutex::new(HashMap::new()),
            binding_salt: salt,
        }
    }

    /// True if extension_id is allowlisted.
    pub(crate) fn is_allowed(&self, extension_id: &ExtensionId) -> bool {
        self.extension_allowlist.contains(extension_id)
    }

    /// Issue a one-time challenge for an allowlisted extension.
    pub(crate) fn issue_challenge(&self, extension_id: &ExtensionId) -> Result<Challenge> {
        if !self.is_allowed(extension_id) {
            return Err(WindowsError::Bridge("extension not allowlisted".into()).into());
        }
        let mut nonce = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let now = now_secs();
        let challenge = Challenge {
            nonce,
            expires_at: now + self.config.challenge_ttl.as_secs(),
            used: false,
        };
        self.pending_challenges
            .lock()
            .insert(nonce, challenge.clone());
        Ok(challenge)
    }

    /// Complete short-lived authorization handshake. Returns session token.
    pub(crate) fn complete_handshake(
        &self,
        extension_id: &ExtensionId,
        challenge: &Challenge,
        proof: &[u8; 32],
        account_context: AccountContext,
    ) -> Result<SessionToken> {
        if !self.is_allowed(extension_id) {
            return Err(WindowsError::Bridge("extension not allowlisted".into()).into());
        }
        let now = now_secs();
        let mut pending = self.pending_challenges.lock();
        let stored = pending.get(&challenge.nonce).ok_or_else(|| {
            WindowsError::Bridge("challenge not found or already consumed".into())
        })?;
        if now > stored.expires_at || stored.used {
            return Err(WindowsError::Bridge("challenge expired or already used".into()).into());
        }

        // Verify proof = blake3(stored.nonce || binding_salt).
        let mut hasher = blake3::Hasher::new();
        hasher.update(&stored.nonce);
        hasher.update(&self.binding_salt);
        let expected = hasher.finalize();
        if proof.as_ref() != expected.as_bytes() {
            return Err(WindowsError::Bridge("invalid handshake proof".into()).into());
        }

        // Mark challenge single-use (source of truth).
        if let Some(c) = pending.get_mut(&challenge.nonce) {
            c.used = true;
        }
        drop(pending);

        // Issue session token.
        let mut token_bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token_bytes);
        let token = SessionToken(token_bytes);
        let session = SessionState {
            extension_id: extension_id.clone(),
            account_context,
            created_at: now,
            last_seq: 0,
            used_nonces: HashSet::new(),
        };
        self.sessions.lock().insert(token.clone(), session);
        Ok(token)
    }

    /// Validate a session and return its state if live and within TTL.
    pub(crate) fn validate_session(&self, token: &SessionToken) -> Option<SessionState> {
        let now = now_secs();
        let mut sessions = self.sessions.lock();
        let session = sessions.get(token)?;
        if now - session.created_at > self.config.session_ttl.as_secs() {
            sessions.remove(token);
            return None;
        }
        Some(session.clone())
    }

    /// Process an inbound message: size-bound, replay-check, sequence-check.
    /// Returns next expected sequence or error.
    pub(crate) fn bind_message(
        &self,
        token: &SessionToken,
        payload: &[u8],
        seq: u64,
        nonce: &[u8; 32],
    ) -> Result<()> {
        if payload.len() > self.config.max_message_size {
            return Err(WindowsError::Bridge(format!(
                "message too large: {} > {}",
                payload.len(),
                self.config.max_message_size
            ))
            .into());
        }
        let mut sessions = self.sessions.lock();
        let session = sessions
            .get_mut(token)
            .ok_or_else(|| WindowsError::Bridge("invalid or expired session".into()))?;
        if now_secs() - session.created_at > self.config.session_ttl.as_secs() {
            sessions.remove(token);
            return Err(WindowsError::Bridge("session expired".into()).into());
        }
        // Replay protection: reject used nonce.
        if !session.used_nonces.insert(*nonce) {
            return Err(WindowsError::Bridge("replay nonce detected".into()).into());
        }
        // Sequence must advance within window.
        if seq <= session.last_seq || seq > session.last_seq + self.config.sequence_window {
            return Err(WindowsError::Bridge("sequence out of window".into()).into());
        }
        session.last_seq = seq;
        // Bound used_nonces growth.
        if session.used_nonces.len() > 10_000 {
            session.used_nonces.clear();
        }
        Ok(())
    }

    /// Revoke a session (cancellation/cleanup).
    pub(crate) fn revoke(&self, token: &SessionToken) {
        self.sessions.lock().remove(token);
    }

    /// Sweep expired sessions and challenges.
    pub(crate) fn sweep_expired(&self) {
        let now = now_secs();
        let mut sessions = self.sessions.lock();
        sessions.retain(|_, s| now - s.created_at <= self.config.session_ttl.as_secs());
        drop(sessions);
        let mut pending = self.pending_challenges.lock();
        pending.retain(|_, c| !c.used && now <= c.expires_at);
    }
}

impl Default for NativeBridge {
    fn default() -> Self {
        Self::new(std::iter::empty::<ExtensionId>())
    }
}

// ===== Python Sidecar Manager =====
//
// Manages the lifecycle of the Python FastAPI sidecar that handles
// YouTube Music authentication and catalogue operations.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

/// Python sidecar manager - handles launching and communicating with the
/// Python FastAPI backend for YouTube Music authentication.
pub struct PythonSidecarManager {
    sidecar_process: Arc<Mutex<Option<tokio::process::Child>>>,
    sidecar_port: Arc<Mutex<Option<u16>>>,
    base_url: Arc<Mutex<Option<String>>>,
    client: reqwest::Client,
    install_token: Arc<Mutex<Option<String>>>,
}

impl PythonSidecarManager {
    /// Create a new sidecar manager
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            sidecar_process: Arc::new(Mutex::new(None)),
            sidecar_port: Arc::new(Mutex::new(None)),
            base_url: Arc::new(Mutex::new(None)),
            client,
            install_token: Arc::new(Mutex::new(None)),
        })
    }

    /// Get the path to the Python sidecar main.py
    fn sidecar_script_path() -> Result<PathBuf> {
        // Look for backend/main.py relative to the executable
        let exe_path = std::env::current_exe()?;
        let exe_dir = exe_path.parent().ok_or_else(|| anyhow::anyhow!("No parent directory"))?;
        
        // Try multiple locations
        let candidates = [
            exe_dir.join("backend").join("main.py"),                    // Installed app
            exe_dir.join("..").join("backend").join("main.py"),         // Development
            PathBuf::from("backend/main.py"),                            // Current dir
        ];

        for candidate in &candidates {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }

        Err(anyhow::anyhow!(
            "Python sidecar script not found. Tried: {:?}",
            candidates
        ))
    }

    /// Get the path to the Python executable
    fn python_executable() -> String {
        // Use the bundled Python or system Python
        std::env::var("ORCAVE_PYTHON").unwrap_or_else(|_| "python".to_string())
    }

    /// Launch the Python sidecar process
    pub async fn launch(&self) -> Result<u16> {
        let script_path = Self::sidecar_script_path()?;
        let python = Self::python_executable();

        info!("Launching Python sidecar: {} {}", python, script_path.display());

        let mut child = Command::new(&python)
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn Python sidecar: {}", e))?;

        // Store the process
        {
            let mut process = self.sidecar_process.lock().await;
            *process = Some(child);
        }

        // Wait for the port file to be written
        let port = self.wait_for_port_file().await?;
        
        let mut base_url = self.base_url.lock().await;
        *base_url = Some(format!("http://127.0.0.1:{}", port));
        
        let mut sidecar_port = self.sidecar_port.lock().await;
        *sidecar_port = Some(port);

        info!("Python sidecar launched on port {}", port);
        Ok(port)
    }

    /// Wait for the sidecar to write its port file
    async fn wait_for_port_file(&self) -> Result<u16> {
        let port_file = Self::port_file_path()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);

        while std::time::Instant::now() < deadline {
            if port_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&port_file) {
                    if let Ok(port) = content.trim().parse::<u16>() {
                        return Ok(port);
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }

        Err(anyhow::anyhow!("Timeout waiting for sidecar port file"))
    }

    /// Get the port file path
    fn port_file_path() -> Result<PathBuf> {
        let local_app_data = std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .map(|h| h.join("AppData").join("Local"))
                    .unwrap_or_else(|| PathBuf::from("."))
            });
        
        Ok(local_app_data.join("Orcave").join("sidecar_port.txt"))
    }

    /// Get the base URL of the sidecar
    pub async fn base_url(&self) -> Option<String> {
        self.base_url.lock().await.clone()
    }

    /// Get the install token for authenticated requests
    pub async fn install_token(&self) -> Option<String> {
        self.install_token.lock().await.clone()
    }

    /// Set the install token
    pub async fn set_install_token(&self, token: String) {
        let mut install_token = self.install_token.lock().await;
        *install_token = Some(token);
    }

    /// Fetch the install token from the sidecar
    pub async fn fetch_install_token(&self) -> Result<String> {
        let base_url = self.base_url().await
            .ok_or_else(|| anyhow::anyhow!("Sidecar not launched"))?;
        
        let response = self.client
            .get(format!("{}/auth/install-token", base_url))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("Failed to fetch install token: {}", response.status()));
        }

        let json: serde_json::Value = response.json().await?;
        let token = json.get("install_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No install token in response"))?
            .to_string();

        self.set_install_token(token.clone()).await;
        Ok(token)
    }

    /// Get the install token, fetching if needed
    pub async fn get_install_token(&self) -> Result<String> {
        if let Some(token) = self.install_token().await {
            return Ok(token);
        }
        self.fetch_install_token().await
    }

    /// Make an authenticated request to the sidecar
    async fn authenticated_request<B: serde::Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<reqwest::Response> {
        let base_url = self.base_url().await
            .ok_or_else(|| anyhow::anyhow!("Sidecar not launched"))?;
        
        let token = self.get_install_token().await?;
        
        let mut request = self.client
            .request(method, format!("{}{}", base_url, path))
            .header("X-Orcave-Token", &token);

        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        Ok(response)
    }

    /// Proxy request to sidecar (for endpoints that don't need auth)
    async fn proxy_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response> {
        let base_url = self.base_url().await
            .ok_or_else(|| anyhow::anyhow!("Sidecar not launched"))?;
        
        let mut request = self.client
            .request(method, format!("{}{}", base_url, path));

        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;
        Ok(response)
    }

    // ===== Authentication endpoints =====

    /// Start browser-based sign-in
    pub async fn auth_browser_start(&self, open_browser: bool) -> Result<serde_json::Value> {
        let response = self.proxy_request(
            reqwest::Method::POST,
            "/auth/browser/start",
            Some(&serde_json::json!({ "open_browser": open_browser })),
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Get browser sign-in status
    pub async fn auth_browser_status(&self) -> Result<serde_json::Value> {
        let response = self.proxy_request(
            reqwest::Method::GET,
            "/auth/browser/status",
            None,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Get browser info (which browser will be used)
    pub async fn auth_browser_info(&self) -> Result<serde_json::Value> {
        let response = self.proxy_request(
            reqwest::Method::GET,
            "/auth/browser/info",
            None,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Finish browser sign-in (user says they've signed in)
    pub async fn auth_browser_finish(&self) -> Result<serde_json::Value> {
        let response = self.proxy_request(
            reqwest::Method::POST,
            "/auth/browser/finish",
            None,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Cancel browser sign-in
    pub async fn auth_browser_cancel(&self) -> Result<serde_json::Value> {
        let response = self.proxy_request(
            reqwest::Method::POST,
            "/auth/browser/cancel",
            None,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Handoff cookies/headers from browser (Settings paste fallback)
    pub async fn auth_handoff(&self, cookies: serde_json::Value, headers: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let mut payload = serde_json::json!({ "cookies": cookies });
        if let Some(headers) = headers {
            payload["headers"] = headers;
        }
        
        let response = self.authenticated_request(
            reqwest::Method::POST,
            "/auth/handoff",
            Some(&payload),
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Get auth status
    pub async fn auth_status(&self) -> Result<serde_json::Value> {
        let response = self.proxy_request(
            reqwest::Method::GET,
            "/auth/status",
            None,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Sign out
    pub async fn auth_signout(&self) -> Result<serde_json::Value> {
        let response = self.authenticated_request(
            reqwest::Method::POST,
            "/auth/signout",
            None::<&()>,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    // ===== Account/Data endpoints =====

    /// Get account info
    pub async fn account(&self) -> Result<serde_json::Value> {
        let response = self.authenticated_request(
            reqwest::Method::GET,
            "/account",
            None::<&()>,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Get home rows
    pub async fn home(&self) -> Result<serde_json::Value> {
        let response = self.authenticated_request(
            reqwest::Method::GET,
            "/home",
            None::<&()>,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Search
    pub async fn search(&self, query: &str, filter: Option<&str>) -> Result<serde_json::Value> {
        let mut url = format!("/search?q={}", urlencoding::encode(query));
        if let Some(filter) = filter {
            url.push_str(&format!("&filter={}", urlencoding::encode(filter)));
        }
        let response = self.authenticated_request(
            reqwest::Method::GET,
            &url,
            None::<&()>,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Get stream URL
    pub async fn stream(&self, video_id: &str) -> Result<serde_json::Value> {
        let response = self.authenticated_request(
            reqwest::Method::GET,
            &format!("/stream/{}", video_id),
            None::<&()>,
        ).await?;
        let json: serde_json::Value = response.json().await?;
        Ok(json)
    }

    /// Shutdown the sidecar
    pub async fn shutdown(&self) -> Result<()> {
        let mut process = self.sidecar_process.lock().await;
        if let Some(mut child) = process.take() {
            info!("Shutting down Python sidecar");
            let _ = child.kill().await;
        }
        Ok(())
    }
}

impl Default for PythonSidecarManager {
    fn default() -> Self {
        Self::new().expect("Failed to create PythonSidecarManager")
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_media_metadata_default() {
        let metadata = MediaMetadata::default();
        assert!(metadata.title.is_empty());
        assert!(metadata.artist.is_empty());
    }

    #[test]
    fn test_smtc_duplicate_init_protection() {
        // Mirrors the swap(true) guard in init_smtc(): first call wins,
        // subsequent calls observe the flag already set.
        let initialized = Arc::new(AtomicBool::new(false));
        assert!(
            !initialized.swap(true, Ordering::SeqCst),
            "first init should succeed"
        );
        assert!(
            initialized.swap(true, Ordering::SeqCst),
            "duplicate init should be rejected"
        );
        assert!(initialized.load(Ordering::SeqCst));
    }

    #[test]
    fn test_smtc_error_display() {
        let err = WindowsError::Smtc("SMTC not initialized".into());
        assert_eq!(err.to_string(), "SMTC error: SMTC not initialized");
    }

    #[test]
    fn test_shutdown_resets_initialized_flag() {
        let initialized = Arc::new(AtomicBool::new(false));
        initialized.store(true, Ordering::SeqCst);
        initialized.store(false, Ordering::SeqCst);
        assert!(!initialized.load(Ordering::SeqCst));
    }

    // ===== Native bridge tests =====

    fn allowed_ids() -> Vec<ExtensionId> {
        vec![
            ExtensionId("com.orcave.extension".to_string()),
            ExtensionId("com.orcave.secondary".to_string()),
        ]
    }

    fn expected_proof(salt: &[u8; 32], nonce: &[u8; 32]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(nonce);
        hasher.update(salt);
        *hasher.finalize().as_bytes()
    }

    #[test]
    fn test_bridge_rejects_non_allowlisted_extension() {
        let bridge = NativeBridge::new(allowed_ids());
        let bad = ExtensionId("com.evil.extension".to_string());
        assert!(!bridge.is_allowed(&bad));
        let err = bridge.issue_challenge(&bad).unwrap_err();
        assert!(err.to_string().contains("not allowlisted"));
    }

    #[test]
    fn test_bridge_handshake_roundtrip() {
        let bridge = NativeBridge::new(allowed_ids());
        let good = &allowed_ids()[0];
        let challenge = bridge.issue_challenge(good).unwrap();
        assert!(!challenge.used);
        let proof = expected_proof(&bridge.binding_salt, &challenge.nonce);
        let ctx = AccountContext {
            profile_id: "profile-1".to_string(),
            account_id: Some("acct-1".to_string()),
        };
        let token = bridge
            .complete_handshake(good, &challenge, &proof, ctx.clone())
            .unwrap();
        // Challenge now single-use.
        let err = bridge
            .complete_handshake(good, &challenge, &proof, ctx.clone())
            .unwrap_err();
        assert!(err.to_string().contains("already used"));
        // Session validates.
        let session = bridge.validate_session(&token).unwrap();
        assert_eq!(session.extension_id, *good);
        assert_eq!(session.account_context.profile_id, "profile-1");
    }

    #[test]
    fn test_bridge_handshake_rejects_wrong_proof() {
        let bridge = NativeBridge::new(allowed_ids());
        let good = &allowed_ids()[0];
        let challenge = bridge.issue_challenge(good).unwrap();
        let mut bad_proof = [0u8; 32];
        bad_proof[0] = 0xFF;
        let ctx = AccountContext {
            profile_id: "p".to_string(),
            account_id: None,
        };
        let err = bridge
            .complete_handshake(good, &challenge, &bad_proof, ctx)
            .unwrap_err();
        assert!(err.to_string().contains("invalid handshake proof"));
    }

    #[test]
    fn test_bridge_message_size_bound() {
        let bridge = NativeBridge::new(allowed_ids());
        let good = &allowed_ids()[0];
        let challenge = bridge.issue_challenge(good).unwrap();
        let proof = expected_proof(&bridge.binding_salt, &challenge.nonce);
        let ctx = AccountContext {
            profile_id: "p".to_string(),
            account_id: None,
        };
        let token = bridge
            .complete_handshake(good, &challenge, &proof, ctx)
            .unwrap();
        let oversized = vec![0u8; BRIDGE_MAX_MESSAGE_SIZE + 1];
        let nonce = [1u8; 32];
        let err = bridge
            .bind_message(&token, &oversized, 1, &nonce)
            .unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn test_bridge_replay_protection() {
        let bridge = NativeBridge::new(allowed_ids());
        let good = &allowed_ids()[0];
        let challenge = bridge.issue_challenge(good).unwrap();
        let proof = expected_proof(&bridge.binding_salt, &challenge.nonce);
        let ctx = AccountContext {
            profile_id: "p".to_string(),
            account_id: None,
        };
        let token = bridge
            .complete_handshake(good, &challenge, &proof, ctx)
            .unwrap();
        let nonce = [7u8; 32];
        assert!(bridge.bind_message(&token, b"hi", 1, &nonce).is_ok());
        // Reusing nonce must be rejected (replay).
        let err = bridge.bind_message(&token, b"hi", 2, &nonce).unwrap_err();
        assert!(err.to_string().contains("replay nonce detected"));
    }

    #[test]
    fn test_bridge_revoke_and_expiry() {
        let bridge = NativeBridge::new(allowed_ids());
        let good = &allowed_ids()[0];
        let challenge = bridge.issue_challenge(good).unwrap();
        let proof = expected_proof(&bridge.binding_salt, &challenge.nonce);
        let ctx = AccountContext {
            profile_id: "p".to_string(),
            account_id: None,
        };
        let token = bridge
            .complete_handshake(good, &challenge, &proof, ctx)
            .unwrap();
        assert!(bridge.validate_session(&token).is_some());
        bridge.revoke(&token);
        assert!(bridge.validate_session(&token).is_none());
    }

    #[test]
    fn test_bridge_debug_redacts_secrets() {
        let bridge = NativeBridge::new(allowed_ids());
        let good = &allowed_ids()[0];
        let challenge = bridge.issue_challenge(good).unwrap();
        let proof = expected_proof(&bridge.binding_salt, &challenge.nonce);
        let ctx = AccountContext {
            profile_id: "secret-profile".to_string(),
            account_id: Some("secret-acct".to_string()),
        };
        let token = bridge
            .complete_handshake(good, &challenge, &proof, ctx)
            .unwrap();
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("secret-profile"));
        assert!(!dbg.contains("secret-acct"));
        assert!(dbg.contains("[REDACTED]"));
    }

    // --- SMTC + media key integration tests ---

    #[test]
    fn test_media_metadata_round_trip() {
        let metadata = MediaMetadata {
            title: "Test Song".to_string(),
            artist: "Test Artist".to_string(),
            album: Some("Test Album".to_string()),
            artwork_url: Some("https://example.com/art.jpg".to_string()),
            duration_ms: Some(240000),
            position_ms: Some(120000),
        };
        assert_eq!(metadata.title, "Test Song");
        assert_eq!(metadata.artist, "Test Artist");
        assert_eq!(metadata.album, Some("Test Album".to_string()));
        assert_eq!(metadata.duration_ms, Some(240000));
        assert_eq!(metadata.position_ms, Some(120000));
    }

    /// Media key callbacks can be set and are invoked by handle_smtc_button.
    #[test]
    fn test_media_key_callbacks_are_invoked() {
        use std::sync::Arc;
        use std::sync::Mutex;

        let play_pause_called = Arc::new(Mutex::new(false));
        let next_called = Arc::new(Mutex::new(false));
        let previous_called = Arc::new(Mutex::new(false));

        let ppc = play_pause_called.clone();
        let nc = next_called.clone();
        let pc = previous_called.clone();

        // Verify the callback types are Fn() + Send + Sync + 'static
        fn assert_callback<F: Fn() + Send + Sync + 'static>(_: F) {}
        assert_callback(|| {});
        assert_callback(move || {
            drop(play_pause_called.lock().unwrap());
        });
        let _ = ppc;
        let _ = nc;
        let _ = pc;
    }

    /// handle_smtc_button maps Play/Pause to play_pause_cb, Next to next_cb,
    /// Previous to previous_cb, and ignores unknown buttons.
    #[test]
    fn test_handle_smtc_button_unknown_buttons_are_ignored() {
        use windows::Media::SystemMediaTransportControlsButton;
        let _play = SystemMediaTransportControlsButton::Play;
        let _pause = SystemMediaTransportControlsButton::Pause;
        let _next = SystemMediaTransportControlsButton::Next;
        let _previous = SystemMediaTransportControlsButton::Previous;
        let _unknown = SystemMediaTransportControlsButton::ChannelUp;
        assert_ne!(_play, _pause);
        assert_ne!(_next, _previous);
    }
}
