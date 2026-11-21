//! Terminal front end: subcommands and an interactive REPL for SSH / headless use.

use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent, CommandOutcome};
use crate::commands::{self, Command};
use crate::config::{ApprovalMode, Config, Paths, ProviderConfig, ProviderKind};

#[derive(Parser, Debug)]
#[command(name = "mercury", version, about = "Mercury — a desktop agent client for Nous Portal and local OpenAI-compatible models")]
pub struct Cli {
    /// Profile to use (default: the last one used, or $MERCURY_PROFILE).
    #[arg(short = 'p', long, global = true)]
    pub profile: Option<String>,
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// List profiles (the active one is marked).
    List,
    /// Switch to a profile (Mercury opens in it from now on).
    Use { name: String },
    /// Create a profile.
    Create { name: String },
    /// Delete a profile with its chats, memory and skills.
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Save a profile (chats, memory, skills, settings; no API keys) as a .tar.gz.
    Export {
        /// Profile to export (default: the active one).
        name: Option<String>,
        /// Output file (default: mercury-<profile>-<date>.tar.gz here).
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },
    /// Add a profile from an exported .tar.gz.
    Import {
        file: std::path::PathBuf,
        /// Name for the new profile (default: the name it was exported under).
        #[arg(long = "as")]
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Open the desktop app (default when a display is available).
    Gui,
    /// Chat in the terminal. With -q, answer one prompt and exit.
    Chat {
        #[arg(short, long)]
        query: Option<String>,
        /// Resume a session by id (prefix is fine).
        #[arg(long)]
        resume: Option<String>,
        /// provider:model or model
        #[arg(short, long)]
        model: Option<String>,
        /// Run dangerous commands without asking.
        #[arg(long)]
        yolo: bool,
        /// Attach a file to the first message: images for vision models, text/code inlined.
        #[arg(short, long = "attach")]
        attach: Vec<std::path::PathBuf>,
    },
    /// Sign in to Nous Portal (device code).
    Login,
    /// Forget the Nous Portal sign-in.
    Logout,
    /// List models from the active (or given) provider.
    Models { provider: Option<String> },
    /// Find OpenAI-compatible servers on this machine (and the LAN with --lan).
    Discover {
        #[arg(long)]
        lan: bool,
        /// Add everything found as providers.
        #[arg(long)]
        add: bool,
    },
    /// Add an OpenAI-compatible endpoint. Known services need no URL: `mercury add-provider deepinfra`.
    AddProvider {
        name: String,
        /// Base URL (ends in /v1 for most servers). Optional for presets (see `mercury presets`).
        url: Option<String>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        key_env: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Don't send tool definitions (for models without function calling).
        #[arg(long)]
        no_tools: bool,
    },
    /// Switch the active provider (and optionally model).
    Use { provider: String, model: Option<String> },
    /// List configured providers.
    Providers,
    /// Hosted services Mercury knows the address of (DeepInfra, OpenRouter, Groq…).
    Presets,
    /// Check that a provider works: lists models and sends a one-word test message.
    TestProvider {
        /// Provider id (default: the active one).
        id: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Change a provider's request options.
    ConfigureProvider {
        id: String,
        /// none | low | medium | high, or "default" to stop sending it.
        #[arg(long)]
        effort: Option<String>,
        /// Extra header, "Name: value" (repeatable; "Name:" removes one).
        #[arg(long = "header")]
        headers: Vec<String>,
        /// Extra JSON fields for every request, e.g. '{"service_tier":"flex"}' ("{}" clears).
        #[arg(long)]
        extra_json: Option<String>,
        /// Context window in tokens (0 = what the server reports).
        #[arg(long)]
        context_window: Option<u32>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        key_env: Option<String>,
        /// Turn tool calling on or off.
        #[arg(long)]
        tools: Option<bool>,
    },
    /// Give a provider a label (e.g. "Basement GPU", "Laptop Ollama").
    LabelProvider { id: String, label: String },
    /// Manage profiles: separate chats, memory, skills and model per profile.
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Pick up from a Hermes install: models, keys, MCP servers, memory, SOUL.md, skills, history.
    ImportHermes {
        /// Hermes home, its profiles folder, or one profile (default: $HERMES_HOME or ~/.hermes).
        path: Option<std::path::PathBuf>,
        /// Hermes profile to import (default: `default`, or the only one found).
        #[arg(long = "from")]
        from: Option<String>,
        /// Mercury profile to import into (default: same name as the Hermes profile).
        #[arg(long)]
        into: Option<String>,
        /// Use the profile's memory, skills and SOUL.md in place instead of copying them.
        #[arg(long)]
        in_place: bool,
        /// Skip conversation history.
        #[arg(long)]
        no_sessions: bool,
    },
    /// List the Hermes profiles Mercury can pick up from.
    HermesProfiles { path: Option<std::path::PathBuf> },
    /// Connect the configured MCP servers and list their tools.
    Mcp,
    /// List or search past sessions.
    Sessions {
        #[arg(long)]
        search: Option<String>,
    },
    /// Check configuration and connectivity.
    Doctor,
}

pub fn has_display() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var(v).map(|x| !x.is_empty()).unwrap_or(false))
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

pub fn run(cli: Cli, paths: Paths) -> Result<()> {
    paths.ensure()?;
    let mut cfg = Config::load_profile(&paths)?;
    match cli.command.unwrap_or(Cmd::Chat { query: None, resume: None, model: None, yolo: false, attach: vec![] }) {
        Cmd::Gui => Err(anyhow!("this build has no GUI")),
        Cmd::Chat { query, resume, model, yolo, attach } => rt()?.block_on(chat(paths, cfg, query, resume, model, yolo, attach)),
        Cmd::Login => rt()?.block_on(async {
            let cancel = CancellationToken::new();
            let c2 = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                c2.cancel();
            });
            let state = crate::nous::login(&paths, &mut cfg, crate::nous::PollTiming::default(), &cancel, |dc| {
                println!("To sign in to Nous Portal:\n  1. Open {}\n  2. If asked, enter code: {}\n", dc.open_url(), dc.user_code);
                if has_display() {
                    let _ = open::that_detached(dc.open_url());
                }
                println!("Waiting for approval… (Ctrl+C to cancel)");
            })
            .await?;
            cfg.save_profile(&paths)?;
            println!("Signed in. Inference endpoint: {}", state.inference_base_url);
            let p = cfg.providers.iter().find(|p| p.kind == ProviderKind::NousOauth).cloned().unwrap();
            if let Ok((base, key)) = crate::nous::credentials(&paths, &p).await
                && let Ok(models) = crate::provider::Client::with_options(&base, key, p.client_options(&cfg.agent)).list_models().await
            {
                println!("{} models available. Pick one with: mercury use {} <model>", models.len(), p.id);
            }
            Ok(())
        }),
        Cmd::Logout => {
            crate::nous::logout(&paths)?;
            println!("Signed out of Nous Portal.");
            Ok(())
        }
        Cmd::Models { provider } => rt()?.block_on(async {
            let p = match provider {
                Some(id) => cfg.provider(&id).cloned().ok_or_else(|| anyhow!("no provider {id}"))?,
                None => cfg.active().cloned().ok_or_else(|| anyhow!("no provider configured; try `mercury discover --add`"))?,
            };
            let (base, key) = crate::nous::credentials(&paths, &p).await?;
            for m in crate::provider::Client::with_options(&base, key, p.client_options(&cfg.agent)).list_model_info().await? {
                let mut extra = vec![];
                if let Some(n) = m.context_length {
                    extra.push(format!("{}k context", n / 1000));
                }
                if m.vision() {
                    extra.push("vision".into());
                }
                if m.reasoning() {
                    extra.push("reasoning".into());
                }
                if extra.is_empty() { println!("{}", m.id) } else { println!("{:<50} {}", m.id, extra.join(", ")) }
            }
            Ok(())
        }),
        Cmd::Discover { lan, add } => rt()?.block_on(async {
            println!("Looking on this machine{}…", if lan { " and the local network" } else { "" });
            let mut found = crate::discovery::discover_local().await;
            if lan {
                found.extend(crate::discovery::scan_lan().await);
            }
            if found.is_empty() {
                println!("Nothing found on the usual ports. Start Ollama / LM Studio / llama.cpp, or add one with `mercury add-provider`.");
                return Ok(());
            }
            for d in &found {
                println!("{:<32} {:<36} {} models", d.display_name(), d.base_url, d.models.len());
                if add && !cfg.providers.iter().any(|p| p.base_url == d.base_url) {
                    let p = d.to_provider(&cfg);
                    println!("  added as `{}`", p.id);
                    let first = cfg.active_provider.is_none();
                    let id = p.id.clone();
                    cfg.upsert_provider(p);
                    if first {
                        cfg.set_active(&id, d.models.first().cloned());
                    }
                }
            }
            if add {
                cfg.save_profile(&paths)?;
            }
            Ok(())
        }),
        Cmd::AddProvider { name, url, key, key_env, model, no_tools } => {
            let preset =
                crate::config::presets().into_iter().find(|p| p.id.eq_ignore_ascii_case(&name) || p.name.eq_ignore_ascii_case(&name));
            let (name, url, preset_env) = match (url, &preset) {
                (Some(u), _) => (name, u, None),
                (None, Some(p)) => (p.name.clone(), p.base_url.clone(), p.api_key_env.clone()),
                (None, None) => return Err(anyhow!("give the server's URL, or one of the presets (see `mercury presets`)")),
            };
            url::Url::parse(&url).map_err(|e| anyhow!("bad URL: {e}"))?;
            let id = match &preset {
                Some(p) if cfg.provider(&p.id).is_none() => p.id.clone(),
                _ => crate::config::unique_provider_id(&cfg, &name),
            };
            let mut p = ProviderConfig::openai(&id, &name, &url);
            if let Some(pre) = &preset {
                p.headers = pre.headers.clone();
            }
            p.api_key = key;
            p.api_key_env = key_env.or(if p.api_key.is_none() { preset_env } else { None });
            p.default_model = model.clone().unwrap_or_default();
            p.tools = !no_tools;
            let first = cfg.providers.is_empty();
            cfg.upsert_provider(p);
            if first {
                cfg.set_active(&id, model);
            }
            cfg.save_profile(&paths)?;
            println!("Added `{id}`.");
            Ok(())
        }
        Cmd::Use { provider, model } => {
            let model = model.or_else(|| cfg.provider(&provider).map(|p| p.default_model.clone()).filter(|m| !m.is_empty()));
            if !cfg.set_active(&provider, model) {
                return Err(anyhow!("no provider `{provider}` (see `mercury providers`)"));
            }
            cfg.save_profile(&paths)?;
            println!("Using {} / {}", provider, cfg.active_model_name().unwrap_or_else(|| "(pick a model)".into()));
            Ok(())
        }
        Cmd::Presets => {
            for p in crate::config::presets() {
                let env = p.api_key_env.map(|e| format!("key from ${e}")).unwrap_or_else(|| "no key".into());
                println!("{:<12} {:<28} {:<44} {env}", p.id, p.name, p.base_url);
            }
            println!(
                "\nAdd one with: mercury add-provider <id> [--key KEY]. Any other OpenAI-compatible server: mercury add-provider NAME URL"
            );
            Ok(())
        }
        Cmd::TestProvider { id, model } => rt()?.block_on(async {
            let p = match id {
                Some(id) => cfg.provider(&id).cloned().ok_or_else(|| anyhow!("no provider `{id}`"))?,
                None => cfg.active().cloned().ok_or_else(|| anyhow!("no provider configured"))?,
            };
            println!("Testing {} ({})…", p.name, p.base_url);
            let r = crate::probe::test_provider(&paths, &cfg, &p, model.as_deref()).await;
            println!("{}", r.summary());
            if r.ok() { Ok(()) } else { Err(anyhow!("{} isn't working", p.name)) }
        }),
        Cmd::ConfigureProvider { id, effort, headers, extra_json, context_window, model, key, key_env, tools } => {
            let p = cfg.providers.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow!("no provider `{id}` (see `mercury providers`)"))?;
            if let Some(e) = effort {
                let e = e.trim().to_ascii_lowercase();
                if e == "default" || e.is_empty() {
                    p.reasoning_effort = None;
                } else if crate::config::REASONING_EFFORTS.contains(&e.as_str()) {
                    p.reasoning_effort = Some(e);
                } else {
                    return Err(anyhow!("effort is one of none, low, medium, high, default"));
                }
            }
            for h in headers {
                match h.split_once(':') {
                    Some((k, v)) if v.trim().is_empty() => {
                        p.headers.remove(k.trim());
                    }
                    _ => p.headers.extend(ProviderConfig::parse_headers(&h).map_err(|e| anyhow!(e))?),
                }
            }
            if let Some(j) = extra_json {
                p.extra_body = ProviderConfig::parse_extra_body(&j).map_err(|e| anyhow!("--extra-json {e}"))?;
            }
            if let Some(n) = context_window {
                p.context_window = n;
            }
            if let Some(m) = model {
                p.default_model = m;
            }
            if let Some(k) = key {
                p.api_key = Some(k).filter(|k| !k.is_empty());
            }
            if let Some(k) = key_env {
                p.api_key_env = Some(k).filter(|k| !k.is_empty());
            }
            if let Some(t) = tools {
                p.tools = t;
            }
            let summary = format!(
                "`{id}`: effort {}, {} extra header(s), extra fields {}, tools {}",
                p.reasoning_effort.as_deref().unwrap_or("default"),
                p.headers.len(),
                serde_json::to_string(&p.extra_body).unwrap_or_default(),
                if p.tools { "on" } else { "off" }
            );
            cfg.save_profile(&paths)?;
            println!("{summary}");
            Ok(())
        }
        Cmd::LabelProvider { id, label } => {
            let p = cfg.providers.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow!("no provider `{id}` (see `mercury providers`)"))?;
            p.name = label.trim().to_string();
            cfg.save_profile(&paths)?;
            println!("`{id}` is now labelled “{}”.", label.trim());
            Ok(())
        }
        Cmd::Profile(cmd) => match cmd {
            ProfileCmd::List => {
                for name in crate::profiles::list(&paths) {
                    let pp = paths.with_profile(&name);
                    let n = crate::session::SessionStore::open(&pp.sessions_db())
                        .and_then(|s| s.list(100_000))
                        .map(|l| l.iter().filter(|s| s.message_count > 0).count())
                        .unwrap_or(0);
                    let model = Config::load_profile(&pp).ok().and_then(|c| c.active_model_name()).unwrap_or_else(|| "no model".into());
                    println!("{} {:<16} {:>4} chats  {model}", if name == paths.profile { "*" } else { " " }, name, n);
                }
                Ok(())
            }
            ProfileCmd::Use { name } => {
                if !crate::profiles::exists(&paths, &name) {
                    return Err(anyhow!("no profile `{name}` (create it with `mercury profile create {name}`)"));
                }
                crate::profiles::set_active(&paths, &name)?;
                println!("Now using profile “{name}”.");
                Ok(())
            }
            ProfileCmd::Create { name } => {
                crate::profiles::create(&paths, &name)?;
                println!("Created profile “{name}”. Switch with `mercury profile use {name}`.");
                Ok(())
            }
            ProfileCmd::Export { name, out } => {
                let name = name.unwrap_or_else(|| paths.profile.clone());
                if !crate::profiles::exists(&paths, &name) {
                    return Err(anyhow!("no profile `{name}`"));
                }
                let out = out.unwrap_or_else(|| crate::portable::default_file_name(&name).into());
                let n = crate::portable::export(&paths.with_profile(&name), &out)?;
                println!("Exported “{name}” ({n} files, no API keys) to {}", out.display());
                Ok(())
            }
            ProfileCmd::Import { file, name } => {
                let r = crate::portable::import(&paths, &file, name.as_deref())?;
                println!(
                    "Imported as profile “{}” with {} chats. Open it with `mercury profile use {}`.",
                    r.profile, r.sessions, r.profile
                );
                if !r.providers_added.is_empty() {
                    println!("Added providers (enter their API keys again): {}", r.providers_added.join(", "));
                }
                Ok(())
            }
            ProfileCmd::Delete { name, yes } => {
                if !yes {
                    return Err(anyhow!("this deletes {name}'s chats, memory and skills; add --yes to confirm"));
                }
                crate::profiles::delete(&paths, &name)?;
                println!("Deleted profile “{name}”.");
                Ok(())
            }
        },
        Cmd::Providers => {
            let active = cfg.active().map(|p| p.id.clone()).unwrap_or_default();
            if cfg.providers.is_empty() {
                println!("No providers yet. Try `mercury discover --add` or `mercury login`.");
            }
            for p in &cfg.providers {
                println!("{} {:<18} {:<34} {}", if p.id == active { "*" } else { " " }, p.id, p.name, p.base_url);
            }
            Ok(())
        }
        Cmd::HermesProfiles { path } => {
            let root = path.or_else(crate::hermes_import::hermes_home).ok_or_else(|| anyhow!("no home directory"))?;
            let profiles = crate::hermes_import::list_profiles(&root);
            if profiles.is_empty() {
                println!("No Hermes profiles under {}.", root.display());
            }
            for p in profiles {
                let s = crate::hermes_import::summarize(&p);
                println!(
                    "{:<14} {}\n               {} · {} conversations · memory {}+{} · {} skills · {} MCP{}{}",
                    p.name,
                    p.home.display(),
                    s.model.clone().unwrap_or_else(|| "no model".into()),
                    s.sessions,
                    s.memory_entries,
                    s.user_entries,
                    s.skills,
                    s.mcp_servers,
                    if s.has_soul { " · SOUL.md" } else { "" },
                    if s.nous_oauth { " · Nous sign-in" } else { "" }
                );
            }
            Ok(())
        }
        Cmd::ImportHermes { path, from, into, in_place, no_sessions } => {
            let root = path
                .or_else(crate::hermes_import::detect)
                .ok_or_else(|| anyhow!("no Hermes install found (looked in $HERMES_HOME and ~/.hermes); pass its path"))?;
            let profiles = crate::hermes_import::list_profiles(&root);
            let chosen = match &from {
                Some(name) => profiles.iter().find(|p| &p.name == name).cloned(),
                None if profiles.len() == 1 => profiles.first().cloned(),
                None => profiles.iter().find(|p| p.name == "default").cloned(),
            }
            .ok_or_else(|| {
                anyhow!(
                    "pick a Hermes profile with --from: {}",
                    if profiles.is_empty() {
                        "none found".into()
                    } else {
                        profiles.iter().map(|p| p.name.clone()).collect::<Vec<_>>().join(", ")
                    }
                )
            })?;
            let mode = if in_place { crate::config::LinkMode::InPlace } else { crate::config::LinkMode::Copy };
            let target = into.unwrap_or_else(|| crate::profiles::sanitize(&chosen.name));
            let tpaths = crate::profiles::ensure(&paths, &target)?;
            let mut tcfg = Config::load_profile(&tpaths)?;
            let r = crate::hermes_import::adopt(&chosen, mode, !no_sessions, &tpaths, &mut tcfg)?;
            tcfg.save_profile(&tpaths)?;
            crate::profiles::set_active(&tpaths, &target)?;
            println!(
                "Picked up Hermes profile “{}” ({}) into Mercury profile “{target}” (now active):\n{}",
                chosen.name,
                chosen.home.display(),
                r.summary()
            );
            Ok(())
        }
        Cmd::Mcp => rt()?.block_on(async {
            let m = crate::mcp::McpManager::connect_all(&cfg.mcp_servers).await;
            println!("{}", m.status_text());
            for t in &m.tools {
                println!("  {:<40} {}", t.exposed, crate::provider::truncate(&t.description.replace('\n', " "), 80));
            }
            Ok(())
        }),
        Cmd::Sessions { search } => {
            let store = crate::session::SessionStore::open(&paths.sessions_db())?;
            match search {
                Some(q) => {
                    for h in store.search(&q, 30)? {
                        println!("{}  {}  {}", &h.session_id[..8], h.title, h.snippet);
                    }
                }
                None => {
                    for s in store.list(50)?.into_iter().filter(|s| s.message_count > 0) {
                        println!(
                            "{}  {}  {:>4} msgs  {}",
                            s.updated_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                            &s.id[..8],
                            s.message_count,
                            s.title
                        );
                    }
                }
            }
            Ok(())
        }
        Cmd::Doctor => rt()?.block_on(async {
            print!("{}", doctor(&paths, &cfg).await);
            Ok(())
        }),
    }
}

