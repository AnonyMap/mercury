//! Tool registry: JSON-schema definitions for the model and a dispatcher.

pub mod files;
pub mod terminal;
pub mod web;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::checkpoints::CheckpointStore;
use crate::config::ToolsConfig;
use crate::memory::{MemoryStore, Target};
use crate::session::SessionStore;
use crate::skills::SkillLibrary;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub content: String,
    #[serde(default = "pending")]
    pub status: String,
}

fn pending() -> String {
    "pending".into()
}

/// Everything a tool may touch. Cloned cheaply into each agent turn.
#[derive(Clone)]
pub struct ToolContext {
    pub workdir: Arc<Mutex<PathBuf>>,
    pub memory: MemoryStore,
    pub skills: SkillLibrary,
    pub sessions: Arc<SessionStore>,
    pub session_id: String,
    pub todos: Arc<Mutex<Vec<Todo>>>,
    pub config: ToolsConfig,
    pub log_dir: PathBuf,
    pub checkpoints: CheckpointStore,
    pub processes: Arc<Mutex<Vec<BgProcess>>>,
    /// Connected MCP servers; `None` until the first turn connects them.
    pub mcp: Arc<tokio::sync::RwLock<Option<crate::mcp::McpManager>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BgProcess {
    pub pid: u32,
    pub command: String,
    pub log: PathBuf,
    pub started: chrono::DateTime<chrono::Utc>,
}

fn alive(pid: u32) -> bool {
    // A zombie still has a /proc entry; treat it as finished.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| s.rsplit(')').next().map(|r| !r.trim_start().starts_with('Z')).unwrap_or(false))
        .unwrap_or(false)
}

fn tail(path: &std::path::Path, lines: usize) -> String {
    let t = std::fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = t.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

impl ToolContext {
    /// A context for `session_id`, keeping logs and checkpoints under `data_dir`.
    pub fn new(
        workdir: PathBuf,
        memory: MemoryStore,
        skills: SkillLibrary,
        sessions: Arc<SessionStore>,
        session_id: &str,
        config: ToolsConfig,
        data_dir: &std::path::Path,
    ) -> Self {
        Self {
            workdir: Arc::new(Mutex::new(workdir)),
            memory,
            skills,
            sessions,
            session_id: session_id.to_string(),
            todos: Arc::new(Mutex::new(vec![])),
            config,
            log_dir: data_dir.join("logs"),
            checkpoints: CheckpointStore::new(data_dir.join("checkpoints").join(session_id)),
            processes: Arc::new(Mutex::new(vec![])),
            mcp: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Point at another session (new chat / resume): todos and checkpoints are per session.
    pub fn switch_session(&mut self, session_id: &str, data_dir: &std::path::Path) {
        self.session_id = session_id.to_string();
        self.todos.lock().unwrap_or_else(|p| p.into_inner()).clear();
        self.checkpoints = CheckpointStore::new(data_dir.join("checkpoints").join(session_id));
    }

    pub fn cwd(&self) -> PathBuf {
        self.workdir.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn set_cwd(&self, p: PathBuf) {
        *self.workdir.lock().unwrap_or_else(|p| p.into_inner()) = p;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self { content: format!("Error: {}", s.into()), is_error: true }
    }
}

fn def(name: &str, description: &str, params: Value) -> Value {
    json!({ "type": "function", "function": { "name": name, "description": description, "parameters": params } })
}

/// Tool definitions sent to the model, filtered by config.
pub fn definitions(cfg: &ToolsConfig) -> Vec<Value> {
    let mut v = Vec::new();
    if cfg.terminal {
        v.push(def(
            "terminal",
            "Run a bash command on the user's machine in the session working directory (cd persists between calls). Non-interactive: stdin is closed. Use background=true for servers or watchers.",
            json!({"type":"object","properties":{
                "command":{"type":"string","description":"The shell command"},
                "timeout":{"type":"integer","description":"Seconds before the command is killed"},
                "background":{"type":"boolean","description":"Start detached and return a log file path"}
            },"required":["command"]}),
        ));
    }
    if cfg.files {
        v.push(def("read_file", "Read a text file with line numbers. Paths are relative to the working directory.",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","description":"1-based first line"},"limit":{"type":"integer","description":"Number of lines (default 500)"}},"required":["path"]})));
        v.push(def(
            "write_file",
            "Create or overwrite a file with the given content. Creates parent directories.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        ));
        v.push(def("patch", "Edit a file by replacing old_string with new_string. old_string must match exactly once unless replace_all is true.",
            json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_string","new_string"]})));
        v.push(def("search_files", "Search file contents with a regex (target=content), or find files by glob (target=files). Respects .gitignore.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"Directory to search (default .)"},"target":{"type":"string","enum":["content","files"]},"file_glob":{"type":"string","description":"Limit content search to files matching this glob"},"limit":{"type":"integer"}},"required":["pattern"]})));
    }
    if cfg.web {
        v.push(def(
            "web_extract",
            "Fetch a web page and return its readable text.",
            json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        ));
        if cfg.searxng_url.is_some() || cfg.brave_api_key.is_some() {
            v.push(def(
                "web_search",
                "Search the web. Returns titles, URLs and snippets.",
                json!({"type":"object","properties":{"query":{"type":"string"},"max_results":{"type":"integer"}},"required":["query"]}),
            ));
        }
    }
    if cfg.memory {
        v.push(def("memory", "Save durable facts across sessions. target=memory for your own notes (environment, conventions, lessons), target=user for facts about the user (preferences, background). Keep entries short; replace/remove pick an entry by a unique snippet in old_text.",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["add","replace","remove"]},"target":{"type":"string","enum":["memory","user"]},"content":{"type":"string"},"old_text":{"type":"string"}},"required":["action","target"]})));
    }
    if cfg.todo {
        v.push(def("todo_list", "Track a multi-step task. Pass the full list to set it (or merge=true to update by id); pass nothing to read it. status: pending, in_progress, completed, cancelled.",
            json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string"}},"required":["id","content"]}},"merge":{"type":"boolean"}}})));
    }
    if cfg.skills {
        v.push(def(
            "skills_list",
            "List available skills (reusable procedures) with descriptions.",
            json!({"type":"object","properties":{}}),
        ));
        v.push(def(
            "skill_view",
            "Load a skill's instructions, or one of its supporting files.",
            json!({"type":"object","properties":{"name":{"type":"string"},"file":{"type":"string"}},"required":["name"]}),
        ));
        v.push(def("skill_manage", "Create, edit, patch or delete a skill. Save a skill after solving a non-trivial task you'd want to repeat.",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["create","edit","patch","delete"]},"name":{"type":"string"},"description":{"type":"string"},"content":{"type":"string","description":"create: body markdown; edit: full SKILL.md"},"old_string":{"type":"string"},"new_string":{"type":"string"}},"required":["action","name"]})));
    }
    if cfg.terminal {
        v.push(def(
            "process_manage",
            "Manage background processes started with terminal background=true: list them, poll recent output, wait for one to exit, or kill it.",
            json!({"type":"object","properties":{
                "action":{"type":"string","enum":["list","poll","wait","kill"]},
                "pid":{"type":"integer"},
                "lines":{"type":"integer","description":"Output lines to show (default 40)"},
                "timeout":{"type":"integer","description":"wait: seconds (default 60)"}
            },"required":["action"]}),
        ));
    }
    if cfg.clarify {
        v.push(def(
            "clarify",
            "Ask the user a question when you genuinely can't proceed without their input. Offer short choices when the options are known. Don't use it for things you can find out yourself.",
            json!({"type":"object","properties":{
                "question":{"type":"string"},
                "choices":{"type":"array","items":{"type":"string"},"description":"Up to 5 suggested answers"}
            },"required":["question"]}),
        ));
    }
    if cfg.delegate {
        v.push(def(
            "delegate_task",
            "Hand self-contained sub-tasks to subagents. Each starts with a fresh context (only what you pass in `context`), has the same tools, and returns a short report. Tasks run in parallel. Use for independent research or work that would flood your own context.",
            json!({"type":"object","properties":{
                "tasks":{"type":"array","maxItems":3,"items":{"type":"object","properties":{
                    "goal":{"type":"string","description":"What the subagent must achieve"},
                    "context":{"type":"string","description":"Everything it needs to know: paths, constraints, what you already tried"}
                },"required":["goal"]}}
            },"required":["tasks"]}),
        ));
    }
    if cfg.session_search {
        v.push(def(
            "session_search",
            "Full-text search over past conversations, or read one past session by id.",
            json!({"type":"object","properties":{"query":{"type":"string"},"session_id":{"type":"string"},"limit":{"type":"integer"}}}),
        ));
    }
    v
}

/// Parse tool arguments leniently: models sometimes send empty strings or wrap JSON in fences.
pub fn parse_args(raw: &str) -> Result<Value, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(json!({}));
    }
    let t = t.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(m)) => Ok(Value::Object(m)),
        Ok(Value::String(inner)) => parse_args(&inner),
        Ok(other) => Err(format!("arguments must be a JSON object, got {other}")),
        Err(e) => Err(format!("arguments are not valid JSON ({e}): {}", crate::provider::truncate(t, 200))),
    }
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}
fn n(a: &Value, k: &str) -> Option<usize> {
    a.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).map(|x| x as usize)
}
fn b(a: &Value, k: &str) -> bool {
    a.get(k).map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true"))).unwrap_or(false)
}
fn req<'a>(a: &'a Value, k: &str) -> Result<&'a str, ToolOutput> {
    s(a, k).ok_or_else(|| ToolOutput::err(format!("missing required argument `{k}`")))
}

