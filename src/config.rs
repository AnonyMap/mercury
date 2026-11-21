//! Paths and persistent configuration.
//!
//! Layout (XDG by default, everything under `$MERCURY_HOME` when that is set):
//!   config:   ~/.config/mercury/config.toml          shared: providers, appearance, tools, Nous
//!   data:     ~/.local/share/mercury/auth.json       shared: Nous sign-in
//!   profiles: ~/.local/share/mercury/                the `default` profile (where v0.1/0.2 data lives)
//!             ~/.local/share/mercury/profiles/<name>/ every other profile
//!   each profile: profile.toml, sessions.db, memories/, skills/, SOUL.md, checkpoints/, logs/

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    /// The active profile (`default` unless chosen).
    pub profile: String,
}

pub const DEFAULT_PROFILE: &str = "default";

impl Paths {
    /// Resolve paths from the environment. `MERCURY_HOME` puts everything under one directory,
    /// which is what tests and portable installs use.
    pub fn resolve() -> Self {
        if let Ok(home) = std::env::var("MERCURY_HOME")
            && !home.trim().is_empty()
        {
            return Self::under(PathBuf::from(home));
        }
        let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config")).join("mercury");
        let data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".local/share")).join("mercury");
        Self { config_dir, data_dir, profile: DEFAULT_PROFILE.into() }
    }

    pub fn under(root: PathBuf) -> Self {
        Self { config_dir: root.clone(), data_dir: root, profile: DEFAULT_PROFILE.into() }
    }

    /// The same install, scoped to another profile.
    pub fn with_profile(&self, name: &str) -> Self {
        Self { profile: name.to_string(), ..self.clone() }
    }

    /// Where this profile's chats, memory, skills and settings live.
    pub fn profile_dir(&self) -> PathBuf {
        if self.profile == DEFAULT_PROFILE { self.data_dir.clone() } else { self.data_dir.join("profiles").join(&self.profile) }
    }
    pub fn profile_file(&self) -> PathBuf {
        self.profile_dir().join("profile.toml")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn auth_file(&self) -> PathBuf {
        self.data_dir.join("auth.json")
    }
    pub fn sessions_db(&self) -> PathBuf {
        self.profile_dir().join("sessions.db")
    }
    pub fn default_memories_dir(&self) -> PathBuf {
        self.profile_dir().join("memories")
    }
    pub fn default_skills_dir(&self) -> PathBuf {
        self.profile_dir().join("skills")
    }
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir).with_context(|| format!("creating {}", self.config_dir.display()))?;
        std::fs::create_dir_all(self.profile_dir()).with_context(|| format!("creating {}", self.profile_dir().display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Any server that implements the OpenAI chat-completions API.
    #[default]
    Openai,
    /// Nous Portal, authenticated with the OAuth device-code flow.
    NousOauth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: ProviderKind,
    /// Base URL ending in `/v1` (or wherever `/chat/completions` lives).
    #[serde(default)]
    pub base_url: String,
    /// Literal API key. Prefer `api_key_env` for keys you don't want in a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub default_model: String,
    /// Send tool definitions. Turn off for local models that can't do function calling.
    #[serde(default = "yes")]
    pub tools: bool,
    /// Context window hint used for auto-compression; 0 = use the global default.
    #[serde(default)]
    pub context_window: u32,
}

fn yes() -> bool {
    true
}

impl ProviderConfig {
    pub fn openai(id: &str, name: &str, base_url: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: ProviderKind::Openai,
            base_url: base_url.trim_end_matches('/').into(),
            api_key: None,
            api_key_env: None,
            default_model: String::new(),
            tools: true,
            context_window: 0,
        }
    }

    pub fn nous_oauth() -> Self {
        Self {
            id: "nous".into(),
            name: "Nous Portal".into(),
            kind: ProviderKind::NousOauth,
            base_url: crate::nous::DEFAULT_INFERENCE_URL.into(),
            api_key: None,
            api_key_env: None,
            default_model: String::new(),
            tools: true,
            context_window: 0,
        }
    }

    pub fn is_local(&self) -> bool {
        is_local_url(&self.base_url)
    }

    /// The key to send, resolving `api_key_env` at call time.
    pub fn resolved_api_key(&self) -> Option<String> {
        if let Some(var) = &self.api_key_env
            && let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return Some(v.trim().to_string());
        }
        self.api_key.as_ref().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
    }
}

