//! The agent loop: build the prompt, stream a completion, run tool calls, repeat.
//! Front ends drive it through `run_turn` / `handle_command` and observe `AgentEvent`s.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::commands::{self, Command};
use crate::config::{ApprovalMode, Config, Paths, ProviderConfig};
use crate::memory::{MemoryStore, Target};
use crate::provider::{ChatRequest, Client, Message, ProviderError, Role, StreamDelta, Usage};
use crate::session::{SessionMeta, SessionStore, title_from};
use crate::tools::{self, ToolContext};

pub enum AgentEvent {
    TurnStarted,
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStreaming {
        name: String,
    },
    ToolRunning {
        id: String,
        name: String,
        summary: String,
        args: String,
    },
    ToolFinished {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// The UI must answer through `respond` (true = run it).
    ApprovalNeeded {
        command: String,
        reasons: Vec<String>,
        respond: oneshot::Sender<bool>,
    },
    /// The model asked the user a question (`clarify`); answer through `respond`.
    Clarify {
        question: String,
        choices: Vec<String>,
        respond: oneshot::Sender<String>,
    },
    /// A message was committed to the transcript.
    Committed(Message),
    /// The transcript was rewritten (compression); reload it from the agent.
    TranscriptReplaced,
    Usage(Usage),
    /// How full the context is: prompt tokens of the last request against the window.
    Context {
        used: u64,
        window: u32,
    },
    Notice(String),
    Error(String),
    TurnFinished,
}

impl std::fmt::Debug for AgentEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TurnStarted => write!(f, "TurnStarted"),
            Self::TextDelta(s) => write!(f, "TextDelta({s:?})"),
            Self::ReasoningDelta(s) => write!(f, "ReasoningDelta({s:?})"),
            Self::ToolCallStreaming { name } => write!(f, "ToolCallStreaming({name})"),
            Self::ToolRunning { name, summary, .. } => write!(f, "ToolRunning({name}: {summary})"),
            Self::ToolFinished { name, is_error, .. } => write!(f, "ToolFinished({name}, error={is_error})"),
            Self::ApprovalNeeded { command, .. } => write!(f, "ApprovalNeeded({command})"),
            Self::Clarify { question, .. } => write!(f, "Clarify({question})"),
            Self::Committed(m) => write!(f, "Committed({:?})", m.role),
            Self::TranscriptReplaced => write!(f, "TranscriptReplaced"),
            Self::Usage(u) => write!(f, "Usage({})", u.total_tokens),
            Self::Context { used, window } => write!(f, "Context({used}/{window})"),
            Self::Notice(s) => write!(f, "Notice({s})"),
            Self::Error(s) => write!(f, "Error({s})"),
            Self::TurnFinished => write!(f, "TurnFinished"),
        }
    }
}

pub type Events = mpsc::UnboundedSender<AgentEvent>;

/// A running subagent: resolves to (goal, report, usage).
type SubagentRun = std::pin::Pin<Box<dyn std::future::Future<Output = (String, String, Usage)> + Send>>;

/// What a slash command asks the front end to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandOutcome {
    /// Show this text.
    Text(String),
    /// The transcript changed wholesale (new session, undo, compress); reload it.
    Reload(String),
    /// Send this as a user message (retry, skill invocation).
    Send(String),
    /// Interrupt the running turn.
    Stop,
}

/// Built-in personalities. Names follow Hermes's built-ins so `/personality <name>` carries over;
/// the prompts are Mercury's own.
pub const PERSONALITIES: &[(&str, &str)] = &[
    ("default", ""),
    ("helpful", "Be warm, clear and helpful."),
    ("concise", "Be brief. Prefer short, direct answers and skip preamble."),
    ("technical", "Be precise and technical. Show commands, code and exact values; state assumptions."),
    ("teacher", "Explain your reasoning step by step as a patient teacher, checking understanding."),
    ("socratic", "Challenge the user's assumptions constructively, point out weaknesses, and add new information rather than agreeing."),
    ("creative", "Be imaginative and playful while staying accurate."),
    (
        "philosopher",
        "Approach questions thoughtfully, considering underlying assumptions and different schools of thought, without losing practicality.",
    ),
    ("kawaii", "Be cheerful and cute in tone, with the occasional kaomoji (◕‿◕), while still giving fully correct, useful answers."),
    ("pirate", "Talk like a good-natured pirate captain, matey, but keep every technical detail accurate."),
    ("shakespeare", "Answer in a playful Elizabethan manner, thee and thou, yet keep code and facts modern and exact."),
    ("surfer", "Be laid-back and upbeat like a friendly surfer, dude, while staying accurate."),
    ("noir", "Narrate like a hard-boiled detective in a rain-soaked city; the facts stay straight even when the prose is moody."),
    ("hype", "Be high-energy and encouraging, celebrate progress, and keep the substance solid."),
];

/// A personality's prompt: custom ones (from config / Hermes) override built-ins of the same name.
pub fn personality_prompt(name: &str, custom: &std::collections::BTreeMap<String, String>) -> Option<String> {
    custom.get(name).cloned().or_else(|| PERSONALITIES.iter().find(|(n, _)| *n == name).map(|(_, p)| p.to_string()))
}

pub fn personality_names(custom: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut v: Vec<String> = PERSONALITIES.iter().map(|(n, _)| n.to_string()).collect();
    for k in custom.keys() {
        if !v.contains(k) {
            v.push(k.clone());
        }
    }
    v
}

