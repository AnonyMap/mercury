//! Session storage: SQLite with an FTS5 index over message text.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::provider::{Message, Role, Usage};

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub message_count: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Insights {
    pub days: i64,
    pub sessions: u64,
    pub user_messages: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub by_model: std::collections::BTreeMap<String, u64>,
    pub tool_calls: std::collections::BTreeMap<String, u64>,
}

impl Insights {
    pub fn render(&self) -> String {
        let mut s = format!(
            "Last {} days: {} sessions, {} messages from you, {} tokens ({} in / {} out).",
            self.days,
            self.sessions,
            self.user_messages,
            self.prompt_tokens + self.completion_tokens,
            self.prompt_tokens,
            self.completion_tokens
        );
        let mut models: Vec<_> = self.by_model.iter().collect();
        models.sort_by(|a, b| b.1.cmp(a.1));
        if !models.is_empty() {
            s.push_str("\nModels:");
            for (m, t) in models.iter().take(6) {
                s.push_str(&format!("\n  {m}: {t} tokens"));
            }
        }
        let mut tools: Vec<_> = self.tool_calls.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1));
        if !tools.is_empty() {
            s.push_str("\nTools:");
            for (t, n) in tools.iter().take(10) {
                s.push_str(&format!("\n  {t}: {n}"));
            }
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub role: String,
    pub snippet: String,
}

pub struct SessionStore {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    provider TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, id);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content, session_id UNINDEXED, tokenize='unicode61');
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content, session_id)
    VALUES (new.id, coalesce(json_extract(new.body, '$.content'), ''), new.session_id);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts WHERE rowid = old.id;