/// A human-readable health report.
pub async fn doctor(paths: &Paths, cfg: &Config) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(format!("Mercury {}", env!("CARGO_PKG_VERSION")));
    line(format!("config   {}", paths.config_file().display()));
    line(format!("data     {}", paths.data_dir.display()));
    line(format!("profile  {} ({})", paths.profile, paths.profile_dir().display()));
    line(format!("display  {}", if has_display() { "available (GUI)" } else { "none (terminal mode)" }));
    line(format!("bash     {}", if which("bash") { "found" } else { "MISSING — the terminal tool needs bash" }));
    match crate::nous::status(paths) {
        Some(s) => {
            line(format!("nous     signed in; token valid until {}", s.expires_at.map(|e| e.to_rfc3339()).unwrap_or_else(|| "?".into())))
        }
        None => line("nous     not signed in".into()),
    }
    if let Some(h) = crate::hermes_import::detect() {
        line(format!("hermes   found at {} (`mercury import-hermes` to bring settings over)", h.display()));
    }
    if cfg.providers.is_empty() {
        line("providers: none — run `mercury discover --add`, `mercury login`, or `mercury add-provider`".into());
    }
    let active = cfg.active().map(|p| p.id.clone()).unwrap_or_default();
    for p in &cfg.providers {
        let status = match crate::nous::credentials(paths, p).await {
            Err(e) => format!("credentials: {e}"),
            Ok((base, key)) => {
                match tokio::time::timeout(
                    Duration::from_secs(8),
                    crate::provider::Client::with_options(&base, key, p.client_options(&cfg.agent)).list_models(),
                )
                .await
                {
                    Ok(Ok(m)) => format!("ok, {} models", m.len()),
                    Ok(Err(e)) => format!("error: {}", crate::provider::truncate(&e.to_string(), 160)),
                    Err(_) => "timed out".into(),
                }
            }
        };
        line(format!("{} {:<16} {:<40} {status}", if p.id == active { "*" } else { " " }, p.id, p.base_url));
    }
    line(format!("model    {}", cfg.active_model_name().unwrap_or_else(|| "(none selected)".into())));
    line(format!("approvals {:?}", cfg.approvals));
    out
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file())).unwrap_or(false)
}

fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(s.trim_end_matches(['\n', '\r']).to_string()),
    }
}

const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const YEL: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

fn style(on: bool, code: &'static str) -> &'static str {
    if on { code } else { "" }
}

/// Run a turn, rendering events to the terminal. Returns the agent.
async fn render_turn(agent: Agent, text: String, interactive: bool, color: bool) -> Agent {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let sig = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c2.cancel();
        }
    });
    let mut agent = agent;
    let handle = tokio::spawn(async move {
        agent.run_turn(&text, &tx, &cancel).await;
        agent
    });
    let mut in_reasoning = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => {
                if in_reasoning {
                    println!("{}", style(color, RESET));
                    in_reasoning = false;
                }
                print!("{t}");
            }
            AgentEvent::ReasoningDelta(t) => {
                if !in_reasoning {
                    print!("{}", style(color, DIM));
                    in_reasoning = true;
                }
                print!("{t}");
            }
            AgentEvent::ToolRunning { summary, .. } => println!("\n{}  ⏵ {summary}{}", style(color, CYAN), style(color, RESET)),
            AgentEvent::ToolFinished { output, is_error, .. } => {
                let first: String = output.lines().take(3).collect::<Vec<_>>().join(" ⏎ ");
                println!(
                    "{}    {}{}",
                    style(color, if is_error { RED } else { DIM }),
                    crate::provider::truncate(&first, 160),
                    style(color, RESET)
                );
            }
            AgentEvent::Clarify { question, choices, respond } => {
                let answer = if interactive {
                    println!("\n{}? {question}{}", style(color, YEL), style(color, RESET));
                    for (i, c) in choices.iter().enumerate() {
                        println!("  {}. {c}", i + 1);
                    }
                    let a = tokio::task::spawn_blocking(|| prompt_line("answer (number, text, or Enter to skip): "))
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    match a.trim().parse::<usize>() {
                        Ok(n) if n >= 1 && n <= choices.len() => choices[n - 1].clone(),
                        _ => a.trim().to_string(),
                    }
                } else {
                    String::new()
                };
                let _ = respond.send(answer);
            }
            AgentEvent::ApprovalNeeded { command, reasons, respond } => {
                let ok = if interactive {
                    println!("\n{}⚠ Approve command ({}):{}\n    {command}", style(color, YEL), reasons.join(", "), style(color, RESET));
                    let answer = tokio::task::spawn_blocking(|| prompt_line("Run it? [y/N] ")).await.ok().flatten().unwrap_or_default();
                    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                } else {
                    eprintln!("(denied dangerous command in non-interactive mode: {command}; pass --yolo to allow)");
                    false
                };
                let _ = respond.send(ok);
            }
            AgentEvent::Notice(n) => println!("\n{}{n}{}", style(color, DIM), style(color, RESET)),
            AgentEvent::Error(e) => eprintln!("\n{}{e}{}", style(color, RED), style(color, RESET)),
            AgentEvent::TurnFinished => break,
            _ => {}
        }
        let _ = std::io::stdout().flush();
    }
    if in_reasoning {
        print!("{}", style(color, RESET));
    }
    println!();
    sig.abort();
    handle.await.expect("agent task")
}