/// Project context files, highest priority first; only the first one found is loaded (Hermes's rule).
const CONTEXT_FILES: &[&str] = &[".hermes.md", "HERMES.md", "MERCURY.md", "AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// The project context file for `cwd`: search `cwd` and its parents up to the git root (or `cwd`
/// alone outside a repository).
pub fn project_context(cwd: &Path) -> Option<(PathBuf, String)> {
    let mut dirs = vec![cwd.to_path_buf()];
    let in_repo = cwd.ancestors().any(|d| d.join(".git").exists());
    if in_repo {
        for d in cwd.ancestors().skip(1) {
            dirs.push(d.to_path_buf());
            if d.join(".git").exists() {
                break;
            }
        }
        if cwd.join(".git").exists() {
            dirs.truncate(1);
        }
    }
    for name in CONTEXT_FILES {
        for d in &dirs {
            if let Ok(t) = std::fs::read_to_string(d.join(name))
                && !t.trim().is_empty()
            {
                return Some((d.join(name), t));
            }
        }
    }
    None
}
const CONTEXT_FILE_MAX: usize = 20_000;

pub struct Agent {
    pub paths: Paths,
    pub config: Config,
    pub sessions: Arc<SessionStore>,
    pub session: SessionMeta,
    pub messages: Vec<Message>,
    pub tools: ToolContext,
    pub usage: Usage,
    system_prompt: String,
    /// Memory as it was when the session started (Hermes's frozen-snapshot semantics).
    memory_snapshot: String,
    /// 0 for the main agent, 1 for a subagent spawned by `delegate_task`.
    pub depth: u8,
    /// Context windows reported by servers' model lists, keyed `provider/model`.
    pub model_context: std::collections::HashMap<String, u32>,
    /// Images to attach to the next user message (and kept across `/retry`).
    pub pending_images: Vec<String>,
}

/// Roughly what one attached image costs in prompt tokens.
const IMAGE_TOKENS: usize = 1_000;

fn estimate_tokens(msgs: &[Message], system: &str) -> usize {
    let chars: usize = system.len()
        + msgs
            .iter()
            .map(|m| m.text().len() + m.tool_calls.iter().map(|c| c.function.arguments.len() + c.function.name.len()).sum::<usize>() + 8)
            .sum::<usize>();
    chars / 4 + msgs.iter().map(|m| m.images.len() * IMAGE_TOKENS).sum::<usize>()
}

impl Agent {
    /// Open the stores and start (or resume) a session.
    pub fn new(paths: Paths, config: Config, resume: Option<&str>) -> Result<Self> {
        paths.ensure()?;
        let sessions = Arc::new(SessionStore::open(&paths.sessions_db())?);
        Self::with_store(paths, config, sessions, resume)
    }

    pub fn with_store(paths: Paths, config: Config, sessions: Arc<SessionStore>, resume: Option<&str>) -> Result<Self> {
        let (session, messages) = match resume.and_then(|id| sessions.get(id).ok().flatten()) {
            Some(meta) => {
                let msgs = sessions.messages(&meta.id)?;
                (meta, msgs)
            }
            None => (Self::fresh_session(&sessions, &config)?, vec![]),
        };
        let workdir = config
            .agent
            .workdir
            .clone()
            .filter(|p| p.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
        let tools = ToolContext::new(
            workdir,
            MemoryStore::new(config.memories_dir(&paths)),
            config.skill_library(&paths),
            sessions.clone(),
            &session.id,
            config.tools.clone(),
            &paths.profile_dir(),
        );
        let usage = session.usage.clone();
        let memory_snapshot = tools.memory.snapshot();
        let mut a = Self {
            paths,
            config,
            sessions,
            session,
            messages,
            tools,
            usage,
            system_prompt: String::new(),
            memory_snapshot,
            depth: 0,
            model_context: Default::default(),
            pending_images: vec![],
        };
        a.system_prompt = a.build_system_prompt();
        Ok(a)
    }

    fn fresh_session(sessions: &SessionStore, config: &Config) -> Result<SessionMeta> {
        let provider = config.active().map(|p| p.id.clone()).unwrap_or_default();
        let model = config.active_model_name().unwrap_or_default();
        sessions.create(&provider, &model)
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn workdir(&self) -> PathBuf {
        self.tools.cwd()
    }

    pub fn set_workdir(&mut self, p: &Path) -> Result<()> {
        let p = crate::tools::files::resolve(&self.tools.cwd(), &p.to_string_lossy());
        if !p.is_dir() {
            return Err(anyhow!("{} is not a directory", p.display()));
        }
        *self.tools.workdir.lock().unwrap_or_else(|e| e.into_inner()) = p.canonicalize()?;
        self.system_prompt = self.build_system_prompt();
        Ok(())
    }

    /// Adopt a config edited elsewhere (the settings screen). Memory location and skill
    /// folders follow it; the frozen memory snapshot is kept until the next session.
    pub fn apply_config(&mut self, config: Config) {
        self.tools.config = config.tools.clone();
        self.tools.memory = MemoryStore::new(config.memories_dir(&self.paths));
        self.tools.skills = config.skill_library(&self.paths);
        if config.mcp_servers != self.config.mcp_servers
            && let Ok(mut g) = self.tools.mcp.try_write()
        {
            *g = None; // reconnect with the new server list on the next turn
        }
        if let Some(w) = config.agent.workdir.clone().filter(|w| w.is_dir())
            && self.config.agent.workdir.as_ref() != Some(&w)
        {
            *self.tools.workdir.lock().unwrap_or_else(|e| e.into_inner()) = w;
        }
        self.config = config;
        self.system_prompt = self.build_system_prompt();
    }

    /// Persist config changes made through the agent.
    pub fn save_config(&self) -> Result<()> {
        self.config.save_profile(&self.paths)
    }

    pub fn build_system_prompt(&self) -> String {
        let mut s = String::from(
            "You are Mercury, a capable AI agent running on the user's own computer (a Rust client compatible with Nous Research's Hermes Agent). \
             You help with real work: answering questions, writing and editing code and documents, and operating the machine through your tools. \
             Be direct and accurate; say when you are unsure.",
        );
        let pers = personality_prompt(&self.config.agent.personality, &self.config.custom_personalities).unwrap_or_default();
        let soul = std::fs::read_to_string(self.config.soul_file(&self.paths)).unwrap_or_default();
        if !soul.trim().is_empty() {
            s.push_str("\n\n# Persona (SOUL.md)\n");
            s.push_str(&crate::provider::truncate(soul.trim(), CONTEXT_FILE_MAX));
        }
        if !pers.is_empty() {
            s.push_str("\n\n# Style\n");
            s.push_str(&pers);
        }
        let provider = self.config.active();
        let tools_on = provider.map(|p| p.tools).unwrap_or(true);
        s.push_str(&format!(
            "\n\n# Environment\n- Date: {}\n- OS: {} ({})\n- Working directory: {}\n- Model: {} via {}",
            chrono::Local::now().format("%A %Y-%m-%d %H:%M %Z"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.tools.cwd().display(),
            self.config.active_model_name().unwrap_or_else(|| "unknown".into()),
            provider.map(|p| p.name.as_str()).unwrap_or("unknown provider"),
        ));
        if tools_on {
            s.push_str(
                "\n\n# Tools\nUse tools when they help; don't narrate obvious steps. Read files before editing them and prefer `patch` for small edits. \
                 Commands run non-interactively. Dangerous commands may need the user's approval; if one is denied, find another way or ask. \
                 Save durable facts with `memory` (user preferences in target=user). After solving a non-trivial, repeatable task, consider saving it as a skill.",
            );
        }
        if !self.memory_snapshot.is_empty() {
            s.push_str("\n\n# Memory (snapshot from session start)\n");
            s.push_str(&self.memory_snapshot);
        }
        if tools_on && self.config.tools.skills {
            let idx = self.tools.skills.index();
            if !idx.is_empty() {
                s.push_str("\n\n# Skills (load one with skill_view before following it)\n");
                s.push_str(&idx);
            }
        }
        if let Some((path, t)) = project_context(&self.tools.cwd()) {
            s.push_str(&format!("\n\n# Project context ({})\n{}", path.display(), crate::provider::truncate(t.trim(), CONTEXT_FILE_MAX)));
        }
        if self.depth > 0 {
            s.push_str(
                "\n\n# Role\nYou are a subagent working on one focused task for a main agent. You can't ask the user questions. \
                 Work autonomously, then finish with a concise report: what you did, what you found (with paths, commands and values), and anything unresolved.",
            );
        }
        s
    }

    fn provider(&self) -> Result<ProviderConfig> {
        self.config.active().cloned().ok_or_else(|| anyhow!("No provider configured. Add one in Settings, or run `mercury discover`."))
    }

    pub async fn client(&self) -> Result<(Client, ProviderConfig)> {
        let p = self.provider()?;
        let (base, key) = crate::nous::credentials(&self.paths, &p).await?;
        let opts = p.client_options(&self.config.agent);
        Ok((Client::with_options(&base, key, opts), p))
    }

    pub async fn list_models(&mut self) -> Result<Vec<String>> {
        Ok(self.list_model_info().await?.into_iter().map(|m| m.id).collect())
    }

    /// Model list with metadata; remembers reported context windows for compression.
    pub async fn list_model_info(&mut self) -> Result<Vec<crate::provider::ModelInfo>> {
        let (c, p) = self.client().await?;
        let list = c.list_model_info().await?;
        for m in &list {
            if let Some(n) = m.context_length {
                self.model_context.insert(format!("{}/{}", p.id, m.id), n);
            }
        }
        Ok(list)
    }

    /// The context window to plan for: the provider's setting, else what the server reported for
    /// this model, else the global default.
    pub fn context_window(&self) -> u32 {
        let Ok(p) = self.provider() else { return self.config.agent.default_context_window };
        if p.context_window > 0 {
            return p.context_window;
        }
        let model = self.config.active_model_name().unwrap_or_default();
        self.model_context.get(&format!("{}/{model}", p.id)).copied().unwrap_or(self.config.agent.default_context_window)
    }

    /// Estimated tokens the next request will send.
    pub fn context_estimate(&self) -> usize {
        estimate_tokens(&self.messages, &self.system_prompt)
    }

    fn commit(&mut self, m: Message, ev: &Events) {
        if let Err(e) = self.sessions.append(&self.session.id, &m) {
            let _ = ev.send(AgentEvent::Notice(format!("couldn't save message: {e}")));
        }
        self.messages.push(m.clone());
        let _ = ev.send(AgentEvent::Committed(m));
    }

    /// Run one user turn to completion (or cancellation). Errors are reported as events.
    pub async fn run_turn(&mut self, user_text: &str, ev: &Events, cancel: &CancellationToken) {
        let _ = ev.send(AgentEvent::TurnStarted);
        if let Err(e) = self.run_turn_inner(user_text, ev, cancel).await {
            let msg = match e.downcast_ref::<ProviderError>() {
                Some(ProviderError::Cancelled) => None,
                Some(ProviderError::Stalled(s)) => Some(format!(
                    "The server didn't answer for {s} s. It may still be loading the model or be overloaded; try /retry, or raise the stall timeout in Settings > Tools & safety."
                )),
                Some(pe) if pe.is_auth() => {
                    Some(format!("{pe}\n\nThe provider rejected the credentials. Check the API key, or sign in to Nous Portal again."))
                }
                Some(ProviderError::Connect { url, .. }) => Some(format!(
                    "Can't reach {url}. Is the server running?{}",
                    if crate::config::is_local_url(url) {
                        " For a local model, start Ollama / LM Studio / llama.cpp, or use Settings > Find local models."
                    } else {
                        ""
                    }
                )),
                _ => Some(format!("{e:#}")),
            };
            if let Some(m) = msg {
                let _ = ev.send(AgentEvent::Error(m));
            } else {
                let _ = ev.send(AgentEvent::Notice("Stopped.".into()));
            }
        }
        let _ = ev.send(AgentEvent::TurnFinished);
    }

    async fn run_turn_inner(&mut self, user_text: &str, ev: &Events, cancel: &CancellationToken) -> Result<()> {
        let (client, provider) = self.client().await?;
        let model = self.config.active_model_name().ok_or_else(|| anyhow!("No model selected. Pick one with /model or the model menu."))?;
        if self.messages.is_empty() && self.session.title.is_empty() {
            let t = title_from(user_text);
            let _ = self.sessions.rename(&self.session.id, &t);
            self.session.title = t;
        }
        if self.session.model != model || self.session.provider != provider.id {
            let _ = self.sessions.set_model(&self.session.id, &provider.id, &model);
            self.session.model = model.clone();
            self.session.provider = provider.id.clone();
        }
        let images = std::mem::take(&mut self.pending_images);
        self.commit(Message::user_with_images(user_text, images), ev);
        self.maybe_compress(&client, &provider, &model, ev, cancel).await?;

        let mut send_tools = provider.tools;
        if send_tools {
            self.ensure_mcp(ev).await;
        }
        let max_iter = self.config.agent.max_iterations.max(1);
        for iteration in 0..max_iter {
            let mut msgs = Vec::with_capacity(self.messages.len() + 1);
            msgs.push(Message::system(self.system_prompt.clone()));
            msgs.extend(self.messages.iter().cloned());
            let req = ChatRequest {
                model: model.clone(),
                messages: msgs,
                tools: if send_tools { self.tool_definitions().await } else { vec![] },
                temperature: self.config.agent.temperature,
            };
            let ev2 = ev.clone();
            let mut on_delta = move |d: StreamDelta| {
                let _ = ev2.send(match d {
                    StreamDelta::Text(t) => AgentEvent::TextDelta(t),
                    StreamDelta::Reasoning(r) => AgentEvent::ReasoningDelta(r),
                    StreamDelta::ToolCallStarted { name, .. } => AgentEvent::ToolCallStreaming { name },
                    StreamDelta::Retrying { attempt, wait, reason } => {
                        AgentEvent::Notice(format!("{reason}; trying again in {} s (attempt {attempt}).", wait.as_secs_f32().ceil() as u64))
                    }
                });
            };
            let completion = match client.chat_stream(&req, cancel, &mut on_delta).await {
                Err(ProviderError::Http { status: 400 | 404 | 422 | 500, body, .. })
                    if send_tools && iteration == 0 && looks_like_no_tool_support(&body) =>
                {
                    send_tools = false;
                    let _ = ev.send(AgentEvent::Notice(format!("{model} doesn't support tool calling here; continuing as plain chat. (Turn tools off for this provider in Settings to skip this check.)")));
                    let req = ChatRequest { tools: vec![], ..req };
                    client.chat_stream(&req, cancel, &mut on_delta).await?
                }
                Err(ProviderError::Interrupted { partial, reason }) => {
                    // Keep what arrived; the user can /continue or /retry.
                    let msg = partial.into_message();
                    if !msg.text().is_empty() || msg.reasoning.is_some() {
                        self.commit(msg, ev);
                    }
                    let _ = ev.send(AgentEvent::Error(format!(
                        "The reply was cut off ({reason}). What arrived is kept; /continue picks up from there, /retry starts over."
                    )));
                    return Ok(());
                }
                other => other?,
            };
            if let Some(u) = &completion.usage {
                self.usage.add(u);
                let _ = self.sessions.add_usage(&self.session.id, u);
                let _ = ev.send(AgentEvent::Usage(self.usage.clone()));
                let _ = ev.send(AgentEvent::Context { used: u.prompt_tokens + u.completion_tokens, window: self.context_window() });
            }
            let truncated = completion.truncated();
            let calls = completion.tool_calls.clone();
            let msg = completion.into_message();
            if msg.text().is_empty() && calls.is_empty() {
                let _ = ev.send(AgentEvent::Notice("The model returned an empty response.".into()));
            }
            self.commit(msg, ev);
            if calls.is_empty() {
                if truncated {
                    let _ =
                        ev.send(AgentEvent::Notice("The reply hit the model's output limit. /continue picks up where it stopped.".into()));
                }
                return Ok(());
            }
            for call in calls {
                if cancel.is_cancelled() {
                    // Keep the transcript valid: every tool call needs a result.
                    self.commit(Message::tool(&call.id, &call.function.name, "Error: cancelled by the user"), ev);
                    continue;
                }
                let out = self.run_tool(&call.id, &call.function.name, &call.function.arguments, ev, cancel).await;
                self.commit(Message::tool(&call.id, &call.function.name, out.content.clone()), ev);
                let _ = ev.send(AgentEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    output: out.content,
                    is_error: out.is_error,
                });
            }
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled.into());
            }
        }
        let _ = ev.send(AgentEvent::Notice(format!("Stopped after {max_iter} tool iterations. Say \"continue\" to keep going.")));
        Ok(())
    }

    /// Boxed with an explicit `Send` bound: `delegate_task` makes the agent loop recursive
    /// (agent → subagent → agent loop), and the compiler can't infer Send through that cycle.
    fn run_tool<'a>(
        &'a mut self,
        id: &'a str,
        name: &'a str,
        raw_args: &'a str,
        ev: &'a Events,
        cancel: &'a CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = tools::ToolOutput> + Send + 'a>> {
        Box::pin(self.run_tool_inner(id, name, raw_args, ev, cancel))
    }

    async fn run_tool_inner(&mut self, id: &str, name: &str, raw_args: &str, ev: &Events, cancel: &CancellationToken) -> tools::ToolOutput {
        let args = match tools::parse_args(raw_args) {
            Ok(a) => a,
            Err(e) => {
                let _ = ev.send(AgentEvent::ToolRunning {
                    id: id.into(),
                    name: name.into(),
                    summary: format!("{name} (bad arguments)"),
                    args: raw_args.into(),
                });
                return tools::ToolOutput::err(e);
            }
        };
        let summary = tools::summarize_call(name, &args);
        let _ = ev.send(AgentEvent::ToolRunning {
            id: id.into(),
            name: name.into(),
            summary,
            args: serde_json::to_string_pretty(&args).unwrap_or_default(),
        });
        if name == "terminal"
            && let Some(cmd) = args.get("command").and_then(Value::as_str)
        {
            let reasons = crate::approval::dangerous_reasons(cmd);
            if !reasons.is_empty() {
                match self.config.approvals {
                    ApprovalMode::Yolo => {}
                    ApprovalMode::Deny => {
                        return tools::ToolOutput::err(format!(
                            "blocked by approval policy ({}). Ask the user to run it, or find a safer way.",
                            reasons.join(", ")
                        ));
                    }
                    ApprovalMode::Ask => {
                        let (tx, rx) = oneshot::channel();
                        let _ = ev.send(AgentEvent::ApprovalNeeded {
                            command: cmd.into(),
                            reasons: reasons.iter().map(|r| r.to_string()).collect(),
                            respond: tx,
                        });
                        let approved = tokio::select! {
                            r = rx => r.unwrap_or(false),
                            _ = cancel.cancelled() => false,
                        };
                        if !approved {
                            return tools::ToolOutput::err("the user denied this command. Don't retry it; ask what they'd like instead.");
                        }
                    }
                }
            }
        }
        match name {
            "clarify" if self.config.tools.clarify && self.depth == 0 => return self.clarify(&args, ev, cancel).await,
            "delegate_task" if self.config.tools.delegate && self.depth == 0 => return self.delegate(&args, ev, cancel).await,
            n if n.starts_with("mcp_") => {
                let guard = self.tools.mcp.read().await;
                if let Some(m) = guard.as_ref().filter(|m| m.find(n).is_some()) {
                    return match m.call(n, args.clone()).await {
                        Ok((text, is_error)) => tools::ToolOutput {
                            content: crate::tools::terminal::truncate_middle(if text.is_empty() { "(no output)" } else { &text }, 40_000),
                            is_error,
                        },
                        Err(e) => tools::ToolOutput::err(format!("{e:#}")),
                    };
                }
            }
            _ => {}
        }
        let out = tools::execute(name, &args, &self.tools, cancel).await;
        if name == "terminal" {
            // `cd` may have changed the working directory.
            self.system_prompt = self.build_system_prompt();
        }
        out
    }

    /// Built-in tool schemas plus connected MCP tools.
    pub async fn tool_definitions(&self) -> Vec<Value> {
        let mut cfg = self.tools.config.clone();
        if self.depth > 0 {
            cfg.delegate = false;
            cfg.clarify = false;
        }
        let mut defs = tools::definitions(&cfg);
        if let Some(m) = self.tools.mcp.read().await.as_ref() {
            defs.extend(m.definitions());
        }
        defs
    }

    /// Connect configured MCP servers once (and again after the server list changes).
    pub async fn ensure_mcp(&mut self, ev: &Events) {
        if !self.config.mcp_servers.values().any(|c| c.enabled) {
            return;
        }
        let mut g = self.tools.mcp.write().await;
        if g.is_some() {
            return;
        }
        let _ = ev.send(AgentEvent::Notice("Starting MCP servers…".into()));
        let m = crate::mcp::McpManager::connect_all(&self.config.mcp_servers).await;
        let failed: Vec<String> = m.status.iter().filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}"))).collect();
        let _ = ev.send(AgentEvent::Notice(if failed.is_empty() {
            format!("MCP ready: {} tools from {} server(s).", m.tools.len(), m.status.len())
        } else {
            format!("MCP: {} tools ready; failed to start {}", m.tools.len(), failed.join("; "))
        }));
        *g = Some(m);
    }

    pub async fn mcp_status(&mut self, ev: &Events) -> String {
        self.ensure_mcp(ev).await;
        match self.tools.mcp.read().await.as_ref() {
            Some(m) => {
                let mut s = m.status_text();
                if !m.tools.is_empty() {
                    s.push_str("\nTools: ");
                    s.push_str(&m.tools.iter().map(|t| t.exposed.as_str()).collect::<Vec<_>>().join(", "));
                }
                s
            }
            None => "No MCP servers configured. Add them in Settings > Tools & safety, or import them from Hermes.".into(),
        }
    }

    async fn clarify(&mut self, args: &Value, ev: &Events, cancel: &CancellationToken) -> tools::ToolOutput {
        let question = args.get("question").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if question.is_empty() {
            return tools::ToolOutput::err("question is required");
        }
        let choices: Vec<String> = args
            .get("choices")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).take(5).collect())
            .unwrap_or_default();
        let (tx, rx) = oneshot::channel();
        let _ = ev.send(AgentEvent::Clarify { question, choices, respond: tx });
        let answer = tokio::select! {
            r = rx => r.ok(),
            _ = cancel.cancelled() => None,
        };
        match answer.filter(|a| !a.trim().is_empty()) {
            Some(a) => tools::ToolOutput::ok(format!("The user answered: {a}")),
            None => tools::ToolOutput::ok("The user didn't answer. Make a reasonable assumption, say what you assumed, and continue."),
        }
    }

    /// A subagent sharing this agent's config, working directory, MCP servers and checkpoints,
    /// with a fresh conversation that isn't saved to history.
    pub fn child(&self) -> Result<Agent> {
        let mut cfg = self.config.clone();
        cfg.tools.delegate = false;
        cfg.tools.clarify = false;
        cfg.agent.max_iterations = cfg.agent.max_iterations.min(40);
        let store = Arc::new(SessionStore::in_memory()?);
        let mut a = Agent::with_store(self.paths.clone(), cfg, store, None)?;
        a.depth = self.depth + 1;
        *a.tools.workdir.lock().unwrap_or_else(|e| e.into_inner()) = self.workdir();
        a.tools.mcp = self.tools.mcp.clone();
        a.tools.processes = self.tools.processes.clone();
        a.tools.checkpoints = self.tools.checkpoints.clone();
        a.memory_snapshot = self.memory_snapshot.clone();
        a.system_prompt = a.build_system_prompt();
        Ok(a)
    }

    async fn delegate(&mut self, args: &Value, ev: &Events, cancel: &CancellationToken) -> tools::ToolOutput {
        let mut tasks: Vec<(String, String)> = args
            .get("tasks")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        let goal = t.get("goal").and_then(Value::as_str).or_else(|| t.as_str())?.trim().to_string();
                        Some((goal, t.get("context").and_then(Value::as_str).unwrap_or("").to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if tasks.is_empty()
            && let Some(g) = args.get("goal").and_then(Value::as_str)
        {
            tasks.push((g.to_string(), args.get("context").and_then(Value::as_str).unwrap_or("").to_string()));
        }
        tasks.retain(|(g, _)| !g.is_empty());
        if tasks.is_empty() {
            return tools::ToolOutput::err("give at least one task with a goal");
        }
        tasks.truncate(3);
        let mut runs: Vec<SubagentRun> = Vec::new();
        for (i, (goal, context)) in tasks.into_iter().enumerate() {
            let mut child = match self.child() {
                Ok(c) => c,
                Err(e) => return tools::ToolOutput::err(format!("couldn't start a subagent: {e:#}")),
            };
            let tag = format!("sub{}", i + 1);
            let (ctx_tx, mut ctx_rx) = mpsc::unbounded_channel::<AgentEvent>();
            let parent = ev.clone();
            let tag2 = tag.clone();
            tokio::spawn(async move {
                while let Some(e) = ctx_rx.recv().await {
                    let fwd = match e {
                        AgentEvent::ApprovalNeeded { command, reasons, respond } => Some(AgentEvent::ApprovalNeeded {
                            command,
                            reasons: reasons.into_iter().chain([format!("from subagent {tag2}")]).collect(),
                            respond,
                        }),
                        AgentEvent::ToolRunning { id, name, summary, args } => {
                            Some(AgentEvent::ToolRunning { id: format!("{tag2}:{id}"), name, summary: format!("[{tag2}] {summary}"), args })
                        }
                        AgentEvent::ToolFinished { id, name, output, is_error } => {
                            Some(AgentEvent::ToolFinished { id: format!("{tag2}:{id}"), name, output, is_error })
                        }
                        AgentEvent::Error(m) => Some(AgentEvent::Notice(format!("[{tag2}] error: {m}"))),
                        _ => None,
                    };
                    if let Some(f) = fwd {
                        let _ = parent.send(f);
                    }
                }
            });
            let cancel = cancel.clone();
            let prompt = format!(
                "Your task: {goal}\n\nContext from the main agent:\n{}\n\nWork autonomously with your tools, then report back concisely.",
                if context.trim().is_empty() { "(none)" } else { context.trim() }
            );
            runs.push(Box::pin(async move {
                child.run_turn(&prompt, &ctx_tx, &cancel).await;
                let report = child
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
                    .map(|m| m.text().to_string())
                    .unwrap_or_else(|| "(the subagent produced no report)".into());
                (goal, report, child.usage.clone())
            }));
        }
        let _ = ev.send(AgentEvent::Notice(format!("Running {} subagent(s)…", runs.len())));
        let results = futures_util::future::join_all(runs).await;
        let mut out = String::new();
        for (i, (goal, report, usage)) in results.into_iter().enumerate() {
            self.usage.add(&usage);
            let _ = self.sessions.add_usage(&self.session.id, &usage);
            out.push_str(&format!("## Subagent {} — {goal}\n{}\n\n", i + 1, report.trim()));
        }
        let _ = ev.send(AgentEvent::Usage(self.usage.clone()));
        if cancel.is_cancelled() {
            return tools::ToolOutput::err("cancelled");
        }
        tools::ToolOutput::ok(out.trim_end().to_string())
    }

    async fn maybe_compress(
        &mut self,
        client: &Client,
        provider: &ProviderConfig,
        model: &str,
        ev: &Events,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _ = provider;
        let window = self.context_window() as usize;
        let est = estimate_tokens(&self.messages, &self.system_prompt);
        if (est as f32) < window as f32 * self.config.agent.compression_threshold {
            return Ok(());
        }
        match self.compress(client, model, cancel).await {
            Ok(Some((before, after))) => {
                let _ = ev.send(AgentEvent::Notice(format!(
                    "Context was getting full (~{est} tokens); summarised {before} older messages into {after}."
                )));
                let _ = ev.send(AgentEvent::TranscriptReplaced);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = ev.send(AgentEvent::Notice(format!("Couldn't compress context: {e:#}")));
            }
        }
        Ok(())
    }

    /// Where to cut so the tail starts at a user message and keeps at least `keep` messages.
    pub fn compression_cut(msgs: &[Message], keep: usize) -> Option<usize> {
        if msgs.len() <= keep + 2 {
            return None;
        }
        (2..=msgs.len().saturating_sub(keep)).rev().find(|&i| msgs[i].role == Role::User)
    }

    /// Summarise everything before the cut. Returns (messages replaced, messages now).
    pub async fn compress(&mut self, client: &Client, model: &str, cancel: &CancellationToken) -> Result<Option<(usize, usize)>> {
        let Some(cut) = Self::compression_cut(&self.messages, 4) else { return Ok(None) };
        let head = &self.messages[..cut];
        let mut transcript = String::new();
        for m in head {
            let who = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
                Role::Tool => "Tool result",
                Role::System => "System",
            };
            let mut t = m.text().to_string();
            for c in &m.tool_calls {
                t.push_str(&format!("\n[called {} {}]", c.function.name, crate::provider::truncate(&c.function.arguments, 300)));
            }
            transcript.push_str(&format!("{who}: {}\n\n", crate::provider::truncate(&t, 2000)));
        }
        let prompt = vec![
            Message::system(
                "You compress conversations. Write a dense summary that preserves: the user's goals and constraints, decisions made, files and commands involved, facts learned, and unfinished work. Use bullet points. No preamble.",
            ),
            Message::user(format!(
                "Summarise this conversation so far:\n\n{}",
                crate::tools::terminal::truncate_middle(&transcript, 60_000)
            )),
        ];
        let summary = client.complete_text(model, prompt, cancel).await?;
        if summary.trim().is_empty() {
            return Err(anyhow!("the model returned an empty summary"));
        }
        let mut next = vec![
            Message::user(format!("[Summary of our earlier conversation]\n{}", summary.trim())),
            Message::assistant("Understood. I have the summary of our earlier conversation and will continue from it."),
        ];
        next.extend_from_slice(&self.messages[cut..]);
        let before = cut;
        self.messages = next;
        self.sessions.replace_messages(&self.session.id, &self.messages)?;
        Ok(Some((before, 2)))
    }

    pub fn new_session(&mut self) -> Result<()> {
        self.session = Self::fresh_session(&self.sessions, &self.config)?;
        self.tools.switch_session(&self.session.id.clone(), &self.paths.profile_dir());
        self.messages.clear();
        self.usage = Usage::default();
        self.memory_snapshot = self.tools.memory.snapshot();
        self.system_prompt = self.build_system_prompt();
        Ok(())
    }

    pub fn load_session(&mut self, id: &str) -> Result<()> {
        let meta = self.sessions.get(id)?.ok_or_else(|| anyhow!("no session {id}"))?;
        self.messages = self.sessions.messages(id)?;
        self.usage = meta.usage.clone();
        self.session = meta;
        self.tools.switch_session(&self.session.id.clone(), &self.paths.profile_dir());
        self.memory_snapshot = self.tools.memory.snapshot();
        self.system_prompt = self.build_system_prompt();
        Ok(())
    }

    /// Remove the last user message and everything after it; returns its text.
    pub fn pop_last_exchange(&mut self) -> Result<Option<String>> {
        let Some(i) = self.messages.iter().rposition(|m| m.role == Role::User) else { return Ok(None) };
        let text = self.messages[i].text().to_string();
        self.pending_images = self.messages[i].images.clone();
        self.messages.truncate(i);
        self.sessions.replace_messages(&self.session.id, &self.messages)?;
        Ok(Some(text))
    }

    pub fn set_model(&mut self, provider: Option<&str>, model: &str) -> Result<String> {
        let pid = match provider {
            Some(p) => p.to_string(),
            None => self.provider()?.id,
        };
        if !self.config.set_active(&pid, Some(model.to_string())) {
            return Err(anyhow!("no provider `{pid}`"));
        }
        self.save_config()?;
        let _ = self.sessions.set_model(&self.session.id, &pid, model);
        self.session.provider = pid.clone();
        self.session.model = model.to_string();
        self.system_prompt = self.build_system_prompt();
        Ok(format!("Now using {model} via {pid}."))
    }

    pub async fn handle_command(&mut self, cmd: Command, ev: &Events, cancel: &CancellationToken) -> Result<CommandOutcome> {
        use CommandOutcome::*;
        Ok(match cmd {
            Command::New => {
                self.new_session()?;
                Reload("New conversation.".into())
            }
            Command::Retry => match self.pop_last_exchange()? {
                Some(t) => Send(t),
                None => Text("Nothing to retry.".into()),
            },
            Command::Continue => {
                if self.messages.last().is_some_and(|m| m.role == Role::Assistant) {
                    Send("Continue exactly where you stopped, without repeating what you already wrote.".into())
                } else {
                    Text("Nothing to continue.".into())
                }
            }
            Command::Undo => match self.pop_last_exchange()? {
                Some(_) => {
                    self.pending_images.clear();
                    Reload("Removed the last exchange.".into())
                }
                None => Text("Nothing to undo.".into()),
            },
            Command::Compress => {
                let (client, _) = self.client().await?;
                let model = self.config.active_model_name().ok_or_else(|| anyhow!("no model selected"))?;
                match self.compress(&client, &model, cancel).await? {
                    Some((b, a)) => Reload(format!("Summarised {b} messages into {a}.")),
                    None => Text("Not enough conversation to compress yet.".into()),
                }
            }
            Command::Usage => {
                let est = estimate_tokens(&self.messages, &self.system_prompt);
                Text(format!(
                    "This session: {} prompt + {} completion = {} tokens reported by the server.\nCurrent context: ~{est} tokens across {} messages.",
                    self.usage.prompt_tokens,
                    self.usage.completion_tokens,
                    self.usage.total_tokens,
                    self.messages.len()
                ))
            }
            Command::Help => Text(commands::help_text()),
            Command::Stop => Stop,
            Command::Yolo(v) => {
                let on = v.unwrap_or(self.config.approvals != ApprovalMode::Yolo);
                self.config.approvals = if on { ApprovalMode::Yolo } else { ApprovalMode::Ask };
                self.save_config()?;
                Text(if on {
                    "YOLO on: dangerous commands run without asking.".into()
                } else {
                    "Approvals on: you'll be asked before dangerous commands.".into()
                })
            }
            Command::Model(None) => {
                let current = self.config.active_model_name().unwrap_or_else(|| "(none)".into());
                let list = match self.list_models().await {
                    Ok(m) if !m.is_empty() => m.iter().map(|x| format!("  {x}")).collect::<Vec<_>>().join("\n"),
                    Ok(_) => "  (the server listed no models)".into(),
                    Err(e) => format!("  (couldn't list models: {e:#})"),
                };
                Text(format!("Current: {current}\nAvailable:\n{list}"))
            }
            Command::Model(Some(spec)) => {
                let ids: Vec<String> = self.config.providers.iter().map(|p| p.id.clone()).collect();
                let (p, m) = commands::split_model_spec(&spec, &ids);
                Text(self.set_model(p.as_deref(), &m)?)
            }
            Command::Provider(None) => {
                let active = self.config.active().map(|p| p.id.clone()).unwrap_or_default();
                Text(
                    self.config
                        .providers
                        .iter()
                        .map(|p| format!("{} {} — {} ({})", if p.id == active { "*" } else { " " }, p.id, p.name, p.base_url))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            Command::Provider(Some(id)) => {
                let model = self.config.provider(&id).map(|p| p.default_model.clone()).ok_or_else(|| anyhow!("no provider `{id}`"))?;
                self.config.set_active(&id, Some(model).filter(|m| !m.is_empty()));
                self.save_config()?;
                self.system_prompt = self.build_system_prompt();
                Text(format!("Switched to {id}. Pick a model with /model if needed."))
            }
            Command::Personality(None) => Text(format!(
                "Current: {}\nAvailable: {}",
                self.config.agent.personality,
                personality_names(&self.config.custom_personalities).join(", ")
            )),
            Command::Personality(Some(p)) => {
                if personality_prompt(&p, &self.config.custom_personalities).is_none() {
                    return Ok(Text(format!("Unknown personality `{p}`.")));
                }
                self.config.agent.personality = p.clone();
                self.save_config()?;
                self.system_prompt = self.build_system_prompt();
                Text(format!("Personality set to {p}."))
            }
            Command::Title(t) => {
                if t.trim().is_empty() {
                    return Ok(Text(format!("Title: {}", self.session.title)));
                }
                self.sessions.rename(&self.session.id, &t)?;
                self.session.title = t.trim().to_string();
                Text(format!("Renamed to “{}”.", self.session.title))
            }
            Command::Skills => {
                let idx = self.tools.skills.index();
                Text(if idx.is_empty() {
                    "No skills yet. The agent creates them as it learns, or drop SKILL.md folders into the skills directory.".into()
                } else {
                    idx
                })
            }
            Command::Skill { name, rest } => {
                let skill = self.tools.skills.get(&name).ok_or_else(|| anyhow!("no skill {name}"))?;
                let body = std::fs::read_to_string(skill.skill_md())?;
                Send(commands::skill_invocation(&name, crate::skills::split_frontmatter(&body).1, &rest))
            }
            Command::Sessions => Text(
                self.sessions
                    .list(15)?
                    .iter()
                    .map(|s| {
                        format!(
                            "{}  {}  {}",
                            s.updated_at.with_timezone(&chrono::Local).format("%m-%d %H:%M"),
                            &s.id[..8],
                            if s.title.is_empty() { "(untitled)" } else { &s.title }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Command::Search(q) => {
                let hits = self.sessions.search(&q, 15)?;
                Text(if hits.is_empty() {
                    "No matches.".into()
                } else {
                    hits.iter().map(|h| format!("{}  {}: {}", &h.session_id[..8], h.title, h.snippet)).collect::<Vec<_>>().join("\n")
                })
            }
            Command::Memory => {
                let snap = self.tools.memory.snapshot();
                Text(if snap.is_empty() {
                    format!("Nothing remembered yet. (Files live in {})", self.tools.memory.dir().display())
                } else {
                    let (mu, mc) = self.tools.memory.usage(Target::Memory);
                    let (uu, uc) = self.tools.memory.usage(Target::User);
                    format!("{snap}Used: MEMORY {mu}/{mc}, USER {uu}/{uc} chars.")
                })
            }
            Command::Tools => {
                let provider_tools = self.config.active().map(|p| p.tools).unwrap_or(true);
                let names: Vec<String> = tools::definitions(&self.tools.config)
                    .iter()
                    .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
                    .collect();
                Text(format!(
                    "{}\nApprovals: {:?}",
                    if provider_tools { format!("Enabled: {}", names.join(", ")) } else { "Tools are off for this provider.".into() },
                    self.config.approvals
                ))
            }
            Command::Cwd(None) => Text(self.workdir().display().to_string()),
            Command::Cwd(Some(p)) => {
                self.set_workdir(Path::new(&p))?;
                Text(format!("Working directory: {}", self.workdir().display()))
            }
            Command::Rollback(None) => Text(self.tools.checkpoints.render()),
            Command::Rollback(Some(n)) => {
                let done = self.tools.checkpoints.rollback(n)?;
                Text(format!("Rolled back {} change(s):\n{}", done.len(), done.join("\n")))
            }
            Command::Insights(days) => Text(self.sessions.insights(days.unwrap_or(30))?.render()),
            Command::Mcp => Text(self.mcp_status(ev).await),
            Command::Unknown(c) => {
                let _ = ev;
                Text(format!("Unknown command /{c}. Type /help for the list."))
            }
        })
    }
}

fn looks_like_no_tool_support(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    (b.contains("tool") || b.contains("function"))
        && (b.contains("not support")
            || b.contains("unsupported")
            || b.contains("does not support")
            || b.contains("not enabled")
            || b.contains("tool_choice"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_points_start_at_user_messages() {
        let mut msgs = vec![Message::user("u1"), Message::assistant("a1")];
        assert_eq!(Agent::compression_cut(&msgs, 4), None);
        for i in 2..6 {
            msgs.push(Message::user(format!("u{i}")));
            let mut a = Message::assistant("");
            a.tool_calls.push(crate::provider::ToolCall {
                id: format!("c{i}"),
                kind: "function".into(),
                function: crate::provider::FunctionCall { name: "t".into(), arguments: "{}".into() },
            });
            msgs.push(a);
            msgs.push(Message::tool(&format!("c{i}"), "t", "r"));
        }
        let cut = Agent::compression_cut(&msgs, 4).unwrap();
        assert_eq!(msgs[cut].role, Role::User);
        assert!(msgs.len() - cut >= 4);
    }

    #[test]
    fn tool_support_errors() {
        assert!(looks_like_no_tool_support("{\"error\":\"registry.ollama.ai/library/gemma:2b does not support tools\"}"));
        assert!(looks_like_no_tool_support("tool_choice auto is not supported"));
        assert!(!looks_like_no_tool_support("model not found"));
    }

    #[test]
    fn prompt_includes_context() {
        let d = tempfile::tempdir().unwrap();
        let work = d.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("AGENTS.md"), "Use 4-space indents.").unwrap();
        let mut cfg = Config::default();
        cfg.agent.workdir = Some(work.clone());
        cfg.agent.personality = "concise".into();
        cfg.upsert_provider(ProviderConfig::openai("x", "X", "http://127.0.0.1:1/v1"));
        let paths = Paths::under(d.path().join("home"));
        let mut agent = Agent::new(paths, cfg, None).unwrap();
        agent.tools.memory.add(Target::User, "Name: Ali").unwrap();
        assert!(agent.system_prompt().contains("Use 4-space indents."));
        assert!(agent.system_prompt().contains("Be brief."));
        // Memory is a frozen snapshot: a mid-session write doesn't change the prompt...
        assert!(!agent.build_system_prompt().contains("Name: Ali"));
        // ...but the next session sees it.
        agent.new_session().unwrap();
        assert!(agent.system_prompt().contains("Name: Ali"));
    }
}
