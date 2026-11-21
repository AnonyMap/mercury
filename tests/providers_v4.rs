//! v0.4: hosted-provider behaviour (DeepInfra-shaped model lists, retries, broken and stalled
//! streams, request options, images) and the connection test.

mod common;

use std::time::Duration;

use common::{Reply, start, text};
use mercury::agent::Agent;
use mercury::config::ProviderConfig;
use mercury::provider::{ChatRequest, Client, ClientOptions, Message, ProviderError, StreamDelta};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fast() -> ClientOptions {
    ClientOptions { retry_base: Duration::from_millis(5), ..Default::default() }
}

fn req(msgs: Vec<Message>) -> ChatRequest {
    ChatRequest { model: "test-model".into(), messages: msgs, tools: vec![], temperature: None }
}

#[tokio::test]
async fn deepinfra_style_model_list_is_filtered_and_annotated() {
    let m = start().await;
    *m.state.models_json.lock().unwrap() = Some(json!({"object": "list", "data": [
        {"id": "deepseek-ai/DeepSeek-V4-Flash", "object": "model", "created": 1, "owned_by": "deepinfra", "root": "x",
         "metadata": {"description": "", "pricing": {"input_tokens": 0.1}, "tags": ["chat", "reasoning_effort", "prompt_cache"], "context_length": 163840}},
        {"id": "Qwen/Qwen3-VL-32B", "metadata": {"description": "", "pricing": {}, "tags": ["chat", "vision"], "context_length": 131072}},
        {"id": "black-forest-labs/FLUX-2", "metadata": {"description": "", "pricing": {}, "tags": ["image-gen"]}},
        {"id": "BAAI/bge-m3", "metadata": {"description": "", "pricing": {}, "tags": ["embeddings"]}},
        {"id": "hexgrad/Kokoro-82M", "metadata": {"description": "", "pricing": {}, "tags": ["tts"]}},
        {"id": "plain-model-without-metadata", "metadata": null}
    ]}));
    let c = Client::new(&m.base(), Some("k".into()));
    let list = c.list_model_info().await.unwrap();
    let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["Qwen/Qwen3-VL-32B", "deepseek-ai/DeepSeek-V4-Flash", "plain-model-without-metadata"]);
    assert_eq!(list[1].context_length, Some(163840));
    assert!(list[1].reasoning() && !list[1].vision());
    assert!(list[0].vision());
    assert_eq!(c.list_models().await.unwrap().len(), 3);
}

