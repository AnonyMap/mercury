//! End-to-end agent tests against a scripted OpenAI-compatible server.

mod common;

use std::time::Duration;

use common::*;
use mercury::agent::{Agent, CommandOutcome};
use mercury::commands::{self, Command};
use mercury::config::ApprovalMode;
use mercury::provider::Role;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn plain_streaming_answer_is_persisted() {
    let mock = start().await;
    mock.push_chat(text("Hello from the mock model."));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, out) = turn(agent, "Say hello", true, None).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.text, "Hello from the mock model.");
    assert_eq!(agent.usage.total_tokens, 15);

    let req = &mock.requests()[0];
    assert_eq!(req["model"], "test-model");
    assert_eq!(req["stream"], true);
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][1]["content"], "Say hello");
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "terminal"));

    // Reopen from disk: the session, its title and both messages survive.
    let again = Agent::new(h.paths.clone(), agent.config.clone(), Some(&agent.session.id)).unwrap();
    assert_eq!(again.messages.len(), 2);
    assert_eq!(again.session.title, "Say hello");
    assert_eq!(again.session.usage.total_tokens, 15);
}

#[tokio::test]
async fn tool_round_trip_writes_file_and_returns_result() {
    let mock = start().await;
    mock.push_chat(tool_call("call_w1", "write_file", json!({"path": "notes/todo.md", "content": "- ship mercury\n"})));
    mock.push_chat(text("Done, I wrote the file."));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, out) = turn(agent, "write a todo", true, None).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(std::fs::read_to_string(h.work.join("notes/todo.md")).unwrap(), "- ship mercury\n");
    assert_eq!(out.tools.len(), 1);
    assert!(!out.tools[0].2);

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let msgs = reqs[1]["messages"].as_array().unwrap();
    let assistant = &msgs[2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_w1");
    assert_eq!(assistant["content"], serde_json::Value::Null);
    let tool = &msgs[3];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_w1");
    assert!(tool["content"].as_str().unwrap().starts_with("Created"));
    // user, assistant(tool call), tool, assistant(final)
    assert_eq!(agent.messages.iter().map(|m| m.role).collect::<Vec<_>>(), vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
}

#[tokio::test]
async fn dangerous_command_denied_is_not_run() {
    let mock = start().await;
    mock.push_chat(tool_call("c1", "terminal", json!({"command": "rm -rf precious"})));
    mock.push_chat(text("Okay, I won't."));
    let (h, cfg) = harness(&mock.base());
    std::fs::create_dir_all(h.work.join("precious")).unwrap();
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_agent, out) = turn(agent, "clean up", false, None).await;
    assert_eq!(out.approvals, vec!["rm -rf precious"]);
    assert!(h.work.join("precious").exists(), "denied command must not run");
    assert!(out.tools[0].2 && out.tools[0].1.contains("denied"));
    let tool_msg = &mock.requests()[1]["messages"][3];
    assert!(tool_msg["content"].as_str().unwrap().contains("denied"));
}

#[tokio::test]
async fn dangerous_command_approved_runs() {
    let mock = start().await;
    mock.push_chat(tool_call("c1", "terminal", json!({"command": "rm -rf junk && echo cleaned"})));
    mock.push_chat(text("Cleaned."));
    let (h, cfg) = harness(&mock.base());
    std::fs::create_dir_all(h.work.join("junk/deep")).unwrap();
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_agent, out) = turn(agent, "clean up", true, None).await;
    assert_eq!(out.approvals.len(), 1);
    assert!(!h.work.join("junk").exists());
    assert!(out.tools[0].1.contains("cleaned"));
}

#[tokio::test]
async fn yolo_and_deny_modes_skip_the_prompt() {
    for (mode, should_run) in [(ApprovalMode::Yolo, true), (ApprovalMode::Deny, false)] {
        let mock = start().await;
        mock.push_chat(tool_call("c1", "terminal", json!({"command": "rm -r tmpdir"})));
        mock.push_chat(text("ok"));
        let (h, mut cfg) = harness(&mock.base());
        cfg.approvals = mode;
        std::fs::create_dir_all(h.work.join("tmpdir")).unwrap();
        let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
        let (_a, out) = turn(agent, "go", true, None).await;
        assert!(out.approvals.is_empty(), "{mode:?} must not prompt");
        assert_eq!(!h.work.join("tmpdir").exists(), should_run, "{mode:?}");
    }
}