END;
"#;

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Turn free text into a safe FTS5 query: each word quoted, AND-ed; trailing `*` keeps prefix search.
pub fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|t| {
            let prefix = t.ends_with('*');
            let core: String = t.trim_end_matches('*').chars().filter(|c| *c != '"').collect();
            (core, prefix)
        })
        .filter(|(c, _)| !c.is_empty())
        .map(|(c, p)| if p { format!("\"{c}\"*") } else { format!("\"{c}\"") })
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        // v2: where an imported session came from (e.g. `hermes:default:<id>`), for idempotent imports.
        let has_origin: bool =
            conn.prepare("PRAGMA table_info(sessions)")?.query_map([], |r| r.get::<_, String>(1))?.flatten().any(|c| c == "origin");
        if !has_origin {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN origin TEXT;")?;
        }
        conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS sessions_origin ON sessions(origin) WHERE origin IS NOT NULL;")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn has_origin(&self, origin: &str) -> Result<bool> {
        Ok(self.c().query_row("SELECT 1 FROM sessions WHERE origin = ?1", params![origin], |_| Ok(())).optional()?.is_some())
    }

    /// Insert a whole transcript from elsewhere. Returns false if `origin` was already imported.
    #[allow(clippy::too_many_arguments)]
    pub fn import_session(
        &self,
        origin: &str,
        title: &str,
        created: Option<DateTime<Utc>>,
        updated: Option<DateTime<Utc>>,
        provider: &str,
        model: &str,
        msgs: &[Message],
    ) -> Result<bool> {
        let mut c = self.c();
        let tx = c.transaction()?;
        if tx.query_row("SELECT 1 FROM sessions WHERE origin = ?1", params![origin], |_| Ok(())).optional()?.is_some() {
            return Ok(false);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let created = created.unwrap_or_else(Utc::now).to_rfc3339();
        let updated = updated.map(|u| u.to_rfc3339()).unwrap_or_else(|| created.clone());
        tx.execute(
            "INSERT INTO sessions(id, title, created_at, updated_at, provider, model, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, title, created, updated, provider, model, origin],
        )?;
        for m in msgs {
            tx.execute(
                "INSERT INTO messages(session_id, role, body, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, role_str(m.role), serde_json::to_string(m)?, updated],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Token totals and counts over the last `days` days (for /insights).
    pub fn insights(&self, days: i64) -> Result<Insights> {
        let since = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        let c = self.c();
        let mut ins = Insights { days, ..Default::default() };
        let mut st = c.prepare(
            "SELECT s.model, s.provider, s.prompt_tokens, s.completion_tokens, s.id FROM sessions s WHERE s.updated_at >= ?1 AND s.origin IS NULL",
        )?;
        let rows = st.query_map(params![since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?))
        })?;
        let mut ids = Vec::new();
        for r in rows.flatten() {
            ins.sessions += 1;
            ins.prompt_tokens += r.2 as u64;
            ins.completion_tokens += r.3 as u64;
            let key = if r.0.is_empty() { "(unknown)".to_string() } else { format!("{} via {}", r.0, r.1) };
            *ins.by_model.entry(key).or_default() += (r.2 + r.3) as u64;
            ids.push(r.4);
        }
        let mut st = c.prepare(
            "SELECT m.role, m.body FROM messages m JOIN sessions s ON s.id = m.session_id WHERE s.updated_at >= ?1 AND s.origin IS NULL AND m.role IN ('user','assistant')",
        )?;
        for r in st.query_map(params![since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.flatten() {
            if r.0 == "user" {
                ins.user_messages += 1;
            } else if let Ok(m) = serde_json::from_str::<Message>(&r.1) {
                for call in m.tool_calls {
                    *ins.tool_calls.entry(call.function.name).or_default() += 1;
                }
            }
        }
        Ok(ins)
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn create(&self, provider: &str, model: &str) -> Result<SessionMeta> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        self.c().execute(
            "INSERT INTO sessions(id, title, created_at, updated_at, provider, model) VALUES (?1, '', ?2, ?2, ?3, ?4)",
            params![id, now.to_rfc3339(), provider, model],
        )?;
        Ok(self.get(&id)?.expect("just inserted"))
    }

    pub fn get(&self, id: &str) -> Result<Option<SessionMeta>> {
        let c = self.c();
        Ok(c.query_row(
            "SELECT s.id, s.title, s.created_at, s.updated_at, s.provider, s.model, s.prompt_tokens, s.completion_tokens,
                    (SELECT count(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s WHERE s.id = ?1",
            params![id],
            row_to_meta,
        )
        .optional()?)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<SessionMeta>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT s.id, s.title, s.created_at, s.updated_at, s.provider, s.model, s.prompt_tokens, s.completion_tokens,
                    (SELECT count(*) FROM messages m WHERE m.session_id = s.id)
             FROM sessions s ORDER BY s.updated_at DESC LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit as i64], row_to_meta)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn append(&self, session_id: &str, m: &Message) -> Result<i64> {
        let c = self.c();
        let now = Utc::now().to_rfc3339();
        c.execute(
            "INSERT INTO messages(session_id, role, body, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, role_str(m.role), serde_json::to_string(m)?, now],
        )?;
        let id = c.last_insert_rowid();
        c.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, now])?;
        Ok(id)
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let c = self.c();
        let mut st = c.prepare("SELECT body FROM messages WHERE session_id = ?1 ORDER BY id")?;
        let rows = st.query_map(params![session_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for body in rows {
            out.push(serde_json::from_str(&body?)?);
        }
        Ok(out)
    }

    /// Replace the whole transcript (used by /undo, /retry and compression).
    pub fn replace_messages(&self, session_id: &str, msgs: &[Message]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id])?;
        let now = Utc::now().to_rfc3339();
        for m in msgs {
            tx.execute(
                "INSERT INTO messages(session_id, role, body, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, role_str(m.role), serde_json::to_string(m)?, now],
            )?;
        }
        tx.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, now])?;
        tx.commit()?;
        Ok(())
    }

    pub fn rename(&self, session_id: &str, title: &str) -> Result<()> {
        self.c().execute("UPDATE sessions SET title = ?2 WHERE id = ?1", params![session_id, title.trim()])?;
        Ok(())
    }

    pub fn set_model(&self, session_id: &str, provider: &str, model: &str) -> Result<()> {
        self.c().execute("UPDATE sessions SET provider = ?2, model = ?3 WHERE id = ?1", params![session_id, provider, model])?;
        Ok(())
    }

    pub fn add_usage(&self, session_id: &str, u: &Usage) -> Result<()> {
        self.c().execute(
            "UPDATE sessions SET prompt_tokens = prompt_tokens + ?2, completion_tokens = completion_tokens + ?3 WHERE id = ?1",
            params![session_id, u.prompt_tokens as i64, u.completion_tokens as i64],
        )?;
        Ok(())
    }

    pub fn delete(&self, session_id: &str) -> Result<()> {
        let c = self.c();
        c.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id])?;
        c.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        Ok(())
    }

    /// Drop sessions that never got a message (every launch starts one), except `keep`.
    pub fn prune_empty(&self, keep: &str) -> Result<usize> {
        Ok(self.c().execute(
            "DELETE FROM sessions WHERE id != ?1 AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.session_id = sessions.id)",
            params![keep],
        )?)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(q) = fts_query(query) else { return Ok(vec![]) };
        let c = self.c();
        let mut st = c.prepare(
            "SELECT f.session_id, s.title, m.role, snippet(messages_fts, 0, '[', ']', '…', 12)
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             JOIN sessions s ON s.id = f.session_id
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = st.query_map(params![q, limit as i64], |r| {
            Ok(SearchHit { session_id: r.get(0)?, title: r.get(1)?, role: r.get(2)?, snippet: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn parse_ts(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)).unwrap_or_else(|_| Utc::now())
}

fn row_to_meta(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    let p: i64 = r.get(6)?;
    let c: i64 = r.get(7)?;
    Ok(SessionMeta {
        id: r.get(0)?,
        title: r.get(1)?,
        created_at: parse_ts(r.get(2)?),
        updated_at: parse_ts(r.get(3)?),
        provider: r.get(4)?,
        model: r.get(5)?,
        usage: Usage { prompt_tokens: p as u64, completion_tokens: c as u64, total_tokens: (p + c) as u64 },
        message_count: r.get::<_, i64>(8)? as u64,
    })
}

/// A short title from the first user message.
pub fn title_from(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("New chat");
    let mut t: String = first.chars().take(60).collect();
    if first.chars().count() > 60 {
        t = t.trim_end().to_string() + "…";
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("s.db")).unwrap();
        let s = store.create("ollama", "qwen3").unwrap();
        store.append(&s.id, &Message::user("How do I configure the reverse proxy for Gitea?")).unwrap();
        let mut a = Message::assistant("Use nginx with proxy_pass.");
        a.reasoning = Some("thinking".into());
        store.append(&s.id, &a).unwrap();
        let s2 = store.create("nous", "Hermes-4").unwrap();
        store.append(&s2.id, &Message::user("bake sourdough bread")).unwrap();

        let msgs = store.messages(&s.id).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].reasoning.as_deref(), Some("thinking"));

        let hits = store.search("gitea proxy", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, s.id);
        assert!(hits[0].snippet.contains("[Gitea]"));
        assert_eq!(store.search("sourd*", 10).unwrap()[0].session_id, s2.id);
        assert!(store.search("\"\" ", 10).unwrap().is_empty());
        // Hostile FTS syntax is neutralised rather than erroring.
        assert!(store.search("NEAR( OR \"", 10).is_ok());

        store.rename(&s.id, "Gitea proxy").unwrap();
        store.add_usage(&s.id, &Usage { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 }).unwrap();
        let meta = store.get(&s.id).unwrap().unwrap();
        assert_eq!(meta.title, "Gitea proxy");
        assert_eq!(meta.usage.total_tokens, 15);
        assert_eq!(meta.message_count, 2);

        store.replace_messages(&s.id, &msgs[..1]).unwrap();
        assert_eq!(store.messages(&s.id).unwrap().len(), 1);
        assert!(store.search("nginx", 10).unwrap().is_empty(), "FTS rows follow deletes");

        assert_eq!(store.list(10).unwrap().len(), 2);
        store.delete(&s2.id).unwrap();
        assert_eq!(store.list(10).unwrap().len(), 1);
        assert!(store.search("sourdough", 10).unwrap().is_empty());
    }

    #[test]
    fn titles() {
        assert_eq!(title_from("\n  hello there\nmore"), "hello there");
        assert_eq!(title_from(""), "New chat");
        assert!(title_from(&"x".repeat(100)).ends_with('…'));
    }
}
