//! Nous Portal device-code login, refresh and inference against a mock Portal.

mod common;

use std::time::Duration;

use chrono::Utc;
use common::*;
use mercury::config::{Config, Paths, ProviderKind};
use mercury::nous::{self, AuthError, AuthStore, PollTiming};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fast() -> PollTiming {
    PollTiming { interval_cap: Duration::from_millis(20), max_edge_backoff: Duration::from_millis(50) }
}

fn setup(mock: &Mock) -> (tempfile::TempDir, Paths, Config) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path().to_path_buf());
    let mut cfg = Config::default();
    cfg.nous.portal_url = mock.url.clone();
    (dir, paths, cfg)
}

#[tokio::test]
async fn device_login_happy_path() {
    let mock = start().await;
    mock.push_token(Reply::Json(400, json!({"error": "authorization_pending"})));
    mock.push_token(Reply::Json(400, json!({"error": "slow_down"})));
    mock.push_token(Reply::Json(
        200,
        json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600, "token_type": "Bearer",
            "scope": "inference:invoke", "inference_base_url": "https://attacker.example/v1"
        }),
    ));
    let (_d, paths, mut cfg) = setup(&mock);
    let mut shown = None;
    let state = nous::login(&paths, &mut cfg, fast(), &CancellationToken::new(), |dc| shown = Some(dc.clone())).await.unwrap();

    let dc = shown.unwrap();
    assert_eq!(dc.user_code, "ABCD-EFGH");
    assert_eq!(dc.open_url(), "https://portal.example/device?user_code=ABCD-EFGH");
    let dev = mock.state.device_requests.lock().unwrap()[0].clone();
    assert!(dev.contains(&("client_id".into(), "hermes-cli".into())));
    assert!(dev.contains(&("scope".into(), "inference:invoke".into())));

    let polls = mock.state.token_requests.lock().unwrap().clone();
    assert_eq!(polls.len(), 3);
    assert!(polls[0].0.contains(&("grant_type".into(), "urn:ietf:params:oauth:grant-type:device_code".into())));
    assert!(polls[0].0.contains(&("device_code".into(), "dev-123".into())));

    // A hostile inference URL from the network is ignored in favour of the default.
    assert_eq!(state.inference_base_url, nous::DEFAULT_INFERENCE_URL);
    assert_eq!(state.access_token, "at-1");
    let stored = AuthStore::load(&paths.auth_file()).unwrap().nous.unwrap();
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-1"));
    assert!(cfg.providers.iter().any(|p| p.kind == ProviderKind::NousOauth));
    assert_eq!(cfg.active_provider.as_deref(), Some("nous"));
}

#[tokio::test]
async fn device_login_denied_and_expired() {
    let mock = start().await;
    mock.push_token(Reply::Json(400, json!({"error": "access_denied"})));
    let (_d, paths, mut cfg) = setup(&mock);
    let err = nous::login(&paths, &mut cfg, fast(), &CancellationToken::new(), |_| {}).await.unwrap_err();
    assert_eq!(err.downcast_ref::<AuthError>(), Some(&AuthError::AccessDenied));

    // Never approved: the 5 s device code runs out and the message points at the Portal.
    let (_d, paths, mut cfg) = setup(&mock);
    let err = nous::login(&paths, &mut cfg, fast(), &CancellationToken::new(), |_| {}).await.unwrap_err();
    let e = err.downcast_ref::<AuthError>().unwrap();
    assert!(matches!(e, AuthError::Expired(_)));
    assert!(e.to_string().contains("/login") && e.to_string().contains("API key"));
}

#[tokio::test]
async fn device_login_survives_edge_firewall() {
    let mock = start().await;
    mock.push_token(Reply::Raw(
        403,
        "<html>Vercel Security Checkpoint</html>".into(),
        vec![("x-vercel-mitigated".into(), "challenge".into())],
    ));
    mock.push_token(Reply::Raw(503, "upstream down".into(), vec![]));
    mock.push_token(Reply::Json(200, json!({"access_token": "at-edge", "expires_in": 600})));
    let (_d, paths, mut cfg) = setup(&mock);
    let s = nous::login(&paths, &mut cfg, fast(), &CancellationToken::new(), |_| {}).await.unwrap();
    assert_eq!(s.access_token, "at-edge");
}

#[tokio::test]
async fn device_login_cancel() {
    let mock = start().await;
    let (_d, paths, mut cfg) = setup(&mock);
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let err = nous::login(&paths, &mut cfg, fast(), &cancel, |_| {}).await.unwrap_err();
    assert_eq!(err.downcast_ref::<AuthError>(), Some(&AuthError::Cancelled));
}

