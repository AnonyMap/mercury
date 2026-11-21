//! MCP (stdio + HTTP), subagents, clarify, background processes, checkpoints, insights,
//! personalities and project context files.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::*;
use mercury::agent::{Agent, AgentEvent, CommandOutcome};
use mercury::commands::Command;
use mercury::config::McpServerConfig;
use mercury::mcp::{McpClient, McpManager};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn python() -> Option<String> {
    ["python3", "python"].iter().find(|p| std::process::Command::new(p).arg("--version").output().is_ok()).map(|s| s.to_string())
}

fn stdio_cfg() -> Option<McpServerConfig> {
    let py = python()?;
    Some(McpServerConfig {
        command: Some(py),
        args: vec![format!("{}/tests/fixtures/mock_mcp.py", env!("CARGO_MANIFEST_DIR"))],
        enabled: true,
        timeout: 10,
        ..Default::default()
    })
}

#[tokio::test]
async fn stdio_mcp_server_tools() {
    let Some(cfg) = stdio_cfg() else { return eprintln!("skipped: no python") };
    let c = McpClient::connect("mock", &cfg).await.unwrap();
    assert_eq!(c.server_info, "mock-mcp 1.2.3");
    let tools = c.list_tools().await.unwrap();
    assert_eq!(tools.iter().map(|t| t.exposed.as_str()).collect::<Vec<_>>(), vec!["mcp_mock_echo", "mcp_mock_fail"]);
    let (text, err) = c.call_tool("echo", json!({"text": "hi"})).await.unwrap();
    assert_eq!((text.as_str(), err), ("echo: hi (pong)", false), "server ping was answered");
    let (text, err) = c.call_tool("fail", json!({})).await.unwrap();
    assert_eq!((text.as_str(), err), ("it broke", true));
    assert!(c.call_tool("nope", json!({})).await.unwrap_err().to_string().contains("unknown tool"));
}

#[tokio::test]
async fn mcp_manager_reports_failures_without_blocking_others() {
    let Some(good) = stdio_cfg() else { return };
    let mut servers = BTreeMap::new();
    servers.insert("mock".to_string(), good);
    servers.insert(
        "missing".to_string(),
        McpServerConfig { command: Some("/definitely/not/here".into()), enabled: true, ..Default::default() },
    );
    servers.insert("off".to_string(), McpServerConfig { command: Some("/x".into()), enabled: false, ..Default::default() });
    let m = McpManager::connect_all(&servers).await;
    assert_eq!(m.tools.len(), 2);
    let st = m.status_text();
    assert!(st.contains("✔ mock: mock-mcp 1.2.3 — 2 tools") && st.contains("✖ missing"), "{st}");
    assert!(!st.contains("off"));
    assert_eq!(m.definitions()[0]["function"]["parameters"]["required"][0], "text");
}

#[derive(Default)]
struct HttpState {
    sessions_seen: Mutex<Vec<Option<String>>>,
}

async fn http_mcp(
    axum::extract::State(st): axum::extract::State<Arc<HttpState>>,
    headers: HeaderMap,
    axum::Json(msg): axum::Json<Value>,
) -> Response {
    st.sessions_seen.lock().unwrap().push(headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_string));
    let id = msg.get("id").cloned();
    match msg["method"].as_str().unwrap_or("") {
        "initialize" => (
            [("mcp-session-id", "sess-42")],
            axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"http-mock","version":"0.1"}}})),
        )
            .into_response(),
        "tools/list" => {
            // Answer as an SSE stream, as streamable-HTTP servers may.
            let body = format!(
                "event: message\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"time","description":"Current time","inputSchema":{"type":"object"}}]}})
            );
            ([("content-type", "text/event-stream")], body).into_response()
        }
        "tools/call" => axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"12:00"}]}})).into_response(),
        _ => axum::http::StatusCode::ACCEPTED.into_response(),
    }
}

#[tokio::test]
async fn http_mcp_server_with_session_and_sse() {
    let st = Arc::new(HttpState::default());
    let app = Router::new().route("/mcp", post(http_mcp)).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = McpServerConfig { url: Some(url), enabled: true, ..Default::default() };
    let c = McpClient::connect("clock", &cfg).await.unwrap();
    let tools = c.list_tools().await.unwrap();
    assert_eq!(tools[0].exposed, "mcp_clock_time");
    assert_eq!(c.call_tool("time", json!({})).await.unwrap().0, "12:00");
    let seen = st.sessions_seen.lock().unwrap().clone();
    assert_eq!(seen[0], None);
    assert!(seen[1..].iter().all(|s| s.as_deref() == Some("sess-42")), "{seen:?}");
}

