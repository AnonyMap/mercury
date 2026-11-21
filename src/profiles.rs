//! Mercury profiles: independent agents in one install, each with its own chats, memory,
//! skills, SOUL.md, model choice and MCP servers. Same idea (and naming rules) as Hermes profiles.

use anyhow::{Result, bail};

use crate::config::{Config, DEFAULT_PROFILE, Paths};

/// Lowercase letters, digits, `-` and `_`, starting with a letter or digit, at most 64 chars.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Turn any name (e.g. a Hermes profile folder) into a valid profile name.
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .trim_start_matches(['-', '_'])
        .chars()
        .take(64)
        .collect();
    if s.is_empty() { DEFAULT_PROFILE.into() } else { s }
}

/// All profiles: `default` first, then the rest alphabetically.
pub fn list(paths: &Paths) -> Vec<String> {
    let mut out = vec![DEFAULT_PROFILE.to_string()];
    if let Ok(rd) = std::fs::read_dir(paths.data_dir.join("profiles")) {
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| valid_name(n) && n != DEFAULT_PROFILE)
            .collect();
        names.sort();
        out.extend(names);
    }
    out
}

pub fn exists(paths: &Paths, name: &str) -> bool {
    name == DEFAULT_PROFILE || paths.data_dir.join("profiles").join(name).is_dir()
}

/// Create a profile (it starts from the last-used model and personality). Returns its paths.
pub fn create(paths: &Paths, name: &str) -> Result<Paths> {
    if !valid_name(name) {
        bail!("profile names use lowercase letters, digits, '-' and '_' (e.g. work, coder-2)");
    }
    let p = paths.with_profile(name);
    if exists(paths, name) {
        bail!("a profile named {name} already exists");
    }
    p.ensure()?;
    Config::load_profile(&p)?.save_profile(&p)?;
    Ok(p)
}

/// Create the profile if needed; returns its paths either way.
pub fn ensure(paths: &Paths, name: &str) -> Result<Paths> {
    if exists(paths, name) {
        let p = paths.with_profile(name);
        p.ensure()?;
        Ok(p)
    } else {
        create(paths, name)
    }
}

/// Delete a profile and all its chats, memory and skills. `default` and the active one can't be.
pub fn delete(paths: &Paths, name: &str) -> Result<()> {
    if name == DEFAULT_PROFILE {
        bail!("the default profile can't be deleted");
    }
    if name == paths.profile {
        bail!("switch to another profile before deleting {name}");
    }
    if !exists(paths, name) {
        bail!("no profile named {name}");
    }
    std::fs::remove_dir_all(paths.with_profile(name).profile_dir())?;
    Ok(())
}

/// Make `name` the profile Mercury opens in.
pub fn set_active(paths: &Paths, name: &str) -> Result<()> {
    let mut shared = Config::load(&paths.config_file())?;
    shared.active_profile = (name != DEFAULT_PROFILE).then(|| name.to_string());
    shared.save(&paths.config_file())
}

/// The profile to open: an explicit choice, then `$MERCURY_PROFILE`, then the saved one.
pub fn resolve(paths: &Paths, explicit: Option<&str>) -> String {
    let wanted = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("MERCURY_PROFILE").ok().filter(|p| !p.trim().is_empty()))
        .or_else(|| Config::load(&paths.config_file()).ok().and_then(|c| c.active_profile))
        .unwrap_or_else(|| DEFAULT_PROFILE.into());
    if valid_name(&wanted) { wanted } else { DEFAULT_PROFILE.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["default", "work", "coder-2", "a_b", "9lives"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "Work", "-x", "_x", "a b", "a/b", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(sanitize("My Coder!"), "my-coder-");
        assert_eq!(sanitize("--x"), "x");
        assert_eq!(sanitize("  "), "default");
    }

    #[test]
    fn lifecycle_and_separate_chats() {
        let dir = tempfile::tempdir().unwrap();
        let root = Paths::under(dir.path().to_path_buf());
        root.ensure().unwrap();
        assert_eq!(list(&root), vec!["default"]);
        let work = create(&root, "work").unwrap();
        create(&root, "alpha").unwrap();
        assert!(create(&root, "work").is_err());
        assert!(create(&root, "Bad Name").is_err());
        assert_eq!(list(&root), vec!["default", "alpha", "work"]);

        // Chats are per profile.
        let a = crate::session::SessionStore::open(&root.sessions_db()).unwrap();
        let b = crate::session::SessionStore::open(&work.sessions_db()).unwrap();
        let s = a.create("p", "m").unwrap();
        a.append(&s.id, &crate::provider::Message::user("default-only secret")).unwrap();
        assert!(b.list(10).unwrap().is_empty());
        assert!(b.search("secret", 10).unwrap().is_empty());

        set_active(&root, "work").unwrap();
        assert_eq!(resolve(&root, None), "work");
        assert_eq!(resolve(&root, Some("alpha")), "alpha");
        assert_eq!(resolve(&root, Some("../etc")), "default");
        assert!(delete(&root.with_profile("work"), "work").is_err(), "can't delete the active profile");
        assert!(delete(&root, "default").is_err());
        delete(&root, "alpha").unwrap();
        assert_eq!(list(&root), vec!["default", "work"]);
        set_active(&root, "default").unwrap();
        assert_eq!(Config::load(&root.config_file()).unwrap().active_profile, None);
    }
}