fn expired_state(portal: &str) -> nous::NousAuth {
    let mut s = nous::auth_from_token(
        &json!({"access_token": "old", "refresh_token": "rt-old", "expires_in": 60}),
        portal,
        "hermes-cli",
        "inference:invoke",
        nous::DEFAULT_INFERENCE_URL,
        None,
    );
    s.expires_at = Some(Utc::now() + chrono::Duration::seconds(30)); // inside the 120 s skew
    s
}

#[tokio::test]
async fn credentials_refresh_expiring_token_with_header() {
    let mock = start().await;
    mock.push_token(Reply::Json(200, json!({"access_token": "fresh", "expires_in": 3600})));
    let (_d, paths, mut cfg) = setup(&mock);
    let provider = nous::ensure_nous_provider(&mut cfg);
    AuthStore { version: 1, nous: Some(expired_state(&mock.url)) }.save(&paths.auth_file()).unwrap();

    let (base, key) = nous::credentials(&paths, &provider).await.unwrap();
    assert_eq!(base, nous::DEFAULT_INFERENCE_URL);
    assert_eq!(key.as_deref(), Some("fresh"));
    let (form, headers) = mock.state.token_requests.lock().unwrap()[0].clone();
    assert_eq!(headers.get("x-nous-refresh-token").unwrap(), "rt-old");
    assert!(form.contains(&("grant_type".into(), "refresh_token".into())));
    // The Portal didn't rotate the refresh token, so the old one is kept.
    let stored = AuthStore::load(&paths.auth_file()).unwrap().nous.unwrap();
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-old"));
    assert_eq!(stored.access_token, "fresh");
}

#[tokio::test]
async fn dead_refresh_token_signs_out_but_outage_keeps_credentials() {
    let mock = start().await;
    mock.push_token(Reply::Raw(503, "down".into(), vec![]));
    mock.push_token(Reply::Json(400, json!({"error": "invalid_grant"})));
    let (_d, paths, mut cfg) = setup(&mock);
    let provider = nous::ensure_nous_provider(&mut cfg);
    AuthStore { version: 1, nous: Some(expired_state(&mock.url)) }.save(&paths.auth_file()).unwrap();

    let e = nous::credentials(&paths, &provider).await.unwrap_err();
    assert_eq!(e.downcast_ref::<AuthError>(), Some(&AuthError::Unavailable(503)));
    assert!(nous::status(&paths).is_some(), "outage must not sign the user out");

    let e = nous::credentials(&paths, &provider).await.unwrap_err();
    assert_eq!(e.downcast_ref::<AuthError>(), Some(&AuthError::ReloginRequired));
    assert!(nous::status(&paths).is_none());
}

#[tokio::test]
async fn chat_through_nous_provider_uses_bearer_token() {
    let mock = start().await;
    mock.push_chat(text("hi from hermes"));
    let (h, mut cfg) = harness("http://unused/v1");
    cfg.nous.portal_url = mock.url.clone();
    let mut p = nous::ensure_nous_provider(&mut cfg);
    // A user-configured inference URL (staging / proxy) is honoured.
    p.base_url = mock.base();
    p.default_model = "Hermes-4-70B".into();
    cfg.upsert_provider(p);
    cfg.set_active("nous", Some("Hermes-4-70B".into()));
    let mut state = nous::auth_from_token(
        &json!({"access_token": "nous-at", "expires_in": 3600}),
        &mock.url,
        "hermes-cli",
        "inference:invoke",
        &mock.base(),
        None,
    );
    state.refresh_token = Some("rt".into());
    AuthStore { version: 1, nous: Some(state) }.save(&h.paths.auth_file()).unwrap();

    let agent = mercury::agent::Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "hello", true, None).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.text, "hi from hermes");
    let headers = mock.state.chat_headers.lock().unwrap()[0].clone();
    assert_eq!(headers.get("authorization").unwrap(), "Bearer nous-at");
    assert_eq!(mock.requests()[0]["model"], "Hermes-4-70B");
}

#[tokio::test]
async fn not_logged_in_is_reported() {
    let (h, mut cfg) = harness("http://unused/v1");
    nous::ensure_nous_provider(&mut cfg);
    cfg.set_active("nous", Some("Hermes-4-70B".into()));
    let agent = mercury::agent::Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "hello", true, None).await;
    assert!(out.errors[0].contains("not signed in to Nous Portal"), "{:?}", out.errors);
}