async fn chat(
    paths: Paths,
    mut cfg: Config,
    query: Option<String>,
    resume: Option<String>,
    model: Option<String>,
    yolo: bool,
    attach: Vec<std::path::PathBuf>,
) -> Result<()> {
    let mut atts = attach.iter().map(|f| crate::attach::load(f).map_err(|e| anyhow!(e))).collect::<Result<Vec<_>>>()?;
    if yolo {
        cfg.approvals = ApprovalMode::Yolo;
    }
    if cfg.providers.is_empty() {
        println!("No provider configured yet. Looking for local models…");
        let found = crate::discovery::discover_local().await;
        match found.first() {
            Some(d) => {
                let p = d.to_provider(&cfg);
                println!("Using {} at {} ({}).", d.display_name(), d.base_url, d.models.first().map(String::as_str).unwrap_or("no models"));
                let id = p.id.clone();
                cfg.upsert_provider(p);
                cfg.set_active(&id, d.models.first().cloned());
                cfg.save_profile(&paths)?;
            }
            None => {
                return Err(anyhow!(
                    "No provider configured and nothing found locally. Run `mercury login` for Nous Portal or `mercury add-provider NAME URL`."
                ));
            }
        }
    }
    let store = std::sync::Arc::new(crate::session::SessionStore::open(&paths.sessions_db())?);
    let resume_id = resume.and_then(|r| store.list(500).ok()?.into_iter().find(|s| s.id.starts_with(&r)).map(|s| s.id));
    let mut agent = Agent::with_store(paths, cfg, store.clone(), resume_id.as_deref())?;
    if let Some(spec) = model {
        let ids: Vec<String> = agent.config.providers.iter().map(|p| p.id.clone()).collect();
        let (p, m) = commands::split_model_spec(&spec, &ids);
        if let Some(p) = &p {
            agent.config.set_active(p, Some(m.clone()));
        } else {
            agent.config.active_model = Some(m.clone());
        }
    }
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if let Some(q) = query {
        let (text, images) = crate::attach::compose(&q, &atts);
        agent.pending_images = images;
        let _ = render_turn(agent, text, false, color).await;
        return Ok(());
    }
    if !atts.is_empty() {
        println!("Attached to your first message: {}", atts.iter().map(|a| a.label()).collect::<Vec<_>>().join(", "));
    }
    let _ = store.prune_empty(&agent.session.id);
    println!(
        "{}Mercury{} — {} via {}. /help for commands, Ctrl+C stops a reply, Ctrl+D quits.",
        style(color, CYAN),
        style(color, RESET),
        agent.config.active_model_name().unwrap_or_else(|| "no model".into()),
        agent.config.active().map(|p| p.name.clone()).unwrap_or_default()
    );
    if !agent.messages.is_empty() {
        println!("(resumed “{}”, {} messages)", agent.session.title, agent.messages.len());
    }
    loop {
        let line = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            let mut first = true;
            loop {
                let l = prompt_line(if first { "\n› " } else { "… " })?;
                first = false;
                if let Some(stripped) = l.strip_suffix('\\') {
                    buf.push_str(stripped);
                    buf.push('\n');
                } else {
                    buf.push_str(&l);
                    return Some(buf);
                }
            }
        })
        .await?;
        let Some(line) = line else {
            println!();
            return Ok(());
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if matches!(text, "/exit" | "/quit" | "exit" | "quit") {
            return Ok(());
        }
        let skills: Vec<String> = agent.tools.skills.list().into_iter().map(|s| s.name).collect();
        match commands::parse(text, &skills) {
            None => {
                let mut msg = if text.starts_with("//") { text[1..].to_string() } else { text.to_string() };
                if !atts.is_empty() {
                    let (t, images) = crate::attach::compose(&msg, &std::mem::take(&mut atts));
                    agent.pending_images = images;
                    msg = t;
                }
                agent = render_turn(agent, msg, true, color).await;
            }
            Some(Command::Help) => println!("{}\n/exit                   quit", commands::help_text()),
            Some(cmd) => {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                match agent.handle_command(cmd, &tx, &CancellationToken::new()).await {
                    Ok(CommandOutcome::Text(t)) | Ok(CommandOutcome::Reload(t)) => println!("{t}"),
                    Ok(CommandOutcome::Send(t)) => agent = render_turn(agent, t, true, color).await,
                    Ok(CommandOutcome::Stop) => {}
                    Err(e) => eprintln!("{e:#}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subcommands() {
        let c = Cli::try_parse_from(["mercury", "chat", "-q", "hi", "--yolo"]).unwrap();
        assert!(matches!(c.command, Some(Cmd::Chat { query: Some(_), yolo: true, .. })));
        let c = Cli::try_parse_from(["mercury", "add-provider", "Box", "http://10.0.0.2:8000/v1", "--no-tools"]).unwrap();
        assert!(matches!(c.command, Some(Cmd::AddProvider { no_tools: true, .. })));
        assert!(Cli::try_parse_from(["mercury"]).unwrap().command.is_none());
        let c = Cli::try_parse_from(["mercury", "-p", "work", "chat"]).unwrap();
        assert_eq!(c.profile.as_deref(), Some("work"));
        let c = Cli::try_parse_from(["mercury", "import-hermes", "--from", "coder", "--into", "dev", "--profile", "x"]).unwrap();
        assert!(matches!(c.command, Some(Cmd::ImportHermes { from: Some(_), into: Some(_), .. })));
        assert!(matches!(
            Cli::try_parse_from(["mercury", "profile", "use", "work"]).unwrap().command,
            Some(Cmd::Profile(ProfileCmd::Use { .. }))
        ));
    }

    #[tokio::test]
    async fn doctor_reports_unreachable_provider() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(d.path().to_path_buf());
        let mut cfg = Config::default();
        cfg.upsert_provider(ProviderConfig::openai("dead", "Dead", "http://127.0.0.1:9/v1"));
        let r = doctor(&paths, &cfg).await;
        assert!(r.contains("dead") && r.contains("error"), "{r}");
        assert!(r.contains("nous     not signed in"));
    }
}
