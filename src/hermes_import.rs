//! Adopt an existing Hermes install: point Mercury at a Hermes home (`~/.hermes`), its `profiles/`
//! folder, or a single profile, pick a profile, and carry on from it.
//!
//! Two modes:
//! * **Copy** — model/provider, keys, MCP servers, personalities, memories, SOUL.md and conversation
//!   history are copied into Mercury once; Hermes's skills are linked read-only. Hermes keeps running
//!   untouched. Right when you'll use both.
//! * **In place** — same config import, but Mercury then reads and writes the profile's own
//!   `memories/`, `skills/` and `SOUL.md`. Right when you're switching over. Hermes's own docs warn
//!   against two agents writing one profile at the same time, so don't run both on it at once.
//!
//! Nous OAuth tokens are never copied: the Portal's refresh tokens are single-use, and a second
//! client redeeming the same one trips reuse detection and signs *both* out. A `NOUS_API_KEY` in the
//! profile's `.env` is imported (static keys are safe to share).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_yaml::Value;

use crate::config::{Config, HermesLink, LinkMode, McpServerConfig, Paths, ProviderConfig};
use crate::provider::{FunctionCall, Message, Role, ToolCall};

pub fn hermes_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HERMES_HOME")
        && !h.trim().is_empty()
    {
        return Some(PathBuf::from(h));
    }
    dirs::home_dir().map(|h| h.join(".hermes"))
}

/// Files that make a directory a Hermes profile (same rule as `hermes profile list`).
const IDENTITY_FILES: &[&str] = &["config.yaml", ".env", "SOUL.md", "profile.yaml", "auth.json", "state.db"];

pub fn is_profile_dir(p: &Path) -> bool {
    IDENTITY_FILES.iter().any(|f| p.join(f).exists()) || p.join("memories").is_dir()
}

pub fn detect() -> Option<PathBuf> {
    hermes_home().filter(|h| is_profile_dir(h) || h.join("profiles").is_dir())
}

#[derive(Debug, Clone, PartialEq)]
pub struct HermesProfile {
    pub name: String,
    pub home: PathBuf,
}

/// Profiles reachable from `path`, which may be a Hermes root, its `profiles/` folder, or one
/// profile. The root itself is the `default` profile.
pub fn list_profiles(path: &Path) -> Vec<HermesProfile> {
    let mut out = Vec::new();
    let (root, profiles_dir) = if path.file_name().map(|n| n == "profiles").unwrap_or(false) {
        (path.parent().map(Path::to_path_buf), path.to_path_buf())
    } else {
        (Some(path.to_path_buf()), path.join("profiles"))
    };
    if let Some(r) = &root
        && is_profile_dir(r)
    {
        let name = if r.parent().map(|p| p.file_name().map(|n| n == "profiles").unwrap_or(false)).unwrap_or(false) {
            r.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "default".into())
        } else {
            "default".into()
        };
        out.push(HermesProfile { name, home: r.clone() });
    }
    if let Ok(rd) = std::fs::read_dir(&profiles_dir) {
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && is_profile_dir(p)).collect();
        dirs.sort();
        for d in dirs {
            let name = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if !out.iter().any(|p| p.home == d) {
                out.push(HermesProfile { name, home: d });
            }
        }
    }
    out
}

/// What a profile holds, for the picker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileSummary {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub memory_entries: usize,
    pub user_entries: usize,
    pub skills: usize,
    pub sessions: usize,
    pub mcp_servers: usize,
    pub has_soul: bool,
    pub nous_oauth: bool,
    pub nous_api_key: bool,
}

fn read_yaml(home: &Path) -> Value {
    std::fs::read_to_string(home.join("config.yaml")).ok().and_then(|t| serde_yaml::from_str(&t).ok()).unwrap_or(Value::Null)
}

fn read_env(home: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(home.join(".env")).map(|t| parse_env(&t)).unwrap_or_default()
}

