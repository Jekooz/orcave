//! Orcave session validation. Production InnerTube access is blocked pending
//! express provider permission. Only loopback synthetic readbacks are enabled.

use std::{collections::HashMap, time::Duration};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use orcave_core::UserInfo;
use reqwest::{header::HeaderMap, redirect::Policy, Client};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Native-only session material. Never serialize this type over frontend IPC.
#[derive(Clone, Deserialize)]
pub struct BrowserSessionContext {
    pub cookie: String,
    pub x_goog_auth_user: String,
    pub x_origin: String,
    pub user_agent: String,
    pub authorization: Option<String>,
    pub captured_at: DateTime<Utc>,
}

impl std::fmt::Debug for BrowserSessionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserSessionContext")
            .field("cookie", &"[REDACTED]")
            .field("x_goog_auth_user", &self.x_goog_auth_user)
            .field("x_origin", &self.x_origin)
            .field("user_agent", &self.user_agent)
            .field("authorization", &"[REDACTED]")
            .field("captured_at", &self.captured_at)
            .finish()
    }
}

impl Default for BrowserSessionContext {
    fn default() -> Self {
        Self {
            cookie: String::new(),
            x_goog_auth_user: "0".into(),
            x_origin: "https://music.youtube.com".into(),
            user_agent: String::new(),
            authorization: None,
            captured_at: Utc::now(),
        }
    }
}

