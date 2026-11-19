//! Orcave Auth - YouTube Music authentication and session management
//!
//! Handles OAuth 2.0 flows, token refresh, credential storage, and
//! Windows Credential Manager integration for secure token persistence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use parking_lot::RwLock;
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest::{header::AUTHORIZATION, Client};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, instrument, warn};
use url::Url;

pub use orcave_core::{AuthState, Credentials, TokenSet, UserInfo};
use orcave_core::{TauriError, TauriResult};
use orcave_settings::SettingsService;
use orcave_windows::{WindowsError, WindowsIntegration};

/// YouTube Music OAuth configuration
const YTM_OAUTH_BASE: &str = "https://accounts.google.com";
/// Set in the native process environment from an owner-controlled Desktop app
/// OAuth client. A client ID is public; never distribute a client secret.
const CLIENT_ID_ENV: &str = "ORCAVE_GOOGLE_DESKTOP_CLIENT_ID";
const REDIRECT_URI: &str = "http://127.0.0.1:8080/callback";
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/youtube.readonly",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/userinfo.email",
];

#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("OAuth flow failed: {0}")]
    OAuthFlow(String),
    #[error("Token exchange failed: {0}")]
    TokenExchange(String),
    #[error("Token refresh failed: {0}")]
    TokenRefresh(String),
    #[error("Invalid token: {0}")]
    InvalidToken(String),
    #[error("Credential storage failed: {0}")]
    Storage(String),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("JWT error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),
    #[error("Windows integration error: {0}")]
    Windows(#[from] WindowsError),
    #[error("Settings error: {0}")]
    Settings(#[from] orcave_settings::SettingsError),
    #[error("Internal: {0}")]
    Internal(String),
    #[error("OAuth callback timeout")]
    CallbackTimeout,
    #[error("OAuth flow cancelled")]
    CallbackCancelled,
    #[error("OAuth state mismatch")]
    StateMismatch,
    #[error("OAuth authorization code already consumed")]
    CodeAlreadyConsumed,
    #[error("OAuth callback parse error: {0}")]
    CallbackParse(String),
    #[error("OAuth flow already in progress")]
    FlowInProgress,
    #[error("OAuth port unavailable: {0}")]
    PortUnavailable(String),
}

impl From<anyhow::Error> for AuthError {
    fn from(e: anyhow::Error) -> Self {
        AuthError::Internal(e.to_string())
    }
}

/// Authentication service - manages OAuth flow and token lifecycle
pub struct AuthService {
    client: Client,
    settings: Arc<SettingsService>,
    windows: Arc<WindowsIntegration>,

    // Current credentials (encrypted in memory)
    credentials: Arc<RwLock<Option<Credentials>>>,
    auth_state: Arc<RwLock<AuthState>>,
    user_info: Arc<RwLock<Option<UserInfo>>>,

    // OAuth PKCE state
    pkce_verifier: Arc<RwLock<Option<String>>>,
    pkce_challenge: Arc<RwLock<Option<String>>>,

    // Event bus
    event_tx: broadcast::Sender<AuthEvent>,

    // Background refresh task handle
    refresh_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,

    // OAuth callback lifecycle
    oauth_state: Arc<RwLock<Option<String>>>,
    oauth_in_flight: Arc<AtomicBool>,
    oauth_code_consumed: Arc<AtomicBool>,
    oauth_cancelled: Arc<AtomicBool>,
    oauth_commit_gate: Arc<parking_lot::Mutex<()>>,
    oauth_callback_timeout: Duration,
    // Redirect URI actually used in the authorization request (tracks the bound
    // callback port so the token-exchange redirect_uri always matches).
    oauth_redirect_uri: Arc<RwLock<Option<String>>>,
    oauth_client_id: Arc<RwLock<Option<String>>>,
    oauth_rejection_reported: Arc<AtomicBool>,
}

/// Drop guard: clears the in-flight flag so a new OAuth flow can start.
struct OAuthFlowGuard {
    in_flight: Arc<AtomicBool>,
}

impl Drop for OAuthFlowGuard {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::SeqCst);
    }
}

/// Internal authentication events
#[derive(Debug, Clone)]
pub enum AuthEvent {
    StateChanged(AuthState),
    CredentialsUpdated,
    UserInfoUpdated(UserInfo),
    RefreshTriggered,
    RefreshCompleted(std::result::Result<(), String>),
    FlowStatus(AuthFlowStatus),
}

/// Safe flow milestones for native UI bridging; carries no URLs or account data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFlowStatus {
    BrowserOpenerSucceeded,
    CallbackRejected,
    CallbackTimedOut,
}

/// OAuth token response
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    scope: String,
}

/// Google userinfo response
#[derive(Debug, Deserialize)]
struct UserInfoResponse {
    sub: String,
    email: String,
    name: String,
    picture: Option<String>,
    email_verified: bool,
}

