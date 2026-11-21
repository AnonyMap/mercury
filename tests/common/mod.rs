//! Shared test helpers: a scriptable OpenAI-compatible / Nous Portal mock server.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use mercury::agent::{Agent, AgentEvent};
use mercury::config::{Config, Paths, ProviderConfig};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::Path;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub enum Reply {
    /// SSE events (each is one `data:` payload), with a delay between them.
    Sse(Vec<String>, u64),
    Status(u16, String),
    /// Non-JSON body with extra headers (edge firewall responses).
    Raw(u16, String, Vec<(String, String)>),
    Json(u16, Value),
}

pub type TokenRequest = (Vec<(String, String)>, HeaderMap);

#[derive(Default)]
pub struct MockState {
    pub chat_requests: Mutex<Vec<Value>>,
    pub chat_headers: Mutex<Vec<HeaderMap>>,
    pub chat_script: Mutex<VecDeque<Reply>>,
    pub models: Mutex<Vec<String>>,
    pub token_script: Mutex<VecDeque<Reply>>,
    pub token_requests: Mutex<Vec<TokenRequest>>,
    pub device_requests: Mutex<Vec<Vec<(String, String)>>>,
    pub html: Mutex<String>,
}

pub struct Mock {
    pub url: String,
    pub state: Arc<MockState>,
}

impl Mock {
    pub fn base(&self) -> String {
        format!("{}/v1", self.url)
    }
    pub fn push_chat(&self, r: Reply) {
        self.state.chat_script.lock().unwrap().push_back(r);
    }
    pub fn push_token(&self, r: Reply) {
        self.state.token_script.lock().unwrap().push_back(r);
    }
    pub fn requests(&self) -> Vec<Value> {
        self.state.chat_requests.lock().unwrap().clone()
    }
}

fn reply(r: Reply) -> Response {
    match r {
        Reply::Sse(events, delay) => {
            let stream = futures_util::stream::unfold((events.into_iter(), false), move |(mut it, started)| async move {
                if started && delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let next = it.next()?;
                Some((Ok::<_, std::io::Error>(format!("data: {next}\n\n")), (it, true)))
            });
            Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(stream)).unwrap()
        }
        Reply::Status(code, body) => (StatusCode::from_u16(code).unwrap(), body).into_response(),
        Reply::Raw(code, body, headers) => {
            let mut b = Response::builder().status(code);
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(Body::from(body)).unwrap()
        }
        Reply::Json(code, v) => (StatusCode::from_u16(code).unwrap(), axum::Json(v)).into_response(),
    }
}

async fn chat(State(s): State<Arc<MockState>>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
    s.chat_requests.lock().unwrap().push(body);
    s.chat_headers.lock().unwrap().push(headers);
    let next = s.chat_script.lock().unwrap().pop_front();
    reply(next.unwrap_or_else(|| Reply::Status(500, "{\"error\":\"script exhausted\"}".into())))
}

async fn models(State(s): State<Arc<MockState>>) -> Response {
    let data: Vec<Value> = s.models.lock().unwrap().iter().map(|m| json!({"id": m, "object": "model"})).collect();
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

async fn device_code(State(s): State<Arc<MockState>>, Form(form): Form<Vec<(String, String)>>) -> Response {
    s.device_requests.lock().unwrap().push(form);
    axum::Json(json!({
        "device_code": "dev-123", "user_code": "ABCD-EFGH",
        "verification_uri": "https://portal.example/device",
        "verification_uri_complete": "https://portal.example/device?user_code=ABCD-EFGH",
        "expires_in": 5, "interval": 1
    }))
    .into_response()
}

async fn token(State(s): State<Arc<MockState>>, headers: HeaderMap, Form(form): Form<Vec<(String, String)>>) -> Response {
    s.token_requests.lock().unwrap().push((form, headers));
    let next = s.token_script.lock().unwrap().pop_front();
    reply(next.unwrap_or_else(|| Reply::Json(400, json!({"error": "authorization_pending"}))))
}

async fn page(State(s): State<Arc<MockState>>) -> Response {
    ([("content-type", "text/html")], s.html.lock().unwrap().clone()).into_response()
}

async fn searx(State(_s): State<Arc<MockState>>, axum::extract::Query(q): axum::extract::Query<Vec<(String, String)>>) -> Response {
    let query = q.iter().find(|(k, _)| k == "q").map(|(_, v)| v.clone()).unwrap_or_default();
    axum::Json(json!({"results": [
        {"title": format!("Result for {query}"), "url": "https://example.org/a", "content": "first snippet"},
        {"title": "Second", "url": "https://example.org/b", "content": "second snippet"}
    ]}))
    .into_response()
}

pub async fn start() -> Mock {
    let state = Arc::new(MockState::default());
    *state.models.lock().unwrap() = vec!["test-model".into(), "other-model".into()];
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(models))
        .route("/api/oauth/device/code", post(device_code))
        .route("/api/oauth/token", post(token))
        .route("/page", get(page))
        .route("/search", get(searx))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock { url, state }
}

