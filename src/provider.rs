//! OpenAI-compatible chat-completions client with streaming and tool calls.
//!
//! Written against the behaviour of the servers people actually run locally (Ollama, LM Studio,
//! llama.cpp, vLLM, KoboldCpp) as well as hosted ones: reasoning can arrive as
//! `reasoning_content`, `reasoning`, or inline `<think>` tags; some servers reject
//! `stream_options`; some ignore `stream: true` and answer with plain JSON.

use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".into()
}

/// A chat message. `reasoning` is kept for display and never sent back to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl Message {
    fn new(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: Some(content.into()), tool_calls: vec![], tool_call_id: None, name: None, reasoning: None }
    }
    pub fn system(c: impl Into<String>) -> Self {
        Self::new(Role::System, c)
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self::new(Role::User, c)
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self::new(Role::Assistant, c)
    }
    pub fn tool(call_id: &str, name: &str, c: impl Into<String>) -> Self {
        let mut m = Self::new(Role::Tool, c);
        m.tool_call_id = Some(call_id.into());
        m.name = Some(name.into());
        m
    }
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }

    /// The wire form: only fields the chat-completions API defines.
    pub fn to_api(&self) -> Value {
        let mut v = json!({ "role": self.role });
        match (&self.content, self.tool_calls.is_empty()) {
            (Some(c), _) if !c.is_empty() => v["content"] = json!(c),
            (_, false) => v["content"] = Value::Null,
            _ => v["content"] = json!(""),
        }
        if !self.tool_calls.is_empty() {
            v["tool_calls"] = json!(self.tool_calls);
        }
        if let Some(id) = &self.tool_call_id {
            v["tool_call_id"] = json!(id);
        }
        if self.role == Role::Tool
            && let Some(n) = &self.name
        {
            v["name"] = json!(n);
        }
        v
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    Text(String),
    Reasoning(String),
    /// A tool call started streaming (name known).
    ToolCallStarted {
        index: usize,
        name: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

impl Completion {
    pub fn into_message(self) -> Message {
        Message {
            role: Role::Assistant,
            content: if self.content.is_empty() && !self.tool_calls.is_empty() { None } else { Some(self.content) },
            tool_calls: self.tool_calls,
            tool_call_id: None,
            name: None,
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("cannot reach {url}: {source}")]
    Connect {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP {status} from {url}: {body}")]
    Http { status: u16, url: String, body: String },
    #[error("bad response: {0}")]
    Protocol(String),
    #[error("cancelled")]
    Cancelled,
}

impl ProviderError {
    pub fn is_auth(&self) -> bool {
        matches!(self, ProviderError::Http { status: 401 | 403, .. })
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Value>,
    pub temperature: Option<f32>,
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl Client {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("Mercury/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }

    /// `GET /models`, sorted ids.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let resp = self
            .auth(self.http.get(&url).timeout(Duration::from_secs(15)))
            .send()
            .await
            .map_err(|source| ProviderError::Connect { url: url.clone(), source })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ProviderError::Http { status: status.as_u16(), url, body: truncate(&body, 500) });
        }
        parse_model_list(&body)
    }

    /// Stream a chat completion, calling `on_delta` for visible progress.
    pub async fn chat_stream(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<Completion, ProviderError> {
        match self.chat_stream_inner(req, true, cancel, on_delta).await {
            Err(ProviderError::Http { status: 400 | 422, ref body, .. }) if body.contains("stream_options") => {
                self.chat_stream_inner(req, false, cancel, on_delta).await
            }
            other => other,
        }
    }

    async fn chat_stream_inner(
        &self,
        req: &ChatRequest,
        include_usage: bool,
        cancel: &CancellationToken,
        on_delta: &mut (dyn FnMut(StreamDelta) + Send),
    ) -> Result<Completion, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = json!({
            "model": req.model,
            "messages": req.messages.iter().map(Message::to_api).collect::<Vec<_>>(),
            "stream": true,
        });
        if include_usage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools);
            body["tool_choice"] = json!("auto");
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        let send = self.auth(self.http.post(&url).json(&body)).send();
        let resp = tokio::select! {
            r = send => r.map_err(|source| ProviderError::Connect { url: url.clone(), source })?,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Http { status: status.as_u16(), url, body: truncate(&body, 2000) });
        }
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("event-stream"))
            .unwrap_or(true);

        let mut acc = Accumulator::default();
        if !is_sse {
            let text = tokio::select! {
                t = resp.text() => t.map_err(|e| ProviderError::Protocol(e.to_string()))?,
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            };
            acc.absorb_full_response(&text, on_delta)?;
            return Ok(acc.finish());
        }

        let mut stream = resp.bytes_stream();
        let mut parser = SseParser::default();
        loop {
            let chunk = tokio::select! {
                c = stream.next() => c,
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|e| ProviderError::Protocol(format!("stream error: {e}")))?;
            for data in parser.push(&chunk) {
                if data.trim() == "[DONE]" {
                    return Ok(acc.finish());
                }
                acc.absorb_chunk(&data, on_delta)?;
            }
        }
        for data in parser.finish() {
            if data.trim() != "[DONE]" {
                acc.absorb_chunk(&data, on_delta)?;
            }
        }
        Ok(acc.finish())
    }

    /// Non-streaming convenience used for summaries (compression, titles).
    pub async fn complete_text(&self, model: &str, messages: Vec<Message>, cancel: &CancellationToken) -> Result<String, ProviderError> {
        let req = ChatRequest { model: model.into(), messages, tools: vec![], temperature: Some(0.2) };
        let c = self.chat_stream(&req, cancel, &mut |_| {}).await?;
        Ok(c.content)
    }
}

pub fn parse_model_list(body: &str) -> Result<Vec<String>, ProviderError> {
    let v: Value = serde_json::from_str(body).map_err(|e| ProviderError::Protocol(format!("models: {e}")))?;
    let arr = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Protocol("models: no `data` array".into()))?;
    let mut ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").or_else(|| m.get("name")).or_else(|| m.get("model")).and_then(Value::as_str).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Incremental Server-Sent-Events parser yielding each event's joined `data:` payload.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            self.line(line, &mut out);
        }
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                out.push(self.data.join("\n"));
                self.data.clear();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            // Some servers don't send the blank separator line between events.
            if !self.data.is_empty() && looks_complete(&self.data.join("\n")) {
                out.push(self.data.join("\n"));
                self.data.clear();
            }
            self.data.push(payload.to_string());
        }
        // `event:`, `id:`, `retry:` and `:` comments are ignored.
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).to_string();
            for l in rest.lines() {
                self.line(l, &mut out);
            }
        }
        if !self.data.is_empty() {
            out.push(self.data.join("\n"));
            self.data.clear();
        }
        out
    }
}

