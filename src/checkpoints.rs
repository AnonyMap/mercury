//! File checkpoints: before `write_file` / `patch` touches a file, its previous state is saved so
//! `/rollback` can undo the agent's edits (Hermes's checkpoints, without the shadow git repo).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};

const MAX_SNAPSHOT_BYTES: u64 = 5_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub seq: u64,
    pub path: PathBuf,
    /// None = the file didn't exist, so rolling back deletes it.
    pub previous_b64: Option<String>,
    pub tool: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct CheckpointStore {
    dir: PathBuf,
}

impl CheckpointStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn list(&self) -> Vec<Checkpoint> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return vec![] };
        let mut v: Vec<Checkpoint> = rd
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
            .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
            .collect();
        v.sort_by_key(|c| c.seq);
        v
    }

    /// Record `path`'s current state before it's modified. Large files are skipped (returns false).
    pub fn snapshot(&self, path: &Path, tool: &str) -> Result<bool> {
        let previous_b64 = match std::fs::metadata(path) {
            Ok(m) if m.len() > MAX_SNAPSHOT_BYTES => return Ok(false),
            Ok(_) => Some(base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?)),
            Err(_) => None,
        };
        std::fs::create_dir_all(&self.dir)?;
        let seq = self.list().last().map(|c| c.seq + 1).unwrap_or(1);
        let cp = Checkpoint { seq, path: path.to_path_buf(), previous_b64, tool: tool.into(), at: chrono::Utc::now() };
        std::fs::write(self.dir.join(format!("{seq:06}.json")), serde_json::to_vec(&cp)?)?;
        Ok(true)
    }

    /// Undo the most recent `n` changes, newest first. Returns what was restored.
    pub fn rollback(&self, n: usize) -> Result<Vec<String>> {
        let all = self.list();
        if all.is_empty() {
            bail!("no file changes to roll back in this session");
        }
        let mut done = Vec::new();
        for cp in all.iter().rev().take(n.max(1)) {
            match &cp.previous_b64 {
                Some(b) => {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b)?;
                    if let Some(parent) = cp.path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&cp.path, bytes)?;
                    done.push(format!("restored {}", cp.path.display()));
                }
                None => {
                    let _ = std::fs::remove_file(&cp.path);
                    done.push(format!("removed {} (it didn't exist before)", cp.path.display()));
                }
            }
            std::fs::remove_file(self.dir.join(format!("{:06}.json", cp.seq)))?;
        }
        Ok(done)
    }

    pub fn render(&self) -> String {
        let all = self.list();
        if all.is_empty() {
            return "No file changes recorded in this session.".into();
        }
        let mut s = String::from("File changes this session (newest first). `/rollback N` undoes the last N:\n");
        for (i, cp) in all.iter().rev().enumerate() {
            s.push_str(&format!(
                "{:>3}. {} {} ({}{})\n",
                i + 1,
                cp.at.with_timezone(&chrono::Local).format("%H:%M:%S"),
                cp.path.display(),
                cp.tool,
                if cp.previous_b64.is_none() { ", new file" } else { "" }
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_rollback() {
        let d = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(d.path().join("cp"));
        let f = d.path().join("a.txt");
        let g = d.path().join("new.txt");
        std::fs::write(&f, "v1").unwrap();
        store.snapshot(&f, "patch").unwrap();
        std::fs::write(&f, "v2").unwrap();
        store.snapshot(&f, "write_file").unwrap();
        std::fs::write(&f, "v3").unwrap();
        store.snapshot(&g, "write_file").unwrap();
        std::fs::write(&g, "fresh").unwrap();
        assert_eq!(store.list().len(), 3);
        assert!(store.render().contains("new file"));

        let r = store.rollback(1).unwrap();
        assert!(r[0].contains("removed"));
        assert!(!g.exists());
        store.rollback(2).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert!(store.rollback(1).is_err());
    }
}