/// Constant-time comparison of two byte slices.
fn ct_equals(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Wait until the cancellation flag is set.
async fn cancelled_wait(cancelled: Arc<AtomicBool>) {
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Accept a connection with a bounded timeout.
async fn timeout_accept(
    timeout: Duration,
    listener: TcpListener,
) -> Result<(TcpStream, std::net::SocketAddr), ()> {
    match tokio::time::timeout(timeout, listener.accept()).await {
        Ok(result) => result.map_err(|_| ()),
        Err(_) => Err(()),
    }
}

/// Read from a socket with a bounded timeout.
async fn timeout_read(timeout: Duration, socket: &mut TcpStream) -> Result<String, ()> {
    let mut buf = [0u8; 8192];
    match tokio::time::timeout(timeout, socket.read(&mut buf)).await {
        Ok(Ok(n)) => Ok(String::from_utf8_lossy(&buf[..n]).into_owned()),
        _ => Err(()),
    }
}

impl AuthService {
    fn configured_client_id() -> Result<String> {
        let id = std::env::var(CLIENT_ID_ENV).map_err(|_| {
            AuthError::OAuthFlow(format!(
                "Set {CLIENT_ID_ENV} to your own Google Cloud Desktop app OAuth client ID before signing in"
            ))
        })?;
        Self::validate_client_id(Some(&id))?;
        Ok(id)
    }

    /// Validate only local syntax. Registration, type, and ownership require
    /// inspection in the owner's Google Cloud project.
    pub fn validate_client_id(id: Option<&str>) -> Result<()> {
        let id = id.ok_or_else(|| {
            AuthError::OAuthFlow(format!(
                "Set {CLIENT_ID_ENV} to your own Google Cloud Desktop app OAuth client ID before signing in"
            ))
        })?;
        // Syntax cannot prove client type or ownership; Google Cloud Console must.
        if id.trim() != id
            || id.len() > 256
            || !id.ends_with(".apps.googleusercontent.com")
            || !id
                .strip_suffix(".apps.googleusercontent.com")
                .is_some_and(|prefix| {
                    !prefix.is_empty()
                        && prefix
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        {
            return Err(AuthError::OAuthFlow(format!(
                "{CLIENT_ID_ENV} is invalid; use your own Google Cloud Desktop app OAuth client ID"
            ))
            .into());
        }
        Ok(())
    }

    /// Create a new authentication service
    #[instrument(skip(settings, windows))]
    pub async fn new(
        settings: Arc<SettingsService>,
        windows: Arc<WindowsIntegration>,
    ) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Orcave/2.0 (Windows; +https://orcave.app)")
            .timeout(Duration::from_secs(30))
            .build()?;

        let (event_tx, _) = broadcast::channel(64);

        let service = Self {
            client,
            settings,
            windows,
            credentials: Arc::new(RwLock::new(None)),
            auth_state: Arc::new(RwLock::new(AuthState::Unauthenticated)),
            user_info: Arc::new(RwLock::new(None)),
            pkce_verifier: Arc::new(RwLock::new(None)),
            pkce_challenge: Arc::new(RwLock::new(None)),
            event_tx,
            refresh_handle: Arc::new(Mutex::new(None)),
            oauth_state: Arc::new(RwLock::new(None)),
            oauth_in_flight: Arc::new(AtomicBool::new(false)),
            oauth_code_consumed: Arc::new(AtomicBool::new(false)),
            oauth_cancelled: Arc::new(AtomicBool::new(false)),
            oauth_commit_gate: Arc::new(parking_lot::Mutex::new(())),
            oauth_callback_timeout: Duration::from_secs(300), // 5 minutes default
            oauth_redirect_uri: Arc::new(RwLock::new(None)),
            oauth_client_id: Arc::new(RwLock::new(None)),
            oauth_rejection_reported: Arc::new(AtomicBool::new(false)),
        };

        // Try to restore credentials from secure storage
        if let Err(e) = service.restore_credentials().await {
            warn!("Failed to restore credentials: {}", e);
        }

        Ok(service)
    }

    /// Get current authentication state
    pub fn state(&self) -> AuthState {
        *self.auth_state.read()
    }

    /// Get current credentials (if authenticated)
    pub fn credentials(&self) -> Option<Credentials> {
        if self.state() != AuthState::Authenticated {
            return None;
        }
        self.credentials
            .read()
            .clone()
            .filter(|creds| !creds.is_expired())
    }

    /// Get current user info
    pub fn user_info(&self) -> Option<UserInfo> {
        if self.state() == AuthState::Authenticated {
            self.user_info.read().clone()
        } else {
            None
        }
    }

    /// Subscribe to auth events
    pub fn subscribe(&self) -> broadcast::Receiver<AuthEvent> {
        self.event_tx.subscribe()
    }

    /// Emit auth event
    fn emit(&self, event: AuthEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Set authentication state
    fn set_state(&self, state: AuthState) {
        *self.auth_state.write() = state;
        self.emit(AuthEvent::StateChanged(state));
    }

    /// Generate PKCE verifier and challenge
    pub(crate) fn generate_pkce() -> (String, String) {
        let mut verifier_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut verifier_bytes);
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

        use ring::digest::{digest, SHA256};
        let challenge_bytes = digest(&SHA256, verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(challenge_bytes.as_ref());

        (verifier, challenge)
    }

    /// Start OAuth 2.0 authorization code flow with PKCE
    #[instrument(skip(self))]
    pub async fn start_auth_flow(&self) -> Result<Url> {
        Err(
            AuthError::OAuthFlow("Use start_oauth_flow to bind and validate the callback".into())
                .into(),
        )
    }

    /// Generate a random OAuth state parameter for CSRF protection.
    fn generate_oauth_state() -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Bind the loopback callback listener. Tries port 8080 first, then falls back
    /// to an OS-assigned port (port 0) if 8080 is unavailable.
    async fn bind_callback_listener() -> Result<TcpListener> {
        match TcpListener::bind("127.0.0.1:8080").await {
            Ok(listener) => return Ok(listener),
            Err(e8080) => match TcpListener::bind("127.0.0.1:0").await {
                Ok(listener) => return Ok(listener),
                Err(e0) => {
                    return Err(AuthError::PortUnavailable(format!(
                        "port 8080: {e8080}; port 0: {e0}"
                    ))
                    .into())
                }
            },
        }
    }

    /// Start the OAuth flow: bind listener, generate state + PKCE, return the
    /// authorization URL and the bound listener. The redirect_uri uses the actual
    /// bound port.
    #[instrument(skip(self))]
    pub async fn start_oauth_flow(&self) -> Result<(Url, TcpListener)> {
        let client_id = Self::configured_client_id()?;
        if self
            .oauth_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(AuthError::FlowInProgress.into());
        }
        {
            let _gate = self.oauth_commit_gate.lock();
            self.oauth_code_consumed.store(false, Ordering::SeqCst);
            self.oauth_cancelled.store(false, Ordering::SeqCst);
            self.oauth_rejection_reported.store(false, Ordering::SeqCst);
        }

        self.set_state(AuthState::Authenticating);
        *self.oauth_client_id.write() = Some(client_id.clone());

        let state = Self::generate_oauth_state();
        *self.oauth_state.write() = Some(state.clone());

        let (verifier, challenge) = Self::generate_pkce();
        *self.pkce_verifier.write() = Some(verifier.clone());
        *self.pkce_challenge.write() = Some(challenge.clone());

        let listener = match Self::bind_callback_listener().await {
            Ok(listener) => listener,
            Err(error) => {
                self.oauth_in_flight.store(false, Ordering::SeqCst);
                self.set_state(AuthState::Unauthenticated);
                return Err(error);
            }
        };
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        // Remember the redirect URI actually sent to Google so the token
        // exchange uses an identical value (required when port 0 fallback binds
        // a port other than 8080).
        *self.oauth_redirect_uri.write() = Some(redirect_uri.clone());

        let auth_url = Self::authorization_url(&client_id, &redirect_uri, &challenge, &state)?;
        Ok((auth_url, listener))
    }

    /// Pure authorization request builder; never opens a browser or contacts Google.
    pub fn authorization_url(
        client_id: &str,
        redirect_uri: &str,
        challenge: &str,
        state: &str,
    ) -> Result<Url> {
        Self::validate_client_id(Some(client_id))?;
        let redirect = Url::parse(redirect_uri)?;
        if redirect.scheme() != "http"
            || redirect.host_str() != Some("127.0.0.1")
            || redirect.port().is_none()
            || redirect.path() != "/callback"
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err(
                AuthError::OAuthFlow("Invalid Desktop loopback redirect URI".into()).into(),
            );
        }
        if challenge.len() != 43
            || !challenge
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(AuthError::OAuthFlow("Invalid PKCE S256 challenge".into()).into());
        }
        let mut auth_url = Url::parse(&format!("{}/o/oauth2/v2/auth", YTM_OAUTH_BASE))?;
        auth_url
            .query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("state", &state);

        Ok(auth_url)
    }

    /// Cancel an in-flight OAuth flow. Causes `wait_for_callback` to return
    /// `CallbackCancelled` instead of blocking.
    pub fn cancel_oauth_flow(&self) {
        let _gate = self.oauth_commit_gate.lock();
        self.oauth_cancelled.store(true, Ordering::SeqCst);
    }

    /// Wait for the OAuth callback on the listener, with bounded timeout and
    /// cancellation. Validates the request, state parameter, and extracts the
    /// authorization code. Rejects duplicate callbacks. Sends an HTML response
    /// back to the browser before returning.
    #[instrument(skip(self, listener))]
    pub async fn wait_for_callback(&self, listener: TcpListener) -> Result<String> {
        let expected_state = self.oauth_state.read().clone();
        let deadline = tokio::time::Instant::now() + self.oauth_callback_timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.emit(AuthEvent::FlowStatus(AuthFlowStatus::CallbackTimedOut));
                return Err(AuthError::CallbackTimeout.into());
            }
            let (mut socket, _) = tokio::select! {
                biased;
                _ = cancelled_wait(self.oauth_cancelled.clone()) => return Err(AuthError::CallbackCancelled.into()),
                accept = tokio::time::timeout(remaining, listener.accept()) => {
                    accept.map_err(|_| {
                        self.emit(AuthEvent::FlowStatus(AuthFlowStatus::CallbackTimedOut));
                        AuthError::CallbackTimeout
                    })??
                }
            };
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let request = tokio::select! {
                biased;
                _ = cancelled_wait(self.oauth_cancelled.clone()) => return Err(AuthError::CallbackCancelled.into()),
                request = timeout_read(remaining, &mut socket) => request,
            };
            let Ok(request) = request else {
                if tokio::time::Instant::now() >= deadline {
                    self.emit(AuthEvent::FlowStatus(AuthFlowStatus::CallbackTimedOut));
                    return Err(AuthError::CallbackTimeout.into());
                }
                continue;
            };
            let parsed =
                Self::parse_callback_request(&request, &expected_state, &self.oauth_code_consumed);
            if parsed.is_err() && !self.oauth_rejection_reported.swap(true, Ordering::SeqCst) {
                self.emit(AuthEvent::FlowStatus(AuthFlowStatus::CallbackRejected));
            }
            // Send a response to the browser regardless of outcome.
            let response = if parsed.is_ok() {
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                 <html><body><h1>Authentication successful!</h1>\
                 <p>You can close this window and return to Orcave.</p>\
                 <script>window.close()</script></body></html>"
            } else {
                "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                 <html><body><h1>Authentication failed</h1></body></html>"
            };
            let _ = tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                socket.write_all(response.as_bytes()),
            )
            .await;
            if let Ok(code) = parsed {
                return Ok(code);
            }
        }
    }

    /// Parse an HTTP callback request: validate method, state, code.
    fn parse_callback_request(
        request: &str,
        expected_state: &Option<String>,
        code_consumed: &AtomicBool,
    ) -> Result<String> {
        // Only GET is allowed.
        let Some(request_line) = request.lines().next() else {
            return Err(AuthError::CallbackParse("empty request".to_string()).into());
        };

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() != 3 || parts[0] != "GET" || parts[2] != "HTTP/1.1" {
            return Err(
                AuthError::CallbackParse("only GET requests are accepted".to_string()).into(),
            );
        }

        let path = parts[1];
        if !path.starts_with('/') || path.starts_with("//") || path.contains('#') {
            return Err(AuthError::CallbackParse("invalid callback path".into()).into());
        }
        let url = Url::parse(&format!("http://127.0.0.1{path}"))
            .map_err(|e| AuthError::CallbackParse(format!("invalid callback URL: {e}")))?;
        if url.path() != "/callback" {
            return Err(AuthError::CallbackParse("unexpected callback path".into()).into());
        }
        if request.len() > 8192 || !request.contains("\r\n\r\n") {
            return Err(AuthError::CallbackParse("incomplete callback request".into()).into());
        }

        // Validate state parameter (constant-time comparison).
        let pairs: Vec<_> = url.query_pairs().collect();
        if pairs.iter().filter(|(key, _)| key == "state").count() != 1
            || pairs.iter().filter(|(key, _)| key == "code").count() != 1
            || pairs.iter().any(|(key, _)| key == "error")
        {
            return Err(AuthError::CallbackParse("invalid callback parameters".into()).into());
        }
        let query_state: Option<String> = pairs
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.to_string());
        let Some(query_state) = query_state else {
            return Err(AuthError::CallbackParse("missing state parameter".to_string()).into());
        };
        let expected = expected_state
            .as_deref()
            .ok_or_else(|| AuthError::OAuthFlow("OAuth state not generated".to_string()))?;
        if !ct_equals(query_state.as_bytes(), expected.as_bytes()) {
            return Err(AuthError::StateMismatch.into());
        }

        // Extract authorization code.
        let code = pairs
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| {
                anyhow::Error::from(AuthError::CallbackParse(
                    "missing authorization code".to_string(),
                ))
            })?;

        if code.is_empty() {
            return Err(anyhow::Error::from(AuthError::CallbackParse(
                "empty authorization code".to_string(),
            )));
        }

        // Reject duplicate callbacks.
        if code_consumed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(anyhow::Error::from(AuthError::CodeAlreadyConsumed));
        }

        Ok(code)
    }

    /// Execute OAuth with a caller-provided, validated system-browser opener.
    /// The in-flight guard covers opener failure, timeout, cancellation, and exchange.
    pub async fn run_oauth_flow<F>(&self, opener: F) -> Result<()>
    where
        F: FnOnce(&Url) -> Result<()>,
    {
        let (auth_url, listener) = self.start_oauth_flow().await?;
        let _guard = OAuthFlowGuard {
            in_flight: self.oauth_in_flight.clone(),
        };
        if let Err(error) = opener(&auth_url) {
            self.set_state(AuthState::Unauthenticated);
            return Err(error);
        }
        self.emit(AuthEvent::FlowStatus(
            AuthFlowStatus::BrowserOpenerSucceeded,
        ));
        let code = match self.wait_for_callback(listener).await {
            Ok(code) => code,
            Err(error) => {
                self.set_state(AuthState::Unauthenticated);
                return Err(error);
            }
        };
        info!("OAuth callback received, code=[REDACTED], state=[REDACTED]");
        self.complete_auth_flow(code).await
    }

    /// Complete OAuth flow with authorization code
    #[instrument(skip(self, code))]
    pub async fn complete_auth_flow(&self, code: String) -> Result<()> {
        let result = self.complete_auth_flow_inner(code).await;
        if result.is_err() {
            self.set_state(if self.oauth_cancelled.load(Ordering::SeqCst) {
                AuthState::Unauthenticated
            } else {
                AuthState::Error
            });
        }
        result
    }

    async fn complete_auth_flow_inner(&self, code: String) -> Result<()> {
        self.ensure_oauth_active()?;
        let client_id = Self::configured_client_id()?;
        if self.oauth_client_id.write().take().as_ref() != Some(&client_id) {
            return Err(AuthError::OAuthFlow(
                "OAuth client changed during sign-in; restart sign-in".into(),
            )
            .into());
        }
        let verifier = self
            .pkce_verifier
            .write()
            .take()
            .ok_or_else(|| AuthError::OAuthFlow("PKCE verifier missing".to_string()))?;

        // Use the redirect URI from the authorization request. Reject missing
        // flow state rather than exchanging a code against a different redirect.
        let redirect_uri = self
            .oauth_redirect_uri
            .write()
            .take()
            .ok_or_else(|| AuthError::OAuthFlow("OAuth redirect URI missing".into()))?;

        let params = vec![
            ("code", code),
            ("client_id", client_id.clone()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code".to_string()),
            ("code_verifier", verifier),
        ];

        let response = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AuthError::TokenExchange(format!("HTTP {}", response.status())).into());
        }

        let token_response: TokenResponse = response.json().await?;
        let credentials = Self::credentials_from_response(token_response)?;
        self.ensure_oauth_active()?;
        // OAuth token and Google profile do not prove access to YouTube Music.
        let user = self.verify_music_access(&credentials).await?;
        self.ensure_oauth_active()?;
        self.windows.store_credentials(&credentials).await?;
        self.settings
            .set_encrypted("auth.oauth_client_id", &client_id)
            .await?;
        self.settings
            .set_encrypted("auth.credentials", &credentials)
            .await?;
        let _gate = self.oauth_commit_gate.lock();
        if self.oauth_cancelled.load(Ordering::SeqCst) {
            drop(_gate);
            let _ = self.windows.clear_credentials().await;
            let _ = self.settings.remove("auth.credentials").await;
            return Err(AuthError::CallbackCancelled.into());
        }
        *self.credentials.write() = Some(credentials);
        *self.user_info.write() = Some(user.clone());
        self.emit(AuthEvent::UserInfoUpdated(user));

        self.set_state(AuthState::Authenticated);
        self.emit(AuthEvent::CredentialsUpdated);
        drop(_gate);
        self.start_auto_refresh().await;

        Ok(())
    }

    fn ensure_oauth_active(&self) -> Result<()> {
        if self.oauth_cancelled.load(Ordering::SeqCst) {
            return Err(AuthError::CallbackCancelled.into());
        }
        Ok(())
    }

    /// Build credentials only after a successful OAuth exchange.
    fn credentials_from_response(token_response: TokenResponse) -> Result<Credentials> {
        if !token_response.token_type.eq_ignore_ascii_case("Bearer")
            || token_response.access_token.is_empty()
            || token_response.expires_in == 0
        {
            return Err(AuthError::InvalidToken("Invalid OAuth token response".into()).into());
        }
        let expires_at =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + token_response.expires_in;

        Ok(Credentials {
            access_token: token_response.access_token,
            refresh_token: token_response
                .refresh_token
                .ok_or_else(|| AuthError::TokenExchange("No refresh token received".to_string()))?,
            id_token: token_response.id_token,
            expires_at,
            token_type: token_response.token_type,
            scope: token_response.scope,
        })
    }

    /// Restore credentials from secure storage
    #[instrument(skip(self))]
    async fn restore_credentials(&self) -> Result<()> {
        // Legacy credentials have no proven client binding and must not be reused.
        let configured = match Self::configured_client_id() {
            Ok(id) => id,
            Err(_) => {
                self.set_state(AuthState::Unauthenticated);
                return Ok(());
            }
        };
        let stored: Option<String> = self.settings.get_encrypted("auth.oauth_client_id").await?;
        if stored.as_ref() != Some(&configured) {
            self.set_state(AuthState::Unauthenticated);
            return Ok(());
        }
        // Try Windows Credential Manager first
        let primary = self.windows.retrieve_credentials().await.ok();
        let backup: Option<Credentials> = self
            .settings
            .get_encrypted("auth.credentials")
            .await
            .ok()
            .flatten();
        for creds in [primary, backup].into_iter().flatten() {
            if !creds.is_expired() {
                if let Ok(user) = self.verify_music_access(&creds).await {
                    *self.credentials.write() = Some(creds);
                    *self.user_info.write() = Some(user);
                    self.set_state(AuthState::Authenticated);
                    self.start_auto_refresh().await;
                    return Ok(());
                }
            }
        }

        self.set_state(AuthState::Unauthenticated);
        Ok(())
    }

    /// Refresh access token using refresh token
    #[instrument(skip(self))]
    pub async fn refresh_token(&self) -> Result<()> {
        self.set_state(AuthState::Refreshing);
        let result = self.refresh_token_inner().await;
        if result.is_err() {
            self.set_state(AuthState::TokenExpired);
        }
        result
    }

    async fn refresh_token_inner(&self) -> Result<()> {
        let client_id = Self::configured_client_id()?;
        let bound: Option<String> = self.settings.get_encrypted("auth.oauth_client_id").await?;
        if bound.as_ref() != Some(&client_id) {
            return Err(AuthError::TokenRefresh(
                "OAuth client changed; sign in again with your configured Desktop client".into(),
            )
            .into());
        }
        let credentials = self
            .credentials
            .read()
            .clone()
            .ok_or_else(|| AuthError::TokenRefresh("No credentials to refresh".to_string()))?;

        let params = vec![
            ("client_id", client_id),
            ("refresh_token", credentials.refresh_token.clone()),
            ("grant_type", "refresh_token".to_string()),
        ];

        let response = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AuthError::TokenRefresh(format!("HTTP {}", response.status())).into());
        }

        let token_response: TokenResponse = response.json().await?;
        if token_response.access_token.is_empty()
            || token_response.expires_in == 0
            || (!token_response.token_type.is_empty()
                && !token_response.token_type.eq_ignore_ascii_case("Bearer"))
        {
            return Err(AuthError::InvalidToken("Invalid refresh response".into()).into());
        }

        // Update credentials with new access token (refresh token may rotate)
        let mut new_credentials = credentials;
        new_credentials.access_token = token_response.access_token;
        if let Some(new_refresh) = token_response.refresh_token {
            new_credentials.refresh_token = new_refresh;
        }
        new_credentials.expires_at =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + token_response.expires_in;

        let user = self.verify_music_access(&new_credentials).await?;
        self.windows.store_credentials(&new_credentials).await?;
        self.settings
            .set_encrypted("auth.credentials", &new_credentials)
            .await?;

        *self.credentials.write() = Some(new_credentials);
        *self.user_info.write() = Some(user);
        self.set_state(AuthState::Authenticated);
        self.emit(AuthEvent::CredentialsUpdated);

        Ok(())
    }

    /// Fetch user profile information
    #[instrument(skip(self))]
    async fn verify_music_access(&self, credentials: &Credentials) -> Result<UserInfo> {
        // Userinfo proves identity only. InnerTube must accept this token before auth succeeds.
        let music = self.client
            .post("https://music.youtube.com/youtubei/v1/browse?prettyPrint=false")
            .header(AUTHORIZATION, format!("Bearer {}", credentials.access_token))
            .json(&serde_json::json!({"context":{"client":{"clientName":"WEB_REMIX","clientVersion":"1.20240101.01.00"}},"browseId":"FEmusic_liked_playlists"}))
            .send().await?;
        if !music.status().is_success() {
            return Err(AuthError::InvalidToken(format!(
                "InnerTube rejected session: HTTP {}",
                music.status()
            ))
            .into());
        }
        let body: serde_json::Value = music.json().await?;
        if !Self::has_music_content(&body) {
            return Err(
                AuthError::InvalidToken("InnerTube did not return music content".into()).into(),
            );
        }
        let response = self
            .client
            .get("https://www.googleapis.com/oauth2/v3/userinfo")
            .header(
                AUTHORIZATION,
                format!("Bearer {}", credentials.access_token),
            )
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(AuthError::InvalidToken("Failed to fetch user info".to_string()).into());
        }

        let user_info: UserInfoResponse = response.json().await?;
        Ok(UserInfo {
            id: user_info.sub,
            email: user_info.email,
            name: user_info.name,
            picture: user_info.picture,
            verified_email: user_info.email_verified,
        })
    }

    fn has_music_content(body: &serde_json::Value) -> bool {
        body.get("contents")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|contents| !contents.is_empty())
            && body.pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                == Some(&serde_json::Value::Bool(false))
            && body.get("error").is_none()
    }

    /// Start automatic token refresh background task
    #[instrument(skip(self))]
    async fn start_auto_refresh(&self) {
        let mut handle = self.refresh_handle.lock().await;
        if handle.is_some() {
            return; // Already running
        }

        let service = self.clone();
        let join_handle = tokio::spawn(async move {
            loop {
                let wait_time = {
                    let creds = service.credentials.read().clone();
                    match creds {
                        Some(c) if !c.is_expired() => {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            // Refresh 5 minutes before expiry
                            let wait = c.expires_at.saturating_sub(now + 300);
                            Duration::from_secs(wait.min(3600)) // Cap at 1 hour
                        }
                        _ => Duration::from_secs(60), // Check every minute if no creds
                    }
                };

                tokio::time::sleep(wait_time).await;

                if service.state() == AuthState::Authenticated {
                    if let Err(e) = service.refresh_token().await {
                        error!("Auto token refresh failed: {}", e);
                        service.set_state(AuthState::TokenExpired);
                    }
                }
            }
        });

        *handle = Some(join_handle);
    }

    /// Sign out and clear all credentials
    #[instrument(skip(self))]
    pub async fn sign_out(&self) -> Result<()> {
        // Stop refresh task
        if let Some(handle) = self.refresh_handle.lock().await.take() {
            handle.abort();
        }

        // Clear both stores even if one fails; never expose a signed-out token.
        let windows_result = self.windows.clear_credentials().await;
        let settings_result = self.settings.remove("auth.credentials").await;
        let binding_result = self.settings.remove("auth.oauth_client_id").await;
        *self.credentials.write() = None;
        *self.user_info.write() = None;
        self.set_state(AuthState::Unauthenticated);
        self.emit(AuthEvent::CredentialsUpdated);

        windows_result?;
        settings_result?;
        binding_result?;
        Ok(())
    }

    /// Get authenticated HTTP client for API calls
    pub fn authenticated_client(&self) -> Option<AuthenticatedClient> {
        if self.state() != AuthState::Authenticated {
            return None;
        }
        let credentials = self.credentials.read().clone()?;
        if credentials.is_expired() {
            return None;
        }
        Some(AuthenticatedClient {
            client: self.client.clone(),
            access_token: credentials.access_token.clone(),
        })
    }

    /// Shutdown the auth service
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        if let Some(handle) = self.refresh_handle.lock().await.take() {
            handle.abort();
        }
        Ok(())
    }
}

