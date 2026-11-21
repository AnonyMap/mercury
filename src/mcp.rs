//! Model Context Protocol client: stdio and streamable-HTTP servers, configured like Hermes's
//! `mcp_servers:`. Each server's tools are offered to the model as `mcp_<server>_<tool>`.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};

use crate::config::McpServerConfig;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    /// The name the model sees.
    pub exposed: String,
    pub description: String,
    pub input_schema: Value,
}

/// `mcp_<server>_<tool>`, limited to the characters and length chat APIs accept.
pub fn exposed_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let mut n = format!("mcp_{}_{}", clean(server), clean(tool));
    n.truncate(64);
    n
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

enum Transport {
    Stdio {
        stdin: Arc<Mutex<tokio::process::ChildStdin>>,
        pending: Pending,
        _child: Mutex<tokio::process::Child>,
        stderr_tail: Arc<std::sync::Mutex<String>>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
        http: reqwest::Client,
    },
}

pub struct McpClient {
    pub name: String,
    transport: Transport,
    next_id: AtomicU64,
    timeout: Duration,
    pub server_info: String,
}

impl McpClient {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self> {
        let timeout = Duration::from_secs(if cfg.timeout == 0 { 120 } else { cfg.timeout });
        let transport = if let Some(cmd) = &cfg.command {
            let mut c = tokio::process::Command::new(cmd);
            c.args(&cfg.args).envs(&cfg.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
            for k in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
                c.env_remove(k);
            }
            let mut child = c.spawn().with_context(|| format!("starting MCP server `{name}` ({cmd})"))?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
            let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let stdin = Arc::new(Mutex::new(stdin));
            let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&l);
                    t.push('\n');
                    if t.len() > 4000 {
                        let cut = (t.len() - 2000..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(0);
                        t.drain(..cut);
                    }
                }
            });
            let (p2, w2) = (pending.clone(), stdin.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else { continue };
                    let is_response = msg.get("result").is_some() || msg.get("error").is_some();
                    if is_response {
                        if let Some(id) = msg.get("id").and_then(Value::as_u64)
                            && let Some(tx) = p2.lock().await.remove(&id)
                        {
                            let _ = tx.send(msg);
                        }
                    } else if msg.get("method").is_some() && msg.get("id").is_some() {
                        // A request from the server (ping, roots/list, sampling…): answer minimally.
                        let reply = match msg["method"].as_str() {
                            Some("ping") => json!({"jsonrpc":"2.0","id":msg["id"],"result":{}}),
                            Some("roots/list") => json!({"jsonrpc":"2.0","id":msg["id"],"result":{"roots":[]}}),
                            _ => json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32601,"message":"not supported by Mercury"}}),
                        };
                        let mut w = w2.lock().await;
                        let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = w.flush().await;
                    }
                }
                // Server exited: fail everything still waiting.
                p2.lock().await.clear();
            });
            Transport::Stdio { stdin, pending, _child: Mutex::new(child), stderr_tail }
        } else if let Some(url) = &cfg.url {
            Transport::Http {
                url: url.clone(),
                headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                session: Mutex::new(None),
                http: reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build()?,
            }
        } else {
            bail!("MCP server `{name}` needs a `command` or a `url`");
        };
        let mut me = Self { name: name.into(), transport, next_id: AtomicU64::new(1), timeout, server_info: String::new() };
        if let Err(e) = me.handshake().await {
            bail!("{e:#}{}", me.stderr_hint());
        }
        Ok(me)
    }

    fn stderr_hint(&self) -> String {
        match &self.transport {
            Transport::Stdio { stderr_tail, .. } => {
                let t = stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
                if t.trim().is_empty() { String::new() } else { format!("\nserver said: {}", crate::provider::truncate(t.trim(), 600)) }
            }
            _ => String::new(),
        }
    }

    async fn handshake(&mut self) -> Result<()> {
        let r = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mercury", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let info = r.get("serverInfo").cloned().unwrap_or(Value::Null);
        self.server_info = format!(
            "{} {}",
            info.get("name").and_then(Value::as_str).unwrap_or(&self.name),
            info.get("version").and_then(Value::as_str).unwrap_or("")
        )
        .trim()
        .to_string();
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut w = stdin.lock().await;
                w.write_all(format!("{msg}\n").as_bytes()).await?;
                w.flush().await?;
            }
            Transport::Http { .. } => {
                let _ = self.http_post(&msg).await?;
            }
        }
        Ok(())
    }

    async fn http_post(&self, msg: &Value) -> Result<Option<Value>> {
        let Transport::Http { url, headers, session, http } = &self.transport else { unreachable!() };
        let mut rb = http
            .post(url)
            .timeout(self.timeout)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(msg);
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        if let Some(s) = session.lock().await.clone() {
            rb = rb.header("Mcp-Session-Id", s);
        }
        let resp = rb.send().await?;
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().await = Some(sid.to_string());
        }
        let status = resp.status();
        let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = resp.text().await?;
        if status.as_u16() == 202 || body.trim().is_empty() {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("HTTP {status}: {}", crate::provider::truncate(&body, 300));
        }
        if ct.contains("event-stream") {
            let mut p = crate::provider::SseParser::default();
            let mut events = p.push(body.as_bytes());
            events.extend(p.finish());
            let want = msg.get("id").cloned();
            for e in events {
                if let Ok(v) = serde_json::from_str::<Value>(&e)
                    && (want.is_none() || v.get("id") == want.as_ref())
                    && (v.get("result").is_some() || v.get("error").is_some())
                {
                    return Ok(Some(v));
                }
            }
            bail!("no response in the event stream");
        }
        Ok(Some(serde_json::from_str(&body)?))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let reply = match &self.transport {
            Transport::Stdio { stdin, pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().await.insert(id, tx);
                {
                    let mut w = stdin.lock().await;
                    w.write_all(format!("{msg}\n").as_bytes()).await.context("MCP server closed its input")?;
                    w.flush().await?;
                }
                match tokio::time::timeout(self.timeout, rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => bail!("MCP server `{}` exited{}", self.name, self.stderr_hint()),
                    Err(_) => {
                        pending.lock().await.remove(&id);
                        bail!("MCP server `{}` didn't answer `{method}` within {:?}", self.name, self.timeout)
                    }
                }
            }
            Transport::Http { .. } => self.http_post(&msg).await?.ok_or_else(|| anyhow!("empty response to {method}"))?,
        };
        if let Some(e) = reply.get("error") {
            bail!("{}", e.get("message").and_then(Value::as_str).unwrap_or("MCP error"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params).await?;
            for t in r.get("tools").and_then(Value::as_array).cloned().unwrap_or_default() {
                let Some(name) = t.get("name").and_then(Value::as_str) else { continue };
                out.push(McpTool {
                    server: self.name.clone(),
                    name: name.into(),
                    exposed: exposed_name(&self.name, name),
                    description: t.get("description").and_then(Value::as_str).unwrap_or("").into(),
                    input_schema: t.get("inputSchema").cloned().unwrap_or(json!({"type":"object","properties":{}})),
                });
            }
            cursor = r.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                return Ok(out);
            }
        }
    }

    /// Call a tool; returns (text, is_error).
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<(String, bool)> {
        let r = self.request("tools/call", json!({"name": name, "arguments": args})).await?;
        let is_error = r.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut parts = Vec::new();
        for c in r.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(c.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                Some("image") => parts.push(format!("[image: {}]", c.get("mimeType").and_then(Value::as_str).unwrap_or("?"))),
                Some("audio") => parts.push("[audio]".into()),
                Some("resource") => parts.push(
                    c.pointer("/resource/text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("[resource {}]", c.pointer("/resource/uri").and_then(Value::as_str).unwrap_or(""))),
                ),
                Some("resource_link") => parts.push(format!("[link {}]", c.get("uri").and_then(Value::as_str).unwrap_or(""))),
                _ => parts.push(c.to_string()),
            }
        }
        if parts.is_empty()
            && let Some(sc) = r.get("structuredContent")
        {
            parts.push(sc.to_string());
        }
        Ok((parts.join("\n"), is_error))
    }
}