pub fn summarize(profile: &HermesProfile) -> ProfileSummary {
    let yaml = read_yaml(&profile.home);
    let env = read_env(&profile.home);
    let model = yaml.get("model").cloned().unwrap_or(Value::Null);
    let mem = crate::memory::MemoryStore::new(profile.home.join("memories"));
    let auth: serde_json::Value = std::fs::read_to_string(profile.home.join("auth.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    ProfileSummary {
        provider: s(&model, "provider"),
        model: s(&model, "default").or_else(|| s(&model, "model")),
        memory_entries: mem.entries(crate::memory::Target::Memory).len(),
        user_entries: mem.entries(crate::memory::Target::User).len(),
        skills: crate::skills::SkillLibrary::new(profile.home.join("skills"), vec![]).list().len(),
        sessions: count_sessions(&profile.home.join("state.db")).unwrap_or(0),
        mcp_servers: yaml.get("mcp_servers").and_then(Value::as_mapping).map(|m| m.len()).unwrap_or(0),
        has_soul: profile.home.join("SOUL.md").is_file(),
        nous_oauth: auth.pointer("/providers/nous").is_some() || auth.get("nous").is_some(),
        nous_api_key: env.get("NOUS_API_KEY").map(|k| !k.is_empty()).unwrap_or(false),
    }
}
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ImportReport {
    pub providers_added: Vec<String>,
    pub active: Option<(String, String)>,
    pub memories_copied: Vec<String>,
    pub skills_dir: Option<PathBuf>,
    pub soul_copied: bool,
    pub mcp_servers: Vec<String>,
    pub personalities: Vec<String>,
    pub sessions_imported: usize,
    pub notes: Vec<String>,
}

impl ImportReport {
    pub fn summary(&self) -> String {
        let n = self.providers_added.len();
        let mut parts = vec![format!("{n} provider{}", if n == 1 { "" } else { "s" })];
        if let Some((p, m)) = &self.active {
            parts.push(format!("active {p}{}", if m.is_empty() { String::new() } else { format!(" / {m}") }));
        }
        if self.sessions_imported > 0 {
            parts.push(format!("{} conversation{}", self.sessions_imported, if self.sessions_imported == 1 { "" } else { "s" }));
        }
        if !self.memories_copied.is_empty() {
            parts.push(format!("memory {}", self.memories_copied.join(", ")));
        }
        if self.soul_copied {
            parts.push("SOUL.md".into());
        }
        if !self.mcp_servers.is_empty() {
            parts.push(format!("MCP: {}", self.mcp_servers.join(", ")));
        }
        if !self.personalities.is_empty() {
            parts.push(format!("personalities: {}", self.personalities.join(", ")));
        }
        if self.skills_dir.is_some() {
            parts.push("skills".into());
        }
        let mut s = parts.join(" · ");
        for n in &self.notes {
            s.push_str("\n• ");
            s.push_str(n);
        }
        s
    }
}

/// Parse a dotenv file: `KEY=value`, optional `export`, quotes, `#` comments.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        let mut v = v.trim().to_string();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
            v = v[1..v.len() - 1].to_string();
        } else if let Some(i) = v.find(" #") {
            v.truncate(i);
            v = v.trim().to_string();
        }
        if !k.is_empty() {
            out.insert(k.to_string(), v);
        }
    }
    out
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string)
}