impl Clone for AuthService {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            settings: self.settings.clone(),
            windows: self.windows.clone(),
            credentials: self.credentials.clone(),
            auth_state: self.auth_state.clone(),
            user_info: self.user_info.clone(),
            pkce_verifier: self.pkce_verifier.clone(),
            pkce_challenge: self.pkce_challenge.clone(),
            event_tx: self.event_tx.clone(),
            refresh_handle: self.refresh_handle.clone(),
            oauth_state: self.oauth_state.clone(),
            oauth_in_flight: self.oauth_in_flight.clone(),
            oauth_code_consumed: self.oauth_code_consumed.clone(),
            oauth_cancelled: self.oauth_cancelled.clone(),
            oauth_commit_gate: self.oauth_commit_gate.clone(),
            oauth_callback_timeout: self.oauth_callback_timeout,
            oauth_redirect_uri: self.oauth_redirect_uri.clone(),
            oauth_client_id: self.oauth_client_id.clone(),
            oauth_rejection_reported: self.oauth_rejection_reported.clone(),
        }
    }
}

/// Pre-authenticated HTTP client for API calls
#[derive(Clone)]
pub struct AuthenticatedClient {
    client: Client,
    access_token: String,
}

impl AuthenticatedClient {
    pub async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<T, reqwest::Error> {
        self.client
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.access_token))
            .send()
            .await?
            .json()
            .await
    }

    pub async fn post_json<T: for<'de> Deserialize<'de>, B: Serialize>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T, reqwest::Error> {
        self.client
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.access_token))
            .json(body)
            .send()
            .await?
            .json()
            .await
    }

    pub async fn post_form<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<T, reqwest::Error> {
        self.client
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.access_token))
            .form(form)
            .send()
            .await?
            .json()
            .await
    }

    /// Send a JSON POST and return the HTTP status with the raw response,
    /// so callers can inspect status codes before parsing the body.
    pub async fn post_json_with_status<B: Serialize>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<(reqwest::StatusCode, reqwest::Response), reqwest::Error> {
        let response = self
            .client
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.access_token))
            .json(body)
            .send()
            .await?;
        let status = response.status();
        Ok((status, response))
    }
}