#[tokio::test]
async fn malformed_tool_arguments_go_back_to_the_model() {
    let mock = start().await;
    mock.push_chat(common::Reply::Sse(
        vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"bad","function":{"name":"read_file","arguments":"{not json"}}]}}]})
                .to_string(),
            "[DONE]".into(),
        ],
        0,
    ));
    mock.push_chat(text("Sorry, retrying differently."));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "read it", true, None).await;
    assert!(out.errors.is_empty());
    let tool_msg = &mock.requests()[1]["messages"][3];
    assert!(tool_msg["content"].as_str().unwrap().contains("not valid JSON"));
}

#[tokio::test]
async fn cancellation_stops_streaming() {
    let mock = start().await;
    let mut chunks = text_chunks(&"word ".repeat(200));
    chunks.truncate(150);
    mock.push_chat(common::Reply::Sse(chunks, 20));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let start = std::time::Instant::now();
    let (_a, out) = turn(agent, "talk a lot", true, Some(Duration::from_millis(300))).await;
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(out.notices.iter().any(|n| n == "Stopped."), "{:?}", out.notices);
    assert!(out.errors.is_empty());
}

#[tokio::test]
async fn retries_without_stream_options_when_rejected() {
    let mock = start().await;
    mock.push_chat(common::Reply::Status(400, "{\"error\":\"Unrecognized request argument: stream_options\"}".into()));
    mock.push_chat(text("fine"));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "hi", true, None).await;
    assert_eq!(out.text, "fine");
    let reqs = mock.requests();
    assert!(reqs[0].get("stream_options").is_some());
    assert!(reqs[1].get("stream_options").is_none());
}

#[tokio::test]
async fn models_without_tool_support_fall_back_to_plain_chat() {
    let mock = start().await;
    mock.push_chat(common::Reply::Status(400, "{\"error\":\"registry.ollama.ai/library/gemma:2b does not support tools\"}".into()));
    mock.push_chat(text("plain answer"));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "hi", true, None).await;
    assert_eq!(out.text, "plain answer");
    assert!(out.notices.iter().any(|n| n.contains("doesn't support tool calling")));
    assert!(mock.requests()[1].get("tools").is_none());
}

#[tokio::test]
async fn provider_with_tools_off_sends_no_tools() {
    let mock = start().await;
    mock.push_chat(text("ok"));
    let (h, mut cfg) = harness(&mock.base());
    cfg.providers[0].tools = false;
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, _) = turn(agent, "hi", true, None).await;
    assert!(mock.requests()[0].get("tools").is_none());
    assert!(!agent.system_prompt().contains("# Tools"));
}

#[tokio::test]
async fn unreachable_local_server_gives_actionable_error() {
    let (h, cfg) = harness("http://127.0.0.1:9/v1");
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "hi", true, None).await;
    assert_eq!(out.errors.len(), 1);
    assert!(out.errors[0].contains("Can't reach") && out.errors[0].contains("local model"), "{}", out.errors[0]);
}

#[tokio::test]
async fn reasoning_is_separated_and_not_sent_back() {
    let mock = start().await;
    mock.push_chat(common::Reply::Sse(
        vec![json!({"choices":[{"delta":{"content":"<think>let me think</think>The answer is 4."}}]}).to_string(), "[DONE]".into()],
        0,
    ));
    mock.push_chat(text("Still 4."));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, out) = turn(agent, "2+2?", true, None).await;
    assert_eq!(out.reasoning, "let me think");
    assert_eq!(out.text, "The answer is 4.");
    let (_a, _) = turn(agent, "sure?", true, None).await;
    let sent = mock.requests()[1]["messages"][2].clone();
    assert_eq!(sent["content"], "The answer is 4.");
    assert!(sent.get("reasoning").is_none());
}