/// Connected servers and their tools.
#[derive(Default)]
pub struct McpManager {
    clients: HashMap<String, Arc<McpClient>>,
    pub tools: Vec<McpTool>,
    pub status: Vec<(String, Result<String, String>)>,
}

impl McpManager {
    /// Connect to every enabled server concurrently; failures are recorded, not fatal.
    pub async fn connect_all(servers: &std::collections::BTreeMap<String, McpServerConfig>) -> Self {
        let futs = servers.iter().filter(|(_, c)| c.enabled).map(|(name, cfg)| {
            let (name, cfg) = (name.clone(), cfg.clone());
            async move {
                let r = async {
                    let c = tokio::time::timeout(Duration::from_secs(60), McpClient::connect(&name, &cfg))
                        .await
                        .map_err(|_| anyhow!("timed out starting"))??;
                    let tools = c.list_tools().await?;
                    Ok::<_, anyhow::Error>((c, tools))
                }
                .await;
                (name, r)
            }
        });
        let mut m = Self::default();
        for (name, r) in futures_util::future::join_all(futs).await {
            match r {
                Ok((c, tools)) => {
                    m.status.push((name.clone(), Ok(format!("{} — {} tools", c.server_info, tools.len()))));
                    m.tools.extend(tools);
                    m.clients.insert(name, Arc::new(c));
                }
                Err(e) => m.status.push((name, Err(format!("{e:#}")))),
            }
        }
        m
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                let mut schema = t.input_schema.clone();
                if schema.get("type").is_none() {
                    schema["type"] = json!("object");
                }
                json!({"type":"function","function":{
                    "name": t.exposed,
                    "description": format!("[{} MCP] {}", t.server, crate::provider::truncate(&t.description, 900)),
                    "parameters": schema
                }})
            })
            .collect()
    }

    pub fn find(&self, exposed: &str) -> Option<&McpTool> {
        self.tools.iter().find(|t| t.exposed == exposed)
    }

    pub async fn call(&self, exposed: &str, args: Value) -> Result<(String, bool)> {
        let t = self.find(exposed).ok_or_else(|| anyhow!("unknown MCP tool {exposed}"))?;
        let c = self.clients.get(&t.server).ok_or_else(|| anyhow!("MCP server {} is not connected", t.server))?;
        c.call_tool(&t.name, args).await
    }

    pub fn status_text(&self) -> String {
        if self.status.is_empty() {
            return "No MCP servers configured.".into();
        }
        self.status
            .iter()
            .map(|(n, r)| match r {
                Ok(s) => format!("✔ {n}: {s}"),
                Err(e) => format!("✖ {n}: {e}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(exposed_name("github", "create_issue"), "mcp_github_create_issue");
        assert_eq!(exposed_name("my server", "do.thing"), "mcp_my_server_do_thing");
        assert!(exposed_name(&"x".repeat(80), "y").len() <= 64);
    }
}