/// Tauri command handlers. The app must manage `Arc<AuthService>`.
pub mod commands {
    use super::*;

    #[tauri::command]
    pub async fn auth_start_flow(_service: tauri::State<'_, Arc<AuthService>>) -> TauriResult<()> {
        Err(TauriError::Auth(
            "Use the app's validated OAuth browser opener".into(),
        ))
    }

    /// Cancel an in-flight OAuth flow. The listener closes and no task hangs.
    #[tauri::command]
    pub async fn auth_cancel_flow(service: tauri::State<'_, Arc<AuthService>>) -> TauriResult<()> {
        service.cancel_oauth_flow();
        Ok(())
    }

    #[tauri::command]
    pub async fn auth_complete_flow(
        _service: tauri::State<'_, Arc<AuthService>>,
        _code: String,
    ) -> TauriResult<()> {
        Err(TauriError::Auth(
            "Direct OAuth code submission is disabled; use auth_start_flow".into(),
        ))
    }

    #[tauri::command]
    pub async fn auth_refresh_token(
        service: tauri::State<'_, Arc<AuthService>>,
    ) -> TauriResult<()> {
        service
            .refresh_token()
            .await
            .map_err(|e| TauriError::Auth(e.to_string()))
    }

    #[tauri::command]
    pub async fn auth_sign_out(service: tauri::State<'_, Arc<AuthService>>) -> TauriResult<()> {
        service
            .sign_out()
            .await
            .map_err(|e| TauriError::Auth(e.to_string()))
    }

