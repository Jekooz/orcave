//! Offline contracts for the public auth value types.
//! OAuth HTTP and browser lifecycle tests need an injectable service harness;
//! do not send requests to the production OAuth or music endpoints here.

use std::time::{SystemTime, UNIX_EPOCH};

use orcave_auth::{AuthError, AuthState, Credentials};

fn synthetic_credentials(expires_at: u64) -> Credentials {
    Credentials {
        access_token: "synthetic-access-not-a-token".into(),
        refresh_token: "synthetic-refresh-not-a-token".into(),
        id_token: Some("synthetic-id-not-a-token".into()),
        expires_at,
        token_type: "Bearer".into(),
        scope: "https://www.googleapis.com/auth/youtube.readonly".into(),
    }
}

#[test]
fn credentials_expiring_within_five_minutes_are_unusable() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_secs();
    assert!(synthetic_credentials(now.saturating_sub(1)).is_expired());
    assert!(synthetic_credentials(now + 299).is_expired());
    assert!(!synthetic_credentials(now + 3600).is_expired());
}

#[test]
fn callback_failure_variants_have_distinct_secret_free_messages() {
    let errors = [
        (AuthError::CallbackTimeout, "OAuth callback timeout"),
        (AuthError::CallbackCancelled, "OAuth flow cancelled"),
        (AuthError::StateMismatch, "OAuth state mismatch"),
        (
            AuthError::CodeAlreadyConsumed,
            "OAuth authorization code already consumed",
        ),
        (AuthError::FlowInProgress, "OAuth flow already in progress"),
    ];
    for (error, expected) in errors {
        assert_eq!(error.to_string(), expected);
        assert!(!error.to_string().contains("synthetic-access-not-a-token"));
    }
}

#[test]
fn auth_state_exposes_expiry_without_exposing_credentials() {
    assert_eq!(
        serde_json::to_string(&AuthState::TokenExpired).unwrap(),
        r#""token_expired""#
    );
    assert_eq!(
        serde_json::to_string(&AuthState::Unauthenticated).unwrap(),
        r#""unauthenticated""#
    );
}