/// True for loopback and RFC1918 / link-local / CGNAT hosts and `.local` / `.lan` names.
pub fn is_local_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
        }
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || ip.is_unicast_link_local(),
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost"
                || d.ends_with(".localhost")
                || d.ends_with(".local")
                || d.ends_with(".lan")
                || d.ends_with(".home.arpa")
                || !d.contains('.')
        }
        None => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask before running a command that matches a dangerous pattern.
    #[default]
    Ask,
    /// Run everything (Hermes `/yolo`).
    Yolo,
    /// Refuse anything dangerous without asking.
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub terminal: bool,
    pub files: bool,
    pub web: bool,
    pub memory: bool,
    pub skills: bool,
    pub todo: bool,
    pub session_search: bool,
    pub terminal_timeout_secs: u64,
    /// SearXNG base URL (e.g. http://192.168.1.10:8888) for `web_search`.
    pub searxng_url: Option<String>,
    pub brave_api_key: Option<String>,
    /// `delegate_task`: hand sub-tasks to subagents with their own fresh context.
    pub delegate: bool,
    /// `clarify`: let the model ask you a question (with choices) mid-task.
    pub clarify: bool,
    /// Snapshot files before `write_file` / `patch` so `/rollback` can undo them.
    pub checkpoints: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            terminal: true,
            files: true,
            web: true,
            memory: true,
            skills: true,
            todo: true,
            session_search: true,
            terminal_timeout_secs: 180,
            searxng_url: None,
            brave_api_key: None,
            delegate: true,
            clarify: true,
            checkpoints: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub temperature: Option<f32>,
    /// Compress when estimated prompt tokens exceed this fraction of the context window.
    pub compression_threshold: f32,
    pub default_context_window: u32,
    pub personality: String,
    pub workdir: Option<PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 90,
            temperature: None,
            compression_threshold: 0.8,
            default_context_window: 32_768,
            personality: "default".into(),
            workdir: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NousConfig {
    pub portal_url: String,
    /// OAuth client id. Hermes's public id is the only registered one today; see PLAN.md R1.
    pub client_id: String,
    pub scope: String,
}