#[tokio::test]
async fn busy_servers_are_retried_until_they_answer() {
    let m = start().await;
    m.push_chat(Reply::Raw(
        429,
        r#"{"error":{"code":"engine_overloaded","message":"overloaded"}}"#.into(),
        vec![("retry-after".into(), "0".into())],
    ));
    m.push_chat(Reply::Status(503, "upstream unavailable".into()));
    m.push_chat(text("finally here"));
    let c = Client::with_options(&m.base(), None, fast());
    let mut retries = vec![];
    let done = c
        .chat_stream(&req(vec![Message::user("hi")]), &CancellationToken::new(), &mut |d| {
            if let StreamDelta::Retrying { attempt, reason, .. } = d {
                retries.push((attempt, reason));
            }
        })
        .await
        .unwrap();
    assert_eq!(done.content, "finally here");
    assert_eq!(retries.len(), 2);
    assert_eq!(retries[0], (1, "the model is overloaded".to_string()));
    assert_eq!(m.requests().len(), 3);
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    let m = start().await;
    m.push_chat(Reply::Status(400, r#"{"error":"bad model"}"#.into()));
    let c = Client::with_options(&m.base(), None, fast());
    let e = c.chat_stream(&req(vec![Message::user("hi")]), &CancellationToken::new(), &mut |_| {}).await.unwrap_err();
    assert!(matches!(e, ProviderError::Http { status: 400, .. }));
    assert_eq!(m.requests().len(), 1);

    // Giving up after the configured number of retries.
    for _ in 0..3 {
        m.push_chat(Reply::Status(502, "bad gateway".into()));
    }
    let c = Client::with_options(&m.base(), None, ClientOptions { retries: 2, ..fast() });
    let e = c.chat_stream(&req(vec![Message::user("hi")]), &CancellationToken::new(), &mut |_| {}).await.unwrap_err();
    assert!(matches!(e, ProviderError::Http { status: 502, .. }));
    assert_eq!(m.requests().len(), 4, "1 + the first try and 2 retries");
}

#[tokio::test]
async fn a_broken_stream_keeps_what_arrived_and_is_not_retried() {
    let m = start().await;
    m.push_chat(Reply::SseThenDrop(vec![
        json!({"choices":[{"delta":{"content":"The first half "}}]}).to_string(),
        json!({"choices":[{"delta":{"content":"of the answer"}}]}).to_string(),
    ]));
    m.push_chat(text("should not be requested"));
    let c = Client::with_options(&m.base(), None, fast());
    let e = c.chat_stream(&req(vec![Message::user("hi")]), &CancellationToken::new(), &mut |_| {}).await.unwrap_err();
    match e {
        ProviderError::Interrupted { partial, .. } => assert_eq!(partial.content, "The first half of the answer"),
        other => panic!("expected Interrupted, got {other:?}"),
    }
    assert_eq!(m.requests().len(), 1, "output was already shown, so no silent retry");
}

#[tokio::test]
async fn stalls_are_detected_before_and_after_output() {
    let m = start().await;
    // Silent from the start: Stalled (and retried once, then given up).
    m.push_chat(Reply::SseThenStall(vec![json!({"choices":[{"delta":{"content":"late"}}]}).to_string()], 3000));
    m.push_chat(Reply::SseThenStall(vec![json!({"choices":[{"delta":{"content":"late"}}]}).to_string()], 3000));
    let opts = ClientOptions { retries: 1, first_token_timeout: Duration::from_millis(400), ..fast() };
    let c = Client::with_options(&m.base(), None, opts.clone());
    let t = std::time::Instant::now();
    let e = c.chat_stream(&req(vec![Message::user("hi")]), &CancellationToken::new(), &mut |_| {}).await.unwrap_err();
    assert!(matches!(e, ProviderError::Stalled(_)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(m.requests().len(), 2);

    // Goes quiet mid-reply: Interrupted with the text so far.
    m.push_chat(Reply::SseThenStall(
        vec![
            json!({"choices":[{"delta":{"content":"partial "}}]}).to_string(),
            json!({"choices":[{"delta":{"content":"late"}}]}).to_string(),
        ],
        3000,
    ));
    let c = Client::with_options(&m.base(), None, ClientOptions { idle_timeout: Duration::from_millis(400), ..opts });
    let e = c.chat_stream(&req(vec![Message::user("hi")]), &CancellationToken::new(), &mut |_| {}).await.unwrap_err();
    assert!(matches!(&e, ProviderError::Interrupted { partial, .. } if partial.content == "partial "), "{e:?}");
}

#[tokio::test]
async fn provider_options_reach_the_server() {
    let m = start().await;
    m.push_chat(text("ok"));
    let mut p = ProviderConfig::openai("deepinfra", "DeepInfra", &m.base());
    p.reasoning_effort = Some("low".into());
    p.headers.insert("X-Title".into(), "Mercury".into());
    p.extra_body = ProviderConfig::parse_extra_body(r#"{"service_tier": "flex", "top_k": 40}"#).unwrap();
    let c = Client::with_options(&m.base(), Some("sk-test".into()), p.client_options(&Default::default()));
    let img = "data:image/png;base64,AAAA".to_string();
    c.chat_stream(&req(vec![Message::user_with_images("what is this?", vec![img.clone()])]), &CancellationToken::new(), &mut |_| {})
        .await
        .unwrap();
    let body = &m.requests()[0];
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["service_tier"], "flex");
    assert_eq!(body["top_k"], 40);
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0], json!({"type": "text", "text": "what is this?"}));
    assert_eq!(content[1], json!({"type": "image_url", "image_url": {"url": img}}));
    let h = &m.state.chat_headers.lock().unwrap()[0];
    assert_eq!(h["x-title"], "Mercury");
    assert_eq!(h["authorization"], "Bearer sk-test");
}

#[tokio::test]
async fn agent_keeps_cut_off_replies_and_continue_resumes() {
    let m = start().await;
    let (h, cfg) = common::harness(&m.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    m.push_chat(Reply::SseThenDrop(vec![json!({"choices":[{"delta":{"content":"Step one is "}}]}).to_string()]));
    let (agent, out) = common::turn(agent, "explain", true, None).await;
    assert!(out.errors.iter().any(|e| e.contains("cut off") && e.contains("/continue")), "{:?}", out.errors);
    assert_eq!(agent.messages.last().unwrap().text(), "Step one is ", "the partial reply is saved");

    // Output-limit truncation: a notice pointing at /continue.
    m.push_chat(Reply::Sse(
        vec![
            json!({"choices":[{"delta":{"content":"a very long"}}]}).to_string(),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}).to_string(),
            "[DONE]".into(),
        ],
        0,
    ));
    let (mut agent, out) = common::turn(agent, "go on", true, None).await;
    assert!(out.notices.iter().any(|n| n.contains("output limit")), "{:?}", out.notices);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = agent.handle_command(mercury::commands::Command::Continue, &tx, &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome, mercury::agent::CommandOutcome::Send(t) if t.starts_with("Continue exactly")));
}