impl Zeroize for BrowserSessionContext {
    fn zeroize(&mut self) {
        self.cookie.zeroize();
        if let Some(auth) = &mut self.authorization {
            auth.zeroize();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCheck {
    SessionAvailable,
    AccountValidated,
    HomeRead,
    LibraryRead,
    SearchAccess,
    PlaylistAccess,
    PlaybackAvailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    Fail { reason: String },
    Pending,
}

/// `valid` denotes only synthetic technical account binding, never provider permission.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionValidationResult {
    pub valid: bool,
    pub checks: HashMap<SessionCheck, CheckOutcome>,
    pub account: Option<UserInfo>,
    pub validated_at: Option<DateTime<Utc>>,
    pub failure_reason: Option<String>,
}

impl SessionValidationResult {
    pub fn is_valid(&self) -> bool {
        self.valid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Disconnected,
    AvailableUnvalidated,
    Validating,
    Valid,
    Invalid,
}

#[derive(thiserror::Error, Debug)]
pub enum SessionError {
    #[error("No browser session context present")]
    NoSessionContext,
    #[error("Session validation failed: {0}")]
    ValidationFailed(String),
    #[error("Account readback failed: {0}")]
    AccountReadback(String),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("URL parse error: {0}")]
    UrlParse(#[from] url::ParseError),
    #[error("InnerTube rejected session: HTTP {0}")]
    InnerTubeRejected(u16),
    #[error("Playback availability not proven by session cookies")]
    PlaybackNotProven,
}

const POLICY_BLOCK: &str =
    "Provider InnerTube readback BLOCKED: express permission not established";
const MAX_CONTEXT_AGE_MINUTES: i64 = 10;

/// Only the loopback-only synthetic constructor can perform HTTP readback.
/// Production construction never sends credentials to an undocumented API.
pub struct SessionService {
    client: Client,
    base_url: Option<String>,
    context: BrowserSessionContext,
    expected_account_id: Option<String>,
    state: SessionState,
    last_validation: Option<SessionValidationResult>,
}

impl SessionService {
    pub fn with_context(context: BrowserSessionContext) -> Result<Self> {
        Self::build(context, None, None)
    }

    /// Legacy test entrypoint: without an independent intended account identity
    /// this cannot validate, even if a localhost server returns HTTP 200.
    #[doc(hidden)]
    pub fn with_base_url(context: BrowserSessionContext, base_url: String) -> Result<Self> {
        Self::build(context, None, Some(base_url))
    }

    /// For synthetic loopback tests only. Expected identity must come from a
    /// trusted, independent input; never derive it from a provider response.
    #[doc(hidden)]
    pub fn with_synthetic_base_url(
        context: BrowserSessionContext,
        expected_account_id: String,
        base_url: String,
    ) -> Result<Self> {
        if expected_account_id.trim().is_empty() {
            bail!("intended account identity required");
        }
        Self::build(context, Some(expected_account_id), Some(base_url))
    }

    fn build(
        context: BrowserSessionContext,
        expected_account_id: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self> {
        if let Some(base_url) = &base_url {
            let url = url::Url::parse(base_url)?;
            if url.scheme() != "http"
                || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
                || url.port().is_none()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!("synthetic validation requires an HTTP loopback origin with explicit port");
            }
        }
        let mut headers = HeaderMap::new();
        headers.insert("Accept", "application/json".parse()?);
        headers.insert("Content-Type", "application/json".parse()?);
        headers.insert("X-Goog-AuthUser", context.x_goog_auth_user.parse()?);
        headers.insert("x-origin", context.x_origin.parse()?);
        headers.insert("User-Agent", context.user_agent.parse()?);
        if let Some(auth) = &context.authorization {
            headers.insert("Authorization", auth.parse()?);
        }
        headers.insert("Cookie", context.cookie.parse()?);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            client,
            base_url,
            context,
            expected_account_id,
            state: SessionState::AvailableUnvalidated,
            last_validation: None,
        })
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn last_validation(&self) -> Option<&SessionValidationResult> {
        self.last_validation.as_ref()
    }

    pub fn is_valid(&self) -> bool {
        self.state == SessionState::Valid
            && self.is_session_available()
            && self.context.captured_at <= Utc::now() + chrono::Duration::minutes(1)
            && Utc::now().signed_duration_since(self.context.captured_at)
                <= chrono::Duration::minutes(MAX_CONTEXT_AGE_MINUTES)
            && self.last_validation.as_ref().is_some_and(|r| r.valid)
    }

    pub fn is_session_available(&self) -> bool {
        !self.context.cookie.trim().is_empty()
    }

    /// Cancel a validation future before calling this; `&mut self` means an
    /// active validation must be dropped before cancellation can borrow state.
    pub fn cancel(&mut self) {
        self.state = SessionState::Invalid;
        self.last_validation = None;
    }

    pub fn disconnect(&mut self) {
        self.cancel();
        self.client = Client::new();
        self.context.zeroize();
        self.context = BrowserSessionContext::default();
        self.expected_account_id = None;
        self.base_url = None;
        self.state = SessionState::Disconnected;
    }

    /// Reconnection requires fresh context and new independent account binding.
    #[doc(hidden)]
    pub fn reconnect_synthetic(
        &mut self,
        context: BrowserSessionContext,
        expected_account_id: String,
        base_url: String,
    ) -> Result<()> {
        self.disconnect();
        *self = Self::with_synthetic_base_url(context, expected_account_id, base_url)?;
        Ok(())
    }

    pub async fn validate(&mut self) -> Result<SessionValidationResult> {
        self.state = SessionState::Validating;
        let mut checks = HashMap::new();
        for check in [
            SessionCheck::AccountValidated,
            SessionCheck::HomeRead,
            SessionCheck::LibraryRead,
            SessionCheck::SearchAccess,
            SessionCheck::PlaylistAccess,
            SessionCheck::PlaybackAvailable,
        ] {
            checks.insert(check, CheckOutcome::Pending);
        }
        let now = Utc::now();
        let reason = if !self.is_session_available() {
            Some("no browser session context present")
        } else if self.context.captured_at > now + chrono::Duration::minutes(1)
            || now.signed_duration_since(self.context.captured_at)
                > chrono::Duration::minutes(MAX_CONTEXT_AGE_MINUTES)
        {
            Some("browser session context expired or captured in future")
        } else if self.base_url.is_none() {
            Some(POLICY_BLOCK)
        } else if self.expected_account_id.is_none() {
            Some("intended account identity missing")
        } else {
            None
        };
        checks.insert(
            SessionCheck::SessionAvailable,
            if reason == Some("no browser session context present") {
                CheckOutcome::Fail {
                    reason: "no browser session context present".into(),
                }
            } else {
                CheckOutcome::Pass
            },
        );

        let (account, failure_reason) = if let Some(reason) = reason {
            checks.insert(
                SessionCheck::AccountValidated,
                CheckOutcome::Fail {
                    reason: reason.into(),
                },
            );
            (None, Some(reason.to_string()))
        } else {
            let base_url = self.base_url.as_deref().expect("checked above");
            let body = serde_json::json!({
                "context": {"client": {"clientName": "SYNTHETIC_TEST_ONLY", "clientVersion": "0"}},
                "browseId": "FEmusic_library"
            });
            let readback = async {
                let response = self
                    .client
                    .post(format!("{base_url}/youtubei/v1/browse"))
                    .json(&body)
                    .send()
                    .await?;
                if !response.status().is_success() {
                    return Err(SessionError::InnerTubeRejected(response.status().as_u16()).into());
                }
                let value: serde_json::Value = response.json().await?;
                Ok::<_, anyhow::Error>(value)
            }
            .await;
            match readback {
                Ok(value) => {
                    let account = Self::extract_library_account(&value).filter(|account| {
                        Some(account.id.as_str()) == self.expected_account_id.as_deref()
                    });
                    if let Some(account) = account {
                        checks.insert(SessionCheck::LibraryRead, CheckOutcome::Pass);
                        checks.insert(SessionCheck::AccountValidated, CheckOutcome::Pass);
                        (Some(account), None)
                    } else {
                        let reason = "library identity missing or differs from intended account";
                        checks.insert(
                            SessionCheck::LibraryRead,
                            CheckOutcome::Fail {
                                reason: reason.into(),
                            },
                        );
                        checks.insert(
                            SessionCheck::AccountValidated,
                            CheckOutcome::Fail {
                                reason: reason.into(),
                            },
                        );
                        (None, Some(reason.into()))
                    }
                }
                Err(_) => {
                    let reason = "library readback rejected or unavailable";
                    checks.insert(
                        SessionCheck::LibraryRead,
                        CheckOutcome::Fail {
                            reason: reason.into(),
                        },
                    );
                    checks.insert(
                        SessionCheck::AccountValidated,
                        CheckOutcome::Fail {
                            reason: reason.into(),
                        },
                    );
                    (None, Some(reason.into()))
                }
            }
        };
        let valid = account.is_some();
        let result = SessionValidationResult {
            valid,
            checks,
            account,
            validated_at: valid.then_some(Utc::now()),
            failure_reason,
        };
        self.state = if valid {
            SessionState::Valid
        } else {
            SessionState::Invalid
        };
        self.last_validation = Some(result.clone());
        Ok(result)
    }

    fn extract_library_account(value: &serde_json::Value) -> Option<UserInfo> {
        if value.get("error").is_some()
            || value.pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                != Some(&serde_json::Value::Bool(false))
            || value
                .get("contents")
                .and_then(serde_json::Value::as_object)
                .is_none()
        {
            return None;
        }
        // This shape is a synthetic contract, NOT a verified provider schema.
        let user = value.pointer("/header/headerMusicLibraryUser")?;
        let id = user.get("userId")?.as_str()?.trim();
        if id.is_empty() {
            return None;
        }
        Some(UserInfo {
            id: id.into(),
            name: user
                .get("userName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .into(),
            email: String::new(),
            picture: None,
            verified_email: false,
        })
    }
}