#[tokio::test]
async fn agent_calls_mcp_tools() {
    let Some(srv) = stdio_cfg() else { return };
    let mock = start().await;
    mock.push_chat(tool_call("c1", "mcp_mock_echo", json!({"text": "from the model"})));
    mock.push_chat(text("done"));
    let (h, mut cfg) = harness(&mock.base());
    cfg.mcp_servers.insert("mock".into(), srv);
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (_a, out) = turn(agent, "use the echo tool", true, None).await;
    assert!(out.notices.iter().any(|n| n.contains("MCP ready: 2 tools")), "{:?}", out.notices);
    assert_eq!(out.tools[0].1, "echo: from the model (pong)");
    let tools = mock.requests()[0]["tools"].as_array().unwrap().clone();
    assert!(tools.iter().any(|t| t["function"]["name"] == "mcp_mock_echo"));
}

/// Run a turn, answering clarify questions with `answer`.
async fn turn_with_answer(agent: Agent, prompt: &str, answer: Option<&str>) -> (Agent, Vec<String>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let p = prompt.to_string();
    let mut agent = agent;
    let h = tokio::spawn(async move {
        agent.run_turn(&p, &tx, &cancel).await;
        agent
    });
    let mut asked = vec![];
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.unwrap() {
        match ev {
            AgentEvent::Clarify { question, choices, respond } => {
                asked.push(format!("{question} {choices:?}"));
                match answer {
                    Some(a) => {
                        let _ = respond.send(a.to_string());
                    }
                    None => drop(respond),
                }
            }
            AgentEvent::ApprovalNeeded { respond, .. } => {
                let _ = respond.send(true);
            }
            AgentEvent::TurnFinished => break,
            _ => {}
        }
    }
    (h.await.unwrap(), asked)
}