/// Run a tool. Approval for dangerous commands is the agent's job and happens before this.
pub async fn execute(name: &str, args: &Value, ctx: &ToolContext, cancel: &CancellationToken) -> ToolOutput {
    match run(name, args, ctx, cancel).await {
        Ok(o) | Err(o) => o,
    }
}

async fn run(name: &str, a: &Value, ctx: &ToolContext, cancel: &CancellationToken) -> Result<ToolOutput, ToolOutput> {
    let wd = ctx.cwd();
    let anyhow_err = |e: anyhow::Error| ToolOutput::err(format!("{e:#}"));
    Ok(match name {
        "terminal" => {
            let cmd = req(a, "command")?;
            if b(a, "background") {
                let (pid, log) = terminal::spawn_background(cmd, &wd, &ctx.log_dir).map_err(|e| ToolOutput::err(e.to_string()))?;
                ctx.processes.lock().unwrap_or_else(|p| p.into_inner()).push(BgProcess {
                    pid,
                    command: cmd.to_string(),
                    log: log.clone(),
                    started: chrono::Utc::now(),
                });
                ToolOutput::ok(format!(
                    "Started in background (pid {pid}). Output goes to {}. Use process_manage to poll, wait for or kill it.",
                    log.display()
                ))
            } else {
                let secs = n(a, "timeout").map(|t| t as u64).unwrap_or(ctx.config.terminal_timeout_secs).clamp(1, 3600);
                let r = terminal::run(cmd, &wd, Duration::from_secs(secs), cancel).await.map_err(|e| ToolOutput::err(e.to_string()))?;
                if let Some(c) = r.new_cwd.clone().filter(|p| p.is_dir()) {
                    ctx.set_cwd(c);
                }
                let failed = r.timed_out || r.exit_code != Some(0);
                ToolOutput { content: r.render(), is_error: failed }
            }
        }
        "read_file" => ToolOutput::ok(files::read_file(&wd, req(a, "path")?, n(a, "offset"), n(a, "limit")).map_err(anyhow_err)?),
        "write_file" => {
            let path = req(a, "path")?;
            checkpoint(ctx, &wd, path, "write_file");
            ToolOutput::ok(files::write_file(&wd, path, s(a, "content").unwrap_or("")).map_err(anyhow_err)?)
        }
        "patch" => {
            let path = req(a, "path")?;
            let old = req(a, "old_string")?;
            checkpoint(ctx, &wd, path, "patch");
            ToolOutput::ok(files::patch(&wd, path, old, s(a, "new_string").unwrap_or(""), b(a, "replace_all")).map_err(anyhow_err)?)
        }
        "search_files" => ToolOutput::ok(
            files::search_files(
                &wd,
                req(a, "pattern")?,
                s(a, "path"),
                s(a, "target").unwrap_or("content"),
                s(a, "file_glob"),
                n(a, "limit"),
            )
            .map_err(anyhow_err)?,
        ),
        "process_manage" => {
            let procs = ctx.processes.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let find = |pid: Option<usize>| -> Result<BgProcess, ToolOutput> {
                let pid = pid.ok_or_else(|| ToolOutput::err("pid is required"))? as u32;
                procs
                    .iter()
                    .find(|p| p.pid == pid)
                    .cloned()
                    .ok_or_else(|| ToolOutput::err(format!("no background process {pid} from this session")))
            };
            let lines = n(a, "lines").unwrap_or(40).clamp(1, 500);
            match s(a, "action").unwrap_or("list") {
                "list" => {
                    if procs.is_empty() {
                        ToolOutput::ok("No background processes.")
                    } else {
                        ToolOutput::ok(
                            procs
                                .iter()
                                .map(|p| {
                                    format!(
                                        "{} [{}] {} (log {})",
                                        p.pid,
                                        if alive(p.pid) { "running" } else { "exited" },
                                        p.command,
                                        p.log.display()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                }
                "poll" => {
                    let p = find(n(a, "pid"))?;
                    ToolOutput::ok(format!("[{}]\n{}", if alive(p.pid) { "running" } else { "exited" }, tail(&p.log, lines)))
                }
                "wait" => {
                    let p = find(n(a, "pid"))?;
                    let secs = n(a, "timeout").unwrap_or(60).clamp(1, 600) as u64;
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
                    while alive(p.pid) && tokio::time::Instant::now() < deadline {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                            _ = cancel.cancelled() => return Err(ToolOutput::err("cancelled")),
                        }
                    }
                    ToolOutput::ok(format!("[{}]\n{}", if alive(p.pid) { "still running" } else { "exited" }, tail(&p.log, lines)))
                }
                "kill" => {
                    let p = find(n(a, "pid"))?;
                    let _ = std::process::Command::new("kill").args(["-TERM", "--", &format!("-{}", p.pid)]).status();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if alive(p.pid) {
                        let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{}", p.pid)]).status();
                    }
                    ToolOutput::ok(format!("Stopped {}.\n{}", p.pid, tail(&p.log, 10)))
                }
                other => ToolOutput::err(format!("unknown action {other}")),
            }
        }
        "web_extract" => ToolOutput::ok(web::extract(req(a, "url")?).await.map_err(anyhow_err)?),
        "web_search" => {
            let q = req(a, "query")?;
            let k = n(a, "max_results").unwrap_or(8).clamp(1, 20);
            let results = if let Some(base) = &ctx.config.searxng_url {
                web::search_searxng(base, q, k).await
            } else if let Some(key) = &ctx.config.brave_api_key {
                web::search_brave(key, q, k).await
            } else {
                Err(anyhow::anyhow!("no search backend configured (set a SearXNG URL or Brave key in Settings > Tools & safety)"))
            };
            ToolOutput::ok(web::format_results(&results.map_err(anyhow_err)?))
        }
        "memory" => {
            let t = Target::parse(req(a, "target")?).ok_or_else(|| ToolOutput::err("target must be `memory` or `user`"))?;
            let r = match req(a, "action")? {
                "add" => ctx.memory.add(t, req(a, "content")?),
                "replace" => ctx.memory.replace(t, req(a, "old_text")?, req(a, "content")?),
                "remove" => ctx.memory.remove(t, req(a, "old_text")?),
                other => return Err(ToolOutput::err(format!("unknown action {other}"))),
            };
            let (used, cap) = ctx.memory.usage(t);
            ToolOutput::ok(format!("{} [{used}/{cap} chars used]", r.map_err(anyhow_err)?))
        }
        "todo_list" => {
            let mut todos = ctx.todos.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(list) = a.get("todos").filter(|v| !v.is_null()) {
                let incoming: Vec<Todo> = serde_json::from_value(list.clone()).map_err(|e| ToolOutput::err(format!("bad todos: {e}")))?;
                if b(a, "merge") {
                    for t in incoming {
                        match todos.iter_mut().find(|x| x.id == t.id) {
                            Some(x) => *x = t,
                            None => todos.push(t),
                        }
                    }
                } else {
                    *todos = incoming;
                }
            }
            ToolOutput::ok(render_todos(&todos))
        }
        "skills_list" => {
            let idx = ctx.skills.index();
            ToolOutput::ok(if idx.is_empty() { "No skills installed yet.".into() } else { idx })
        }
        "skill_view" => ToolOutput::ok(ctx.skills.view(req(a, "name")?, s(a, "file")).map_err(anyhow_err)?),
        "skill_manage" => {
            let name = req(a, "name")?;
            match req(a, "action")? {
                "create" => {
                    let sk =
                        ctx.skills.create(name, s(a, "description").unwrap_or(""), s(a, "content").unwrap_or("")).map_err(anyhow_err)?;
                    ToolOutput::ok(format!("Created skill {} at {}", sk.name, sk.dir.display()))
                }
                "edit" => {
                    ctx.skills.edit(name, req(a, "content")?).map_err(anyhow_err)?;
                    ToolOutput::ok(format!("Updated {name}"))
                }
                "patch" => {
                    ctx.skills.patch(name, req(a, "old_string")?, s(a, "new_string").unwrap_or("")).map_err(anyhow_err)?;
                    ToolOutput::ok(format!("Patched {name}"))
                }
                "delete" => {
                    ctx.skills.delete(name).map_err(anyhow_err)?;
                    ToolOutput::ok(format!("Deleted {name}"))
                }
                other => ToolOutput::err(format!("unknown action {other}")),
            }
        }
        "session_search" => {
            if let Some(id) = s(a, "session_id") {
                let msgs = ctx.sessions.messages(id).map_err(anyhow_err)?;
                if msgs.is_empty() {
                    return Err(ToolOutput::err(format!("no session {id}")));
                }
                let text: String = msgs
                    .iter()
                    .filter(|m| matches!(m.role, crate::provider::Role::User | crate::provider::Role::Assistant) && !m.text().is_empty())
                    .map(|m| format!("[{:?}] {}\n", m.role, m.text()))
                    .collect();
                ToolOutput::ok(terminal::truncate_middle(&text, 20_000))
            } else {
                let q = req(a, "query")?;
                let hits = ctx.sessions.search(q, n(a, "limit").unwrap_or(10).clamp(1, 50)).map_err(anyhow_err)?;
                let hits: Vec<_> = hits.into_iter().filter(|h| h.session_id != ctx.session_id).collect();
                if hits.is_empty() {
                    ToolOutput::ok("No matches in past sessions.")
                } else {
                    ToolOutput::ok(
                        hits.iter()
                            .map(|h| format!("- session {} \"{}\" ({}): {}", h.session_id, h.title, h.role, h.snippet))
                            .collect::<Vec<_>>()
                            .join("\n")
                            + "\n(Read a whole session with session_id.)",
                    )
                }
            }
        }
        other => ToolOutput::err(format!("unknown tool `{other}`")),
    })
}

/// Save a file's state before an edit (best effort; never blocks the edit).
fn checkpoint(ctx: &ToolContext, wd: &std::path::Path, path: &str, tool: &str) {
    if ctx.config.checkpoints && !path.trim().is_empty() {
        let _ = ctx.checkpoints.snapshot(&files::resolve(wd, path), tool);
    }
}

pub fn render_todos(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".into();
    }
    todos
        .iter()
        .map(|t| {
            let mark = match t.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[>]",
                "cancelled" => "[-]",
                _ => "[ ]",
            };
            format!("{mark} {} {}", t.id, t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-line human summary of a tool call, for the UI.
pub fn summarize_call(name: &str, args: &Value) -> String {
    let get = |k: &str| s(args, k).unwrap_or("").to_string();
    let short = |x: String| crate::provider::truncate(&x.replace('\n', " "), 90);
    match name {
        "terminal" => format!("$ {}", short(get("command"))),
        "read_file" => format!("read {}", get("path")),
        "write_file" => format!("write {}", get("path")),
        "patch" => format!("edit {}", get("path")),
        "search_files" => format!("search {:?} in {}", get("pattern"), s(args, "path").unwrap_or(".")),
        "web_extract" => format!("fetch {}", get("url")),
        "web_search" => format!("search the web for {:?}", get("query")),
        "memory" => format!("memory {} ({})", get("action"), get("target")),
        "todo_list" => "update todo list".into(),
        "process_manage" => format!("{} background process {}", get("action"), args.get("pid").map(|p| p.to_string()).unwrap_or_default())
            .trim()
            .to_string(),
        "clarify" => format!("ask: {}", short(get("question"))),
        "delegate_task" => {
            let n = args.get("tasks").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            format!("delegate {n} task{} to subagents", if n == 1 { "" } else { "s" })
        }
        "skills_list" => "list skills".into(),
        "skill_view" => format!("open skill {}", get("name")),
        "skill_manage" => format!("{} skill {}", get("action"), get("name")),
        "session_search" => match s(args, "session_id") {
            Some(id) => format!("read session {id}"),
            None => format!("search sessions for {:?}", get("query")),
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(
            dir.to_path_buf(),
            MemoryStore::new(dir.join(".mem")),
            SkillLibrary::new(dir.join(".skills"), vec![]),
            Arc::new(SessionStore::in_memory().unwrap()),
            "current",
            ToolsConfig::default(),
            &dir.join(".data"),
        )
    }

    #[test]
    fn args_parsing() {
        assert_eq!(parse_args("").unwrap(), json!({}));
        assert_eq!(parse_args("```json\n{\"a\":1}\n```").unwrap(), json!({"a":1}));
        assert_eq!(parse_args("\"{\\\"a\\\":1}\"").unwrap(), json!({"a":1}));
        assert!(parse_args("[1]").is_err());
        assert!(parse_args("{bad").unwrap_err().contains("not valid JSON"));
    }

    #[test]
    fn definitions_follow_config() {
        let mut cfg = ToolsConfig::default();
        let names =
            |c: &ToolsConfig| definitions(c).iter().map(|d| d["function"]["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let all = names(&cfg);
        assert!(all.contains(&"terminal".to_string()));
        assert!(!all.contains(&"web_search".to_string()), "no backend → no web_search");
        cfg.terminal = false;
        cfg.searxng_url = Some("http://s".into());
        let some = names(&cfg);
        assert!(!some.contains(&"terminal".to_string()));
        assert!(some.contains(&"web_search".to_string()));
        for d in definitions(&cfg) {
            assert_eq!(d["type"], "function");
            assert_eq!(d["function"]["parameters"]["type"], "object");
        }
    }

    #[tokio::test]
    async fn dispatch_files_memory_todo() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let tok = CancellationToken::new();
        let o = execute("write_file", &json!({"path":"a.txt","content":"one\ntwo\n"}), &c, &tok).await;
        assert!(!o.is_error, "{}", o.content);
        let o = execute("patch", &json!({"path":"a.txt","old_string":"two","new_string":"2"}), &c, &tok).await;
        assert!(!o.is_error);
        let o = execute("read_file", &json!({"path":"a.txt"}), &c, &tok).await;
        assert_eq!(o.content, "1|one\n2|2\n");
        let o = execute("read_file", &json!({}), &c, &tok).await;
        assert!(o.is_error && o.content.contains("`path`"));
        let o = execute("memory", &json!({"action":"add","target":"user","content":"likes tea"}), &c, &tok).await;
        assert!(o.content.contains("Added") && o.content.contains("/1375"));
        let o = execute(
            "todo_list",
            &json!({"todos":[{"id":"1","content":"plan"},{"id":"2","content":"build","status":"in_progress"}]}),
            &c,
            &tok,
        )
        .await;
        assert_eq!(o.content, "[ ] 1 plan\n[>] 2 build");
        let o = execute("todo_list", &json!({"merge":true,"todos":[{"id":"1","content":"plan","status":"completed"}]}), &c, &tok).await;
        assert!(o.content.starts_with("[x] 1 plan"));
        let o = execute("nope", &json!({}), &c, &tok).await;
        assert!(o.is_error);
    }

    #[tokio::test]
    async fn terminal_updates_cwd_and_reports_failure() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("w")).unwrap();
        let c = ctx(d.path());
        let tok = CancellationToken::new();
        let o = execute("terminal", &json!({"command":"cd w && pwd"}), &c, &tok).await;
        assert!(!o.is_error);
        assert_eq!(c.cwd().canonicalize().unwrap(), d.path().join("w").canonicalize().unwrap());
        let o = execute("terminal", &json!({"command":"false"}), &c, &tok).await;
        assert!(o.is_error && o.content.contains("exit code 1"));
    }

    #[tokio::test]
    async fn skills_and_sessions_tools() {
        let d = tempfile::tempdir().unwrap();
        let c = ctx(d.path());
        let tok = CancellationToken::new();
        assert!(execute("skills_list", &json!({}), &c, &tok).await.content.contains("No skills"));
        let o = execute(
            "skill_manage",
            &json!({"action":"create","name":"backup-db","description":"Back up postgres","content":"pg_dump ..."}),
            &c,
            &tok,
        )
        .await;
        assert!(!o.is_error, "{}", o.content);
        assert!(execute("skills_list", &json!({}), &c, &tok).await.content.contains("backup-db: Back up postgres"));
        assert!(execute("skill_view", &json!({"name":"backup-db"}), &c, &tok).await.content.contains("pg_dump"));

        let old = c.sessions.create("p", "m").unwrap();
        c.sessions.append(&old.id, &crate::provider::Message::user("the wifi password is in the drawer")).unwrap();
        let o = execute("session_search", &json!({"query":"wifi"}), &c, &tok).await;
        assert!(o.content.contains(&old.id), "{}", o.content);
        let o = execute("session_search", &json!({"session_id": old.id}), &c, &tok).await;
        assert!(o.content.contains("drawer"));
    }

    #[test]
    fn summaries() {
        assert_eq!(summarize_call("terminal", &json!({"command":"ls\n-la"})), "$ ls -la");
        assert_eq!(summarize_call("patch", &json!({"path":"x.rs"})), "edit x.rs");
    }
}