// ---- SSE builders -------------------------------------------------------------------------

pub fn text_chunks(text: &str) -> Vec<String> {
    let mut v: Vec<String> =
        text.split_inclusive(' ').map(|w| json!({"choices":[{"index":0,"delta":{"content": w}}]}).to_string()).collect();
    v.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}).to_string());
    v.push(json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}).to_string());
    v.push("[DONE]".into());
    v
}

pub fn text(textv: &str) -> Reply {
    Reply::Sse(text_chunks(textv), 0)
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Reply {
    let a = args.to_string();
    let (first, second) = a.split_at(a.len() / 2);
    Reply::Sse(
        vec![
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":""}}]}}]}).to_string(),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":first}}]}}]}).to_string(),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":second}}]}}]}).to_string(),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
            "[DONE]".into(),
        ],
        0,
    )
}

// ---- Agent harness -------------------------------------------------------------------------

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
    pub work: std::path::PathBuf,
}

pub fn harness(base_url: &str) -> (Harness, Config) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path().join("home"));
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut cfg = Config::default();
    let mut p = ProviderConfig::openai("mock", "Mock", base_url);
    p.default_model = "test-model".into();
    cfg.upsert_provider(p);
    cfg.set_active("mock", Some("test-model".into()));
    cfg.agent.workdir = Some(work.clone());
    (Harness { dir, paths, work }, cfg)
}

#[derive(Debug, Default)]
pub struct Collected {
    pub text: String,
    pub reasoning: String,
    pub notices: Vec<String>,
    pub errors: Vec<String>,
    pub tools: Vec<(String, String, bool)>,
    pub approvals: Vec<String>,
    pub committed: usize,
}

/// Run one turn, answering approvals with `approve`, and optionally cancelling after `cancel_after`.
pub async fn turn(agent: Agent, text: &str, approve: bool, cancel_after: Option<Duration>) -> (Agent, Collected) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let text = text.to_string();
    let mut agent = agent;
    let handle = tokio::spawn(async move {
        agent.run_turn(&text, &tx, &c2).await;
        agent
    });
    if let Some(d) = cancel_after {
        let c3 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            c3.cancel();
        });
    }
    let mut out = Collected::default();
    while let Some(ev) = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("turn timed out") {
        match ev {
            AgentEvent::TextDelta(t) => out.text.push_str(&t),
            AgentEvent::ReasoningDelta(t) => out.reasoning.push_str(&t),
            AgentEvent::Notice(n) => out.notices.push(n),
            AgentEvent::Error(e) => out.errors.push(e),
            AgentEvent::ToolFinished { name, output, is_error, .. } => out.tools.push((name, output, is_error)),
            AgentEvent::ApprovalNeeded { command, respond, .. } => {
                out.approvals.push(command);
                let _ = respond.send(approve);
            }
            AgentEvent::Committed(_) => out.committed += 1,
            AgentEvent::TurnFinished => break,
            _ => {}
        }
    }
    (handle.await.unwrap(), out)
}

// ---- A fake Hermes install ----------------------------------------------------------------