#[tokio::test]
async fn clarify_answered_and_skipped() {
    let mock = start().await;
    for _ in 0..2 {
        mock.push_chat(tool_call("q1", "clarify", json!({"question": "Which database?", "choices": ["Postgres", "SQLite"]})));
        mock.push_chat(text("ok"));
    }
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, asked) = turn_with_answer(agent, "set up a db", Some("SQLite")).await;
    assert_eq!(asked, vec![r#"Which database? ["Postgres", "SQLite"]"#]);
    assert_eq!(mock.requests()[1]["messages"][3]["content"], "The user answered: SQLite");
    let (_a, _) = turn_with_answer(agent, "again", None).await;
    assert!(mock.requests()[3]["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().contains("didn't answer"));
}

#[tokio::test]
async fn delegate_task_runs_subagents_in_parallel() {
    let mock = start().await;
    mock.push_chat(tool_call(
        "d1",
        "delegate_task",
        json!({"tasks": [{"goal": "Count the Rust files", "context": "repo at ."}, {"goal": "Find the license"}]}),
    ));
    // Each subagent answers once (order between them doesn't matter), then the parent summarises.
    mock.push_chat(text("Report: done."));
    mock.push_chat(text("Report: done."));
    mock.push_chat(text("Both subagents finished."));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, out) = turn(agent, "survey the repo", true, None).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);
    // Subagent requests: fresh context, a subagent role, and no delegate/clarify tools.
    for r in &reqs[1..3] {
        let msgs = r["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0]["content"].as_str().unwrap().contains("You are a subagent"));
        let names: Vec<&str> = r["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"delegate_task") && !names.contains(&"clarify") && names.contains(&"terminal"));
    }
    let goals: Vec<String> = reqs[1..3].iter().map(|r| r["messages"][1]["content"].as_str().unwrap().to_string()).collect();
    assert!(goals.iter().any(|g| g.contains("Count the Rust files") && g.contains("repo at .")));
    assert!(goals.iter().any(|g| g.contains("Find the license") && g.contains("(none)")));
    let result = reqs[3]["messages"][3]["content"].as_str().unwrap();
    assert!(result.contains("## Subagent 1 — Count the Rust files") && result.contains("## Subagent 2 — Find the license"));
    // Usage from the subagents is counted in the parent session.
    // Two subagent replies + the final reply report usage (the mock tool-call reply has none).
    assert_eq!(agent.usage.total_tokens, 15 * 3);
    // Subagent conversations don't clutter history.
    assert_eq!(agent.sessions.list(10).unwrap().iter().filter(|s| s.message_count > 0).count(), 1);
}

#[tokio::test]
async fn background_process_poll_and_kill() {
    let mock = start().await;
    mock.push_chat(tool_call("b1", "terminal", json!({"command": "echo started; sleep 30", "background": true})));
    mock.push_chat(text("started"));
    let (h, cfg) = harness(&mock.base());
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (agent, out) = turn(agent, "start a server", true, None).await;
    let pid: u32 = out.tools[0].1.split("pid ").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
    let ctx = agent.tools.clone();
    let cancel = CancellationToken::new();
    let list = mercury::tools::execute("process_manage", &json!({"action": "list"}), &ctx, &cancel).await;
    assert!(list.content.contains(&pid.to_string()) && list.content.contains("running"), "{}", list.content);
    let mut polled = String::new();
    for _ in 0..40 {
        polled = mercury::tools::execute("process_manage", &json!({"action": "poll", "pid": pid}), &ctx, &cancel).await.content;
        if polled.contains("started") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(polled.starts_with("[running]") && polled.contains("started"), "{polled}");
    let killed = mercury::tools::execute("process_manage", &json!({"action": "kill", "pid": pid}), &ctx, &cancel).await;
    assert!(killed.content.starts_with("Stopped"));
    let after = mercury::tools::execute("process_manage", &json!({"action": "wait", "pid": pid, "timeout": 2}), &ctx, &cancel).await;
    assert!(after.content.starts_with("[exited]"), "{}", after.content);
    assert!(mercury::tools::execute("process_manage", &json!({"action": "poll", "pid": 1}), &ctx, &cancel).await.is_error);
}

#[tokio::test]
async fn rollback_undoes_agent_edits() {
    let mock = start().await;
    mock.push_chat(tool_call("w1", "write_file", json!({"path": "notes.md", "content": "v2"})));
    mock.push_chat(tool_call("w2", "write_file", json!({"path": "new.md", "content": "fresh"})));
    mock.push_chat(text("edited"));
    let (h, cfg) = harness(&mock.base());
    std::fs::write(h.work.join("notes.md"), "v1").unwrap();
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    let (mut agent, _) = turn(agent, "edit things", true, None).await;
    assert_eq!(std::fs::read_to_string(h.work.join("notes.md")).unwrap(), "v2");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let c = CancellationToken::new();
    let CommandOutcome::Text(list) = agent.handle_command(Command::Rollback(None), &tx, &c).await.unwrap() else { panic!() };
    assert!(list.contains("new.md") && list.contains("new file") && list.contains("notes.md"), "{list}");
    let CommandOutcome::Text(done) = agent.handle_command(Command::Rollback(Some(2)), &tx, &c).await.unwrap() else { panic!() };
    assert!(done.contains("Rolled back 2"));
    assert_eq!(std::fs::read_to_string(h.work.join("notes.md")).unwrap(), "v1");
    assert!(!h.work.join("new.md").exists());
    // Checkpoints are per session.
    agent.new_session().unwrap();
    assert!(agent.tools.checkpoints.list().is_empty());
}

#[tokio::test]
async fn insights_personalities_and_context_files() {
    let mock = start().await;
    mock.push_chat(tool_call("t1", "terminal", json!({"command": "true"})));
    mock.push_chat(text("ok"));
    let (h, mut cfg) = harness(&mock.base());
    cfg.custom_personalities.insert("mentor".into(), "Be a supportive mentor.".into());
    // Hermes's priority: .hermes.md beats AGENTS.md; only one is loaded. Found up to the git root.
    std::fs::create_dir_all(h.work.join(".git")).unwrap();
    std::fs::write(h.work.join("AGENTS.md"), "agents rules").unwrap();
    std::fs::write(h.work.join(".hermes.md"), "hermes rules").unwrap();
    std::fs::create_dir_all(h.work.join("sub/dir")).unwrap();
    cfg.agent.workdir = Some(h.work.join("sub/dir"));
    let agent = Agent::new(h.paths.clone(), cfg, None).unwrap();
    assert!(agent.system_prompt().contains("hermes rules") && !agent.system_prompt().contains("agents rules"));
    let (mut agent, _) = turn(agent, "hi", true, None).await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let c = CancellationToken::new();
    let CommandOutcome::Text(ins) = agent.handle_command(Command::Insights(Some(7)), &tx, &c).await.unwrap() else { panic!() };
    assert!(ins.contains("1 sessions") && ins.contains("terminal: 1") && ins.contains("test-model via mock"), "{ins}");
    let CommandOutcome::Text(list) = agent.handle_command(Command::Personality(None), &tx, &c).await.unwrap() else { panic!() };
    assert!(list.contains("kawaii") && list.contains("mentor"));
    agent.handle_command(Command::Personality(Some("mentor".into())), &tx, &c).await.unwrap();
    assert!(agent.system_prompt().contains("Be a supportive mentor."));
}
