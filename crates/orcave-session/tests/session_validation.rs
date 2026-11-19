//! Synthetic-only tests. No provider calls, owner cookies, or live credentials.

use chrono::{Duration, Utc};
use orcave_session::{
    BrowserSessionContext, CheckOutcome, SessionCheck, SessionService, SessionState,
};
use serde_json::json;
use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

const INTENDED: &str = "owner-account-42";

fn context() -> BrowserSessionContext {
    BrowserSessionContext {
        cookie: "SID=synthetic-only".into(),
        x_goog_auth_user: "0".into(),
        x_origin: "https://music.youtube.com".into(),
        user_agent: "synthetic-test-client".into(),
        authorization: None,
        captured_at: Utc::now(),
    }
}

fn library(owner: &str) -> serde_json::Value {
    json!({
        "contents": {"sectionListRenderer": {"contents": []}},
        "responseContext": {"mainAppWebResponseContext": {"loggedOut": false}},
        "header": {"headerMusicLibraryUser": {"userId": owner, "userName": "Test Owner"}}
    })
}

async fn service(body: serde_json::Value, status: u16) -> (SessionService, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/browse"))
        .and(body_partial_json(json!({"browseId": "FEmusic_library"})))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    (
        SessionService::with_synthetic_base_url(context(), INTENDED.into(), server.uri()).unwrap(),
        server,
    )
}

#[tokio::test]
async fn library_binding_validates_only_intended_account_without_other_probes() {
    let (mut svc, server) = service(library(INTENDED), 200).await;
    let result = svc.validate().await.unwrap();
    assert!(result.valid);
    assert!(svc.is_valid());
    assert_eq!(svc.state(), SessionState::Valid);
    assert_eq!(result.account.unwrap().id, INTENDED);
    assert!(matches!(
        result.checks.get(&SessionCheck::PlaybackAvailable),
        Some(CheckOutcome::Pending)
    ));
    assert!(matches!(
        result.checks.get(&SessionCheck::PlaylistAccess),
        Some(CheckOutcome::Pending)
    ));
    assert!(matches!(
        result.checks.get(&SessionCheck::SearchAccess),
        Some(CheckOutcome::Pending)
    ));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/youtubei/v1/browse");
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().unwrap()["browseId"],
        "FEmusic_library"
    );
}

#[tokio::test]
async fn wrong_account_and_missing_identity_never_validate() {
    for body in [
        library("different-account"),
        json!({
            "contents": {"sectionListRenderer": {}},
            "responseContext": {"mainAppWebResponseContext": {"loggedOut": false}}
        }),
    ] {
        let (mut svc, _) = service(body, 200).await;
        let result = svc.validate().await.unwrap();
        assert!(!result.valid);
        assert!(result.account.is_none());
        assert!(matches!(
            result.checks.get(&SessionCheck::AccountValidated),
            Some(CheckOutcome::Fail { .. })
        ));
    }
}

#[tokio::test]
async fn expired_context_and_missing_context_never_send_requests() {
    let (_, server) = service(library(INTENDED), 200).await;
    // Reconstruct with expired capture: no provider request should be sent.
    let mut old = context();
    old.captured_at = Utc::now() - Duration::minutes(11);
    let mut expired =
        SessionService::with_synthetic_base_url(old, INTENDED.into(), server.uri()).unwrap();
    assert!(!expired.validate().await.unwrap().valid);
    let mut empty = context();
    empty.cookie.clear();
    let mut missing =
        SessionService::with_synthetic_base_url(empty, INTENDED.into(), server.uri()).unwrap();
    assert!(!missing.validate().await.unwrap().valid);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn provider_rejection_and_logged_out_body_fail_closed() {
    for (body, status) in [
        (library(INTENDED), 403),
        (
            json!({
                "contents": {"sectionListRenderer": {}},
                "responseContext": {"mainAppWebResponseContext": {"loggedOut": true}},
                "header": {"headerMusicLibraryUser": {"userId": INTENDED, "userName": "Test Owner"}}
            }),
            200,
        ),
    ] {
        let (mut svc, _) = service(body, status).await;
        assert!(!svc.validate().await.unwrap().valid);
        assert_eq!(svc.state(), SessionState::Invalid);
    }
}

#[tokio::test]
async fn disconnect_cancel_and_reconnect_do_not_reuse_validation() {
    let (mut svc, server) = service(library(INTENDED), 200).await;
    assert!(svc.validate().await.unwrap().valid);
    svc.cancel();
    assert!(!svc.is_valid());
    assert!(svc.last_validation().is_none());
    assert!(svc.validate().await.unwrap().valid);
    svc.disconnect();
    assert!(!svc.is_session_available());
    assert!(!svc.validate().await.unwrap().valid);
    svc.reconnect_synthetic(context(), INTENDED.into(), server.uri())
        .unwrap();
    assert!(!svc.is_valid());
    assert!(svc.validate().await.unwrap().valid);
}

#[tokio::test]
async fn production_constructor_is_blocked_even_with_synthetic_headers() {
    let mut svc = SessionService::with_context(context()).unwrap();
    let result = svc.validate().await.unwrap();
    assert!(!result.valid);
    assert_eq!(svc.state(), SessionState::Invalid);
    assert!(result
        .failure_reason
        .unwrap()
        .contains("express permission"));
}

#[tokio::test]
async fn legacy_mock_constructor_without_expected_identity_fails_closed() {
    let (svc, server) = service(library(INTENDED), 200).await;
    drop(svc);
    let mut legacy = SessionService::with_base_url(context(), server.uri()).unwrap();
    assert!(!legacy.validate().await.unwrap().valid);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn loopback_redirect_does_not_send_headers_to_another_origin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", "https://music.youtube.com/youtubei/v1/browse"),
        )
        .mount(&server)
        .await;
    let mut svc =
        SessionService::with_synthetic_base_url(context(), INTENDED.into(), server.uri()).unwrap();
    assert!(!svc.validate().await.unwrap().valid);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn synthetic_transport_rejects_non_loopback_target() {
    assert!(SessionService::with_synthetic_base_url(
        context(),
        INTENDED.into(),
        "https://music.youtube.com".into()
    )
    .is_err());
    assert!(SessionService::with_synthetic_base_url(
        context(),
        INTENDED.into(),
        "http://example.com".into()
    )
    .is_err());
}