fn looks_complete(s: &str) -> bool {
    s.trim() == "[DONE]" || serde_json::from_str::<Value>(s).is_ok()
}

/// Splits inline `<think>…</think>` blocks out of streamed text, across chunk boundaries.
#[derive(Default)]
pub struct ThinkSplitter {
    in_think: bool,
    pending: String,
}

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

impl ThinkSplitter {
    /// Returns (visible_text, reasoning_text) for this piece.
    pub fn push(&mut self, piece: &str) -> (String, String) {
        self.pending.push_str(piece);
        let mut text = String::new();
        let mut reasoning = String::new();
        loop {
            let tag = if self.in_think { CLOSE } else { OPEN };
            if let Some(i) = self.pending.find(tag) {
                let before: String = self.pending[..i].to_string();
                if self.in_think {
                    reasoning.push_str(&before)
                } else {
                    text.push_str(&before)
                }
                self.pending.drain(..i + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // Keep a possible partial tag at the end for the next chunk.
            let keep = partial_suffix(&self.pending, tag);
            let emit_len = self.pending.len() - keep;
            let emit: String = self.pending.drain(..emit_len).collect();
            if self.in_think {
                reasoning.push_str(&emit)
            } else {
                text.push_str(&emit)
            }
            break;
        }
        (text, reasoning)
    }

    pub fn flush(&mut self) -> (String, String) {
        let rest = std::mem::take(&mut self.pending);
        if self.in_think { (String::new(), rest) } else { (rest, String::new()) }
    }
}

fn partial_suffix(s: &str, tag: &str) -> usize {
    (1..tag.len()).rev().find(|&n| s.len() >= n && s.is_char_boundary(s.len() - n) && tag.starts_with(&s[s.len() - n..])).unwrap_or(0)
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Accumulator {
    content: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    think: ThinkSplitter,
}

impl Accumulator {
    fn text(&mut self, piece: &str, on_delta: &mut (dyn FnMut(StreamDelta) + Send)) {
        let (t, r) = self.think.push(piece);
        self.emit(t, r, on_delta);
    }

    fn emit(&mut self, t: String, r: String, on_delta: &mut (dyn FnMut(StreamDelta) + Send)) {
        if !r.is_empty() {
            self.reasoning.push_str(&r);
            on_delta(StreamDelta::Reasoning(r));
        }
        if !t.is_empty() {
            self.content.push_str(&t);
            on_delta(StreamDelta::Text(t));
        }
    }

    fn absorb_chunk(&mut self, data: &str, on_delta: &mut (dyn FnMut(StreamDelta) + Send)) -> Result<(), ProviderError> {
        let v: Value =
            serde_json::from_str(data).map_err(|e| ProviderError::Protocol(format!("bad stream chunk ({e}): {}", truncate(data, 200))))?;
        if let Some(err) = v.get("error") {
            return Err(ProviderError::Protocol(format!("server error: {err}")));
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null())
            && let Ok(u) = serde_json::from_value::<Usage>(u.clone())
        {
            self.usage = Some(u);
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) else {
            return Ok(());
        };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(r) = delta.get(key).and_then(Value::as_str)
                && !r.is_empty()
            {
                self.reasoning.push_str(r);
                on_delta(StreamDelta::Reasoning(r.to_string()));
            }
        }
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            self.text(c, on_delta);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (pos, call) in calls.iter().enumerate() {
                let index = call.get("index").and_then(Value::as_u64).map(|i| i as usize).unwrap_or(pos);
                while self.calls.len() <= index {
                    self.calls.push(PartialCall::default());
                }
                let slot = &mut self.calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.id.push_str(id);
                }
                if let Some(f) = call.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        let was_empty = slot.name.is_empty();
                        slot.name.push_str(n);
                        if was_empty && !n.is_empty() {
                            on_delta(StreamDelta::ToolCallStarted { index, name: slot.name.clone() });
                        }
                    }
                    match f.get("arguments") {
                        Some(Value::String(a)) => slot.arguments.push_str(a),
                        Some(obj @ Value::Object(_)) => slot.arguments.push_str(&obj.to_string()),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn absorb_full_response(&mut self, text: &str, on_delta: &mut (dyn FnMut(StreamDelta) + Send)) -> Result<(), ProviderError> {
        // A server that ignored `stream: true`. It may still be SSE without the header.
        if text.trim_start().starts_with("data:") {
            let mut p = SseParser::default();
            let mut events = p.push(text.as_bytes());
            events.extend(p.finish());
            for e in events.into_iter().filter(|e| e.trim() != "[DONE]") {
                self.absorb_chunk(&e, on_delta)?;
            }
            return Ok(());
        }
        self.absorb_chunk(text, on_delta)
    }

    fn finish(mut self) -> Completion {
        let (t, r) = self.think.flush();
        self.emit(t, r, &mut |_| {});
        let tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() { format!("call_{i}_{}", uuid::Uuid::new_v4().simple()) } else { c.id },
                kind: "function".into(),
                function: FunctionCall { name: c.name, arguments: if c.arguments.trim().is_empty() { "{}".into() } else { c.arguments } },
            })
            .collect();
        Completion { content: self.content, reasoning: self.reasoning, tool_calls, finish_reason: self.finish_reason, usage: self.usage }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_chunks(chunks: &[&str]) -> (Completion, Vec<StreamDelta>) {
        let mut acc = Accumulator::default();
        let mut deltas = vec![];
        let mut parser = SseParser::default();
        for c in chunks {
            for d in parser.push(c.as_bytes()) {
                if d.trim() != "[DONE]" {
                    acc.absorb_chunk(&d, &mut |x| deltas.push(x)).unwrap();
                }
            }
        }
        for d in parser.finish() {
            if d.trim() != "[DONE]" {
                acc.absorb_chunk(&d, &mut |x| deltas.push(x)).unwrap();
            }
        }
        (acc.finish(), deltas)
    }

    #[test]
    fn sse_split_across_chunks() {
        let (c, _) = run_chunks(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel",
            "lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
            ": keepalive\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\ndata: [DONE]\n\n",
        ]);
        assert_eq!(c.content, "Hello world");
        assert_eq!(c.finish_reason.as_deref(), Some("stop"));
        assert_eq!(c.usage.unwrap().total_tokens, 7);
    }

    #[test]
    fn sse_without_blank_separators() {
        let (c, _) =
            run_chunks(&["data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\ndata: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n"]);
        assert_eq!(c.content, "ab");
    }

    #[test]
    fn tool_call_deltas_assemble() {
        let (c, deltas) = run_chunks(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}},{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"terminal\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
        ]);
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].id, "call_1");
        assert_eq!(c.tool_calls[0].function.arguments, "{\"path\":\"a.txt\"}");
        assert_eq!(c.tool_calls[1].function.name, "terminal");
        assert!(deltas.contains(&StreamDelta::ToolCallStarted { index: 0, name: "read_file".into() }));
        let msg = c.into_message();
        assert!(msg.content.is_none());
        assert_eq!(msg.to_api()["content"], Value::Null);
    }

    #[test]
    fn ollama_style_object_arguments_and_missing_ids() {
        let (c, _) = run_chunks(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"memory\",\"arguments\":{\"action\":\"add\"}}}]}}]}\n\n",
        ]);
        assert_eq!(c.tool_calls[0].function.arguments, "{\"action\":\"add\"}");
        assert!(c.tool_calls[0].id.starts_with("call_0_"));
    }

    #[test]
    fn reasoning_fields_and_think_tags() {
        let (c, _) = run_chunks(&[
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"<thi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"nk>plan</th\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ink>Answer <b>bold</b>\"}}]}\n\n",
        ]);
        assert_eq!(c.reasoning, "hmm plan");
        assert_eq!(c.content, "Answer <b>bold</b>");
    }

    #[test]
    fn think_splitter_unclosed_flushes_as_reasoning() {
        let mut s = ThinkSplitter::default();
        let (t, r) = s.push("x<think>still going");
        assert_eq!((t.as_str(), r.as_str()), ("x", "still going"));
        let (t, r) = s.flush();
        assert_eq!((t.as_str(), r.as_str()), ("", ""));
        let mut s = ThinkSplitter::default();
        assert_eq!(s.push("a <").0, "a ");
        assert_eq!(s.flush().0, "<");
    }

    #[test]
    fn server_error_in_stream() {
        let mut acc = Accumulator::default();
        let e = acc.absorb_chunk("{\"error\":{\"message\":\"model not loaded\"}}", &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("model not loaded"));
    }

    #[test]
    fn full_json_response() {
        let mut acc = Accumulator::default();
        acc.absorb_full_response(
            "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}",
            &mut |_| {},
        )
        .unwrap();
        let c = acc.finish();
        assert_eq!(c.content, "hi");
        assert_eq!(c.usage.unwrap().total_tokens, 2);
    }

    #[test]
    fn model_list_formats() {
        assert_eq!(
            parse_model_list("{\"object\":\"list\",\"data\":[{\"id\":\"b\"},{\"id\":\"a\"},{\"id\":\"a\"}]}").unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(parse_model_list("{\"models\":[{\"name\":\"llama3\"}]}").unwrap(), vec!["llama3"]);
        assert!(parse_model_list("not json").is_err());
    }

    #[test]
    fn api_shape_strips_reasoning() {
        let mut m = Message::assistant("x");
        m.reasoning = Some("secret".into());
        let v = m.to_api();
        assert!(v.get("reasoning").is_none());
        let t = Message::tool("c1", "terminal", "out");
        assert_eq!(t.to_api()["tool_call_id"], "c1");
        assert_eq!(t.to_api()["name"], "terminal");
        assert_eq!(Message::user("").to_api()["content"], "");
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate("héllo", 2), "h…");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
