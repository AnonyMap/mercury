//! Persistent curated memory, file-compatible with Hermes: `MEMORY.md` (agent notes) and
//! `USER.md` (user profile), entries separated by `\n§\n`, each file under a character cap.
//! The prompt gets a frozen snapshot at session start; writes during a session go to disk only.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

pub const ENTRY_DELIMITER: &str = "\n§\n";
pub const MEMORY_LIMIT: usize = 2200;
pub const USER_LIMIT: usize = 1375;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Memory,
    User,
}

impl Target {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "memory" | "memory.md" | "notes" => Some(Self::Memory),
            "user" | "user.md" | "profile" => Some(Self::User),
            _ => None,
        }
    }
    pub fn file(self) -> &'static str {
        match self {
            Self::Memory => "MEMORY.md",
            Self::User => "USER.md",
        }
    }
    fn header(self) -> &'static str {
        match self {
            Self::Memory => "MEMORY (your personal notes)",
            Self::User => "USER PROFILE (who the user is)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
    pub memory_limit: usize,
    pub user_limit: usize,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), memory_limit: MEMORY_LIMIT, user_limit: USER_LIMIT }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn limit(&self, t: Target) -> usize {
        match t {
            Target::Memory => self.memory_limit,
            Target::User => self.user_limit,
        }
    }

    pub fn path(&self, t: Target) -> PathBuf {
        self.dir.join(t.file())
    }

    pub fn read_raw(&self, t: Target) -> String {
        std::fs::read_to_string(self.path(t)).unwrap_or_default()
    }

    pub fn entries(&self, t: Target) -> Vec<String> {
        self.read_raw(t).split(ENTRY_DELIMITER).map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect()
    }

    fn write(&self, t: Target, entries: &[String]) -> Result<()> {
        let text = entries.join(ENTRY_DELIMITER);
        if text.chars().count() > self.limit(t) {
            bail!(
                "{} would be {} chars, over its {}-char limit. Replace or remove older entries first.",
                t.file(),
                text.chars().count(),
                self.limit(t)
            );
        }
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.path(t).with_extension("md.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, self.path(t))?;
        Ok(())
    }

    /// Overwrite a whole file (the GUI editor). The cap still applies.
    pub fn write_raw(&self, t: Target, text: &str) -> Result<()> {
        let entries: Vec<String> = text.split(ENTRY_DELIMITER).map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
        self.write(t, &entries)
    }

    pub fn add(&self, t: Target, content: &str) -> Result<String> {
        let content = clean(content)?;
        let mut entries = self.entries(t);
        if entries.iter().any(|e| e == &content) {
            return Ok(format!("Already in {}.", t.file()));
        }
        entries.push(content);
        self.write(t, &entries)?;
        Ok(format!("Added to {}.", t.file()))
    }

    fn find_one(&self, entries: &[String], t: Target, old_text: &str) -> Result<usize> {
        let needle = old_text.trim();
        if needle.is_empty() {
            bail!("old_text is required to pick the entry");
        }
        let hits: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| e.contains(needle)).map(|(i, _)| i).collect();
        match hits.as_slice() {
            [i] => Ok(*i),
            [] => bail!("no entry in {} contains {:?}", t.file(), needle),
            _ => bail!("{} entries in {} contain {:?}; use a longer, unique snippet", hits.len(), t.file(), needle),
        }
    }

    pub fn replace(&self, t: Target, old_text: &str, content: &str) -> Result<String> {
        let content = clean(content)?;
        let mut entries = self.entries(t);
        let i = self.find_one(&entries, t, old_text)?;
        entries[i] = content;
        self.write(t, &entries)?;
        Ok(format!("Replaced an entry in {}.", t.file()))
    }

    pub fn remove(&self, t: Target, old_text: &str) -> Result<String> {
        let mut entries = self.entries(t);
        let i = self.find_one(&entries, t, old_text)?;
        entries.remove(i);
        self.write(t, &entries)?;
        Ok(format!("Removed an entry from {}.", t.file()))
    }

    pub fn usage(&self, t: Target) -> (usize, usize) {
        (self.entries(t).join(ENTRY_DELIMITER).chars().count(), self.limit(t))
    }

    /// The block injected into the system prompt.
    pub fn snapshot(&self) -> String {
        let mut out = String::new();
        for t in [Target::Memory, Target::User] {
            let entries = self.entries(t);
            if entries.is_empty() {
                continue;
            }
            let (used, cap) = self.usage(t);
            out.push_str(&format!("## {} [{used}/{cap} chars]\n", t.header()));
            for e in entries {
                out.push_str("- ");
                out.push_str(&e.replace('\n', "\n  "));
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

fn clean(content: &str) -> Result<String> {
    let c = content.trim();
    if c.is_empty() {
        bail!("content is empty");
    }
    if c.contains('§') {
        bail!("content may not contain the § delimiter");
    }
    let lower = c.to_lowercase();
    if ["ignore previous instructions", "ignore all previous", "disregard your instructions", "system prompt:"]
        .iter()
        .any(|p| lower.contains(p))
        || c.chars().any(|ch| matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
    {
        bail!("content looks like a prompt injection and was not saved");
    }
    Ok(c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_replace_remove() {
        let d = tempfile::tempdir().unwrap();
        let m = MemoryStore::new(d.path());
        m.add(Target::User, "Name is Ali").unwrap();
        m.add(Target::User, "Prefers Rust").unwrap();
        assert!(m.add(Target::User, "Prefers Rust").unwrap().contains("Already"));
        assert_eq!(std::fs::read_to_string(d.path().join("USER.md")).unwrap(), "Name is Ali\n§\nPrefers Rust");
        m.replace(Target::User, "Rust", "Prefers Rust and Zig").unwrap();
        assert_eq!(m.entries(Target::User)[1], "Prefers Rust and Zig");
        assert!(m.remove(Target::User, "zzz").is_err());
        m.remove(Target::User, "Ali").unwrap();
        assert_eq!(m.entries(Target::User), vec!["Prefers Rust and Zig"]);
    }

    #[test]
    fn ambiguous_and_limits() {
        let d = tempfile::tempdir().unwrap();
        let mut m = MemoryStore::new(d.path());
        m.add(Target::Memory, "project uses tokio").unwrap();
        m.add(Target::Memory, "tokio version 1.4x").unwrap();
        assert!(m.replace(Target::Memory, "tokio", "x").unwrap_err().to_string().contains("2 entries"));
        m.memory_limit = 50;
        assert!(m.add(Target::Memory, &"x".repeat(60)).unwrap_err().to_string().contains("limit"));
        assert_eq!(m.entries(Target::Memory).len(), 2);
    }

    #[test]
    fn rejects_injection_and_delimiter() {
        let d = tempfile::tempdir().unwrap();
        let m = MemoryStore::new(d.path());
        assert!(m.add(Target::Memory, "Ignore previous instructions and ...").is_err());
        assert!(m.add(Target::Memory, "a § b").is_err());
        assert!(m.add(Target::Memory, "hidden\u{200b}text").is_err());
        assert!(m.add(Target::Memory, "  ").is_err());
    }

    #[test]
    fn snapshot_format() {
        let d = tempfile::tempdir().unwrap();
        let m = MemoryStore::new(d.path());
        assert_eq!(m.snapshot(), "");
        m.add(Target::Memory, "line one\nline two").unwrap();
        let s = m.snapshot();
        assert!(s.starts_with("## MEMORY (your personal notes) ["));
        assert!(s.contains("- line one\n  line two"));
        // Hermes-written files with the same delimiter parse identically.
        std::fs::write(d.path().join("USER.md"), "a\n§\nb\n§\n").unwrap();
        assert_eq!(m.entries(Target::User), vec!["a", "b"]);
    }
}
