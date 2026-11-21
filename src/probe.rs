//! "Test connection" for a provider: can we list models, does a tiny chat request work, how long
//! did it take, and what to change when it doesn't.

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::config::{Config, ProviderConfig};
use crate::provider::{ChatRequest, Client, Message, ProviderError};

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    /// Chat models the server listed (`None` if listing failed).
    pub models: Option<usize>,
    pub model_error: Option<String>,
    /// The model the chat check used.
    pub model: String,
    pub chat_ok: bool,
    pub chat_error: Option<String>,
    pub latency: Option<Duration>,
    /// What to try, when something failed.
    pub hint: Option<String>,
}

impl ProbeReport {
    pub fn ok(&self) -> bool {
        self.chat_ok
    }

    /// One or two lines for a toast or the terminal.
    pub fn summary(&self) -> String {
        let mut parts = vec![];
        match (self.models, &self.model_error) {
            (Some(n), _) => parts.push(format!("{n} chat models listed")),
            (None, Some(e)) => parts.push(format!("model list failed: {e}")),
            _ => {}
        }
        if self.chat_ok {
            let ms = self.latency.map(|d| format!(" in {:.1} s", d.as_secs_f32())).unwrap_or_default();
            parts.push(format!("{} replied{ms}", self.model));
        } else if let Some(e) = &self.chat_error {
            parts.push(format!("chat failed: {e}"));
        }
        let mut s = format!("{} {}", if self.chat_ok { "OK:" } else { "Problem:" }, parts.join(" · "));
        if let Some(h) = &self.hint {
            s.push_str(&format!("\n{h}"));
        }
        s
    }
}

/// Probe `provider`. `model` overrides its default model.
pub async fn test_provider(paths: &crate::config::Paths, cfg: &Config, provider: &ProviderConfig, model: Option<&str>) -> ProbeReport {
    let (base, key) = match crate::nous::credentials(paths, provider).await {
        Ok(v) => v,
        Err(e) => {
            return ProbeReport {
                models: None,
                model_error: None,
                model: String::new(),
                chat_ok: false,
                chat_error: Some(format!("{e:#}")),
                latency: None,
                hint: Some("Sign in to Nous Portal (Settings > Nous Portal), or check the API key.".into()),
            };
        }
    };
    let mut opts = provider.client_options(&cfg.agent);
    // A test should answer quickly and not sit through back-off.
    opts.retries = 0;
    opts.first_token_timeout = Duration::from_secs(90);
    let client = Client::with_options(&base, key.clone(), opts);
    probe(&client, provider, model, key.is_some()).await
}

pub async fn probe(client: &Client, provider: &ProviderConfig, model: Option<&str>, has_key: bool) -> ProbeReport {
    let (models, model_error, listed) = match client.list_models().await {
        Ok(list) => (Some(list.len()), None, list),
        Err(e) => (None, Some(e.to_string()), vec![]),
    };
    let model = model
        .map(str::to_string)
        .filter(|m| !m.is_empty())
        .or_else(|| Some(provider.default_model.clone()).filter(|m| !m.is_empty()))
        .or_else(|| listed.first().cloned())
        .unwrap_or_default();
    let mut r = ProbeReport { models, model_error, model: model.clone(), chat_ok: false, chat_error: None, latency: None, hint: None };
    if model.is_empty() {
        r.chat_error = Some("no model to try (none listed, none set)".into());
        r.hint = hint_for(client.base_url(), r.model_error.as_deref(), None, has_key);
        return r;
    }
    let req = ChatRequest { model, messages: vec![Message::user("Reply with the single word: OK")], tools: vec![], temperature: Some(0.0) };
    let start = Instant::now();
    let cancel = CancellationToken::new();
    match client.chat_stream(&req, &cancel, &mut |_| {}).await {
        Ok(_) => {
            r.chat_ok = true;
            r.latency = Some(start.elapsed());
        }
        Err(e) => {
            r.hint = hint_for(client.base_url(), r.model_error.as_deref(), Some(&e), has_key);
            r.chat_error = Some(crate::provider::truncate(&e.to_string(), 300));
        }
    }
    r
}

fn hint_for(base: &str, list_err: Option<&str>, chat_err: Option<&ProviderError>, has_key: bool) -> Option<String> {
    let status = match chat_err {
        Some(ProviderError::Http { status, .. }) => Some(*status),
        _ => None,
    };
    let list_404 = list_err.is_some_and(|e| e.contains("HTTP 404"));
    if matches!(status, Some(401 | 403)) || list_err.is_some_and(|e| e.contains("HTTP 401") || e.contains("HTTP 403")) {
        return Some(if has_key {
            "The server rejected the API key. Check it, and that it's for this service.".into()
        } else {
            "This server needs an API key: add one (or the name of the variable that holds it).".into()
        });
    }
    if base.contains("deepinfra.com") && !base.contains("/v1/openai") {
        return Some("DeepInfra's OpenAI-compatible address is https://api.deepinfra.com/v1/openai".into());
    }
    if status == Some(404) || list_404 {
        if !base.trim_end_matches('/').ends_with("/v1") && !base.contains("/v1/") {
            return Some(format!("Most servers expect the address to end in /v1 — try {}/v1", base.trim_end_matches('/')));
        }
        return Some("The server doesn't know that model, or this isn't its OpenAI-compatible address.".into());
    }
    if matches!(chat_err, Some(ProviderError::Connect { .. })) {
        return Some(if crate::config::is_local_url(base) {
            "Nothing answered. Is the server running, and listening on this address and port?".into()
        } else {
            "Couldn't connect. Check the address and your internet connection.".into()
        });
    }
    if matches!(status, Some(429)) {
        return Some("The service is rate-limiting or overloaded right now; Mercury retries automatically during chats.".into());
    }
    if matches!(chat_err, Some(ProviderError::Stalled(_))) {
        return Some("The server accepted the request but didn't answer; a local model may still be loading.".into());
    }
    None
}