#[tokio::test]
async fn agent_retries_transparently_and_learns_context_windows() {
    let m = start().await;
    *m.state.models_json.lock().unwrap() = Some(
        json!({"data": [{"id": "test-model", "metadata": {"tags": ["chat"], "context_length": 65536, "description": "", "pricing": {}}}]}),
    );
    let (h, cfg) = common::harness(&m.base());
    let mut agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    assert_eq!(agent.context_window(), 32_768, "global default until the server says otherwise");
    agent.list_models().await.unwrap();
    assert_eq!(agent.context_window(), 65_536);

    m.push_chat(Reply::Status(429, "rate limited".into()));
    m.push_chat(text("hello after a retry"));
    let (_agent, out) = common::turn(agent, "hi", true, None).await;
    assert_eq!(out.text, "hello after a retry");
    assert!(out.notices.iter().any(|n| n.contains("rate limited; trying again")), "{:?}", out.notices);
    assert!(out.errors.is_empty());
}

#[tokio::test]
async fn retry_keeps_attached_images() {
    let m = start().await;
    let (h, cfg) = common::harness(&m.base());
    let mut agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    agent.pending_images = vec!["data:image/png;base64,AAAA".into()];
    m.push_chat(text("a cat"));
    let (mut agent, _) = common::turn(agent, "what is it", true, None).await;
    assert_eq!(agent.messages[0].images.len(), 1, "stored with the message");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = agent.handle_command(mercury::commands::Command::Retry, &tx, &CancellationToken::new()).await.unwrap();
    assert!(matches!(outcome, mercury::agent::CommandOutcome::Send(_)));
    m.push_chat(text("still a cat"));
    let (_agent, _) = common::turn(agent, "what is it", true, None).await;
    let last = m.requests().last().cloned().unwrap();
    let user = last["messages"].as_array().unwrap().iter().rfind(|x| x["role"] == "user").unwrap().clone();
    assert_eq!(user["content"][1]["type"], "image_url", "the retried message still carries its image");
}

#[tokio::test]
async fn connection_test_reports_and_explains() {
    let m = start().await;
    let d = tempfile::tempdir().unwrap();
    let paths = mercury::config::Paths::under(d.path().to_path_buf());
    let cfg = mercury::config::Config::default();

    m.push_chat(text("OK"));
    let p = ProviderConfig::openai("mock", "Mock", &m.base());
    let r = mercury::probe::test_provider(&paths, &cfg, &p, None).await;
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.models, Some(2));
    assert_eq!(r.model, "other-model", "first listed model when none is set");
    assert!(r.summary().starts_with("OK: 2 chat models listed · other-model replied in"));

    m.push_chat(Reply::Status(401, r#"{"error":"invalid api key"}"#.into()));
    let r = mercury::probe::test_provider(&paths, &cfg, &p, Some("test-model")).await;
    assert!(!r.ok());
    assert!(r.hint.unwrap().contains("needs an API key"));

    // Missing /v1: both endpoints 404 and the hint says what to add.
    let p = ProviderConfig::openai("bare", "Bare", &m.url);
    let r = mercury::probe::test_provider(&paths, &cfg, &p, Some("x")).await;
    assert!(r.hint.as_deref().unwrap_or("").contains("end in /v1"), "{r:?}");

    // DeepInfra's root URL instead of its OpenAI-compatible one.
    let p = ProviderConfig::openai("di", "DeepInfra", "http://api.deepinfra.com.invalid/v1");
    let r =
        mercury::probe::probe(&Client::with_options(&p.base_url, None, ClientOptions { retries: 0, ..fast() }), &p, Some("m"), true).await;
    assert!(r.hint.as_deref().unwrap_or("").contains("/v1/openai"), "{r:?}");
}

#[test]
fn deepinfra_preset_and_option_parsing() {
    let p = mercury::config::presets().into_iter().find(|p| p.id == "deepinfra").unwrap();
    assert_eq!(p.base_url, "https://api.deepinfra.com/v1/openai");
    assert_eq!(p.api_key_env.as_deref(), Some("DEEPINFRA_API_KEY"));
    assert!(ProviderConfig::parse_extra_body("[1]").is_err());
    assert!(ProviderConfig::parse_extra_body(r#"{"a": null}"#).unwrap_err().contains("null"));
    assert!(ProviderConfig::parse_extra_body("  ").unwrap().is_empty());
    let h = ProviderConfig::parse_headers("HTTP-Referer: https://x.dev\n\nX-Title:  Mercury ").unwrap();
    assert_eq!(h["X-Title"], "Mercury");
    assert!(ProviderConfig::parse_headers("Authorization: Bearer x").is_err());
    assert!(ProviderConfig::parse_headers("no colon").is_err());
    // Settings round-trip through TOML, including nested JSON values.
    let mut cfg = mercury::config::Config::default();
    let mut di = p.clone();
    di.extra_body = ProviderConfig::parse_extra_body(r#"{"service_tier":"flex","reasoning":{"enabled":false}}"#).unwrap();
    cfg.providers.push(di);
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("c.toml");
    cfg.save(&f).unwrap();
    assert_eq!(mercury::config::Config::load(&f).unwrap(), cfg);
}