impl Default for NousConfig {
    fn default() -> Self {
        Self {
            portal_url: crate::nous::DEFAULT_PORTAL_URL.into(),
            client_id: crate::nous::DEFAULT_CLIENT_ID.into(),
            scope: crate::nous::DEFAULT_SCOPE.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// The profile Mercury opens in (shared setting).
    pub active_profile: Option<String>,
    pub active_provider: Option<String>,
    pub active_model: Option<String>,
    pub approvals: ApprovalMode,
    pub providers: Vec<ProviderConfig>,
    pub tools: ToolsConfig,
    pub agent: AgentConfig,
    pub nous: NousConfig,
    /// Override where MEMORY.md/USER.md live (e.g. `~/.hermes/memories` to share with Hermes).
    pub memories_dir: Option<PathBuf>,
    /// Extra read-only skill directories (e.g. `~/.hermes/skills`).
    pub extra_skill_dirs: Vec<PathBuf>,
    /// Override the writable skills folder (a Hermes profile's `skills/` when used in place).
    pub skills_dir: Option<PathBuf>,
    /// Override the persona file (a Hermes profile's `SOUL.md` when used in place).
    pub soul_file: Option<PathBuf>,
    /// MCP servers, same shape as Hermes's `mcp_servers:` block.
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Extra personalities for `/personality` (name → prompt), e.g. imported from Hermes.
    pub custom_personalities: BTreeMap<String, String>,
    /// Set when Mercury was connected to an existing Hermes profile.
    pub hermes: Option<HermesLink>,
    pub ui: UiConfig,
}

/// The part of the configuration that belongs to one profile (stored in its `profile.toml`).
/// Everything else in [`Config`] — providers, appearance, tools, approvals, Nous — is shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProfileSettings {
    pub active_provider: Option<String>,
    pub active_model: Option<String>,
    pub personality: Option<String>,
    pub memories_dir: Option<PathBuf>,
    pub skills_dir: Option<PathBuf>,
    pub soul_file: Option<PathBuf>,
    pub extra_skill_dirs: Vec<PathBuf>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub custom_personalities: BTreeMap<String, String>,
    pub hermes: Option<HermesLink>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    /// stdio server: program to launch.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// HTTP (streamable) server URL.
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Per-call timeout in seconds (0 = 120).
    pub timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkMode {
    /// Copied once; Hermes and Mercury then evolve separately.
    #[default]
    Copy,
    /// Mercury reads and writes the Hermes profile's memory, skills and SOUL.md directly.
    InPlace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HermesLink {
    pub home: PathBuf,
    pub profile: String,
    pub mode: LinkMode,
    #[serde(default)]
    pub sessions_imported: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Logo {
    /// An image the user picked (`ui.custom_logo`).
    Custom,
    /// The quicksilver orb. Also what any unknown value (e.g. an older build's setting) reads as.
    #[default]
    #[serde(other)]
    Orb,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub dark: bool,
    pub show_reasoning: bool,
    /// UI zoom. 0 = pick automatically from the monitor size on first run.
    pub font_scale: f32,
    pub logo: Logo,
    /// Mercury's own copy of the custom logo image (PNG).
    pub custom_logo: Option<PathBuf>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { dark: true, show_reasoning: false, font_scale: 0.0, logo: Logo::Orb, custom_logo: None }
    }
}

pub const MIN_SCALE: f32 = 0.75;
pub const MAX_SCALE: f32 = 3.0;

/// Default zoom for a monitor `width` logical points wide: 1.0 up to 1920, rising to 1.75 at 3840
/// (a 4K panel without desktop scaling, where text is otherwise tiny).
pub fn auto_scale(monitor_width: f32) -> f32 {
    if !monitor_width.is_finite() || monitor_width <= 0.0 {
        return 1.0;
    }
    (1.0 + (monitor_width - 1920.0) / 1920.0 * 0.75).clamp(1.0, 2.0)
}

/// Snap a zoom value to the 5% grid inside the allowed range.
pub fn clamp_scale(s: f32) -> f32 {
    ((s.clamp(MIN_SCALE, MAX_SCALE) * 20.0).round()) / 20.0
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Shared settings from `config.toml` merged with `paths.profile`'s own `profile.toml`.
    ///
    /// A profile without a `profile.toml` is either the `default` profile of an older install
    /// (its settings are still in `config.toml`, so they're kept) or a brand-new profile, which
    /// starts with the last-used model and personality but its own empty memory, skills and MCP.
    pub fn load_profile(paths: &Paths) -> Result<Self> {
        let mut cfg = Self::load(&paths.config_file())?;
        let file = paths.profile_file();
        match std::fs::read_to_string(&file) {
            Ok(t) => {
                let ps: ProfileSettings = toml::from_str(&t).with_context(|| format!("parsing {}", file.display()))?;
                cfg.apply_profile(ps);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if paths.profile != DEFAULT_PROFILE {
                    let inherited = ProfileSettings {
                        active_provider: cfg.active_provider.clone(),
                        active_model: cfg.active_model.clone(),
                        personality: Some(cfg.agent.personality.clone()),
                        ..Default::default()
                    };
                    cfg.apply_profile(inherited);
                }
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        }
        if cfg.active_provider.as_deref().is_some_and(|id| cfg.provider(id).is_none()) {
            cfg.active_provider = None;
            cfg.active_model = None;
        }
        Ok(cfg)
    }

    /// Write this profile's part to its `profile.toml` and the shared part to `config.toml`.
    /// The shared file keeps the last-used model and personality, which new profiles start from.
    pub fn save_profile(&self, paths: &Paths) -> Result<()> {
        let file = paths.profile_file();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(&self.profile_settings())?)?;
        std::fs::rename(&tmp, &file)?;
        let mut shared = self.clone();
        shared.memories_dir = None;
        shared.skills_dir = None;
        shared.soul_file = None;
        shared.extra_skill_dirs.clear();
        shared.mcp_servers.clear();
        shared.custom_personalities.clear();
        shared.hermes = None;
        shared.save(&paths.config_file())
    }

    pub fn profile_settings(&self) -> ProfileSettings {
        ProfileSettings {
            active_provider: self.active_provider.clone(),
            active_model: self.active_model.clone(),
            personality: Some(self.agent.personality.clone()),
            memories_dir: self.memories_dir.clone(),
            skills_dir: self.skills_dir.clone(),
            soul_file: self.soul_file.clone(),
            extra_skill_dirs: self.extra_skill_dirs.clone(),
            mcp_servers: self.mcp_servers.clone(),
            custom_personalities: self.custom_personalities.clone(),
            hermes: self.hermes.clone(),
        }
    }

    pub fn apply_profile(&mut self, p: ProfileSettings) {
        self.active_provider = p.active_provider;
        self.active_model = p.active_model;
        if let Some(pers) = p.personality {
            self.agent.personality = pers;
        }
        self.memories_dir = p.memories_dir;
        self.skills_dir = p.skills_dir;
        self.soul_file = p.soul_file;
        self.extra_skill_dirs = p.extra_skill_dirs;
        self.mcp_servers = p.mcp_servers;
        self.custom_personalities = p.custom_personalities;
        self.hermes = p.hermes;
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn active(&self) -> Option<&ProviderConfig> {
        self.active_provider.as_deref().and_then(|id| self.provider(id)).or_else(|| self.providers.first())
    }

    /// The model to use with the active provider.
    pub fn active_model_name(&self) -> Option<String> {
        let p = self.active()?;
        self.active_model.clone().filter(|m| !m.is_empty()).or_else(|| Some(p.default_model.clone()).filter(|m| !m.is_empty()))
    }

    /// Insert or replace a provider by id; returns true when it was new.
    pub fn upsert_provider(&mut self, p: ProviderConfig) -> bool {
        if let Some(existing) = self.providers.iter_mut().find(|x| x.id == p.id) {
            *existing = p;
            false
        } else {
            self.providers.push(p);
            true
        }
    }

    pub fn remove_provider(&mut self, id: &str) {
        self.providers.retain(|p| p.id != id);
        if self.active_provider.as_deref() == Some(id) {
            self.active_provider = self.providers.first().map(|p| p.id.clone());
            self.active_model = None;
        }
    }

    pub fn set_active(&mut self, provider: &str, model: Option<String>) -> bool {
        if self.provider(provider).is_none() {
            return false;
        }
        self.active_provider = Some(provider.to_string());
        self.active_model = model;
        true
    }

    pub fn skills_dir(&self, paths: &Paths) -> PathBuf {
        self.skills_dir.clone().unwrap_or_else(|| paths.default_skills_dir())
    }

    pub fn soul_file(&self, paths: &Paths) -> PathBuf {
        self.soul_file.clone().unwrap_or_else(|| paths.profile_dir().join("SOUL.md"))
    }

    pub fn memories_dir(&self, paths: &Paths) -> PathBuf {
        self.memories_dir.clone().unwrap_or_else(|| paths.default_memories_dir())
    }
}

/// Built-in endpoints offered in the "Add endpoint" dialog.
pub fn presets() -> Vec<ProviderConfig> {
    let mut v = vec![
        ProviderConfig::openai("ollama", "Ollama (this machine)", "http://127.0.0.1:11434/v1"),
        ProviderConfig::openai("lmstudio", "LM Studio (this machine)", "http://127.0.0.1:1234/v1"),
        ProviderConfig::openai("llamacpp", "llama.cpp server (this machine)", "http://127.0.0.1:8080/v1"),
        ProviderConfig::openai("vllm", "vLLM (this machine)", "http://127.0.0.1:8000/v1"),
    ];
    let mut nous_key = ProviderConfig::openai("nous-api", "Nous Portal (API key)", crate::nous::DEFAULT_INFERENCE_URL);
    nous_key.api_key_env = Some("NOUS_API_KEY".into());
    v.push(nous_key);
    let mut openrouter = ProviderConfig::openai("openrouter", "OpenRouter", "https://openrouter.ai/api/v1");
    openrouter.api_key_env = Some("OPENROUTER_API_KEY".into());
    v.push(openrouter);
    let mut openai = ProviderConfig::openai("openai", "OpenAI", "https://api.openai.com/v1");
    openai.api_key_env = Some("OPENAI_API_KEY".into());
    v.push(openai);
    v
}

/// Turn a display name into a provider id that doesn't collide with existing ones.
pub fn unique_provider_id(cfg: &Config, wanted: &str) -> String {
    let base: String = wanted
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if base.is_empty() { "provider".into() } else { base };
    if cfg.provider(&base).is_none() {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| cfg.provider(id).is_none()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let missing = Config::load(&path).unwrap();
        assert_eq!(missing, Config::default());
        assert_eq!(missing.agent.max_iterations, 90);

        let mut cfg = Config::default();
        cfg.upsert_provider(ProviderConfig::openai("ollama", "Ollama", "http://127.0.0.1:11434/v1/"));
        assert!(cfg.set_active("ollama", Some("qwen3:8b".into())));
        assert!(!cfg.set_active("nope", None));
        cfg.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.active().unwrap().base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(back.active_model_name().as_deref(), Some("qwen3:8b"));
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let cfg: Config = toml::from_str("approvals = \"yolo\"\n[[providers]]\nid='x'\nname='X'\nbase_url='http://h:1/v1'\n").unwrap();
        assert_eq!(cfg.approvals, ApprovalMode::Yolo);
        assert!(cfg.providers[0].tools);
        assert_eq!(cfg.providers[0].kind, ProviderKind::Openai);
        assert_eq!(cfg.tools.terminal_timeout_secs, 180);
    }

    #[test]
    fn local_url_detection() {
        for u in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:1234/v1",
            "http://192.168.1.20:8080/v1",
            "http://10.0.0.5/v1",
            "http://gpubox.local:8000/v1",
            "http://gpubox:8000/v1",
            "http://100.101.1.2:8000/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(is_local_url(u), "{u}");
        }
        for u in ["https://api.openai.com/v1", "https://inference-api.nousresearch.com/v1", "http://8.8.8.8/v1", "nonsense"] {
            assert!(!is_local_url(u), "{u}");
        }
    }

    #[test]
    fn api_key_env_wins() {
        let mut p = ProviderConfig::openai("a", "A", "http://x/v1");
        p.api_key = Some(" literal ".into());
        assert_eq!(p.resolved_api_key().as_deref(), Some("literal"));
        p.api_key_env = Some("MERCURY_TEST_KEY_ENV_WINS".into());
        // SAFETY: test-local variable name, not read by other tests.
        unsafe { std::env::set_var("MERCURY_TEST_KEY_ENV_WINS", "fromenv") };
        assert_eq!(p.resolved_api_key().as_deref(), Some("fromenv"));
    }

    #[test]
    fn remove_active_falls_back() {
        let mut cfg = Config::default();
        cfg.upsert_provider(ProviderConfig::openai("a", "A", "http://a/v1"));
        cfg.upsert_provider(ProviderConfig::openai("b", "B", "http://b/v1"));
        cfg.set_active("b", Some("m".into()));
        cfg.remove_provider("b");
        assert_eq!(cfg.active_provider.as_deref(), Some("a"));
        assert_eq!(cfg.active_model, None);
    }

    #[test]
    fn profiles_split_settings() {
        let dir = tempfile::tempdir().unwrap();
        let root = Paths::under(dir.path().to_path_buf());
        // An older install: everything in config.toml.
        let mut legacy = Config::default();
        legacy.upsert_provider(ProviderConfig::openai("box", "GPU box", "http://10.0.0.2:8000/v1"));
        legacy.upsert_provider(ProviderConfig::openai("laptop", "Laptop", "http://127.0.0.1:11434/v1"));
        legacy.set_active("box", Some("qwen3".into()));
        legacy.memories_dir = Some("/legacy/memories".into());
        legacy.save(&root.config_file()).unwrap();

        // The default profile keeps its legacy settings.
        let def = Config::load_profile(&root).unwrap();
        assert_eq!(def.memories_dir.as_deref(), Some(Path::new("/legacy/memories")));
        def.save_profile(&root).unwrap();
        assert!(root.profile_file().exists());
        assert_eq!(Config::load(&root.config_file()).unwrap().memories_dir, None, "moved out of the shared file");
        assert_eq!(Config::load_profile(&root).unwrap().memories_dir.as_deref(), Some(Path::new("/legacy/memories")));

        // A new profile starts from the last-used model, with its own (empty) memory location.
        let work = root.with_profile("work");
        assert_eq!(work.profile_dir(), dir.path().join("profiles/work"));
        assert_eq!(work.sessions_db(), dir.path().join("profiles/work/sessions.db"));
        let mut w = Config::load_profile(&work).unwrap();
        assert_eq!(w.active_model_name().as_deref(), Some("qwen3"));
        assert_eq!(w.memories_dir, None);
        w.set_active("laptop", Some("llama3".into()));
        w.providers[0].name = "Basement GPU".into(); // shared: visible from every profile
        w.save_profile(&work).unwrap();

        let def = Config::load_profile(&root).unwrap();
        assert_eq!(def.active_model_name().as_deref(), Some("qwen3"), "each profile keeps its own model");
        assert_eq!(def.provider("box").unwrap().name, "Basement GPU");
        assert_eq!(Config::load_profile(&work).unwrap().active_model_name().as_deref(), Some("llama3"));

        // A profile pointing at a provider that was removed falls back cleanly.
        let mut shared = Config::load(&root.config_file()).unwrap();
        shared.remove_provider("laptop");
        shared.save(&root.config_file()).unwrap();
        let w = Config::load_profile(&work).unwrap();
        assert_eq!(w.active().unwrap().id, "box");
        assert_eq!(w.active_model, None);
    }

    #[test]
    fn unknown_logo_values_fall_back_to_orb() {
        let cfg: Config = toml::from_str("[ui]\nlogo = \"headphones\"\n").unwrap();
        assert_eq!(cfg.ui.logo, Logo::Orb);
    }

    #[test]
    fn scale_helpers() {
        assert_eq!(auto_scale(1920.0), 1.0);
        assert_eq!(auto_scale(1366.0), 1.0);
        assert!((auto_scale(2560.0) - 1.25).abs() < 1e-4);
        assert_eq!(auto_scale(3840.0), 1.75);
        assert_eq!(auto_scale(7680.0), 2.0);
        assert_eq!(auto_scale(f32::NAN), 1.0);
        assert_eq!(clamp_scale(1.234), 1.25);
        assert_eq!(clamp_scale(9.0), MAX_SCALE);
        assert_eq!(clamp_scale(0.1), MIN_SCALE);
    }

    #[test]
    fn mcp_and_link_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        let mut cfg = Config::default();
        cfg.mcp_servers.insert(
            "time".into(),
            McpServerConfig { command: Some("uvx".into()), args: vec!["mcp-server-time".into()], enabled: true, ..Default::default() },
        );
        cfg.hermes = Some(HermesLink { home: "/h".into(), profile: "coder".into(), mode: LinkMode::InPlace, sessions_imported: 3 });
        cfg.ui.logo = Logo::Custom;
        cfg.ui.custom_logo = Some("/data/custom-logo.png".into());
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn unique_ids() {
        let mut cfg = Config::default();
        assert_eq!(unique_provider_id(&cfg, "My GPU Box!"), "my-gpu-box");
        cfg.upsert_provider(ProviderConfig::openai("my-gpu-box", "x", "http://x/v1"));
        assert_eq!(unique_provider_id(&cfg, "My GPU Box!"), "my-gpu-box-2");
        assert_eq!(unique_provider_id(&cfg, "!!!"), "provider");
    }
}
