//! Skills: folders containing a `SKILL.md` with YAML frontmatter (the agentskills.io / Hermes
//! format). The model sees names and descriptions up front and loads a body on demand.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde_yaml::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub writable: bool,
    pub tags: Vec<String>,
}

impl Skill {
    pub fn skill_md(&self) -> PathBuf {
        self.dir.join("SKILL.md")
    }
}

/// Split `---\nyaml\n---\nbody` into (frontmatter, body).
pub fn split_frontmatter(text: &str) -> (Option<Value>, &str) {
    let t = text.trim_start_matches('\u{feff}');
    let Some(rest) = t.strip_prefix("---") else { return (None, t) };
    let rest = rest.trim_start_matches(['\r']).strip_prefix('\n').unwrap_or(rest);
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (serde_yaml::from_str(yaml).ok(), body);
        }
        offset += line.len();
    }
    (None, t)
}

fn parse_skill(dir: &Path, writable: bool) -> Option<Skill> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (fm, body) = split_frontmatter(&text);
    let fm = fm.unwrap_or(Value::Null);
    let name =
        fm.get("name").and_then(Value::as_str).map(str::to_string).or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))?;
    let description = fm
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("").to_string());
    let tags = fm
        .get("metadata")
        .and_then(|m| m.get("hermes"))
        .and_then(|h| h.get("tags"))
        .or_else(|| fm.get("tags"))
        .and_then(Value::as_sequence)
        .map(|s| s.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Some(Skill { name, description, dir: dir.to_path_buf(), writable, tags })
}

#[derive(Debug, Clone)]
pub struct SkillLibrary {
    /// Mercury's own, writable directory.
    pub home: PathBuf,
    /// Read-only extras (e.g. `~/.hermes/skills`).
    pub extra: Vec<PathBuf>,
    /// Names switched off in settings: hidden from `list`, `index` and the skill tools.
    pub disabled: Vec<String>,
}

