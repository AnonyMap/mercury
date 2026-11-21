//! Nous Portal sign-in: OAuth 2.0 device authorization grant (RFC 8628), the same endpoints
//! Hermes uses (`/api/oauth/device/code`, `/api/oauth/token`), token refresh through the
//! `x-nous-refresh-token` header, and an allowlist for the inference host the Portal hands back.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Paths, ProviderConfig, ProviderKind};

pub const DEFAULT_PORTAL_URL: &str = "https://portal.nousresearch.com";
pub const DEFAULT_INFERENCE_URL: &str = "https://inference-api.nousresearch.com/v1";
pub const DEFAULT_CLIENT_ID: &str = "hermes-cli";
pub const DEFAULT_SCOPE: &str = "inference:invoke";
pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Refresh this long before expiry.
pub const REFRESH_SKEW_SECS: i64 = 120;
const ALLOWED_INFERENCE_HOSTS: &[&str] = &["inference-api.nousresearch.com", "welcome-api.nousresearch.com"];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

impl DeviceCode {
    /// The URL to open: the complete one carries the code so the user doesn't have to type it.
    pub fn open_url(&self) -> &str {
        self.verification_uri_complete.as_deref().unwrap_or(&self.verification_uri)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NousAuth {
    pub portal_base_url: String,
    pub inference_base_url: String,
    pub client_id: String,
    pub scope: String,
    pub token_type: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub obtained_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl NousAuth {
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            Some(exp) => exp <= now + chrono::Duration::seconds(REFRESH_SKEW_SECS),
            None => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AuthStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub nous: Option<NousAuth>,
}

impl AuthStore {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Written atomically with mode 0600 — it holds bearer tokens.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&Self { version: 1, ..self.clone() })?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AuthError {
    #[error("sign-in was denied in the browser")]
    AccessDenied,
    #[error(
        "timed out waiting for approval. If the Portal showed a CAPTCHA or a subscription page, finish signing in at {0}/login in a normal browser tab and try again, or use a Nous Portal API key instead"
    )]
    Expired(String),
    #[error("sign-in cancelled")]
    Cancelled,
    #[error("Nous Portal rejected the request: {code}: {description}")]
    OAuth { code: String, description: String },
    #[error("your Nous Portal session has ended; sign in again")]
    ReloginRequired,
    #[error("Nous Portal is temporarily unavailable (HTTP {0}); credentials kept")]
    Unavailable(u16),
    #[error("not signed in to Nous Portal")]
    NotLoggedIn,
    #[error("network error: {0}")]
    Network(String),
}

/// Timings are injectable so tests don't sleep for real seconds.
#[derive(Debug, Clone, Copy)]
pub struct PollTiming {
    /// Hermes polls at most every 1 s regardless of the server's `interval`; so do we.
    pub interval_cap: Duration,
    pub max_edge_backoff: Duration,
}

impl Default for PollTiming {
    fn default() -> Self {
        Self { interval_cap: Duration::from_secs(1), max_edge_backoff: Duration::from_secs(60) }
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("Mercury/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

pub async fn request_device_code(portal: &str, client_id: &str, scope: &str) -> Result<DeviceCode> {
    let url = format!("{}/api/oauth/device/code", portal.trim_end_matches('/'));
    let mut form = vec![("client_id", client_id)];
    if !scope.is_empty() {
        form.push(("scope", scope));
    }
    let resp = http()
        .post(&url)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| anyhow!(AuthError::Network(e.to_string())))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("device code request failed (HTTP {status}): {}", crate::provider::truncate(&body, 300));
    }
    let dc: DeviceCode = serde_json::from_str(&body).context("device code response")?;
    Ok(dc)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

async fn sleep_or_cancel(d: Duration, cancel: &CancellationToken) -> Result<(), AuthError> {
    tokio::select! {
        _ = tokio::time::sleep(d) => Ok(()),
        _ = cancel.cancelled() => Err(AuthError::Cancelled),
    }
}

/// Poll the token endpoint until the user approves, denies, or the code expires.
pub async fn poll_for_token(
    portal: &str,
    client_id: &str,
    dc: &DeviceCode,
    timing: PollTiming,
    cancel: &CancellationToken,
) -> Result<Value, AuthError> {
    let portal = portal.trim_end_matches('/');
    let url = format!("{portal}/api/oauth/token");
    let client = http();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in.max(1));
    let mut interval = Duration::from_secs(dc.interval.max(1)).min(timing.interval_cap);
    let mut edge_backoff = Duration::ZERO;
    while tokio::time::Instant::now() < deadline {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let resp = client
            .post(&url)
            .header("Accept", "application/json")
            .form(&[("grant_type", DEVICE_CODE_GRANT), ("client_id", client_id), ("device_code", &dc.device_code)])
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => {
                // Transient network failure: the user may still be approving.
                sleep_or_cancel(interval.max(Duration::from_millis(10)), cancel).await?;
                continue;
            }
        };
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();
        if status == 200 {
            let v: Value = serde_json::from_str(&body).map_err(|e| AuthError::Network(e.to_string()))?;
            if v.get("access_token").and_then(Value::as_str).is_none() {
                return Err(AuthError::Network("token response missing access_token".into()));
            }
            return Ok(v);
        }
        match serde_json::from_str::<Value>(&body) {
            Ok(v) => {
                edge_backoff = Duration::ZERO;
                let code = v.get("error").and_then(Value::as_str).unwrap_or("").to_string();
                match code.as_str() {
                    "authorization_pending" => sleep_or_cancel(interval, cancel).await?,
                    "slow_down" => {
                        interval = (interval + Duration::from_secs(1)).min(Duration::from_secs(30));
                        sleep_or_cancel(interval, cancel).await?;
                    }
                    "access_denied" => return Err(AuthError::AccessDenied),
                    "expired_token" => return Err(AuthError::Expired(portal.into())),
                    _ => {
                        return Err(AuthError::OAuth {
                            description: v.get("error_description").and_then(Value::as_str).unwrap_or("unknown error").into(),
                            code: if code.is_empty() { format!("http_{status}") } else { code },
                        });
                    }
                }
            }
            Err(_) => {
                let edge = status == 408 || status == 429 || status >= 500 || (status == 403 && headers.contains_key("x-vercel-mitigated"));
                if !edge {
                    return Err(AuthError::OAuth { code: format!("http_{status}"), description: crate::provider::truncate(&body, 200) });
                }
                edge_backoff = match retry_after(&headers) {
                    Some(ra) => ra.max(interval),
                    None => (edge_backoff * 2).max(interval * 2),
                }
                .min(timing.max_edge_backoff);
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                sleep_or_cancel(edge_backoff.min(left), cancel).await?;
            }
        }
    }
    Err(AuthError::Expired(portal.into()))
}

/// Accept a network-supplied inference URL only for Nous's production hosts, or for the host the
/// user configured themselves. A tampered response must not be able to redirect the bearer token.
pub fn validate_inference_url(url: Option<&str>, user_override: Option<&str>) -> Option<String> {
    let cleaned = url?.trim();
    let parsed = url::Url::parse(cleaned).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let override_host = user_override.and_then(|o| url::Url::parse(o).ok()).and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let user_named = override_host.as_deref() == Some(host.as_str());
    let allowed = (parsed.scheme() == "https" && ALLOWED_INFERENCE_HOSTS.contains(&host.as_str())) || user_named;
    allowed.then(|| cleaned.trim_end_matches('/').to_string())
}

fn ttl_from(v: &Value) -> Option<i64> {
    match v.get("expires_in") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
    .filter(|&t| t > 0)
}

/// Build the stored state from a token response.
pub fn auth_from_token(
    token: &Value,
    portal: &str,
    client_id: &str,
    scope: &str,
    fallback_inference: &str,
    user_override: Option<&str>,
) -> NousAuth {
    let now = Utc::now();
    NousAuth {
        portal_base_url: portal.trim_end_matches('/').into(),
        inference_base_url: validate_inference_url(token.get("inference_base_url").and_then(Value::as_str), user_override)
            .unwrap_or_else(|| fallback_inference.trim_end_matches('/').to_string()),
        client_id: client_id.into(),
        scope: token.get("scope").and_then(Value::as_str).unwrap_or(scope).into(),
        token_type: token.get("token_type").and_then(Value::as_str).unwrap_or("Bearer").into(),
        access_token: token.get("access_token").and_then(Value::as_str).unwrap_or_default().into(),
        refresh_token: token.get("refresh_token").and_then(Value::as_str).map(str::to_string),
        obtained_at: now,
        expires_at: ttl_from(token).map(|t| now + chrono::Duration::seconds(t)),
    }
}

/// Exchange the refresh token for a new access token.
pub async fn refresh(state: &NousAuth, user_override: Option<&str>) -> Result<NousAuth, AuthError> {
    let Some(rt) = state.refresh_token.as_deref().filter(|t| !t.is_empty()) else {
        return Err(AuthError::ReloginRequired);
    };
    let url = format!("{}/api/oauth/token", state.portal_base_url.trim_end_matches('/'));
    let resp = http()
        .post(&url)
        .header("Accept", "application/json")
        .header("x-nous-refresh-token", rt)
        .form(&[("grant_type", "refresh_token"), ("client_id", state.client_id.as_str())])
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    let mitigated = resp.headers().contains_key("x-vercel-mitigated");
    let body = resp.text().await.unwrap_or_default();
    if status == 200 {
        let v: Value = serde_json::from_str(&body).map_err(|e| AuthError::Network(e.to_string()))?;
        if v.get("access_token").and_then(Value::as_str).is_none() {
            return Err(AuthError::ReloginRequired);
        }
        let mut next =
            auth_from_token(&v, &state.portal_base_url, &state.client_id, &state.scope, &state.inference_base_url, user_override);
        if next.refresh_token.is_none() {
            next.refresh_token = state.refresh_token.clone();
        }
        return Ok(next);
    }
    if status >= 500 || (mitigated && (status == 403 || status == 429)) {
        return Err(AuthError::Unavailable(status));
    }
    let code = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
    match (status, code.as_deref()) {
        (_, Some("invalid_grant" | "invalid_token" | "unauthorized_client")) | (401 | 403, _) => Err(AuthError::ReloginRequired),
        (_, Some(c)) => Err(AuthError::OAuth { code: c.into(), description: crate::provider::truncate(&body, 200) }),
        _ => Err(AuthError::Unavailable(status)),
    }
}

/// Run the whole sign-in: get a code, hand it to the UI, poll, store, and make sure a Nous
/// provider exists in the config. Returns the stored state.
pub async fn login(
    paths: &Paths,
    cfg: &mut Config,
    timing: PollTiming,
    cancel: &CancellationToken,
    on_code: impl FnOnce(&DeviceCode),
) -> Result<NousAuth> {
    let portal = cfg.nous.portal_url.clone();
    let client_id = cfg.nous.client_id.clone();
    let scope = cfg.nous.scope.clone();
    let dc = request_device_code(&portal, &client_id, &scope).await?;
    on_code(&dc);
    let token = poll_for_token(&portal, &client_id, &dc, timing, cancel).await?;
    let provider = ensure_nous_provider(cfg);
    let user_override = (provider.base_url != DEFAULT_INFERENCE_URL).then_some(provider.base_url.clone());
    let state = auth_from_token(&token, &portal, &client_id, &scope, &provider.base_url, user_override.as_deref());
    let mut store = AuthStore::load(&paths.auth_file())?;
    store.nous = Some(state.clone());
    store.save(&paths.auth_file())?;
    if cfg.active_provider.is_none() {
        cfg.active_provider = Some(provider.id.clone());
    }
    Ok(state)
}

pub fn logout(paths: &Paths) -> Result<()> {
    let mut store = AuthStore::load(&paths.auth_file())?;
    store.nous = None;
    store.save(&paths.auth_file())
}

/// Make sure the config has the OAuth provider entry; returns a copy of it.
pub fn ensure_nous_provider(cfg: &mut Config) -> ProviderConfig {
    if let Some(p) = cfg.providers.iter().find(|p| p.kind == ProviderKind::NousOauth) {
        return p.clone();
    }
    let p = ProviderConfig::nous_oauth();
    cfg.providers.push(p.clone());
    p
}

/// Resolve (base_url, bearer) for any provider, refreshing Nous tokens when they're about to expire.
pub async fn credentials(paths: &Paths, provider: &ProviderConfig) -> Result<(String, Option<String>)> {
    if provider.kind != ProviderKind::NousOauth {
        return Ok((provider.base_url.clone(), provider.resolved_api_key()));
    }
    let mut store = AuthStore::load(&paths.auth_file())?;
    let state = store.nous.clone().ok_or(AuthError::NotLoggedIn)?;
    let user_override = (provider.base_url != DEFAULT_INFERENCE_URL).then_some(provider.base_url.as_str());
    let state = if state.needs_refresh(Utc::now()) {
        match refresh(&state, user_override).await {
            Ok(next) => {
                store.nous = Some(next.clone());
                store.save(&paths.auth_file())?;
                next
            }
            Err(AuthError::ReloginRequired) => {
                store.nous = None;
                store.save(&paths.auth_file())?;
                return Err(AuthError::ReloginRequired.into());
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        state
    };
    // A user-configured inference URL (non-default) always wins; it's their explicit choice.
    let base = match user_override {
        Some(o) => o.to_string(),
        None => state.inference_base_url.clone(),
    };
    Ok((base, Some(state.access_token)))
}

pub fn status(paths: &Paths) -> Option<NousAuth> {
    AuthStore::load(&paths.auth_file()).ok().and_then(|s| s.nous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allowlist() {
        assert_eq!(
            validate_inference_url(Some("https://inference-api.nousresearch.com/v1/"), None).as_deref(),
            Some("https://inference-api.nousresearch.com/v1")
        );
        assert!(validate_inference_url(Some("http://inference-api.nousresearch.com/v1"), None).is_none());
        assert!(validate_inference_url(Some("https://evil.example.com/v1"), None).is_none());
        assert!(validate_inference_url(Some("https://inference-api.nousresearch.com.evil.io/v1"), None).is_none());
        assert!(validate_inference_url(None, None).is_none());
        // The user's own configured host is trusted even over http (dev / staging).
        assert_eq!(
            validate_inference_url(Some("http://127.0.0.1:9/v1"), Some("http://127.0.0.1:9/v1")).as_deref(),
            Some("http://127.0.0.1:9/v1")
        );
    }

    #[test]
    fn token_parsing_and_expiry() {
        let tok = json!({"access_token":"at","refresh_token":"rt","expires_in":"3600","inference_base_url":"https://evil.example/v1"});
        let a = auth_from_token(&tok, "https://p/", "cid", "inference:invoke", DEFAULT_INFERENCE_URL, None);
        assert_eq!(a.inference_base_url, DEFAULT_INFERENCE_URL);
        assert_eq!(a.portal_base_url, "https://p");
        assert!(!a.needs_refresh(Utc::now()));
        assert!(a.needs_refresh(Utc::now() + chrono::Duration::seconds(3600 - 60)));
        let no_exp = auth_from_token(&json!({"access_token":"x"}), "p", "c", "s", "u", None);
        assert!(no_exp.expires_at.is_none() && !no_exp.needs_refresh(Utc::now()));
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = AuthStore { version: 0, nous: Some(auth_from_token(&json!({"access_token":"x"}), "p", "c", "s", "u", None)) };
        store.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = AuthStore::load(&path).unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(back.nous.unwrap().access_token, "x");
    }
}