/// Build provider entries from a parsed Hermes config + env. Pure, so it's easy to test.
pub fn providers_from(yaml: &Value, env: &HashMap<String, String>) -> (Vec<ProviderConfig>, Option<(String, String)>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let model = yaml.get("model").cloned().unwrap_or(Value::Null);
    let provider = s(&model, "provider").unwrap_or_else(|| "auto".into()).to_ascii_lowercase();
    let default_model = s(&model, "default").or_else(|| s(&model, "model")).unwrap_or_default();
    let base_url = s(&model, "base_url");
    let inline_key = s(&model, "api_key");
    let ctx = model.get("context_length").and_then(Value::as_u64).unwrap_or(0) as u32;
    let key = |name: &str| env.get(name).cloned().filter(|k| !k.is_empty());

    let mut primary: Option<ProviderConfig> = match provider.as_str() {
        "nous" => {
            notes.push("Hermes uses Nous Portal sign-in; sign in again from Mercury (tokens are not shared).".into());
            Some(ProviderConfig::nous_oauth())
        }
        "nous-api" => {
            let mut p = ProviderConfig::openai(
                "nous-api",
                "Nous Portal (API key)",
                &base_url.clone().unwrap_or_else(|| crate::nous::DEFAULT_INFERENCE_URL.into()),
            );
            p.api_key = inline_key.clone().or_else(|| key("NOUS_API_KEY"));
            Some(p)
        }
        "openrouter" => {
            let mut p = ProviderConfig::openai("openrouter", "OpenRouter", "https://openrouter.ai/api/v1");
            p.api_key = inline_key.clone().or_else(|| key("OPENROUTER_API_KEY")).or_else(|| key("OPENAI_API_KEY"));
            Some(p)
        }
        "openai-api" | "openai" => {
            let mut p = ProviderConfig::openai(
                "openai",
                "OpenAI",
                &base_url.clone().or_else(|| key("OPENAI_BASE_URL")).unwrap_or_else(|| "https://api.openai.com/v1".into()),
            );
            p.api_key = inline_key.clone().or_else(|| key("OPENAI_API_KEY"));
            Some(p)
        }
        "lmstudio" => {
            let mut p =
                ProviderConfig::openai("lmstudio", "LM Studio", &base_url.clone().unwrap_or_else(|| "http://127.0.0.1:1234/v1".into()));
            p.api_key = inline_key.clone().or_else(|| key("LM_API_KEY"));
            Some(p)
        }
        "custom" | "ollama" | "vllm" | "llamacpp" | "auto" => match base_url.clone() {
            Some(url) => {
                let local = crate::config::is_local_url(&url);
                let name = if local { "Local endpoint (from Hermes)" } else { "Custom endpoint (from Hermes)" };
                let mut p = ProviderConfig::openai("hermes-custom", name, &url);
                p.api_key = inline_key.clone().or_else(|| key("OPENAI_API_KEY"));
                Some(p)
            }
            None => {
                if provider == "auto" {
                    if key("NOUS_API_KEY").is_some() {
                        let mut p = ProviderConfig::openai("nous-api", "Nous Portal (API key)", crate::nous::DEFAULT_INFERENCE_URL);
                        p.api_key = key("NOUS_API_KEY");
                        Some(p)
                    } else if key("OPENROUTER_API_KEY").is_some() {
                        let mut p = ProviderConfig::openai("openrouter", "OpenRouter", "https://openrouter.ai/api/v1");
                        p.api_key = key("OPENROUTER_API_KEY");
                        Some(p)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        },
        other => {
            notes.push(format!("Hermes provider `{other}` isn't OpenAI-compatible in Mercury; skipped. Add its OpenAI-compatible URL manually if it has one."));
            None
        }
    };
    if let Some(p) = primary.as_mut() {
        p.default_model = default_model.clone();
        p.context_window = ctx;
    }
    let active = primary.as_ref().map(|p| (p.id.clone(), default_model.clone()));
    out.extend(primary);

    // Named providers (`providers:` / `custom_providers:` maps or lists).
    for section in ["providers", "custom_providers"] {
        let entries: Vec<(String, Value)> = match yaml.get(section) {
            Some(Value::Mapping(m)) => m.iter().filter_map(|(k, v)| k.as_str().map(|k| (k.to_string(), v.clone()))).collect(),
            Some(Value::Sequence(seq)) => seq.iter().filter_map(|v| s(v, "name").map(|n| (n, v.clone()))).collect(),
            _ => vec![],
        };
        for (name, v) in entries {
            let Some(url) = s(&v, "base_url") else { continue };
            let id = name.to_ascii_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-");
            if out.iter().any(|p: &ProviderConfig| p.id == id) {
                continue;
            }
            let mut p = ProviderConfig::openai(&id, &name, &url);
            p.api_key = s(&v, "api_key").or_else(|| s(&v, "key_env").and_then(|k| key(&k)));
            p.default_model = s(&v, "model").or_else(|| s(&v, "default")).unwrap_or_default();
            out.push(p);
        }
    }
    (out, active, notes)
}

/// MCP servers from Hermes's `mcp_servers:` block.
pub fn mcp_from(yaml: &Value) -> BTreeMap<String, McpServerConfig> {
    let mut out = BTreeMap::new();
    let Some(map) = yaml.get("mcp_servers").and_then(Value::as_mapping) else { return out };
    for (k, v) in map {
        let Some(name) = k.as_str() else { continue };
        let strs = |key: &str| -> Vec<String> {
            v.get(key).and_then(Value::as_sequence).map(|a| a.iter().filter_map(scalar).collect()).unwrap_or_default()
        };
        let map_of = |key: &str| -> BTreeMap<String, String> {
            v.get(key)
                .and_then(Value::as_mapping)
                .map(|m| m.iter().filter_map(|(k, v)| Some((k.as_str()?.to_string(), scalar(v)?))).collect())
                .unwrap_or_default()
        };
        let cfg = McpServerConfig {
            command: s(v, "command"),
            args: strs("args"),
            env: map_of("env"),
            url: s(v, "url"),
            headers: map_of("headers"),
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            timeout: v.get("timeout").and_then(Value::as_u64).unwrap_or(0),
        };
        if cfg.command.is_some() || cfg.url.is_some() {
            out.insert(name.to_string(), cfg);
        }
    }
    out
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Custom personalities (`agent.personalities` or top-level `personalities`): string or `{system_prompt}`.
pub fn personalities_from(yaml: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for m in [yaml.get("agent").and_then(|a| a.get("personalities")), yaml.get("personalities")].into_iter().flatten() {
        let Some(map) = m.as_mapping() else { continue };
        for (k, v) in map {
            let Some(name) = k.as_str() else { continue };
            let prompt = match v {
                Value::String(p) => Some(p.clone()),
                other => s(other, "system_prompt").map(|p| match s(other, "tone") {
                    Some(t) => format!("{p} Tone: {t}."),
                    None => p,
                }),
            };
            if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
                out.insert(name.to_string(), p);
            }
        }
    }
    out
}

/// Copy MEMORY.md/USER.md entries from `src` into `dst`, skipping duplicates and stopping at the cap.
fn merge_memories(src: &Path, dst: &Path, report: &mut ImportReport) -> Result<()> {
    use crate::memory::{MemoryStore, Target};
    let from = MemoryStore::new(src);
    let to = MemoryStore::new(dst);
    for t in [Target::Memory, Target::User] {
        let mut added = 0;
        for e in from.entries(t) {
            if to.entries(t).contains(&e) {
                continue;
            }
            match to.add(t, &e) {
                Ok(_) => added += 1,
                Err(err) => {
                    report.notes.push(format!("{}: stopped copying ({err})", t.file()));
                    break;
                }
            }
        }
        if added > 0 {
            report.memories_copied.push(format!("{} (+{added})", t.file()));
        }
    }
    Ok(())
}

/// Adopt `profile` into Mercury. Idempotent: running it again only brings over what's new.
pub fn adopt(profile: &HermesProfile, mode: LinkMode, import_sessions: bool, paths: &Paths, cfg: &mut Config) -> Result<ImportReport> {
    let home = &profile.home;
    if !is_profile_dir(home) {
        return Err(anyhow!("{} doesn't look like a Hermes profile (no config.yaml, .env, SOUL.md or memories/)", home.display()));
    }
    let mut report = ImportReport::default();
    let yaml = read_yaml(home);
    let env = read_env(home);
    let (providers, active, notes) = providers_from(&yaml, &env);
    report.notes = notes;
    for p in providers {
        report.providers_added.push(p.name.clone());
        cfg.upsert_provider(p);
    }
    if let Some((pid, model)) = &active {
        cfg.set_active(pid, Some(model.clone()).filter(|m| !m.is_empty()));
        report.active = active.clone();
    }
    let mcp = mcp_from(&yaml);
    report.mcp_servers = mcp.keys().cloned().collect();
    cfg.mcp_servers.extend(mcp);
    let pers = personalities_from(&yaml);
    report.personalities = pers.keys().cloned().collect();
    cfg.custom_personalities.extend(pers);
    if let Some(p) = yaml.get("display").and_then(|d| s(d, "personality")) {
        cfg.agent.personality = p;
    }

    let skills = home.join("skills");
    match mode {
        LinkMode::InPlace => {
            cfg.memories_dir = Some(home.join("memories"));
            std::fs::create_dir_all(&skills).ok();
            cfg.skills_dir = Some(skills.clone());
            cfg.extra_skill_dirs.retain(|d| d != &skills);
            if home.join("SOUL.md").is_file() {
                cfg.soul_file = Some(home.join("SOUL.md"));
            }
            report.skills_dir = Some(skills);
            report.notes.push("Using this Hermes profile in place: don't run Hermes on the same profile at the same time.".into());
        }
        LinkMode::Copy => {
            // Undo an earlier in-place link to this profile.
            if cfg.memories_dir.as_deref() == Some(home.join("memories").as_path()) {
                cfg.memories_dir = None;
            }
            if cfg.skills_dir.as_deref() == Some(skills.as_path()) {
                cfg.skills_dir = None;
            }
            if cfg.soul_file.as_deref() == Some(home.join("SOUL.md").as_path()) {
                cfg.soul_file = None;
            }
            merge_memories(&home.join("memories"), &cfg.memories_dir(paths), &mut report)?;
            let soul_dst = cfg.soul_file(paths);
            if home.join("SOUL.md").is_file() && !soul_dst.exists() {
                std::fs::create_dir_all(soul_dst.parent().unwrap_or(Path::new(".")))?;
                std::fs::copy(home.join("SOUL.md"), &soul_dst)?;
                report.soul_copied = true;
            }
            if skills.is_dir() && !cfg.extra_skill_dirs.contains(&skills) {
                cfg.extra_skill_dirs.push(skills.clone());
                report.skills_dir = Some(skills);
            }
        }
    }
    let summary = summarize(profile);
    if summary.nous_oauth && !summary.nous_api_key {
        report.notes.push("Hermes is signed in to Nous Portal with OAuth. Those tokens are single-use, so Mercury needs its own sign-in (Settings > Nous Portal) rather than borrowing them.".into());
    }
    if import_sessions {
        let db = home.join("state.db");
        if db.is_file() {
            let store = crate::session::SessionStore::open(&paths.sessions_db())?;
            let (n, skipped) = import_sessions_from(&db, &profile.name, &store)?;
            report.sessions_imported = n;
            if skipped > 0 {
                report.notes.push(format!("{skipped} empty or unreadable Hermes sessions were skipped."));
            }
        }
    }
    let prev = cfg.hermes.as_ref().map(|h| h.sessions_imported).unwrap_or(0);
    cfg.hermes = Some(HermesLink {
        home: home.clone(),
        profile: profile.name.clone(),
        mode,
        sessions_imported: prev + report.sessions_imported as u64,
    });
    Ok(report)
}

/// Back-compat wrapper: copy-mode import of the profile at `hermes`.
pub fn import(hermes: &Path, paths: &Paths, cfg: &mut Config) -> Result<ImportReport> {
    let profile = HermesProfile { name: "default".into(), home: hermes.to_path_buf() };
    adopt(&profile, LinkMode::Copy, true, paths, cfg)
}

// ---- Hermes session history ------------------------------------------------------------

/// Open Hermes's state.db without ever writing to it: read-only first, a private copy if that fails.
fn open_readonly(db: &Path) -> Result<(rusqlite::Connection, Option<tempfile::TempDir>)> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(c) = rusqlite::Connection::open_with_flags(db, flags)
        && c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_ok()
    {
        return Ok((c, None));
    }
    let tmp = tempfile::tempdir()?;
    for suffix in ["", "-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{suffix}", db.display()));
        if src.exists() {
            std::fs::copy(&src, tmp.path().join(format!("state.db{suffix}")))?;
        }
    }
    let c = rusqlite::Connection::open(tmp.path().join("state.db")).context("opening a copy of Hermes's state.db")?;
    Ok((c, Some(tmp)))
}