    #[tauri::command]
    pub async fn auth_get_state(
        service: tauri::State<'_, Arc<AuthService>>,
    ) -> TauriResult<AuthState> {
        Ok(service.state())
    }

    #[tauri::command]
    pub async fn auth_get_user_info(
        service: tauri::State<'_, Arc<AuthService>>,
    ) -> TauriResult<Option<UserInfo>> {
        Ok(service.user_info())
    }
}

pub use commands::*;

#[cfg(test)]
mod tests {
    use super::*;

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
            expires_at: now + 100, // Expires soon
            token_type: "Bearer".to_string(),
            scope: "test".to_string(),
        };

        assert!(creds.is_expired());

        creds.expires_at = now + 1000; // Expires later
        assert!(!creds.is_expired());
    }

    #[test]
    fn test_pkce_generation() {
        let (verifier, challenge) = AuthService::generate_pkce();
        assert_eq!(verifier.len(), 43); // 32 bytes base64url = 43 chars
        assert_eq!(challenge.len(), 43);
        assert_ne!(verifier, challenge);
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD
                .encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref())
        );
    }

    #[test]
    fn music_access_requires_content_without_error() {
        assert!(!AuthService::has_music_content(
            &serde_json::json!({"error": {"code": 403}})
        ));
        assert!(!AuthService::has_music_content(
            &serde_json::json!({"contents": null})
        ));
        assert!(!AuthService::has_music_content(
            &serde_json::json!({"contents": {}})
        ));
        assert!(!AuthService::has_music_content(
            &serde_json::json!({"contents": {"sectionListRenderer": {}}})
        ));
        assert!(!AuthService::has_music_content(
            &serde_json::json!({"contents": {}, "error": {"code": 403}})
        ));
        assert!(AuthService::has_music_content(
            &serde_json::json!({"contents": {"sectionListRenderer": {}}, "responseContext": {"mainAppWebResponseContext": {"loggedOut": false}}})
        ));
    }

    // --- OAuth callback lifecycle tests ---

    #[test]
    fn test_ct_equals_same_length() {
        assert!(ct_equals(b"abc", b"abc"));
        assert!(ct_equals(b"", b""));
        assert!(!ct_equals(b"abc", b"abd"));
        assert!(!ct_equals(b"abc", b"abcd"));
        assert!(!ct_equals(b"abcd", b"abc"));
        assert!(!ct_equals(b"", b"a"));
    }

    #[test]
    fn test_generate_oauth_state() {
        let s1 = AuthService::generate_oauth_state();
        let s2 = AuthService::generate_oauth_state();
        assert_eq!(s1.len(), 43); // 32 bytes base64url
        assert_ne!(s1, s2);
    }

    #[test]
    fn oauth_guard_releases_in_flight_on_error() {
        let in_flight = Arc::new(AtomicBool::new(true));
        let result: Result<()> = (|| {
            let _guard = OAuthFlowGuard {
                in_flight: in_flight.clone(),
            };
            Err(AuthError::CallbackCancelled.into())
        })();
        assert!(result.is_err());
        assert!(!in_flight.load(Ordering::SeqCst));
    }

    #[test]
    fn test_parse_callback_success() {
        let state = Some("test-state-123".to_string());
        let consumed = AtomicBool::new(false);
        let request =
            "GET /callback?code=def456&state=test-state-123 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let code = AuthService::parse_callback_request(request, &state, &consumed).unwrap();
        assert_eq!(code, "def456");
        assert!(consumed.load(Ordering::SeqCst));
    }

    #[test]
    fn test_parse_callback_wrong_state() {
        let state = Some("expected-state".to_string());
        let consumed = AtomicBool::new(false);
        let request =
            "GET /callback?code=def456&state=wrong-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let err = AuthService::parse_callback_request(request, &state, &consumed).unwrap_err();
        assert!(matches!(err, e if e.to_string().contains("state mismatch")));
        assert!(!consumed.load(Ordering::SeqCst));
    }

    #[test]
    fn test_parse_callback_missing_code() {
        let state = Some("test-state".to_string());
        let consumed = AtomicBool::new(false);
        let request = "GET /callback?state=test-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let err = AuthService::parse_callback_request(request, &state, &consumed).unwrap_err();
        assert!(matches!(err, e if e.to_string().contains("invalid callback parameters")));
        assert!(!consumed.load(Ordering::SeqCst));
    }

    #[test]
    fn test_parse_callback_duplicate() {
        let state = Some("test-state".to_string());
        let consumed = AtomicBool::new(true); // Already consumed
        let request =
            "GET /callback?code=def456&state=test-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let err = AuthService::parse_callback_request(request, &state, &consumed).unwrap_err();
        assert!(matches!(err, e if e.to_string().contains("already consumed")));
    }

    #[test]
    fn test_parse_callback_malformed_non_get() {
        let state = Some("test-state".to_string());
        let consumed = AtomicBool::new(false);
        let request =
            "POST /callback?code=def456&state=test-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let err = AuthService::parse_callback_request(request, &state, &consumed).unwrap_err();
        assert!(matches!(err, e if e.to_string().contains("only GET")));
    }

    #[test]
    fn test_parse_callback_missing_state() {
        let state = Some("expected-state".to_string());
        let consumed = AtomicBool::new(false);
        let request = "GET /callback?code=def456 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let err = AuthService::parse_callback_request(request, &state, &consumed).unwrap_err();
        assert!(matches!(err, e if e.to_string().contains("invalid callback parameters")));
    }

    #[test]
    fn test_parse_callback_empty_request() {
        let state = Some("test-state".to_string());
        let consumed = AtomicBool::new(false);
        let err = AuthService::parse_callback_request("", &state, &consumed).unwrap_err();
        assert!(matches!(err, e if e.to_string().contains("empty request")));
    }

    #[test]
    fn callback_rejects_wrong_path_and_duplicate_parameters_without_consuming_code() {
        let state = Some("synthetic-state".to_string());
        let consumed = AtomicBool::new(false);
        for target in [
            "/other?state=synthetic-state&code=synthetic-code",
            "//attacker.invalid/callback?state=synthetic-state&code=synthetic-code",
            "/callback?state=synthetic-state&state=synthetic-state&code=synthetic-code",
            "/callback?state=synthetic-state&code=synthetic-code&code=another",
            "/callback?state=synthetic-state&code=synthetic-code&error=access_denied",
            "/callback?state=synthetic-state&code=",
        ] {
            let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
            assert!(AuthService::parse_callback_request(&request, &state, &consumed).is_err());
            assert!(!consumed.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn callback_requires_complete_http_request() {
        let state = Some("synthetic-state".to_string());
        let consumed = AtomicBool::new(false);
        let request = "GET /callback?state=synthetic-state&code=synthetic-code HTTP/1.1\r\n";
        assert!(AuthService::parse_callback_request(request, &state, &consumed).is_err());
        assert!(!consumed.load(Ordering::SeqCst));
    }

    #[test]
    fn oauth_configuration_uses_pkce_scopes_and_loopback() {
        let redirect = Url::parse(REDIRECT_URI).unwrap();
        assert_eq!(redirect.scheme(), "http");
        assert_eq!(redirect.host_str(), Some("127.0.0.1"));
        assert_eq!(redirect.port(), Some(8080));
        assert_eq!(redirect.path(), "/callback");
        assert!(SCOPES.contains(&"https://www.googleapis.com/auth/youtube.readonly"));
        assert!(SCOPES.contains(&"https://www.googleapis.com/auth/userinfo.profile"));
        assert!(SCOPES.contains(&"https://www.googleapis.com/auth/userinfo.email"));
        let (verifier, challenge) = AuthService::generate_pkce();
        assert_eq!(verifier.len(), 43);
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD
                .encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref())
        );
        let url = AuthService::authorization_url(
            "synthetic-client.apps.googleusercontent.com",
            REDIRECT_URI,
            &challenge,
            "synthetic-state",
        )
        .unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(url.path(), "/o/oauth2/v2/auth");
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(
            parameters.get("redirect_uri").map(|v| v.as_ref()),
            Some(REDIRECT_URI)
        );
        assert_eq!(
            parameters.get("code_challenge").map(|v| v.as_ref()),
            Some(challenge.as_str())
        );
        assert_eq!(
            parameters.get("code_challenge_method").map(|v| v.as_ref()),
            Some("S256")
        );
        assert_eq!(
            parameters.get("state").map(|v| v.as_ref()),
            Some("synthetic-state")
        );
        assert_eq!(
            parameters.get("scope").map(|v| v.as_ref()),
            Some(SCOPES.join(" ").as_str())
        );
        assert_eq!(
            parameters.get("response_type").map(|v| v.as_ref()),
            Some("code")
        );
        assert_eq!(
            parameters.get("client_id").map(|v| v.as_ref()),
            Some("synthetic-client.apps.googleusercontent.com")
        );
    }

    #[test]
    fn client_id_validation_fails_closed_without_configuration() {
        for invalid in [
            None,
            Some(""),
            Some("tv-client"),
            Some("desktop.apps.googleusercontent.com "),
            Some("desktop.apps.googleusercontent.com?secret=example"),
        ] {
            assert!(AuthService::validate_client_id(invalid).is_err());
        }
        // Syntax only: this does not attest registration, Desktop type, or ownership.
        assert!(AuthService::validate_client_id(Some(
            "synthetic-client.apps.googleusercontent.com"
        ))
        .is_ok());
    }

    #[test]
    fn authorization_builder_rejects_wrong_redirect_and_pkce() {
        let client_id = "synthetic-client.apps.googleusercontent.com";
        let (_, challenge) = AuthService::generate_pkce();
        for redirect in [
            "http://evil.example:8080/callback",
            "http://127.0.0.1:8080/other",
            "http://127.0.0.1:8080/callback?code=example",
            "https://127.0.0.1:8080/callback",
            "http://127.0.0.1/callback",
        ] {
            assert!(
                AuthService::authorization_url(client_id, redirect, &challenge, "state").is_err()
            );
        }
        assert!(AuthService::authorization_url(client_id, REDIRECT_URI, "wrong", "state").is_err());
        let url =
            AuthService::authorization_url(client_id, REDIRECT_URI, &challenge, "state").unwrap();
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert!(!parameters.contains_key("client_secret"));
        assert_eq!(
            parameters.get("client_id").map(|v| v.as_ref()),
            Some(client_id)
        );
    }

    #[tokio::test]
    async fn test_timeout_accept_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let result = timeout_accept(Duration::from_millis(50), listener).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_timeout_accept_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = async move {
            TcpStream::connect(addr).await.unwrap();
        };
        tokio::spawn(connect);
        let result = timeout_accept(Duration::from_secs(5), listener).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cancelled_wait_returns_when_cancelled() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_clone = cancelled.clone();
        let wait_handle = tokio::spawn(async move {
            cancelled_wait(cancelled_clone).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancelled.store(true, Ordering::SeqCst);
        wait_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_bind_callback_port_unavailable() {
        // Bind port 8080 first, then try to bind again - should fall back to port 0.
        let _holder = TcpListener::bind("127.0.0.1:8080").await.unwrap();
        let result = AuthService::bind_callback_listener().await;
        assert!(
            result.is_ok(),
            "Port 8080 unavailable should fall back to port 0"
        );
    }

    #[tokio::test]
    async fn test_redirect_uri_matches_bound_listener_port() {
        // The redirect URI must be built from the actually-bound port so (a) the
        // callback arrives on the same listener and (b) the token-exchange
        // redirect_uri matches the authorization request (Google rejects any
        // mismatch, including port 0 fallback to a non-8080 port).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let url = Url::parse(&redirect_uri).unwrap();
        assert_eq!(url.port().unwrap(), port);
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.path(), "/callback");
        assert_ne!(url.port().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_wait_for_callback_timeout() {
        // wait_for_callback relies on timeout_accept + cancelled_wait + parse_callback_request.
        // Each is tested individually above. This test verifies the listener is released
        // after a timeout by binding a listener, timing out, then binding again on the same port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _result = timeout_accept(Duration::from_millis(50), listener).await;
        // Listener should be dropped after timeout_accept returns; binding again should succeed.
        let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        drop(listener2);
    }

    #[tokio::test]
    async fn test_shutdown_releases_listener() {
        // After a cancelled accept, the listener must be closed so the port can be reused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_clone = cancelled.clone();
        let accept_fut = async {
            tokio::select! {
                _ = cancelled_wait(cancelled_clone) => {}
                _ = timeout_accept(Duration::from_secs(60), listener) => {}
            }
        };
        tokio::spawn(accept_fut);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancelled.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Port should be reusable now (listener closed).
        let relistener = TcpListener::bind(addr).await;
        assert!(
            relistener.is_ok(),
            "Port should be reusable after cancellation"
        );
    }
}