/// A Hermes home with a default profile and a `coder` profile, shaped like the real thing.
pub fn fake_hermes(root: &Path) {
    std::fs::create_dir_all(root.join("memories")).unwrap();
    std::fs::write(
        root.join("config.yaml"),
        r#"
model:
  provider: custom
  default: qwen3:32b
  base_url: http://192.168.1.50:11434/v1
mcp_servers:
  time:
    command: uvx
    args: ["mcp-server-time"]
  notion:
    url: https://mcp.notion.com/mcp
    headers:
      Authorization: Bearer x
  broken:
    args: ["nothing to run"]
agent:
  personalities:
    mentor: "You are a supportive mentor."
    reviewer:
      system_prompt: "You are a meticulous code reviewer."
      tone: direct
display:
  personality: mentor
"#,
    )
    .unwrap();
    std::fs::write(root.join(".env"), "OPENAI_API_KEY=sk-local\n").unwrap();
    std::fs::write(root.join("memories/MEMORY.md"), "Repo uses pnpm\n§\nCI runs on Fridays").unwrap();
    std::fs::write(root.join("memories/USER.md"), "Name is Aliah\n§\nPrefers to be challenged").unwrap();
    std::fs::write(root.join("SOUL.md"), "You are Hermes, calm and precise.").unwrap();
    let sk = root.join("skills/devops/backup-db");
    std::fs::create_dir_all(&sk).unwrap();
    std::fs::write(sk.join("SKILL.md"), "---\nname: backup-db\ndescription: Back up the database\n---\npg_dump ...\n").unwrap();
    std::fs::write(root.join("auth.json"), r#"{"version":1,"providers":{"nous":{"access_token":"secret","refresh_token":"single-use"}}}"#)
        .unwrap();

    let c = rusqlite::Connection::open(root.join("state.db")).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, started_at REAL NOT NULL,
            title TEXT, last_activity_at REAL, hidden INTEGER NOT NULL DEFAULT 0, parent_session_id TEXT);
         CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
            content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL,
            reasoning TEXT, reasoning_content TEXT, active INTEGER NOT NULL DEFAULT 1);",
    )
    .unwrap();
    let s = |id: &str, source: &str, title: Option<&str>, t: f64, hidden: i64| {
        c.execute(
            "INSERT INTO sessions(id, source, model, started_at, title, last_activity_at, hidden) VALUES (?1,?2,'qwen3:32b',?3,?4,?3,?5)",
            params![id, source, t, title, hidden],
        )
        .unwrap();
    };
    let m = |sid: &str,
             role: &str,
             content: Option<&str>,
             call_id: Option<&str>,
             calls: Option<&str>,
             tool: Option<&str>,
             reasoning: Option<&str>,
             active: i64| {
        c.execute(
            "INSERT INTO messages(session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, reasoning_content, active)
             VALUES (?1,?2,?3,?4,?5,?6,1700000000,?7,?8)",
            params![sid, role, content, call_id, calls, tool, reasoning, active],
        )
        .unwrap();
    };
    // A CLI session with a tool round trip and reasoning.
    s("s1", "cli", Some("Fix the flaky test"), 1_700_000_000.0, 0);
    m("s1", "system", Some("system prompt"), None, None, None, None, 1);
    m("s1", "user", Some("The auth test is flaky"), None, None, None, None, 1);
    m(
        "s1",
        "assistant",
        None,
        None,
        Some(r#"[{"id":"call_a","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"cargo test auth\"}"}}]"#),
        None,
        Some("check the test first"),
        1,
    );
    m("s1", "tool", Some("1 failed"), Some("call_a"), None, Some("terminal"), None, 1);
    m("s1", "assistant", Some("It races on a shared port; I pinned it."), None, None, None, None, 1);
    m("s1", "user", Some("old compacted message"), None, None, None, None, 0);
    // A Telegram session, untitled, with list-of-parts content and a dangling tool call.
    s("s2", "telegram", None, 1_700_100_000.0, 0);
    m("s2", "user", Some(r#"[{"type":"text","text":"remind me about the dentist"}]"#), None, None, None, None, 1);
    m(
        "s2",
        "assistant",
        Some(""),
        None,
        Some(r#"[{"id":"call_b","function":{"name":"cronjob_manage","arguments":{"action":"add"}}}]"#),
        None,
        None,
        1,
    );
    m("s2", "tool", Some("orphan result for an unknown call"), Some("call_zzz"), None, Some("x"), None, 1);
    // Hidden and empty sessions are skipped.
    s("s3", "cli", Some("hidden"), 1_700_200_000.0, 1);
    m("s3", "user", Some("secret"), None, None, None, None, 1);
    s("s4", "cli", Some("empty"), 1_700_300_000.0, 0);

    let coder = root.join("profiles/coder");
    std::fs::create_dir_all(coder.join("memories")).unwrap();
    std::fs::write(coder.join("config.yaml"), "model:\n  provider: nous-api\n  default: Hermes-4-70B\n").unwrap();
    std::fs::write(coder.join(".env"), "NOUS_API_KEY=sk-nous\n").unwrap();
    // A leftover directory without identity files is not a profile.
    std::fs::create_dir_all(root.join("profiles/logs-only")).unwrap();
}