fn columns(c: &rusqlite::Connection, table: &str) -> Vec<String> {
    let Ok(mut st) = c.prepare(&format!("PRAGMA table_info({table})")) else { return vec![] };
    st.query_map([], |r| r.get::<_, String>(1)).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

fn count_sessions(db: &Path) -> Result<usize> {
    if !db.is_file() {
        return Ok(0);
    }
    let (c, _tmp) = open_readonly(db)?;
    let hidden = if columns(&c, "sessions").iter().any(|x| x == "hidden") { " WHERE coalesce(hidden,0) = 0" } else { "" };
    Ok(c.query_row(&format!("SELECT count(*) FROM sessions{hidden}"), [], |r| r.get::<_, i64>(0))? as usize)
}

/// Message text from Hermes's `content` column, which may hold a JSON list of content parts.
fn content_text(raw: Option<String>) -> String {
    let Some(t) = raw else { return String::new() };
    if t.trim_start().starts_with('[')
        && let Ok(serde_json::Value::Array(parts)) = serde_json::from_str::<serde_json::Value>(&t)
    {
        return parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|x| x.as_str()).map(str::to_string).or_else(|| p.as_str().map(str::to_string)))
            .collect::<Vec<_>>()
            .join("\n");
    }
    t
}

fn parse_tool_calls(raw: Option<String>) -> Vec<ToolCall> {
    let Some(t) = raw.filter(|t| !t.trim().is_empty()) else { return vec![] };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { return vec![] };
    let list = match v {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    list.iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let f = c.get("function").unwrap_or(c);
            let name = f.get("name").and_then(|x| x.as_str())?.to_string();
            let arguments = match f.get("arguments").or_else(|| f.get("args")) {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".into(),
            };
            let id = c.get("id").and_then(|x| x.as_str()).map(str::to_string).unwrap_or_else(|| format!("hermes_call_{i}"));
            Some(ToolCall { id, kind: "function".into(), function: FunctionCall { name, arguments } })
        })
        .collect()
}