#[tokio::test]
async fn slash_commands_retry_undo_model_and_compress() {
    let mock = start().await;
    let (h, cfg) = harness(&mock.base());
    let mut agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let skills: Vec<String> = vec![];

    // Build up a few exchanges.
    for i in 0..4 {
        mock.push_chat(text(&format!("answer {i}")));
        let (a, _) = turn(agent, &format!("question {i}"), true, None).await;
        agent = a;
    }
    assert_eq!(agent.messages.len(), 8);

    let out = agent.handle_command(commands::parse("/retry", &skills).unwrap(), &tx, &cancel).await.unwrap();
    assert_eq!(out, CommandOutcome::Send("question 3".into()));
    assert_eq!(agent.messages.len(), 6);

    let out = agent.handle_command(Command::Undo, &tx, &cancel).await.unwrap();
    assert!(matches!(out, CommandOutcome::Reload(_)));
    assert_eq!(agent.messages.len(), 4);
    assert_eq!(agent.sessions.messages(&agent.session.id).unwrap().len(), 4);

    // /model lists from the server; /model provider:name switches and persists.
    let CommandOutcome::Text(listing) = agent.handle_command(Command::Model(None), &tx, &cancel).await.unwrap() else { panic!() };
    assert!(listing.contains("other-model"));
    agent.handle_command(commands::parse("/model mock:other-model", &skills).unwrap(), &tx, &cancel).await.unwrap();
    assert_eq!(agent.config.active_model.as_deref(), Some("other-model"));
    let saved = mercury::config::Config::load(&h.paths.config_file()).unwrap();
    assert_eq!(saved.active_model.as_deref(), Some("other-model"));

    // Grow the transcript and compress it.
    for i in 4..8 {
        mock.push_chat(text(&format!("answer {i}")));
        let (a, _) = turn(agent, &format!("question {i}"), true, None).await;
        agent = a;
    }
    mock.push_chat(text("- user asked numbered questions\n- assistant answered them"));
    let out = agent.handle_command(Command::Compress, &tx, &cancel).await.unwrap();
    assert!(matches!(out, CommandOutcome::Reload(_)), "{out:?}");
    assert!(agent.messages[0].text().starts_with("[Summary of our earlier conversation]"));
    assert_eq!(agent.messages[1].role, Role::Assistant);
    assert_eq!(agent.messages[2].role, Role::User);
    let summary_req = mock.requests().last().unwrap().clone();
    assert_eq!(summary_req["model"], "other-model");
    assert!(summary_req.get("tools").is_none());

    // /new starts a fresh, empty session.
    let old = agent.session.id.clone();
    agent.handle_command(Command::New, &tx, &cancel).await.unwrap();
    assert_ne!(agent.session.id, old);
    assert!(agent.messages.is_empty());
}

#[tokio::test]
async fn auto_compression_triggers_on_small_context() {
    let mock = start().await;
    let (h, cfg) = harness(&mock.base());
    let mut agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    for i in 0..6 {
        mock.push_chat(text(&format!("{} {i}", "long answer ".repeat(60))));
        let (a, _) = turn(agent, &format!("q{i}"), true, None).await;
        agent = a;
    }
    // Shrink the window so the next turn crosses the 80% threshold.
    agent.config.agent.default_context_window = 1_000;
    // Next turn: compression summary first, then the real answer.
    mock.push_chat(text("- summary"));
    mock.push_chat(text("final"));
    let (agent, out) = turn(agent, "q-final", true, None).await;
    assert!(out.notices.iter().any(|n| n.contains("summarised")), "{:?}", out.notices);
    assert!(agent.messages[0].text().contains("- summary"));
    assert_eq!(agent.messages.last().unwrap().text(), "final");
}

#[tokio::test]
async fn skill_slash_command_injects_instructions() {
    let mock = start().await;
    let (h, cfg) = harness(&mock.base());
    let mut agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    agent.tools.skills.create("release-notes", "Write release notes", "1. Read git log\n2. Group by type").unwrap();
    let names: Vec<String> = agent.tools.skills.list().into_iter().map(|s| s.name).collect();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let out =
        agent.handle_command(commands::parse("/release-notes for v0.2", &names).unwrap(), &tx, &CancellationToken::new()).await.unwrap();
    let CommandOutcome::Send(msg) = out else { panic!("{out:?}") };
    assert!(msg.contains("Group by type") && msg.contains("Task: for v0.2") && !msg.contains("---"));
}

#[tokio::test]
async fn memory_tool_writes_hermes_compatible_files() {
    let mock = start().await;
    mock.push_chat(tool_call("m1", "memory", json!({"action":"add","target":"user","content":"Prefers to be challenged, not flattered"})));
    mock.push_chat(text("Noted."));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "remember this about me", true, None).await;
    assert!(!out.tools[0].2);
    let path = h.paths.default_memories_dir().join("USER.md");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "Prefers to be challenged, not flattered");
}