fn collect(root: &Path, writable: bool, depth: usize, out: &mut Vec<Skill>) {
    if depth > 4 {
        return;
    }
    if root.join("SKILL.md").is_file() {
        if let Some(s) = parse_skill(root, writable) {
            out.push(s);
        }
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for d in dirs {
        if d.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(true) {
            continue;
        }
        collect(&d, writable, depth + 1, out);
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

impl SkillLibrary {
    pub fn new(home: PathBuf, extra: Vec<PathBuf>) -> Self {
        Self { home, extra, disabled: vec![] }
    }

    pub fn disabling(mut self, names: Vec<String>) -> Self {
        self.disabled = names;
        self
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.iter().any(|d| d == name)
    }

    /// Enabled skills.
    pub fn list(&self) -> Vec<Skill> {
        let mut v = self.list_all();
        v.retain(|s| self.is_enabled(&s.name));
        v
    }

    /// Every skill, enabled or not (for settings).
    pub fn list_all(&self) -> Vec<Skill> {
        let mut out = Vec::new();
        collect(&self.home, true, 0, &mut out);
        for e in &self.extra {
            collect(e, false, 0, &mut out);
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|s| seen.insert(s.name.clone()));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.list().into_iter().find(|s| s.name == name)
    }

    /// The SKILL.md text plus a listing of supporting files.
    pub fn view(&self, name: &str, file: Option<&str>) -> Result<String> {
        let skill = self.get(name).ok_or_else(|| anyhow!("no skill named {name:?}"))?;
        if let Some(rel) = file {
            let p = skill.dir.join(rel);
            let canon = p.canonicalize()?;
            if !canon.starts_with(skill.dir.canonicalize()?) {
                bail!("{rel} is outside the skill folder");
            }
            return Ok(std::fs::read_to_string(canon)?);
        }
        let mut text = std::fs::read_to_string(skill.skill_md())?;
        let mut extras = Vec::new();
        for entry in ignore::WalkBuilder::new(&skill.dir).max_depth(Some(3)).build().flatten() {
            let p = entry.path();
            if p.is_file()
                && p.file_name().map(|n| n != "SKILL.md").unwrap_or(false)
                && let Ok(rel) = p.strip_prefix(&skill.dir)
            {
                extras.push(rel.display().to_string());
            }
        }
        if !extras.is_empty() {
            extras.sort();
            text.push_str("\n\n---\nSupporting files (view with `file`):\n");
            for e in extras {
                text.push_str(&format!("- {e}\n"));
            }
        }
        Ok(text)
    }

    pub fn create(&self, name: &str, description: &str, body: &str) -> Result<Skill> {
        if !valid_name(name) {
            bail!("skill names are lowercase letters, digits and dashes (max 64)");
        }
        if self.get(name).is_some() {
            bail!("a skill named {name} already exists");
        }
        let dir = self.home.join(name);
        std::fs::create_dir_all(&dir)?;
        let desc = description.replace('\n', " ");
        let text = format!(
            "---\nname: {name}\ndescription: {}\nversion: 1.0.0\nauthor: Mercury agent\n---\n\n{}\n",
            serde_yaml::to_string(&desc)?.trim(),
            body.trim()
        );
        std::fs::write(dir.join("SKILL.md"), text)?;
        parse_skill(&dir, true).ok_or_else(|| anyhow!("failed to read back the new skill"))
    }

    fn writable(&self, name: &str) -> Result<Skill> {
        let s = self.get(name).ok_or_else(|| anyhow!("no skill named {name:?}"))?;
        if !s.writable {
            bail!("{name} lives in a read-only folder ({}); copy it before editing", s.dir.display());
        }
        Ok(s)
    }

    pub fn edit(&self, name: &str, content: &str) -> Result<()> {
        let s = self.writable(name)?;
        if split_frontmatter(content).0.is_none() {
            bail!("SKILL.md must start with YAML frontmatter (---)");
        }
        std::fs::write(s.skill_md(), content)?;
        Ok(())
    }

    pub fn patch(&self, name: &str, old: &str, new: &str) -> Result<()> {
        let s = self.writable(name)?;
        let text = std::fs::read_to_string(s.skill_md())?;
        let next = crate::tools::files::replace_unique(&text, old, new, false)?;
        std::fs::write(s.skill_md(), next)?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let s = self.writable(name)?;
        std::fs::remove_dir_all(&s.dir)?;
        Ok(())
    }

    /// Compact index for the system prompt.
    pub fn index(&self) -> String {
        self.list().iter().map(|s| format!("- {}: {}", s.name, s.description)).collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter() {
        let (fm, body) = split_frontmatter("---\nname: x\ndescription: \"d: e\"\n---\n# Body\n");
        assert_eq!(fm.unwrap()["description"].as_str(), Some("d: e"));
        assert_eq!(body, "# Body\n");
        let (fm, body) = split_frontmatter("no fm");
        assert!(fm.is_none());
        assert_eq!(body, "no fm");
    }

    #[test]
    fn library_lifecycle() {
        let home = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        // A Hermes-style categorised read-only skill.
        let ro = extra.path().join("note-taking/obsidian");
        std::fs::create_dir_all(ro.join("references")).unwrap();
        std::fs::write(
            ro.join("SKILL.md"),
            "---\nname: obsidian\ndescription: Edit notes.\nmetadata:\n  hermes:\n    tags: [Notes]\n---\n# Obsidian\n",
        )
        .unwrap();
        std::fs::write(ro.join("references/api.md"), "ref").unwrap();
        let lib = SkillLibrary::new(home.path().to_path_buf(), vec![extra.path().to_path_buf()]);
        let s = lib.get("obsidian").unwrap();
        assert!(!s.writable);
        assert_eq!(s.tags, vec!["Notes"]);
        let v = lib.view("obsidian", None).unwrap();
        assert!(v.contains("references/api.md"));
        assert_eq!(lib.view("obsidian", Some("references/api.md")).unwrap(), "ref");
        assert!(lib.view("obsidian", Some("../../../etc/passwd")).is_err());
        assert!(lib.edit("obsidian", "---\nname: obsidian\n---\n").is_err());

        lib.create("deploy-app", "Deploy: the app\nsafely", "1. build\n2. ship").unwrap();
        assert!(lib.create("deploy-app", "dup", "x").is_err());
        assert!(lib.create("Bad Name", "x", "y").is_err());
        let d = lib.get("deploy-app").unwrap();
        assert_eq!(d.description, "Deploy: the app safely");
        lib.patch("deploy-app", "2. ship", "2. test\n3. ship").unwrap();
        assert!(lib.view("deploy-app", None).unwrap().contains("3. ship"));
        assert!(lib.index().contains("- deploy-app: Deploy: the app safely"));
        lib.delete("deploy-app").unwrap();
        assert!(lib.get("deploy-app").is_none());
    }

    #[test]
    fn names() {
        assert!(valid_name("a-b-1"));
        for n in ["", "A", "a b", "-a", "a-", "a/b"] {
            assert!(!valid_name(n), "{n}");
        }
    }
}