/// Make an imported transcript valid to continue: drop orphan tool results and give every tool
/// call a result, as chat-completions servers reject anything else.
pub fn repair_transcript(msgs: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut open: Vec<(String, String)> = vec![];
    let close_open = |out: &mut Vec<Message>, open: &mut Vec<(String, String)>| {
        for (id, name) in open.drain(..) {
            out.push(Message::tool(&id, &name, "(no result was recorded for this call)"));
        }
    };
    for m in msgs {
        match m.role {
            Role::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                if let Some(i) = open.iter().position(|(cid, _)| *cid == id) {
                    open.remove(i);
                    out.push(m);
                }
            }
            Role::Assistant => {
                close_open(&mut out, &mut open);
                open = m.tool_calls.iter().map(|c| (c.id.clone(), c.function.name.clone())).collect();
                out.push(m);
            }
            Role::User => {
                close_open(&mut out, &mut open);
                out.push(m);
            }
            Role::System => {}
        }
    }
    close_open(&mut out, &mut open);
    out
}

/// Import every visible Hermes session into `store`. Returns (imported, skipped).
pub fn import_sessions_from(db: &Path, profile: &str, store: &crate::session::SessionStore) -> Result<(usize, usize)> {
    let (c, _tmp) = open_readonly(db)?;
    let scols = columns(&c, "sessions");
    let mcols = columns(&c, "messages");
    let has = |cols: &[String], n: &str| cols.iter().any(|x| x == n);
    let pick = |cols: &[String], n: &str| if has(cols, n) { n.to_string() } else { "NULL".to_string() };
    let sql = format!(
        "SELECT id, {}, {}, {}, {}, {} FROM sessions {} ORDER BY started_at",
        pick(&scols, "title"),
        pick(&scols, "started_at"),
        pick(&scols, "last_activity_at"),
        pick(&scols, "model"),
        pick(&scols, "source"),
        if has(&scols, "hidden") { "WHERE coalesce(hidden,0) = 0" } else { "" }
    );
    type Row = (String, Option<String>, Option<f64>, Option<f64>, Option<String>, Option<String>);
    let sessions: Vec<Row> = c
        .prepare(&sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2).ok(), r.get(3).ok(), r.get(4)?, r.get(5)?)))?
        .flatten()
        .collect();
    let reasoning_col =
        if has(&mcols, "reasoning_content") { "coalesce(reasoning_content, reasoning)".to_string() } else { pick(&mcols, "reasoning") };
    let msql = format!(
        "SELECT role, content, {}, {}, {}, {} FROM messages WHERE session_id = ?1 {} ORDER BY id",
        pick(&mcols, "tool_call_id"),
        pick(&mcols, "tool_calls"),
        pick(&mcols, "tool_name"),
        if has(&mcols, "reasoning") || has(&mcols, "reasoning_content") { reasoning_col } else { "NULL".into() },
        if has(&mcols, "active") { "AND active = 1" } else { "" }
    );
    let mut st = c.prepare(&msql)?;
    let (mut imported, mut skipped) = (0, 0);
    for (id, title, started, last, model, source) in sessions {
        let origin = format!("hermes:{profile}:{id}");
        if store.has_origin(&origin)? {
            continue;
        }
        type MRow = (String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
        let rows: Vec<MRow> =
            st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.flatten().collect();
        let msgs: Vec<Message> = rows
            .into_iter()
            .filter_map(|(role, content, call_id, calls, tool_name, reasoning)| {
                let text = content_text(content);
                let mut m = match role.as_str() {
                    "user" => Message::user(text),
                    "assistant" => {
                        let mut m = Message::assistant(text);
                        m.tool_calls = parse_tool_calls(calls);
                        if m.text().is_empty() && !m.tool_calls.is_empty() {
                            m.content = None;
                        }
                        m
                    }
                    "tool" => Message::tool(call_id.as_deref().unwrap_or(""), tool_name.as_deref().unwrap_or("tool"), text),
                    _ => return None,
                };
                m.reasoning = reasoning.filter(|r| !r.trim().is_empty());
                Some(m)
            })
            .collect();
        let msgs = repair_transcript(msgs);
        if !msgs.iter().any(|m| m.role == Role::User) {
            skipped += 1;
            continue;
        }
        let mut title = title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| crate::session::title_from(msgs.iter().find(|m| m.role == Role::User).map(|m| m.text()).unwrap_or("")));
        if let Some(src) = source.filter(|s| !matches!(s.as_str(), "cli" | "tui" | "desktop" | ""))
            && !title.starts_with('[')
        {
            title = format!("[{src}] {title}");
        }
        let ts = |x: Option<f64>| x.and_then(|v| chrono::DateTime::from_timestamp(v as i64, 0));
        store.import_session(&origin, &title, ts(started), ts(last.or(started)), "hermes", model.as_deref().unwrap_or(""), &msgs)?;
        imported += 1;
    }
    Ok((imported, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv() {
        let env = parse_env("# c\nexport A=1\nB=\"two words\"\nC='x'\nD=val # comment\nbroken\nE=\n");
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "two words");
        assert_eq!(env["C"], "x");
        assert_eq!(env["D"], "val");
        assert_eq!(env["E"], "");
        assert!(!env.contains_key("broken"));
    }

    #[test]
    fn custom_local_provider() {
        let y: Value = serde_yaml::from_str(
            "model:\n  provider: custom\n  default: qwen3:32b\n  base_url: http://192.168.1.9:11434/v1\n  context_length: 65536\n",
        )
        .unwrap();
        let (ps, active, _) = providers_from(&y, &HashMap::new());
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].base_url, "http://192.168.1.9:11434/v1");
        assert_eq!(ps[0].name, "Local endpoint (from Hermes)");
        assert_eq!(ps[0].context_window, 65536);
        assert_eq!(active, Some(("hermes-custom".into(), "qwen3:32b".into())));
    }

    #[test]
    fn nous_api_key_and_named_providers() {
        let y: Value = serde_yaml::from_str(
            "model:\n  provider: nous-api\n  model: Hermes-4-70B\nproviders:\n  gpu-box:\n    base_url: http://10.0.0.2:8000/v1\n    key_env: BOX_KEY\n",
        )
        .unwrap();
        let env = HashMap::from([("NOUS_API_KEY".to_string(), "sk-n".to_string()), ("BOX_KEY".to_string(), "bk".to_string())]);
        let (ps, active, _) = providers_from(&y, &env);
        assert_eq!(ps[0].api_key.as_deref(), Some("sk-n"));
        assert_eq!(ps[0].default_model, "Hermes-4-70B");
        assert_eq!(ps[1].id, "gpu-box");
        assert_eq!(ps[1].api_key.as_deref(), Some("bk"));
        assert_eq!(active.unwrap().0, "nous-api");
    }

    #[test]
    fn unsupported_provider_noted() {
        let y: Value = serde_yaml::from_str("model:\n  provider: anthropic\n").unwrap();
        let (ps, active, notes) = providers_from(&y, &HashMap::new());
        assert!(ps.is_empty() && active.is_none());
        assert!(notes[0].contains("anthropic"));
    }

    #[test]
    fn full_import() {
        let h = tempfile::tempdir().unwrap();
        let m = tempfile::tempdir().unwrap();
        std::fs::write(h.path().join("config.yaml"), "model:\n  provider: nous\n  default: Hermes-4-405B\n").unwrap();
        std::fs::create_dir_all(h.path().join("memories")).unwrap();
        std::fs::write(h.path().join("memories/MEMORY.md"), "likes rust").unwrap();
        std::fs::create_dir_all(h.path().join("skills/x")).unwrap();
        let paths = Paths::under(m.path().to_path_buf());
        let mut cfg = Config::default();
        let r = import(h.path(), &paths, &mut cfg).unwrap();
        assert_eq!(r.memories_copied, vec!["MEMORY.md (+1)"]);
        assert_eq!(std::fs::read_to_string(paths.default_memories_dir().join("MEMORY.md")).unwrap(), "likes rust");
        assert_eq!(cfg.active_provider.as_deref(), Some("nous"));
        assert_eq!(cfg.extra_skill_dirs, vec![h.path().join("skills")]);
        // Re-importing merges: Mercury's own entries are kept, nothing is duplicated.
        let r2 = import(h.path(), &paths, &mut cfg).unwrap();
        assert!(r2.memories_copied.is_empty());
        std::fs::write(paths.default_memories_dir().join("MEMORY.md"), "edited").unwrap();
        import(h.path(), &paths, &mut cfg).unwrap();
        assert_eq!(std::fs::read_to_string(paths.default_memories_dir().join("MEMORY.md")).unwrap(), "edited\n§\nlikes rust");
        assert_eq!(cfg.extra_skill_dirs.len(), 1);
    }
}
