//! Desktop front end (eframe/egui).

pub mod browse;
pub mod chat;
pub mod palette;
mod settings;
pub mod theme;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, Key, KeyboardShortcut, Layout, Modifiers, RichText, Ui};
use egui_commonmark::CommonMarkCache;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent, CommandOutcome};
use crate::attach::Attachment;
use crate::commands::{self, Command};
use crate::config::{Config, Paths, ProviderKind};
use crate::discovery::Discovered;
use crate::nous::DeviceCode;
use crate::provider::{Message, ModelInfo, Role, Usage};
use crate::session::{SearchHit, SessionMeta, SessionStore};
use chat::ChatItem;

#[allow(clippy::large_enum_variant)]
pub enum UiMsg {
    Agent(AgentEvent),
    McpStatus(String),
    /// (Mercury profile imported into, summary) or an error.
    HermesDone(Result<(String, String), String>),
    Picked {
        purpose: browse::PickPurpose,
        path: PathBuf,
    },
    /// The system file dialog couldn't be shown: use the built-in browser.
    OpenBrowser(browse::PickPurpose),
    Transcript {
        session: SessionMeta,
        messages: Vec<Message>,
        workdir: PathBuf,
        usage: Usage,
    },
    Config(Config),
    Models {
        provider: String,
        result: Result<Vec<ModelInfo>, String>,
    },
    /// "Test connection" finished for a provider.
    Probe {
        provider: String,
        ok: bool,
        summary: String,
    },
    /// Profiles were added or exported; refresh the list and show this.
    ProfilesChanged(Result<String, String>),
    Discovered(Vec<Discovered>),
    NousCode(DeviceCode),
    NousDone(Result<String, String>),
    Info(String),
    UserEcho(String),
    Toast(String),
    Busy(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Providers,
    Nous,
    Tools,
    Memory,
    Skills,
    Hermes,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Chat,
    Settings(Tab),
}

#[derive(Default, Clone)]
pub struct ProviderDraft {
    pub name: String,
    pub url: String,
    pub key: String,
    /// Variable to read the key from when none is typed (set by presets).
    pub key_env: String,
    pub model: String,
}

/// Give a widget a screen-reader name (ComboBoxes without a visible label have none).
pub(crate) fn a11y_label(r: &egui::Response, label: &str) {
    r.ctx.accesskit_node_builder(r.id, |n| n.set_label(label));
}

pub struct Approval {
    pub command: String,
    pub reasons: Vec<String>,
    respond: oneshot::Sender<bool>,
}

/// A question the model asked through `clarify`.
pub struct Clarify {
    pub question: String,
    pub choices: Vec<String>,
    pub answer: String,
    respond: oneshot::Sender<String>,
}

#[derive(Default, Clone)]
pub struct McpDraft {
    pub name: String,
    pub command: String,
    pub url: String,
}

pub struct MercuryApp {
    rt: Arc<tokio::runtime::Runtime>,
    agent: Arc<tokio::sync::Mutex<Agent>>,
    store: Arc<SessionStore>,
    pub paths: Paths,
    pub config: Config,
    pub config_dirty: bool,
    tx: Sender<UiMsg>,
    rx: Receiver<UiMsg>,
    repaint: Arc<Mutex<Option<egui::Context>>>,
    pub items: Vec<ChatItem>,
    pub session: SessionMeta,
    pub sessions: Vec<SessionMeta>,
    pub input: String,
    pub busy: bool,
    cancel: Option<CancellationToken>,
    pub approval: Option<Approval>,
    pub view: View,
    pub models: HashMap<String, Vec<String>>,
    pub search: String,
    search_hits: Vec<SearchHit>,
    pub discovered: Vec<Discovered>,
    pub discovering: bool,
    pub nous_code: Option<DeviceCode>,
    nous_cancel: Option<CancellationToken>,
    pub nous_status: String,
    toasts: Vec<(String, Instant)>,
    md_cache: CommonMarkCache,
    pub usage: Usage,
    pub workdir: PathBuf,
    pub draft: ProviderDraft,
    memory_draft: Option<(String, String)>,
    selected_skill: Option<String>,
    renaming: Option<(String, String)>,
    theme_applied: Option<(bool, f32)>,
    pub hermes_detected: Option<PathBuf>,
    workdir_draft: String,
    pub clarify: Option<Clarify>,
    pub hermes_path: String,
    pub hermes_profiles: Vec<(crate::hermes_import::HermesProfile, crate::hermes_import::ProfileSummary)>,
    pub hermes_sessions: bool,
    pub hermes_busy: bool,
    pub hermes_result: Option<Result<String, String>>,
    pub mcp_draft: McpDraft,
    pub mcp_status: String,
    /// Cached logo textures keyed by (logo, custom image path, dark theme).
    logo_tex: HashMap<(crate::config::Logo, Option<PathBuf>, bool), egui::TextureHandle>,
    icon_applied: Option<(crate::config::Logo, Option<PathBuf>)>,
    pub logo_path_draft: String,
    pub browser: Option<browse::Browser>,
    /// Mercury profiles, `default` first.
    pub profiles: Vec<String>,
    pub new_profile: String,
    /// A profile the user asked to delete, waiting for "Yes, delete".
    pub confirm_delete: Option<String>,
    /// Index into `hermes_profiles` chosen in the dropdown.
    pub hermes_selected: usize,
    /// Mercury profile to import into ("" = same name as the Hermes profile).
    pub hermes_target: String,
    pub hermes_mode: crate::config::LinkMode,
    hermes_scanned: bool,
    /// Files attached to the message being written.
    pub attachments: Vec<Attachment>,
    /// Unsent text per chat, so switching chats doesn't lose a half-written message.
    pub drafts: HashMap<String, String>,
    /// Earlier messages for ↑/↓ in an empty composer, newest first.
    pub history: Vec<String>,
    history_pos: Option<usize>,
    pub palette: Option<palette::Palette>,
    pub find: Option<FindBar>,
    pub show_archived: bool,
    /// (tokens used by the last request, context window).
    pub context: (u64, u32),
    /// Tokens per second of the last reply.
    pub speed: Option<f32>,
    first_token: Option<Instant>,
    streamed_chars: usize,
    speed_measured: bool,
    /// Metadata from model lists, keyed `provider/model`.
    pub model_meta: HashMap<String, ModelInfo>,
    /// Last "Test connection" result per provider: (ok, summary).
    pub probes: HashMap<String, (bool, String)>,
    pub probing: std::collections::HashSet<String>,
    /// Text being edited for a provider's headers / extra JSON, with a parse error if any.
    pub provider_text: HashMap<String, ProviderText>,
    logs_dir: PathBuf,
}

#[derive(Default, Clone)]
pub struct FindBar {
    pub query: String,
    pub current: usize,
    /// Scroll to the current match on the next frame.
    pub scroll: bool,
    focused: bool,
}

#[derive(Default, Clone)]
pub struct ProviderText {
    pub headers: String,
    pub extra: String,
    pub error: Option<String>,
}

fn send(tx: &Sender<UiMsg>, repaint: &Arc<Mutex<Option<egui::Context>>>, m: UiMsg) {
    let _ = tx.send(m);
    if let Some(ctx) = repaint.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        ctx.request_repaint();
    }
}

impl MercuryApp {
    pub fn new(paths: Paths, config: Config, rt: Arc<tokio::runtime::Runtime>) -> anyhow::Result<Self> {
        paths.ensure()?;
        let store = Arc::new(SessionStore::open(&paths.sessions_db())?);
        let resume = if config.ui.resume_last { store.latest().ok().flatten().map(|s| s.id) } else { None };
        let agent = Agent::with_store(paths.clone(), config.clone(), store.clone(), resume.as_deref())?;
        let _ = store.prune_empty(&agent.session.id);
        let session = agent.session.clone();
        let workdir = agent.workdir();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = Self {
            rt,
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            store,
            paths,
            config,
            config_dirty: false,
            tx,
            rx,
            repaint: Arc::new(Mutex::new(None)),
            items: vec![],
            session,
            sessions: vec![],
            input: String::new(),
            busy: false,
            cancel: None,
            approval: None,
            view: View::Chat,
            models: HashMap::new(),
            search: String::new(),
            search_hits: vec![],
            discovered: vec![],
            discovering: false,
            nous_code: None,
            nous_cancel: None,
            nous_status: String::new(),
            toasts: vec![],
            md_cache: CommonMarkCache::default(),
            usage: Usage::default(),
            workdir_draft: workdir.display().to_string(),
            workdir,
            draft: ProviderDraft::default(),
            memory_draft: None,
            selected_skill: None,
            renaming: None,
            theme_applied: None,
            hermes_path: crate::hermes_import::detect()
                .or_else(crate::hermes_import::hermes_home)
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            hermes_detected: crate::hermes_import::detect(),
            clarify: None,
            hermes_profiles: vec![],
            hermes_sessions: true,
            hermes_busy: false,
            hermes_result: None,
            mcp_draft: McpDraft::default(),
            mcp_status: String::new(),
            logo_tex: HashMap::new(),
            icon_applied: None,
            logo_path_draft: String::new(),
            browser: None,
            profiles: vec![],
            new_profile: String::new(),
            confirm_delete: None,
            hermes_selected: 0,
            hermes_target: String::new(),
            hermes_mode: crate::config::LinkMode::Copy,
            hermes_scanned: false,
            attachments: vec![],
            drafts: HashMap::new(),
            history: vec![],
            history_pos: None,
            palette: None,
            find: None,
            show_archived: false,
            context: (0, 0),
            speed: None,
            first_token: None,
            streamed_chars: 0,
            speed_measured: false,
            model_meta: HashMap::new(),
            probes: HashMap::new(),
            probing: Default::default(),
            provider_text: HashMap::new(),
            logs_dir: PathBuf::new(),
        };
        app.logs_dir = app.paths.data_dir.join("logs");
        if let Some(report) = crate::crashlog::take_previous_crash(&app.logs_dir) {
            app.toast(format!("Mercury closed unexpectedly last time. The report is in {}", report.display()));
        }
        app.history = app.store.recent_user_messages(100).unwrap_or_default();
        app.profiles = crate::profiles::list(&app.paths);
        app.refresh_sessions();
        if resume.is_some() {
            app.reload_transcript();
        }
        if let Some(p) = app.config.active().map(|p| p.id.clone()) {
            app.fetch_models(&p);
        }
        Ok(app)
    }

    fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(&self, f: F) {
        self.rt.spawn(f);
    }

    /// Run agent work so that a panic inside it is reported and the UI leaves its busy state,
    /// instead of the turn hanging forever.
    pub fn spawn_guarded<F: std::future::Future<Output = ()> + Send + 'static>(&self, f: F) {
        use futures_util::FutureExt;
        let (tx, rp) = self.sender();
        let logs = self.logs_dir.clone();
        self.rt.spawn(async move {
            if let Err(p) = std::panic::AssertUnwindSafe(f).catch_unwind().await {
                let report = crate::crashlog::file_recovered(&logs).map(|p| format!(" Details: {}", p.display())).unwrap_or_default();
                send(
                    &tx,
                    &rp,
                    UiMsg::Agent(AgentEvent::Error(format!(
                        "Something went wrong inside Mercury ({}). The chat is saved; you can keep going.{report}",
                        crate::crashlog::panic_message(&*p)
                    ))),
                );
                send(&tx, &rp, UiMsg::Busy(false));
            }
        });
    }

    fn sender(&self) -> (Sender<UiMsg>, Arc<Mutex<Option<egui::Context>>>) {
        (self.tx.clone(), self.repaint.clone())
    }

    /// An AgentEvent channel whose events are forwarded to the UI.
    fn event_bridge(&self) -> crate::agent::Events {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, rp) = self.sender();
        self.rt.spawn(async move {
            while let Some(ev) = erx.recv().await {
                send(&tx, &rp, UiMsg::Agent(ev));
            }
        });
        etx
    }

    pub fn toast(&mut self, s: impl Into<String>) {
        self.toasts.push((s.into(), Instant::now()));
    }

    pub fn refresh_sessions(&mut self) {
        let current = self.session.id.clone();
        let _ = current;
        self.sessions = self.store.list(200).unwrap_or_default().into_iter().filter(|s| s.message_count > 0).collect();
    }

    pub fn skill_names(&self) -> Vec<String> {
        self.config.skill_library(&self.paths).list().into_iter().map(|s| s.name).collect()
    }

    /// Drain background messages. Public so tests can pump the UI state without rendering.
    pub fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            self.handle(m);
        }
    }

    fn handle(&mut self, m: UiMsg) {
        match m {
            UiMsg::Agent(ev) => self.handle_event(ev),
            UiMsg::McpStatus(s) => self.mcp_status = s,
            UiMsg::HermesDone(r) => {
                self.hermes_busy = false;
                match r {
                    Ok((target, msg)) => {
                        // Open the profile that was imported into (reloads chats, memory, model).
                        self.switch_profile(&target, true);
                        self.items.push(ChatItem::Info(msg.clone()));
                        self.hermes_result = Some(Ok(msg));
                    }
                    Err(e) => self.hermes_result = Some(Err(e)),
                }
            }
            UiMsg::Picked { purpose, path } => self.picked(purpose, path),
            UiMsg::OpenBrowser(purpose) => {
                let start = browse::start_dir(purpose, &self.pick_field(purpose));
                self.browser = Some(browse::Browser::new(purpose, start));
            }
            UiMsg::Transcript { session, messages, workdir, usage } => {
                self.items = chat::from_messages(&messages);
                self.session = session;
                self.usage = usage;
                self.workdir_draft = workdir.display().to_string();
                self.workdir = workdir;
                self.refresh_sessions();
            }
            UiMsg::Config(c) => {
                self.config = c;
                self.config_dirty = false;
            }
            UiMsg::Models { provider, result } => match result {
                Ok(list) => {
                    for m in &list {
                        self.model_meta.insert(format!("{provider}/{}", m.id), m.clone());
                    }
                    self.models.insert(provider, list.into_iter().map(|m| m.id).collect());
                }
                Err(e) => {
                    self.models.remove(&provider);
                    self.toast(format!("Couldn't list models: {e}"));
                }
            },
            UiMsg::Probe { provider, ok, summary } => {
                self.probing.remove(&provider);
                self.probes.insert(provider, (ok, summary));
            }
            UiMsg::ProfilesChanged(r) => {
                self.refresh_profiles();
                match r {
                    Ok(m) => self.toast(m),
                    Err(e) => self.toast(format!("Couldn't do that: {e}")),
                }
            }
            UiMsg::Discovered(list) => {
                self.discovering = false;
                if list.is_empty() {
                    self.toast("No OpenAI-compatible servers found on the usual ports.");
                }
                self.discovered = list;
            }
            UiMsg::NousCode(dc) => {
                self.nous_status = "Waiting for you to approve in the browser…".into();
                self.nous_code = Some(dc);
            }
            UiMsg::NousDone(r) => {
                self.nous_code = None;
                self.nous_cancel = None;
                match r {
                    Ok(s) => {
                        self.nous_status = s.clone();
                        self.toast(s);
                        if let Some(p) = self.config.providers.iter().find(|p| p.kind == ProviderKind::NousOauth).map(|p| p.id.clone()) {
                            self.fetch_models(&p);
                        }
                    }
                    Err(e) => self.nous_status = e,
                }
            }
            UiMsg::Info(t) => self.items.push(ChatItem::Info(t)),
            UiMsg::UserEcho(t) => self.items.push(ChatItem::User(t)),
            UiMsg::Toast(t) => self.toast(t),
            UiMsg::Busy(b) => {
                self.busy = b;
                if !b {
                    self.cancel = None;
                    self.approval = None;
                    self.clarify = None;
                    self.refresh_sessions();
                }
            }
        }
    }

    fn handle_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStarted => {
                self.first_token = None;
                self.streamed_chars = 0;
                self.speed_measured = false;
            }
            AgentEvent::ToolCallStreaming { .. } => {}
            AgentEvent::TextDelta(t) => {
                self.note_tokens(&t);
                chat::push_delta(&mut self.items, &t, false)
            }
            AgentEvent::ReasoningDelta(t) => {
                self.note_tokens(&t);
                chat::push_delta(&mut self.items, &t, true)
            }
            AgentEvent::Context { used, window } => self.context = (used, window),
            AgentEvent::ToolRunning { id, name, summary, args } => {
                self.items.push(ChatItem::Tool { id, name, summary, args, output: None, is_error: false });
            }
            AgentEvent::ToolFinished { id, output: out, is_error: err, .. } => {
                if let Some(ChatItem::Tool { output, is_error, .. }) =
                    self.items.iter_mut().rev().find(|i| matches!(i, ChatItem::Tool { id: t, .. } if *t == id))
                {
                    *output = Some(out);
                    *is_error = err;
                }
            }
            AgentEvent::ApprovalNeeded { command, reasons, respond } => self.approval = Some(Approval { command, reasons, respond }),
            AgentEvent::Clarify { question, choices, respond } => {
                self.clarify = Some(Clarify { question, choices, answer: String::new(), respond })
            }
            AgentEvent::Committed(m) => {
                if m.role == Role::Assistant {
                    chat::commit_assistant(&mut self.items, &m);
                }
            }
            AgentEvent::TranscriptReplaced => self.reload_transcript(),
            AgentEvent::Usage(u) => {
                // Speed from the server's own count when it gives one.
                if let Some(t0) = self.first_token.take() {
                    let secs = t0.elapsed().as_secs_f32();
                    let gained = u.completion_tokens.saturating_sub(self.usage.completion_tokens);
                    if secs > 0.2 && gained > 0 {
                        self.speed = Some(gained as f32 / secs);
                        self.speed_measured = true;
                    }
                }
                self.streamed_chars = 0;
                self.usage = u;
            }
            AgentEvent::Notice(t) => self.items.push(ChatItem::Notice(t)),
            AgentEvent::Error(t) => self.items.push(ChatItem::Error(t)),
            AgentEvent::TurnFinished => {
                if !self.speed_measured
                    && let Some(t0) = self.first_token.take()
                {
                    // The server didn't report usage: estimate (~4 characters per token).
                    let secs = t0.elapsed().as_secs_f32();
                    if secs > 0.2 && self.streamed_chars > 0 {
                        self.speed = Some(self.streamed_chars as f32 / 4.0 / secs);
                    }
                }
                // Close any bubble left streaming by an interrupted turn.
                for i in &mut self.items {
                    if let ChatItem::Assistant { streaming, .. } = i {
                        *streaming = false;
                    }
                }
            }
        }
    }

    fn note_tokens(&mut self, t: &str) {
        if self.first_token.is_none() {
            self.first_token = Some(Instant::now());
        }
        self.streamed_chars += t.len();
    }

    fn transcript_msg(a: &Agent) -> UiMsg {
        UiMsg::Transcript { session: a.session.clone(), messages: a.messages.clone(), workdir: a.workdir(), usage: a.usage.clone() }
    }

    pub fn reload_transcript(&self) {
        let (tx, rp) = self.sender();
        let agent = self.agent.clone();
        self.spawn(async move {
            let a = agent.lock().await;
            send(&tx, &rp, Self::transcript_msg(&a));
        });
    }

    // ---- actions --------------------------------------------------------------------------

    pub fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if (text.is_empty() && self.attachments.is_empty()) || self.busy {
            return;
        }
        self.input.clear();
        self.drafts.remove(&self.session.id);
        self.history_pos = None;
        if !text.is_empty() && self.history.first() != Some(&text) {
            self.history.insert(0, text.clone());
            self.history.truncate(200);
        }
        match commands::parse(&text, &self.skill_names()) {
            None => {
                // `//text` escapes a leading slash.
                let msg = if text.starts_with("//") { text[1..].to_string() } else { text };
                let (msg, images) = crate::attach::compose(&msg, &std::mem::take(&mut self.attachments));
                self.start_turn_with(msg, images)
            }
            Some(Command::Stop) => self.stop(),
            Some(Command::Help) => self.items.push(ChatItem::Info(commands::help_text())),
            Some(cmd) => self.run_command(cmd),
        }
    }

    pub fn start_turn(&mut self, text: String) {
        self.start_turn_with(text, vec![]);
    }

    pub fn start_turn_with(&mut self, text: String, images: Vec<String>) {
        self.items.push(ChatItem::User(chat::user_label(&text, images.len())));
        self.begin_busy();
        let cancel = self.cancel.clone().unwrap_or_default();
        let agent = self.agent.clone();
        let ev = self.event_bridge();
        let (tx, rp) = self.sender();
        self.spawn_guarded(async move {
            let mut a = agent.lock().await;
            a.pending_images = images;
            a.run_turn(&text, &ev, &cancel).await;
            send(&tx, &rp, UiMsg::Config(a.config.clone()));
            send(&tx, &rp, UiMsg::Busy(false));
        });
    }

    fn begin_busy(&mut self) {
        self.busy = true;
        self.cancel = Some(CancellationToken::new());
    }

    pub fn run_command(&mut self, cmd: Command) {
        self.begin_busy();
        let cancel = self.cancel.clone().unwrap_or_default();
        let agent = self.agent.clone();
        let ev = self.event_bridge();
        let (tx, rp) = self.sender();
        self.spawn_guarded(async move {
            let mut a = agent.lock().await;
            match a.handle_command(cmd, &ev, &cancel).await {
                Ok(CommandOutcome::Text(t)) => send(&tx, &rp, UiMsg::Info(t)),
                Ok(CommandOutcome::Reload(t)) => {
                    send(&tx, &rp, Self::transcript_msg(&a));
                    send(&tx, &rp, UiMsg::Toast(t));
                }
                Ok(CommandOutcome::Send(text)) => {
                    send(&tx, &rp, Self::transcript_msg(&a));
                    send(&tx, &rp, UiMsg::UserEcho(text.clone()));
                    a.run_turn(&text, &ev, &cancel).await;
                }
                Ok(CommandOutcome::Stop) => {}
                Err(e) => send(&tx, &rp, UiMsg::Agent(AgentEvent::Error(format!("{e:#}")))),
            }
            send(&tx, &rp, UiMsg::Config(a.config.clone()));
            send(&tx, &rp, UiMsg::Busy(false));
        });
    }

    pub fn stop(&mut self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        if let Some(a) = self.approval.take() {
            let _ = a.respond.send(false);
        }
        if let Some(c) = self.clarify.take() {
            let _ = c.respond.send(String::new());
        }
    }

    /// Answer the model's `clarify` question (empty = skip).
    pub fn answer_clarify(&mut self, answer: String) {
        if let Some(c) = self.clarify.take() {
            let _ = c.respond.send(answer);
        }
    }

    /// The zoom in effect (config value, or 1.0 while still "auto" and the monitor is unknown).
    pub fn scale(&self) -> f32 {
        if self.config.ui.font_scale > 0.0 { self.config.ui.font_scale } else { 1.0 }
    }

    pub fn set_scale(&mut self, s: f32) {
        self.config.ui.font_scale = crate::config::clamp_scale(s);
        let _ = self.config.save_profile(&self.paths);
    }

    /// Ask the agent for MCP server status (connects them if needed).
    pub fn check_mcp(&mut self) {
        self.mcp_status = "Connecting…".into();
        let agent = self.agent.clone();
        let ev = self.event_bridge();
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let mut a = agent.lock().await;
            let s = a.mcp_status(&ev).await;
            send(&tx, &rp, UiMsg::McpStatus(s));
        });
    }

    pub fn scan_hermes(&mut self) {
        self.hermes_scanned = true;
        self.hermes_selected = 0;
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let cleaned = browse::clean_path(&self.hermes_path);
        let path = crate::tools::files::resolve(&home, &cleaned);
        self.hermes_profiles =
            crate::hermes_import::list_profiles(&path).into_iter().map(|p| (p.clone(), crate::hermes_import::summarize(&p))).collect();
        // Pre-select the Hermes profile matching the Mercury one we're in, if there is one.
        if let Some(i) = self.hermes_profiles.iter().position(|(p, _)| crate::profiles::sanitize(&p.name) == self.paths.profile) {
            self.hermes_selected = i;
        }
        if !self.hermes_profiles.is_empty() {
            // Show the path as Mercury understood it (no quotes, `file://` or `~`).
            self.hermes_path = path.display().to_string();
        }
        self.hermes_result = if self.hermes_profiles.is_empty() {
            Some(Err(format!("No Hermes profiles found at {}. Point at ~/.hermes, its profiles folder, or one profile.", path.display())))
        } else {
            None
        };
    }

    /// Bring a Hermes profile into a Mercury profile (off the UI thread: history can be large).
    /// `target` empty = a Mercury profile with the same name as the Hermes one.
    pub fn adopt_hermes(&mut self, profile: crate::hermes_import::HermesProfile, mode: crate::config::LinkMode, target: &str) {
        if self.busy {
            self.toast("Wait for the current reply to finish, or stop it, before importing.");
            return;
        }
        self.hermes_busy = true;
        self.hermes_result = None;
        let target = if target.trim().is_empty() { crate::profiles::sanitize(&profile.name) } else { target.trim().to_string() };
        let root = self.paths.clone();
        let sessions = self.hermes_sessions;
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let name = profile.name.clone();
            let t2 = target.clone();
            let r = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                let tpaths = crate::profiles::ensure(&root, &t2)?;
                let mut cfg = Config::load_profile(&tpaths)?;
                let rep = crate::hermes_import::adopt(&profile, mode, sessions, &tpaths, &mut cfg)?;
                cfg.save_profile(&tpaths)?;
                Ok(rep.summary())
            })
            .await;
            let msg = match r {
                Ok(Ok(summary)) => {
                    Ok((target.clone(), format!("Picked up Hermes profile “{name}” into Mercury profile “{target}”: {summary}")))
                }
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            send(&tx, &rp, UiMsg::HermesDone(msg));
        });
    }

    pub fn refresh_profiles(&mut self) {
        self.profiles = crate::profiles::list(&self.paths);
    }

    /// Open another profile: its own chats, memory, skills, SOUL.md and model.
    /// `force` reloads even if it's already the active one (after an import into it).
    pub fn switch_profile(&mut self, name: &str, force: bool) {
        if self.busy {
            self.toast("Stop the current reply before switching profiles.");
            return;
        }
        if name == self.paths.profile && !force {
            return;
        }
        // Keep what this window has (provider labels, unsaved settings): providers are shared, so
        // the profile being opened should see them.
        if !force && let Err(e) = self.config.save_profile(&self.paths) {
            self.toast(format!("Couldn't save settings before switching: {e:#}"));
            return;
        }
        let r = (|| -> anyhow::Result<()> {
            let paths = crate::profiles::ensure(&self.paths, name)?;
            let config = Config::load_profile(&paths)?;
            crate::profiles::set_active(&paths, name)?;
            let store = Arc::new(SessionStore::open(&paths.sessions_db())?);
            let resume = if config.ui.resume_last { store.latest().ok().flatten().map(|s| s.id) } else { None };
            let agent = Agent::with_store(paths.clone(), config.clone(), store.clone(), resume.as_deref())?;
            let _ = store.prune_empty(&agent.session.id);
            self.items = chat::from_messages(&agent.messages);
            self.usage = agent.usage.clone();
            self.session = agent.session.clone();
            self.workdir = agent.workdir();
            self.workdir_draft = self.workdir.display().to_string();
            self.agent = Arc::new(tokio::sync::Mutex::new(agent));
            self.store = store;
            self.paths = paths;
            self.config = config;
            Ok(())
        })();
        if let Err(e) = r {
            self.toast(format!("Couldn't open profile {name}: {e:#}"));
            return;
        }
        self.config_dirty = false;
        self.provider_text.clear();
        self.probes.clear();
        self.input.clear();
        self.attachments.clear();
        self.find = None;
        self.context = (0, 0);
        self.history = self.store.recent_user_messages(100).unwrap_or_default();
        self.history_pos = None;
        self.search.clear();
        self.search_hits.clear();
        self.memory_draft = None;
        self.selected_skill = None;
        self.mcp_status.clear();
        self.approval = None;
        self.clarify = None;
        self.refresh_profiles();
        self.refresh_sessions();
        if let Some(p) = self.config.active().map(|p| p.id.clone()) {
            self.fetch_models(&p);
        }
        if !force {
            self.toast(format!("Profile: {name}"));
        }
    }

    pub fn create_profile(&mut self) {
        let name = self.new_profile.trim().to_string();
        match crate::profiles::create(&self.paths, &name) {
            Ok(_) => {
                self.new_profile.clear();
                self.switch_profile(&name, false);
            }
            Err(e) => self.toast(format!("{e:#}")),
        }
    }

    pub fn delete_profile(&mut self, name: &str) {
        match crate::profiles::delete(&self.paths, name) {
            Ok(()) => {
                self.refresh_profiles();
                self.toast(format!("Deleted profile {name}."));
            }
            Err(e) => self.toast(format!("{e:#}")),
        }
    }

    /// What the field for `purpose` currently holds (where the picker should start).
    fn pick_field(&self, purpose: browse::PickPurpose) -> String {
        match purpose {
            browse::PickPurpose::HermesFolder => self.hermes_path.clone(),
            browse::PickPurpose::Logo => self.logo_path_draft.clone(),
            browse::PickPurpose::Workdir => self.workdir_draft.clone(),
            _ => String::new(),
        }
    }

    /// Open a file/folder picker: the system dialog if the desktop has one, else the built-in one.
    pub fn browse(&mut self, purpose: browse::PickPurpose) {
        let start = browse::start_dir(purpose, &self.pick_field(purpose));
        if !browse::system_dialog_available() {
            self.browser = Some(browse::Browser::new(purpose, start));
            return;
        }
        let (tx, rp) = self.sender();
        self.rt.spawn_blocking(move || match browse::system_pick(purpose, &start) {
            Ok(Some(path)) => send(&tx, &rp, UiMsg::Picked { purpose, path }),
            Ok(None) => {}
            Err(()) => send(&tx, &rp, UiMsg::OpenBrowser(purpose)),
        });
    }

    pub fn picked(&mut self, purpose: browse::PickPurpose, path: PathBuf) {
        match purpose {
            browse::PickPurpose::HermesFolder => {
                self.hermes_path = path.display().to_string();
                self.scan_hermes();
            }
            browse::PickPurpose::Logo => {
                self.logo_path_draft = path.display().to_string();
                self.set_custom_logo(&path);
            }
            browse::PickPurpose::Workdir => {
                self.workdir_draft = path.display().to_string();
                self.set_workdir(self.workdir_draft.clone());
            }
            browse::PickPurpose::Attachment => {
                self.view = View::Chat;
                self.attach_files(vec![path]);
            }
            browse::PickPurpose::ExportDir(i) => {
                if let Some(name) = self.profiles.get(i).cloned() {
                    self.export_profile(&name, path);
                }
            }
            browse::PickPurpose::ProfileArchive => self.import_profile(path),
        }
    }

    /// The logo texture for `logo` (the orb, or the custom image if one is set).
    pub fn logo_texture(&mut self, ctx: &egui::Context, logo: crate::config::Logo) -> egui::TextureHandle {
        let custom = if logo == crate::config::Logo::Custom { self.config.ui.custom_logo.clone() } else { None };
        let key = (logo, custom.clone(), self.config.ui.dark);
        self.logo_tex
            .entry(key.clone())
            .or_insert_with(|| {
                let size = 160;
                let ui_cfg = crate::config::UiConfig { logo, custom_logo: custom, ..Default::default() };
                let rgba = crate::logo::pixels(&ui_cfg, size, key.2);
                ctx.load_texture(
                    format!("logo-{key:?}"),
                    egui::ColorImage::from_rgba_unmultiplied([size as usize, size as usize], &rgba),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone()
    }

    /// Use the image at `path` as the logo. Mercury keeps its own square PNG copy.
    pub fn set_custom_logo(&mut self, path: &std::path::Path) {
        match crate::logo::import_custom(path, &self.paths.data_dir) {
            Ok(saved) => {
                self.config.ui.logo = crate::config::Logo::Custom;
                self.config.ui.custom_logo = Some(saved);
                // The copy is overwritten in place, so drop cached textures for it.
                self.logo_tex.retain(|k, _| k.0 != crate::config::Logo::Custom);
                self.icon_applied = None;
                let _ = self.config.save_profile(&self.paths);
                self.toast("Logo updated.");
            }
            Err(e) => self.toast(format!("{e:#}")),
        }
    }

    pub fn answer_approval(&mut self, approve: bool) {
        if let Some(a) = self.approval.take() {
            let _ = a.respond.send(approve);
        }
    }

    pub fn new_chat(&mut self) {
        if self.busy {
            return;
        }
        self.view = View::Chat;
        self.stash_draft(None);
        self.run_command(Command::New);
    }

    /// Keep the unsent message (and attachments are dropped) when leaving this chat; restore the
    /// one saved for `next`, if any.
    fn stash_draft(&mut self, next: Option<&str>) {
        let text = std::mem::take(&mut self.input);
        if text.trim().is_empty() {
            self.drafts.remove(&self.session.id);
        } else {
            self.drafts.insert(self.session.id.clone(), text);
        }
        self.attachments.clear();
        self.history_pos = None;
        self.find = None;
        if let Some(n) = next {
            self.input = self.drafts.get(n).cloned().unwrap_or_default();
        }
    }

    pub fn open_session(&mut self, id: String) {
        if self.busy || id == self.session.id {
            self.view = View::Chat;
            return;
        }
        self.view = View::Chat;
        self.stash_draft(Some(&id));
        let agent = self.agent.clone();
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let mut a = agent.lock().await;
            match a.load_session(&id) {
                Ok(()) => send(&tx, &rp, Self::transcript_msg(&a)),
                Err(e) => send(&tx, &rp, UiMsg::Toast(format!("{e:#}"))),
            }
        });
    }

    pub fn set_pinned(&mut self, id: &str, pinned: bool) {
        let _ = self.store.set_pinned(id, pinned);
        self.refresh_sessions();
    }

    pub fn set_archived(&mut self, id: &str, archived: bool) {
        let _ = self.store.set_archived(id, archived);
        if archived && id == self.session.id {
            self.new_chat();
        }
        self.refresh_sessions();
    }

    pub fn attach_files(&mut self, files: Vec<PathBuf>) {
        for f in files {
            match crate::attach::load(&f) {
                Ok(a) => {
                    if a.is_image() && !self.model_sees_images() {
                        self.toast(format!(
                            "{} attached. The current model isn't marked as vision-capable; it may ignore or reject images.",
                            a.name
                        ));
                    }
                    self.attachments.push(a);
                }
                Err(e) => self.toast(e),
            }
        }
    }

    /// Whether the selected model is known to take images (unknown counts as yes).
    fn model_sees_images(&self) -> bool {
        let (Some(p), Some(m)) = (self.config.active(), self.config.active_model_name()) else { return true };
        match self.model_meta.get(&format!("{}/{m}", p.id)) {
            Some(info) if !info.tags.is_empty() => info.vision(),
            _ => true,
        }
    }

    pub fn test_provider(&mut self, id: &str) {
        let Some(p) = self.config.provider(id).cloned() else { return };
        self.probing.insert(id.to_string());
        let (paths, cfg) = (self.paths.clone(), self.config.clone());
        let (tx, rp) = self.sender();
        let id = id.to_string();
        self.spawn(async move {
            let r = crate::probe::test_provider(&paths, &cfg, &p, None).await;
            send(&tx, &rp, UiMsg::Probe { provider: id, ok: r.ok(), summary: r.summary() });
        });
    }

    /// Set the active provider's `reasoning_effort` ("" = server default).
    pub fn set_effort(&mut self, effort: &str) {
        let Some(id) = self.config.active().map(|p| p.id.clone()) else { return };
        if let Some(p) = self.config.providers.iter_mut().find(|p| p.id == id) {
            p.reasoning_effort = Some(effort.to_string()).filter(|e| !e.is_empty());
        }
        self.apply_config();
    }

    pub fn toggle_favorite(&mut self, provider: &str, model: &str) {
        let key = format!("{provider}/{model}");
        let favs = &mut self.config.ui.favorite_models;
        if let Some(i) = favs.iter().position(|f| *f == key) {
            favs.remove(i);
        } else {
            favs.push(key);
        }
        let _ = self.config.save_profile(&self.paths);
    }

    pub fn export_profile(&mut self, name: &str, dir: PathBuf) {
        let src = self.paths.with_profile(name);
        let out = dir.join(crate::portable::default_file_name(name));
        let (tx, rp) = self.sender();
        let name = name.to_string();
        self.spawn(async move {
            let r = tokio::task::spawn_blocking(move || {
                crate::portable::export(&src, &out)
                    .map(|n| format!("Exported “{name}” ({n} files, API keys left out) to {}", out.display()))
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            send(&tx, &rp, UiMsg::ProfilesChanged(r));
        });
    }

    pub fn import_profile(&mut self, archive: PathBuf) {
        let paths = self.paths.clone();
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let r = tokio::task::spawn_blocking(move || {
                crate::portable::import(&paths, &archive, None)
                    .map(|r| {
                        let mut m = format!("Added profile “{}” with {} chats.", r.profile, r.sessions);
                        if !r.providers_added.is_empty() {
                            m.push_str(&format!(" New providers (enter their keys): {}.", r.providers_added.join(", ")));
                        }
                        m
                    })
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            send(&tx, &rp, UiMsg::ProfilesChanged(r));
        });
    }

    pub fn open_palette(&mut self) {
        let mut items = vec![
            palette::Item::new("New chat", "Ctrl+N", palette::Action::NewChat),
            palette::Item::new("Find in this chat", "Ctrl+F", palette::Action::Find),
            palette::Item::new("Attach files…", "images, text, code", palette::Action::Attach),
            palette::Item::new("Toggle the chats sidebar", "Ctrl+B", palette::Action::ToggleSidebar),
            palette::Item::new("Toggle light / dark", "", palette::Action::ToggleTheme),
        ];
        for (tab, name) in settings::TABS {
            items.push(palette::Item::new(format!("Settings: {name}"), "Ctrl+,", palette::Action::Settings(*tab)));
        }
        for (c, d) in commands::HELP {
            let name = c.split([',', ' ']).next().unwrap_or(c);
            items.push(palette::Item::new(name, *d, palette::Action::Slash(name.to_string())));
        }
        for p in &self.config.providers {
            items.push(palette::Item::new(
                format!("Test connection: {}", p.name),
                &p.base_url,
                palette::Action::TestProvider(p.id.clone()),
            ));
            let favs = &self.config.ui.favorite_models;
            let mut models = self.models.get(&p.id).cloned().unwrap_or_default();
            models.sort_by_key(|m| !favs.contains(&format!("{}/{m}", p.id)));
            for m in models.into_iter().take(300) {
                items.push(palette::Item::new(format!("Model: {m}"), &p.name, palette::Action::Model { provider: p.id.clone(), model: m }));
            }
        }
        for name in &self.profiles {
            if *name != self.paths.profile {
                items.push(palette::Item::new(format!("Profile: {name}"), "switch", palette::Action::Profile(name.clone())));
            }
        }
        for s in &self.sessions {
            let title = if s.title.is_empty() { "New chat" } else { &s.title };
            let when = s.updated_at.with_timezone(&chrono::Local).format("%b %e").to_string();
            items.push(palette::Item::new(format!("Chat: {title}"), when, palette::Action::OpenSession(s.id.clone())));
        }
        self.palette = Some(palette::Palette::new(items));
    }

    pub fn run_palette(&mut self, action: palette::Action) {
        use palette::Action::*;
        self.palette = None;
        match action {
            NewChat => self.new_chat(),
            OpenSession(id) => self.open_session(id),
            Slash(cmd) => {
                let takes_arg =
                    commands::HELP.iter().any(|(c, _)| c.starts_with(&cmd) && (c.contains('[') || c.contains(" text") || c.contains(" n")));
                self.view = View::Chat;
                if takes_arg || self.busy {
                    self.input = format!("{cmd} ");
                } else {
                    self.input = cmd;
                    self.submit();
                }
            }
            Settings(tab) => self.view = View::Settings(tab),
            Model { provider, model } => {
                if self.config.active().map(|p| p.id.as_str()) != Some(provider.as_str()) {
                    self.config.set_active(&provider, Some(model));
                    self.apply_config();
                    self.fetch_models(&provider);
                } else {
                    self.select_model(&model);
                }
            }
            Profile(name) => self.switch_profile(&name, false),
            ToggleSidebar => self.toggle_sidebar(),
            ToggleTheme => {
                self.config.ui.dark = !self.config.ui.dark;
                let _ = self.config.save_profile(&self.paths);
            }
            Find => self.open_find(),
            Attach => self.browse(browse::PickPurpose::Attachment),
            TestProvider(id) => {
                self.view = View::Settings(Tab::Providers);
                self.test_provider(&id);
            }
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.config.ui.show_sidebar = !self.config.ui.show_sidebar;
        let _ = self.config.save_profile(&self.paths);
    }

    pub fn open_find(&mut self) {
        self.view = View::Chat;
        if self.find.is_none() {
            self.find = Some(FindBar::default());
        } else if let Some(f) = &mut self.find {
            f.focused = false;
        }
    }

    /// ↑/↓ in the composer: step through earlier messages while the box is empty or showing one.
    pub fn history_step(&mut self, older: bool) -> bool {
        let showing_history = self.history_pos.is_some_and(|i| self.history.get(i) == Some(&self.input));
        if !(self.input.is_empty() || showing_history) || self.history.is_empty() {
            return false;
        }
        let next = match (self.history_pos.filter(|_| showing_history), older) {
            (None, true) => Some(0),
            (None, false) => return false,
            (Some(i), true) => Some((i + 1).min(self.history.len() - 1)),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
        };
        self.history_pos = next;
        self.input = next.and_then(|i| self.history.get(i).cloned()).unwrap_or_default();
        true
    }

    pub fn delete_session(&mut self, id: String) {
        let _ = self.store.delete(&id);
        if id == self.session.id {
            self.new_chat();
        }
        self.refresh_sessions();
    }

    /// Save config to disk and hand it to the agent.
    /// Full-text search in the open profile's chats.
    pub fn store_search(&self, q: &str) -> Vec<SearchHit> {
        self.store.search(q, 20).unwrap_or_default()
    }

    pub fn apply_config(&mut self) {
        if let Err(e) = self.config.save_profile(&self.paths) {
            self.toast(format!("Couldn't save settings: {e}"));
            return;
        }
        self.config_dirty = false;
        let cfg = self.config.clone();
        let agent = self.agent.clone();
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let mut a = agent.lock().await;
            a.apply_config(cfg);
            send(&tx, &rp, Self::transcript_msg(&a));
        });
    }

    pub fn select_provider(&mut self, id: &str) {
        let model = self.config.provider(id).map(|p| p.default_model.clone()).filter(|m| !m.is_empty());
        let model = model.or_else(|| self.models.get(id).and_then(|m| m.first().cloned()));
        self.config.set_active(id, model);
        self.apply_config();
        self.fetch_models(id);
    }

    pub fn select_model(&mut self, model: &str) {
        self.config.active_model = Some(model.to_string());
        self.apply_config();
    }

    pub fn fetch_models(&self, provider_id: &str) {
        let Some(p) = self.config.provider(provider_id).cloned() else { return };
        let paths = self.paths.clone();
        let cfg = self.config.clone();
        let (tx, rp) = self.sender();
        let id = provider_id.to_string();
        self.spawn(async move {
            let result = async {
                let (base, key) = crate::nous::credentials(&paths, &p).await.map_err(|e| format!("{e:#}"))?;
                let opts = p.client_options(&cfg.agent);
                crate::provider::Client::with_options(&base, key, opts).list_model_info().await.map_err(|e| e.to_string())
            }
            .await;
            send(&tx, &rp, UiMsg::Models { provider: id, result });
        });
    }

    pub fn discover(&mut self, lan: bool) {
        self.discovering = true;
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let mut found = crate::discovery::discover_local().await;
            if lan {
                found.extend(crate::discovery::scan_lan().await);
            }
            send(&tx, &rp, UiMsg::Discovered(found));
        });
    }

    pub fn add_discovered(&mut self, d: &Discovered) {
        let p = d.to_provider(&self.config);
        let id = p.id.clone();
        let first = self.config.providers.is_empty();
        self.models.insert(id.clone(), d.models.clone());
        self.config.upsert_provider(p);
        if first || self.config.active_provider.is_none() {
            self.config.set_active(&id, d.models.first().cloned());
        }
        self.apply_config();
        self.toast(format!("Added {}", d.display_name()));
    }

    pub fn add_draft_provider(&mut self) {
        let d = std::mem::take(&mut self.draft);
        let url = d.url.trim().trim_end_matches('/').to_string();
        if url::Url::parse(&url).is_err() {
            self.toast("That URL doesn't look valid (e.g. http://192.168.1.20:11434/v1).");
            self.draft = d;
            return;
        }
        let name = if d.name.trim().is_empty() { url.clone() } else { d.name.trim().to_string() };
        let id = crate::config::unique_provider_id(&self.config, &name);
        let mut p = crate::config::ProviderConfig::openai(&id, &name, &url);
        p.api_key = Some(d.key.trim().to_string()).filter(|k| !k.is_empty());
        if p.api_key.is_none() {
            p.api_key_env = Some(d.key_env.trim().to_string()).filter(|k| !k.is_empty());
        }
        if let Some(pre) = crate::config::presets().into_iter().find(|pr| pr.base_url == url) {
            p.headers = pre.headers;
        }
        p.default_model = d.model.trim().to_string();
        let first = self.config.providers.is_empty();
        self.config.upsert_provider(p);
        if first {
            self.config.set_active(&id, Some(d.model.trim().to_string()).filter(|m| !m.is_empty()));
        }
        self.apply_config();
        self.fetch_models(&id);
        self.toast(format!("Added {name}"));
    }

    pub fn nous_login(&mut self) {
        let cancel = CancellationToken::new();
        self.nous_cancel = Some(cancel.clone());
        self.nous_status = "Contacting Nous Portal…".into();
        let paths = self.paths.clone();
        let mut cfg = self.config.clone();
        let agent = self.agent.clone();
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let (tx2, rp2) = (tx.clone(), rp.clone());
            let r = crate::nous::login(&paths, &mut cfg, crate::nous::PollTiming::default(), &cancel, move |dc| {
                let _ = open::that_detached(dc.open_url());
                send(&tx2, &rp2, UiMsg::NousCode(dc.clone()));
            })
            .await;
            match r {
                Ok(_) => {
                    let nous_id =
                        cfg.providers.iter().find(|p| p.kind == ProviderKind::NousOauth).map(|p| p.id.clone()).unwrap_or_default();
                    if cfg.active_provider.as_deref() != Some(nous_id.as_str())
                        && cfg.active().map(|p| p.kind) != Some(ProviderKind::NousOauth)
                    {
                        // Keep the user's current choice; Nous is now one click away in the picker.
                    }
                    let _ = cfg.save_profile(&paths);
                    agent.lock().await.apply_config(cfg.clone());
                    send(&tx, &rp, UiMsg::Config(cfg));
                    send(&tx, &rp, UiMsg::NousDone(Ok("Signed in to Nous Portal.".into())));
                }
                Err(e) => send(&tx, &rp, UiMsg::NousDone(Err(format!("{e:#}")))),
            }
        });
    }

    pub fn nous_cancel(&mut self) {
        if let Some(c) = self.nous_cancel.take() {
            c.cancel();
        }
    }

    pub fn nous_logout(&mut self) {
        match crate::nous::logout(&self.paths) {
            Ok(()) => self.nous_status = "Signed out.".into(),
            Err(e) => self.nous_status = format!("{e:#}"),
        }
    }

    /// Scan the Hermes folder once, the first time the Hermes tab is shown.
    pub fn ensure_hermes_scanned(&mut self) {
        if !self.hermes_scanned && !self.hermes_path.trim().is_empty() {
            self.scan_hermes();
        }
    }

    /// Open the Hermes tab with profiles already listed.
    pub fn import_hermes(&mut self) {
        self.view = View::Settings(Tab::Hermes);
        self.scan_hermes();
    }

    pub fn set_workdir(&mut self, path: String) {
        let path = browse::clean_path(&path);
        let agent = self.agent.clone();
        let (tx, rp) = self.sender();
        self.spawn(async move {
            let mut a = agent.lock().await;
            match a.set_workdir(std::path::Path::new(&path)) {
                Ok(()) => {
                    send(&tx, &rp, Self::transcript_msg(&a));
                    send(&tx, &rp, UiMsg::Toast(format!("Working directory: {}", a.workdir().display())));
                }
                Err(e) => send(&tx, &rp, UiMsg::Toast(format!("{e:#}"))),
            }
        });
    }

    // ---- drawing --------------------------------------------------------------------------

    /// The whole UI. `App::ui` delegates here; tests drive it through egui_kittest.
    pub fn ui_root(&mut self, ui: &mut Ui) {
        {
            let mut r = self.repaint.lock().unwrap_or_else(|e| e.into_inner());
            if r.is_none() {
                *r = Some(ui.ctx().clone());
            }
        }
        if self.config.ui.font_scale <= 0.0
            && let Some(w) = ui.ctx().input(|i| i.viewport().monitor_size.map(|s| s.x))
        {
            // First run: size text for this monitor once, then it's the user's to change.
            self.set_scale(crate::config::auto_scale(w));
        }
        let want = (self.config.ui.dark, self.scale());
        if self.theme_applied != Some(want) {
            theme::install(ui.ctx(), want.0, want.1);
            self.theme_applied = Some(want);
        }
        let icon_key = (self.config.ui.logo, self.config.ui.custom_logo.clone());
        if self.icon_applied.as_ref() != Some(&icon_key) {
            let rgba = crate::logo::pixels(&self.config.ui, 64, true);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(egui::IconData { rgba, width: 64, height: 64 }))));
            self.icon_applied = Some(icon_key);
        }
        // Dropped files: on Settings > General an image becomes the logo; anywhere else they're
        // attached to the message being written.
        let dropped: Vec<PathBuf> =
            ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            if self.view == View::Settings(Tab::General) {
                self.set_custom_logo(&dropped[0]);
            } else {
                self.view = View::Chat;
                self.attach_files(dropped);
            }
        }
        self.poll();
        self.shortcuts(ui);
        let p = theme::palette(self.config.ui.dark);

        egui::Panel::top("topbar")
            .frame(egui::Frame::new().fill(p.surface).inner_margin(egui::Margin::symmetric(12, 8)).stroke(egui::Stroke::new(1.0, p.border)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("statusbar")
            .frame(egui::Frame::new().fill(p.surface).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ui, |ui| self.status_bar(ui));
        if self.config.ui.show_sidebar {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(250.0)
                .min_size(180.0)
                .frame(egui::Frame::new().fill(p.surface).inner_margin(egui::Margin::same(10)))
                .show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(18, 12))).show(ui, |ui| {
            match self.view {
                View::Settings(tab) => settings::show(self, ui, tab),
                View::Chat if self.config.providers.is_empty() => self.onboarding(ui),
                View::Chat => self.chat_view(ui),
            }
        });
        self.draw_browser(ui);
        self.draw_palette(ui);
        self.draw_toasts(ui);
    }

    fn draw_palette(&mut self, ui: &mut Ui) {
        let Some(pal) = self.palette.as_mut() else { return };
        let p = theme::palette(self.config.ui.dark);
        let mut outcome = palette::Outcome::None;
        let mut open = true;
        egui::Window::new("Command palette")
            .id(egui::Id::unique("command_palette"))
            .open(&mut open)
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(620.0)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 70.0))
            .show(ui.ctx(), |ui| outcome = pal.show(ui, p.muted, p.accent));
        match outcome {
            palette::Outcome::Run(a) => self.run_palette(a),
            palette::Outcome::Close => self.palette = None,
            palette::Outcome::None if !open => self.palette = None,
            palette::Outcome::None => {}
        }
    }

    fn shortcuts(&mut self, ui: &mut Ui) {
        let (palette, sidebar, find) = ui.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::K))
                    || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::P)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::B)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::F)),
            )
        });
        if palette {
            if self.palette.is_some() {
                self.palette = None;
            } else {
                self.open_palette();
            }
        }
        if sidebar {
            self.toggle_sidebar();
        }
        if find {
            self.open_find();
        }
        let (new, settings, esc, bigger, smaller, reset, wheel) = ui.input_mut(|i| {
            (
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::N)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma)),
                i.key_pressed(Key::Escape),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Plus))
                    || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Equals))
                    || i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Equals)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Minus)),
                i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Num0)),
                i.zoom_delta(),
            )
        });
        if bigger {
            self.set_scale(self.scale() + 0.1);
        }
        if smaller {
            self.set_scale(self.scale() - 0.1);
        }
        if reset {
            self.set_scale(1.0);
        }
        if (wheel - 1.0).abs() > 0.001 {
            // Ctrl + mouse wheel / pinch.
            self.set_scale(self.scale() * wheel);
        }
        if new {
            self.new_chat();
        }
        if settings {
            self.view = if matches!(self.view, View::Settings(_)) { View::Chat } else { View::Settings(Tab::Providers) };
        }
        if esc && self.palette.is_none() {
            if self.find.is_some() {
                self.find = None;
            } else if self.busy {
                self.stop();
            }
        }
    }

    fn logo(&mut self, ui: &mut Ui, size: f32) {
        let tex = self.logo_texture(ui.ctx(), self.config.ui.logo);
        ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(size, size))).on_hover_text("Change the logo in Settings > General");
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        // At large text sizes the pickers move to a second row instead of overlapping the controls.
        let compact = ui.available_width() < 1130.0;
        ui.horizontal(|ui| {
            let hint = if self.config.ui.show_sidebar { "Hide the chats sidebar (Ctrl+B)" } else { "Show the chats sidebar (Ctrl+B)" };
            if ui.small_button("☰").on_hover_text(hint).clicked() {
                self.toggle_sidebar();
            }
            self.logo(ui, 28.0);
            ui.label(RichText::new("Mercury").strong().size(17.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.top_controls(ui);
                if !compact {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| self.pickers(ui));
                }
            });
        });
        if compact {
            ui.horizontal(|ui| self.pickers(ui));
        }
    }

    fn top_controls(&mut self, ui: &mut Ui) {
        let settings_open = matches!(self.view, View::Settings(_));
        if ui.selectable_label(settings_open, "⚙ Settings").clicked() {
            self.view = if settings_open { View::Chat } else { View::Settings(Tab::Providers) };
        }
        if ui.small_button("Commands").on_hover_text("Command palette (Ctrl+K): chats, commands, models, settings").clicked() {
            self.open_palette();
        }
        // Text size: always one click away.
        if ui.small_button("A+").on_hover_text("Larger text (Ctrl +)").clicked() {
            self.set_scale(self.scale() + 0.1);
        }
        let pct = format!("{:.0}%", self.scale() * 100.0);
        if ui.small_button(pct).on_hover_text("Text size — click to reset to 100% (Ctrl 0)").clicked() {
            self.set_scale(1.0);
        }
        if ui.small_button("A-").on_hover_text("Smaller text (Ctrl -)").clicked() {
            self.set_scale(self.scale() - 0.1);
        }
        let theme_label = if self.config.ui.dark { "☀" } else { "🌙" };
        if ui.small_button(theme_label).on_hover_text("Toggle light/dark").clicked() {
            self.config.ui.dark = !self.config.ui.dark;
            let _ = self.config.save_profile(&self.paths);
        }
    }

    fn pickers(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        let room = (ui.available_width() - 80.0).max(140.0);
        let (prov_w, model_w) = ((room * 0.45).clamp(60.0, 240.0), (room * 0.55).clamp(80.0, 320.0));
        ui.add_space(4.0);
        // Provider picker
        let active_id = self.config.active().map(|p| p.id.clone()).unwrap_or_default();
        let active_name = self.config.active().map(|p| p.name.clone()).unwrap_or_else(|| "No provider".into());
        let mut chosen_provider = None;
        let picker =
            egui::ComboBox::from_id_salt("provider_picker").selected_text(active_name).width(prov_w).truncate().show_ui(ui, |ui| {
                for prov in &self.config.providers {
                    let badge = match (prov.kind, prov.is_local()) {
                        (ProviderKind::NousOauth, _) => "Nous",
                        (_, true) => "local",
                        _ => "cloud",
                    };
                    let host = url::Url::parse(&prov.base_url)
                        .ok()
                        .and_then(|u| {
                            u.host_str().map(|h| match u.port() {
                                Some(port) => format!("{h}:{port}"),
                                None => h.to_string(),
                            })
                        })
                        .unwrap_or_default();
                    let text = format!("{}  · {badge} · {host}", prov.name);
                    if ui.selectable_label(prov.id == active_id, text).clicked() {
                        chosen_provider = Some(prov.id.clone());
                    }
                }
                ui.separator();
                if ui.button("Manage providers…").clicked() {
                    self.view = View::Settings(Tab::Providers);
                }
            });
        a11y_label(&picker.response, "Provider");
        if let Some(id) = chosen_provider {
            self.select_provider(&id);
        }
        // Model picker
        let current = self.config.active_model_name().unwrap_or_else(|| "Choose a model".into());
        let mut list = self.models.get(&active_id).cloned().unwrap_or_default();
        let favs = self.config.ui.favorite_models.clone();
        let is_fav = |m: &str| favs.iter().any(|f| *f == format!("{active_id}/{m}"));
        list.sort_by_key(|m| !is_fav(m));
        let mut chosen_model = None;
        let mut star = None;
        let picker = egui::ComboBox::from_id_salt("model_picker")
            .selected_text(crate::provider::truncate(&current, 40))
            .width(model_w)
            .truncate()
            .height(420.0)
            .show_ui(ui, |ui| {
                if list.is_empty() {
                    ui.label(RichText::new("No model list yet — refresh, or type a name in Settings.").small().color(p.muted));
                }
                for m in &list {
                    ui.horizontal(|ui| {
                        let fav = is_fav(m);
                        let s =
                            ui.small_button(if fav { "★" } else { "☆" }).on_hover_text(if fav { "Unstar" } else { "Star: list it first" });
                        a11y_label(&s, &format!("{} {m}", if fav { "Unstar" } else { "Star" }));
                        if s.clicked() {
                            star = Some(m.clone());
                        }
                        let mut label = m.clone();
                        if let Some(info) = self.model_meta.get(&format!("{active_id}/{m}")) {
                            if let Some(n) = info.context_length {
                                label.push_str(&format!("  · {}k", n / 1000));
                            }
                            if info.vision() {
                                label.push_str("  · vision");
                            }
                        }
                        if ui.selectable_label(*m == current, label).clicked() {
                            chosen_model = Some(m.clone());
                        }
                    });
                }
            });
        a11y_label(&picker.response, "Model");
        if let Some(m) = star {
            self.toggle_favorite(&active_id, &m);
        }
        if let Some(m) = chosen_model {
            self.select_model(&m);
        }
        // Reasoning effort for models that think (sent as `reasoning_effort`).
        let effort = self.config.active().and_then(|p| p.reasoning_effort.clone()).unwrap_or_default();
        let mut new_effort = None;
        let r = egui::ComboBox::from_id_salt("effort_picker")
            .selected_text(if effort.is_empty() { "effort: auto".to_string() } else { format!("effort: {effort}") })
            .width(112.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(effort.is_empty(), "auto (server default)").clicked() {
                    new_effort = Some(String::new());
                }
                for e in crate::config::REASONING_EFFORTS {
                    if ui.selectable_label(effort == *e, *e).clicked() {
                        new_effort = Some(e.to_string());
                    }
                }
            });
        a11y_label(&r.response, "Reasoning effort");
        r.response.on_hover_text("How hard reasoning models think (reasoning_effort). Models that don't reason ignore it.");
        if let Some(e) = new_effort {
            self.set_effort(&e);
        }
        if ui.small_button("⟳").on_hover_text("Refresh the model list").clicked() {
            self.fetch_models(&active_id);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("📁 {}", self.workdir.display())).small().color(p.muted));
            ui.separator();
            if let Some(prov) = self.config.active() {
                let (label, color) = match (prov.kind, prov.is_local()) {
                    (ProviderKind::NousOauth, _) => ("• Nous Portal", p.accent),
                    (_, true) => ("• local — stays on your network", p.ok),
                    _ => ("• cloud", p.muted),
                };
                ui.label(RichText::new(label).small().color(color));
                ui.separator();
            }
            let (used, window) = self.context;
            if window > 0 && used > 0 {
                let pct = used as f32 / window as f32;
                let color = if pct > 0.8 { p.warn } else { p.muted };
                ui.label(RichText::new(format!("context {} / {} ({:.0}%)", short_num(used), short_num(window as u64), pct * 100.0)).small().color(color))
                    .on_hover_text("How full the model's context window is after the last reply. Mercury summarises older turns when it gets full; /compress does it now.");
                ui.separator();
            }
            ui.label(RichText::new(format!("{} tokens this session", short_num(self.usage.total_tokens))).small().color(p.muted));
            if self.config.ui.show_speed
                && let Some(sp) = self.speed
            {
                ui.separator();
                ui.label(RichText::new(format!("{sp:.0} tok/s")).small().color(p.muted)).on_hover_text("Speed of the last reply");
            }
            ui.separator();
            use crate::config::ApprovalMode;
            let (text, color, hint) = match self.config.approvals {
                ApprovalMode::Yolo => ("YOLO: commands run without approval", p.warn, "Click to ask before dangerous commands again"),
                ApprovalMode::Ask => ("approvals: ask", p.muted, "Click for YOLO: run dangerous commands without asking"),
                ApprovalMode::Deny => ("approvals: deny", p.muted, "Dangerous commands are refused. Click for YOLO"),
            };
            let r = ui.add(egui::Button::new(RichText::new(text).small().color(color)).frame(false)).on_hover_text(hint);
            if r.clicked() {
                self.config.approvals = if self.config.approvals == ApprovalMode::Yolo { ApprovalMode::Ask } else { ApprovalMode::Yolo };
                self.apply_config();
            }
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        // Profile switcher: each profile has its own chats, memory, skills and model.
        let mut switch_to = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Profile").small().color(p.muted));
            let current = self.paths.profile.clone();
            let r = egui::ComboBox::from_id_salt("profile_picker").selected_text(&current).width(ui.available_width() - 8.0).show_ui(
                ui,
                |ui| {
                    for name in &self.profiles {
                        if ui.selectable_label(*name == current, name).clicked() {
                            switch_to = Some(name.clone());
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        let r =
                            ui.add(egui::TextEdit::singleline(&mut self.new_profile).hint_text("new profile name").desired_width(140.0));
                        a11y_label(&r, "New profile name");
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (ui.button("Create").clicked() || enter) && !self.new_profile.trim().is_empty() {
                            switch_to = Some("\0create".to_string());
                        }
                    });
                    if ui.button("Manage profiles & Hermes…").clicked() {
                        switch_to = Some("\0manage".into());
                    }
                },
            );
            a11y_label(&r.response, "Choose profile");
        });
        match switch_to.as_deref() {
            Some("\0create") => self.create_profile(),
            Some("\0manage") => self.import_hermes(),
            Some(name) => {
                let name = name.to_string();
                self.switch_profile(&name, false);
            }
            None => {}
        }
        ui.add_space(4.0);
        let new_btn = egui::Button::new(RichText::new("+  New chat").strong()).min_size(egui::vec2(ui.available_width(), 32.0));
        if ui.add_enabled(!self.busy, new_btn).clicked() {
            self.new_chat();
        }
        ui.add_space(6.0);
        let search = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search all chats").desired_width(f32::INFINITY));
        if search.changed() {
            self.search_hits = if self.search.trim().len() >= 2 { self.store.search(&self.search, 30).unwrap_or_default() } else { vec![] };
        }
        ui.add_space(6.0);
        let mut open = None;
        let mut delete = None;
        let mut pin = None;
        let mut archive = None;
        egui::ScrollArea::vertical().id_salt("sessions").auto_shrink(false).show(ui, |ui| {
            if !self.search.trim().is_empty() {
                if self.search_hits.is_empty() {
                    ui.label(RichText::new("No matches").small().color(p.muted));
                }
                for h in &self.search_hits {
                    let title = if h.title.is_empty() { "(untitled)" } else { &h.title };
                    let r = ui.add(egui::Button::new(RichText::new(format!("{title}\n{}", h.snippet)).small()).frame(false).wrap());
                    if r.clicked() {
                        open = Some(h.session_id.clone());
                    }
                    ui.separator();
                }
                return;
            }
            let today = chrono::Local::now().date_naive();
            let mut last_group = String::new();
            let archived_count = self.sessions.iter().filter(|s| s.archived).count();
            let mut shown: Vec<&SessionMeta> = self.sessions.iter().filter(|s| s.archived == self.show_archived).collect();
            shown.sort_by_key(|s| !s.pinned);
            for s in shown {
                let d = s.updated_at.with_timezone(&chrono::Local).date_naive();
                let group = if s.pinned {
                    "Pinned".to_string()
                } else {
                    match (today - d).num_days() {
                        0 => "Today".to_string(),
                        1 => "Yesterday".to_string(),
                        2..=7 => "This week".to_string(),
                        _ => d.format("%B %Y").to_string(),
                    }
                };
                if group != last_group {
                    ui.add_space(4.0);
                    ui.label(RichText::new(&group).small().color(p.muted));
                    last_group = group;
                }
                if let Some((rid, text)) = self.renaming.as_mut()
                    && *rid == s.id
                {
                    let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                    if r.lost_focus() {
                        let _ = self.store.rename(rid, text);
                        if *rid == self.session.id {
                            self.session.title = text.clone();
                        }
                        self.renaming = None;
                        self.refresh_sessions();
                        return;
                    }
                    r.request_focus();
                    continue;
                }
                let title = if s.title.is_empty() { "New chat".to_string() } else { s.title.clone() };
                let selected = s.id == self.session.id;
                let r =
                    ui.add_sized([ui.available_width(), 26.0], egui::Button::selectable(selected, crate::provider::truncate(&title, 34)));
                if r.clicked() {
                    open = Some(s.id.clone());
                }
                r.context_menu(|ui| {
                    if ui.button("Rename").clicked() {
                        self.renaming = Some((s.id.clone(), title.clone()));
                        ui.close();
                    }
                    if ui.button(if s.pinned { "Unpin" } else { "Pin to top" }).clicked() {
                        pin = Some((s.id.clone(), !s.pinned));
                        ui.close();
                    }
                    if ui.button(if s.archived { "Unarchive" } else { "Archive" }).clicked() {
                        archive = Some((s.id.clone(), !s.archived));
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        delete = Some(s.id.clone());
                        ui.close();
                    }
                });
            }
            if archived_count > 0 || self.show_archived {
                ui.add_space(8.0);
                let label = if self.show_archived { "Back to chats".to_string() } else { format!("Archived ({archived_count})") };
                if ui.add(egui::Button::new(RichText::new(label).small().color(p.muted)).frame(false)).clicked() {
                    self.show_archived = !self.show_archived;
                }
            }
        });
        if let Some(id) = open {
            self.open_session(id);
        }
        if let Some((id, on)) = pin {
            self.set_pinned(&id, on);
        }
        if let Some((id, on)) = archive {
            self.set_archived(&id, on);
        }
        if let Some(id) = delete {
            self.delete_session(id);
        }
    }

    fn onboarding(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().id_salt("onboarding").auto_shrink(false).show(ui, |ui| self.onboarding_inner(ui));
    }

    fn onboarding_inner(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        ui.vertical_centered(|ui| {
            ui.add_space(30.0);
            ui.label(RichText::new("Welcome to Mercury").size(28.0).strong());
            ui.label(RichText::new("Pick where your model runs. You can add more later.").color(p.muted));
            ui.add_space(20.0);
        });
        let card = |ui: &mut Ui, title: &str, body: &str| {
            egui::Frame::new()
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(12)
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().min(640.0));
                    ui.label(RichText::new(title).strong().size(16.0));
                    ui.label(RichText::new(body).color(p.muted));
                });
        };
        ui.vertical_centered_justified(|ui| {
            ui.set_max_width(640.0);
            card(
                ui,
                "A model on this computer or your network",
                "Ollama, LM Studio, llama.cpp, vLLM, KoboldCpp or Jan — Mercury looks on their usual ports.",
            );
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.discovering, egui::Button::new("Find local models")).clicked() {
                    self.discover(false);
                }
                if ui.add_enabled(!self.discovering, egui::Button::new("Also scan my network")).clicked() {
                    self.discover(true);
                }
                if self.discovering {
                    ui.spinner();
                }
            });
            self.discovered_list(ui);
            ui.add_space(14.0);
            card(ui, "Nous Portal", "Sign in with your Nous Research account for Hermes models and 300+ others.");
            settings::nous_login_controls(self, ui);
            ui.add_space(14.0);
            card(ui, "Any OpenAI-compatible endpoint", "A URL ending in /v1, plus an API key if it needs one.");
            if ui.button("Add an endpoint…").clicked() {
                self.view = View::Settings(Tab::Providers);
            }
            ui.add_space(14.0);
            card(
                ui,
                "Already use Hermes?",
                "Pick up where you left off: choose a Hermes profile and bring its models, keys, memory, skills, MCP servers and conversations.",
            );
            if ui.button("Pick up from Hermes…").clicked() {
                self.import_hermes();
            }
        });
    }

    pub(crate) fn discovered_list(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        let mut add = None;
        for d in &self.discovered {
            ui.horizontal(|ui| {
                ui.label(RichText::new(d.display_name()).strong());
                ui.label(RichText::new(format!("{} · {} models", d.base_url, d.models.len())).small().color(p.muted));
                let exists = self.config.providers.iter().any(|x| x.base_url == d.base_url);
                if exists {
                    ui.label(RichText::new("added").small().color(p.ok));
                } else if ui.button("Add").clicked() {
                    add = Some(d.clone());
                }
            });
        }
        if let Some(d) = add {
            self.add_discovered(&d);
        }
    }

    fn chat_view(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        // Composer at the bottom.
        egui::Panel::bottom("composer")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 0, right: 0, top: 8, bottom: 0 }))
            .show(ui, |ui| self.composer(ui));
        if self.find.is_some() {
            egui::Panel::top("find_bar")
                .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 0, right: 0, top: 0, bottom: 6 }))
                .show(ui, |ui| self.find_bar(ui));
        }
        egui::CentralPanel::default().frame(egui::Frame::new()).show(ui, |ui| {
            if self.items.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.22);
                    self.logo(ui, 96.0);
                    ui.add_space(8.0);
                    ui.label(RichText::new("What are we working on?").size(22.0).color(p.muted));
                    ui.label(
                        RichText::new(format!(
                            "{} · / for commands",
                            self.config.active_model_name().unwrap_or_else(|| "no model selected".into())
                        ))
                        .small()
                        .color(p.muted),
                    );
                });
                return;
            }
            egui::ScrollArea::vertical().id_salt("chat_scroll").stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(980.0));
                let matches = self.find.as_ref().map(|f| chat::find_matches(&self.items, &f.query)).unwrap_or_default();
                let hl = match &mut self.find {
                    Some(f) if !matches.is_empty() => {
                        f.current = f.current.min(matches.len() - 1);
                        let scroll = std::mem::take(&mut f.scroll);
                        chat::Highlight { matches: &matches, current: Some(matches[f.current]), scroll }
                    }
                    _ => chat::Highlight::default(),
                };
                chat::render(ui, &self.items, p, &mut self.md_cache, self.config.ui.show_reasoning, &hl);
            });
        });
    }

    fn find_bar(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        let n = self.find.as_ref().map(|f| chat::find_matches(&self.items, &f.query).len()).unwrap_or(0);
        let Some(f) = self.find.as_mut() else { return };
        let mut close = false;
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut f.query).hint_text("Find in this chat").desired_width(260.0));
            a11y_label(&r, "Find in chat");
            if !f.focused {
                r.request_focus();
                f.focused = true;
            }
            if r.changed() {
                f.current = 0;
                f.scroll = true;
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let shift = ui.input(|i| i.modifiers.shift);
            if n > 0 {
                ui.label(RichText::new(format!("{} of {n}", f.current.min(n - 1) + 1)).small().color(p.muted));
            } else if !f.query.trim().is_empty() {
                ui.label(RichText::new("no matches").small().color(p.muted));
            }
            if (ui.small_button("Previous").clicked() || (enter && shift)) && n > 0 {
                f.current = (f.current + n - 1) % n;
                f.scroll = true;
                r.request_focus();
            } else if (ui.small_button("Next").clicked() || enter) && n > 0 {
                f.current = (f.current + 1) % n;
                f.scroll = true;
                r.request_focus();
            }
            if ui.small_button("Close").on_hover_text("Esc").clicked() {
                close = true;
            }
        });
        if close {
            self.find = None;
        }
    }

    fn composer(&mut self, ui: &mut Ui) {
        let p = theme::palette(self.config.ui.dark);
        if let Some(c) = &mut self.clarify {
            let mut answer = None;
            egui::Frame::new()
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.5, p.accent))
                .corner_radius(10)
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("The agent has a question").color(p.accent).strong());
                    ui.add(egui::Label::new(RichText::new(&c.question)).wrap());
                    ui.horizontal_wrapped(|ui| {
                        for choice in &c.choices {
                            if ui.button(choice).clicked() {
                                answer = Some(choice.clone());
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut c.answer)
                                .hint_text("Or type an answer")
                                .desired_width(ui.available_width() - 150.0),
                        );
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (ui.button("Answer").clicked() || enter) && !c.answer.trim().is_empty() {
                            answer = Some(c.answer.trim().to_string());
                        }
                        if ui.button("Skip").clicked() {
                            answer = Some(String::new());
                        }
                    });
                });
            if let Some(a) = answer {
                self.answer_clarify(a);
            }
            ui.add_space(6.0);
        }
        if let Some(a) = &self.approval {
            let mut decision = None;
            egui::Frame::new()
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.5, p.warn))
                .corner_radius(10)
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(format!("Approve this command? ({})", a.reasons.join(", "))).color(p.warn).strong());
                    ui.add(egui::Label::new(RichText::new(&a.command).monospace()).wrap());
                    ui.horizontal(|ui| {
                        if ui.button("Run it").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("Deny").clicked() {
                            decision = Some(false);
                        }
                    });
                });
            if let Some(d) = decision {
                self.answer_approval(d);
            }
            ui.add_space(6.0);
        }
        // Slash-command hints.
        if self.input.starts_with('/') && !self.input.contains(' ') {
            let typed = self.input.to_ascii_lowercase();
            let mut pick = None;
            let skills = self.skill_names();
            ui.horizontal_wrapped(|ui| {
                for (c, d) in commands::HELP {
                    let name = c.split([',', ' ']).next().unwrap_or(c);
                    if name.starts_with(&typed) && ui.small_button(name).on_hover_text(*d).clicked() {
                        pick = Some(format!("{name} "));
                    }
                }
                for s in skills.iter().filter(|s| format!("/{s}").starts_with(&typed)) {
                    if ui.small_button(format!("/{s}")).on_hover_text("skill").clicked() {
                        pick = Some(format!("/{s} "));
                    }
                }
            });
            if let Some(t) = pick {
                self.input = t;
            }
        }
        if !self.attachments.is_empty() {
            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Attached:").small().color(p.muted));
                for (i, a) in self.attachments.iter().enumerate() {
                    egui::Frame::new()
                        .fill(p.surface)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(8, 2))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(a.label()).small());
                                let x = ui.small_button("x").on_hover_text("Remove");
                                a11y_label(&x, &format!("Remove {}", a.name));
                                if x.clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                }
            });
            if let Some(i) = remove {
                self.attachments.remove(i);
            }
            ui.add_space(4.0);
        }
        egui::Frame::new()
            .fill(p.surface)
            .stroke(egui::Stroke::new(1.0, p.border))
            .corner_radius(12)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let attach = ui
                        .small_button("+")
                        .on_hover_text("Attach files (or drop them on the window): images for vision models, text and code inline");
                    a11y_label(&attach, "Attach files");
                    if attach.clicked() {
                        self.browse(browse::PickPurpose::Attachment);
                    }
                    let btn_w = 84.0;
                    // ↑/↓ recall earlier messages while the box is empty (or showing one).
                    let focused = ui.memory(|m| m.focused()) == Some(egui::Id::unique("chat_input"));
                    if focused && !self.busy {
                        let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
                        let stepped = (up && self.history_step(true)) || (down && self.history_step(false));
                        if stepped {
                            ui.input_mut(|i| {
                                i.consume_key(Modifiers::NONE, Key::ArrowUp);
                                i.consume_key(Modifiers::NONE, Key::ArrowDown);
                            });
                        }
                    }
                    let edit = egui::TextEdit::multiline(&mut self.input)
                        .id(egui::Id::unique("chat_input"))
                        .hint_text("Message Mercury  ·  Enter to send, Shift+Enter for a new line, / for commands")
                        .desired_rows(2)
                        .desired_width(ui.available_width() - btn_w - 8.0)
                        .frame(egui::Frame::new())
                        .return_key(Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)));
                    let r = ui.add(edit);
                    let enter = r.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
                    if self.busy {
                        if ui.add_sized([btn_w, 34.0], egui::Button::new("Stop")).clicked() {
                            self.stop();
                        }
                    } else {
                        let can = !self.input.trim().is_empty() || !self.attachments.is_empty();
                        if ui
                            .add_enabled(can, egui::Button::new(RichText::new("Send").strong()).min_size(egui::vec2(btn_w, 34.0)))
                            .clicked()
                            || (enter && can)
                        {
                            self.submit();
                            r.request_focus();
                        }
                    }
                });
            });
    }

    fn draw_browser(&mut self, ui: &mut Ui) {
        let Some(b) = self.browser.as_mut() else { return };
        let p = theme::palette(self.config.ui.dark);
        let mut open = true;
        let mut action = browse::BrowserAction::None;
        egui::Window::new(b.purpose.title())
            .id(egui::Id::unique("file_browser"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| action = b.show(ui, p.muted));
        match action {
            browse::BrowserAction::Chosen(path) => {
                let purpose = b.purpose;
                self.browser = None;
                self.picked(purpose, path);
            }
            browse::BrowserAction::Cancel => self.browser = None,
            browse::BrowserAction::None if !open => self.browser = None,
            browse::BrowserAction::None => {}
        }
    }

    fn draw_toasts(&mut self, ui: &mut Ui) {
        self.toasts.retain(|(_, t)| t.elapsed() < Duration::from_secs(5));
        if self.toasts.is_empty() {
            return;
        }
        let p = theme::palette(self.config.ui.dark);
        egui::Area::new(egui::Id::unique("toasts"))
            .interactable(false)
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 56.0))
            .show(ui.ctx(), |ui| {
                for (t, _) in &self.toasts {
                    egui::Frame::new()
                        .fill(p.raised)
                        .stroke(egui::Stroke::new(1.0, p.accent))
                        .corner_radius(10)
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_max_width(380.0);
                            ui.add(egui::Label::new(t.as_str()).wrap());
                        });
                    ui.add_space(4.0);
                }
            });
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
}

impl eframe::App for MercuryApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.ui_root(ui);
    }
}

/// 1234 → "1.2k", 128000 → "128k".
pub fn short_num(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1000.0),
        10_000..=999_999 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub fn run(paths: Paths, config: Config, warnings: Vec<String>) -> anyhow::Result<()> {
    let rt = Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?);
    let mut app = MercuryApp::new(paths, config.clone(), rt)?;
    for w in warnings {
        app.toast(w);
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mercury")
            .with_app_id("mercury")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(egui::IconData { rgba: crate::logo::pixels(&config.ui, 64, true), width: 64, height: 64 }),
        ..Default::default()
    };
    eframe::run_native(
        "Mercury",
        options,
        Box::new(|cc| {
            theme::install_fallback_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
