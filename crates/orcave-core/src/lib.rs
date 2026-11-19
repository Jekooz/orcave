//! Orcave Core - Core types and event definitions
//!
//! This crate defines the core types that are shared across all subsystem crates.
//! It has NO dependencies on other orcave crates to avoid circular dependencies.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

/// Unique identifier for the application instance
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppInstanceId(Uuid);

impl AppInstanceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AppInstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AppInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Application lifecycle state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppLifecycle {
    Starting,
    Running,
    Suspending,
    Resuming,
    ShuttingDown,
    Terminated,
}

/// Authentication state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    Unauthenticated,
    Authenticating,
    Authenticated,
    TokenExpired,
    Refreshing,
    Error,
}

/// Shared seed kinds for radio queues and recommendations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedType {
    Track,
    Album,
    Artist,
    Playlist,
    Genre,
}

/// User credentials container
#[derive(Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub expires_at: u64, // Unix timestamp
    pub token_type: String,
    pub scope: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("id_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("token_type", &"[REDACTED]")
            .field("scope", &"[REDACTED]")
            .finish()
    }
}

impl Credentials {
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Refresh 5 minutes before expiry
        now + 300 >= self.expires_at
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }
}

/// Token set for internal use
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// User profile information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub email: String,
    pub name: String,
    pub picture: Option<String>,
    pub verified_email: bool,
}

/// Result type for Tauri commands
pub type TauriResult<T> = std::result::Result<T, TauriError>;

/// Error type for Tauri command surface
#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "code", content = "message")]
pub enum TauriError {
    #[error("Authentication: {0}")]
    Auth(String),
    #[error("Catalogue: {0}")]
    Catalogue(String),
    #[error("Audio: {0}")]
    Audio(String),
    #[error("Queue: {0}")]
    Queue(String),
    #[error("Recommendations: {0}")]
    Recommendations(String),
    #[error("Settings: {0}")]
    Settings(String),
    #[error("Windows integration: {0}")]
    Windows(String),
    #[error("Internal: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for TauriError {
    fn from(e: anyhow::Error) -> Self {
        TauriError::Internal(e.to_string())
    }
}

/// Diagnostic events for the test UI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
#[non_exhaustive]
pub enum DiagnosticEvent {
    AuthFlowStarted {
        provider: String,
    },
    AuthFlowCompleted {
        success: bool,
        duration_ms: u64,
    },
    CatalogueRequest {
        endpoint: String,
        latency_ms: u64,
        cached: bool,
    },
    AudioPipelineEvent {
        stage: String,
        detail: String,
    },
    QueueOperation {
        operation: String,
        item_count: usize,
    },
    RecommendationRequest {
        seed_type: String,
        count: usize,
        latency_ms: u64,
    },
    SettingsChanged {
        key: String,
        old: String,
        new: String,
    },
    WindowsApiCall {
        api: String,
        success: bool,
        error: Option<String>,
    },
    PerformanceMetric {
        name: String,
        value: f64,
        unit: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_app_instance_id_unique() {
        let id1 = AppInstanceId::new();
        let id2 = AppInstanceId::new();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn test_app_lifecycle_transitions() {
        let mut lifecycle = AppLifecycle::Starting;
        assert_eq!(lifecycle, AppLifecycle::Starting);

        lifecycle = AppLifecycle::Running;
        assert_eq!(lifecycle, AppLifecycle::Running);

        lifecycle = AppLifecycle::Suspending;
        assert_eq!(lifecycle, AppLifecycle::Suspending);
    }

    #[test]
    fn test_credentials_expiry() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut creds = Credentials {
            access_token: "test".to_string(),
            refresh_token: "test".to_string(),
            id_token: None,
            expires_at: now + 100,
            token_type: "Bearer".to_string(),
            scope: "test".to_string(),
        };

        assert!(creds.is_expired());

        creds.expires_at = now + 1000;
        assert!(!creds.is_expired());
    }

    #[test]
    fn credential_debug_redacts_every_string_field_but_serde_keeps_wire_values() {
        let signed_url = "https://example.invalid/art?signature=synthetic-signed-url";
        let creds = Credentials {
            access_token: "synthetic-access-canary".into(),
            refresh_token: "synthetic-refresh-canary".into(),
            id_token: Some("synthetic-id-canary".into()),
            expires_at: 123456789,
            token_type: signed_url.into(),
            scope: signed_url.into(),
        };
        let debug = format!("{creds:?}");
        for secret in [
            creds.access_token.as_str(),
            creds.refresh_token.as_str(),
            creds.id_token.as_deref().unwrap(),
            signed_url,
        ] {
            assert!(!debug.contains(secret), "Debug exposed synthetic secret");
        }
        assert!(debug.contains("Credentials"));
        assert!(debug.contains("123456789"));
        let encoded = serde_json::to_string(&creds).unwrap();
        let restored: Credentials = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.access_token, creds.access_token);
        assert_eq!(restored.refresh_token, creds.refresh_token);
        assert_eq!(restored.id_token, creds.id_token);
        assert_eq!(restored.token_type, creds.token_type);
        assert_eq!(restored.scope, creds.scope);
        assert_eq!(restored.expires_at, creds.expires_at);
    }

    #[test]
    fn token_set_debug_redacts_tokens_but_serde_keeps_wire_values() {
        let tokens = TokenSet {
            access_token: "synthetic-access-canary".into(),
            refresh_token: "https://example.invalid/art?signature=synthetic-signed-url".into(),
            expires_at: 123456789,
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains(&tokens.access_token));
        assert!(!debug.contains(&tokens.refresh_token));
        assert!(debug.contains("TokenSet"));
        assert!(debug.contains("123456789"));
        let encoded = serde_json::to_string(&tokens).unwrap();
        let restored: TokenSet = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.access_token, tokens.access_token);
        assert_eq!(restored.refresh_token, tokens.refresh_token);
        assert_eq!(restored.expires_at, tokens.expires_at);
    }

    #[test]
    fn seed_type_serializes_for_cross_crate_events() {
        for (seed, expected) in [
            (SeedType::Track, "\"track\""),
            (SeedType::Album, "\"album\""),
            (SeedType::Artist, "\"artist\""),
            (SeedType::Playlist, "\"playlist\""),
            (SeedType::Genre, "\"genre\""),
        ] {
            assert_eq!(serde_json::to_string(&seed).unwrap(), expected);
            assert_eq!(serde_json::from_str::<SeedType>(expected).unwrap(), seed);
        }
    }
}
