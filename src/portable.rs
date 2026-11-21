//! Profile export and import as `.tar.gz` (like Hermes Desktop's): chats, memory, skills, SOUL.md
//! and settings travel; API keys don't. Provider entries go along without their keys, and MCP
//! servers lose their `env` values and headers, which is where their tokens live.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Paths, ProviderConfig};

const MANIFEST: &str = "mercury-profile.json";
/// Refuse archives that unpack to more than this (a sessions.db of years of chats is far smaller).
const MAX_UNPACKED: u64 = 4 << 30;
const MAX_ENTRIES: usize = 200_000;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    name: String,
    mercury: String,
    exported_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProviderList {
    #[serde(default)]
    providers: Vec<ProviderConfig>,
}

/// Write profile `paths.profile` to `out` (a `.tar.gz`). Returns the number of files written.
pub fn export(paths: &Paths, out: &Path) -> Result<usize> {
    let cfg = Config::load_profile(paths)?;
    let staging = tempfile::tempdir()?;
    let root = staging.path();

    let mut ps = cfg.profile_settings();
    // Paths into this machine (or a linked Hermes profile) don't mean anything elsewhere: what
    // they point at is copied in below instead.
    ps.memories_dir = None;
    ps.skills_dir = None;
    ps.soul_file = None;
    ps.extra_skill_dirs.clear();
    ps.hermes = None;
    for s in ps.mcp_servers.values_mut() {
        for v in s.env.values_mut() {
            v.clear();
        }
        s.headers.clear();
    }
    std::fs::write(root.join("profile.toml"), toml::to_string_pretty(&ps)?)?;

    let providers = ProviderList {
        providers: cfg
            .providers
            .iter()
            .map(|p| {
                let mut p = p.clone();
                p.api_key = None;
                p.headers.retain(|k, _| !looks_secret(k));
                p
            })
            .collect(),
    };
    std::fs::write(root.join("providers.toml"), toml::to_string_pretty(&providers)?)?;

    let db = paths.sessions_db();
    if db.is_file() {
        // A consistent snapshot even while Mercury has the database open (WAL mode).
        let conn = rusqlite::Connection::open(&db)?;
        let target = root.join("sessions.db");
        conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()]).context("snapshotting the chat database")?;
    }
    copy_dir(&cfg.memories_dir(paths), &root.join("memories"))?;
    copy_dir(&cfg.skills_dir(paths), &root.join("skills"))?;
    let soul = cfg.soul_file(paths);
    if soul.is_file() {
        std::fs::copy(&soul, root.join("SOUL.md"))?;
    }
    let manifest = Manifest {
        format: 1,
        name: paths.profile.clone(),
        mercury: env!("CARGO_PKG_VERSION").into(),
        exported_at: chrono::Utc::now().to_rfc3339(),
    };
    std::fs::write(root.join(MANIFEST), serde_json::to_string_pretty(&manifest)?)?;

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("partial");
    let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::default()));
    tar.follow_symlinks(false);
    let mut count = 0;
    for entry in walk(root)? {
        let rel = entry.strip_prefix(root)?;
        if entry.is_dir() {
            tar.append_dir(rel, &entry)?;
        } else {
            tar.append_path_with_name(&entry, rel)?;
            count += 1;
        }
    }
    tar.into_inner()?.finish()?.sync_all()?;
    std::fs::rename(&tmp, out)?;
    Ok(count)
}

/// What [`import`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub profile: String,
    pub providers_added: Vec<String>,
    pub sessions: usize,
}

/// Unpack an exported profile as a new profile (`name`, or the name it was exported under; a
/// suffix is added if that's taken). Providers it used that aren't set up here are added, keyless.
pub fn import(root_paths: &Paths, archive: &Path, name: Option<&str>) -> Result<Imported> {
    let staging = tempfile::tempdir()?;
    unpack_safely(archive, staging.path())?;
    let dir = staging.path();
    let manifest: Manifest = serde_json::from_str(
        &std::fs::read_to_string(dir.join(MANIFEST)).map_err(|_| anyhow!("{} isn't a Mercury profile export", archive.display()))?,
    )
    .context("reading the export's manifest")?;
    if manifest.format != 1 {
        bail!("this export uses format {}, newer than this Mercury understands; update Mercury", manifest.format);
    }

    let wanted = crate::profiles::sanitize(name.unwrap_or(&manifest.name));
    let target = if crate::profiles::exists(root_paths, &wanted) {
        if name.is_some() {
            bail!("a profile named {wanted} already exists");
        }
        (2..).map(|n| format!("{wanted}-{n}")).find(|n| !crate::profiles::exists(root_paths, n)).unwrap()
    } else {
        wanted
    };
    if !crate::profiles::valid_name(&target) {
        bail!("{target} isn't a valid profile name");
    }
    let paths = crate::profiles::create(root_paths, &target)?;
    let pdir = paths.profile_dir();
    for item in ["memories", "skills"] {
        copy_dir(&dir.join(item), &pdir.join(item))?;
    }
    if dir.join("SOUL.md").is_file() {
        std::fs::copy(dir.join("SOUL.md"), pdir.join("SOUL.md"))?;
    }
    let mut sessions = 0;
    if dir.join("sessions.db").is_file() {
        // Validate before moving it in: a corrupt database would break the profile on open.
        let store = crate::session::SessionStore::open(&dir.join("sessions.db")).context("the export's chat database is unreadable")?;
        sessions = store.list(1_000_000)?.len();
        drop(store);
        std::fs::copy(dir.join("sessions.db"), paths.sessions_db())?;
    }
    if dir.join("profile.toml").is_file() {
        std::fs::copy(dir.join("profile.toml"), paths.profile_file())?;
    }

    // Providers: add the ones this install doesn't have (keys must be entered again).
    let mut shared = Config::load(&root_paths.config_file())?;
    let mut providers_added = vec![];
    if let Ok(t) = std::fs::read_to_string(dir.join("providers.toml")) {
        let list: ProviderList = toml::from_str(&t).context("reading the export's providers")?;
        for p in list.providers {
            if shared.provider(&p.id).is_none() {
                providers_added.push(p.name.clone());
                shared.providers.push(p);
            }
        }
        if !providers_added.is_empty() {
            shared.save(&root_paths.config_file())?;
        }
    }
    // Re-save through the normal path so the profile file is in this version's shape.
    let cfg = Config::load_profile(&paths)?;
    cfg.save_profile(&paths)?;
    Ok(Imported { profile: target, providers_added, sessions })
}

fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["key", "token", "secret", "auth", "password", "cookie"].iter().any(|s| n.contains(s))
}

/// Unpack regular files and directories only: no symlinks, hard links or devices, no absolute
/// paths or `..`, bounded size. Archives come from other people.
fn unpack_safely(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut total = 0u64;
    let mut n = 0usize;
    for entry in tar.entries().context("not a .tar.gz file")? {
        let mut entry = entry.context("the archive is damaged")?;
        n += 1;
        if n > MAX_ENTRIES {
            bail!("the archive has too many files");
        }
        let path = entry.path()?.into_owned();
        if path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
            bail!("the archive contains an unsafe path: {}", path.display());
        }
        let out = dest.join(&path);
        match entry.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&out)?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                total += entry.header().size()?;
                if total > MAX_UNPACKED {
                    bail!("the archive is too large");
                }
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut buf = Vec::new();
                entry.by_ref().take(MAX_UNPACKED).read_to_end(&mut buf)?;
                std::fs::write(&out, buf)?;
            }
            // Links, devices, FIFOs, pax headers handled by the tar crate: skip anything else.
            tar::EntryType::XHeader | tar::EntryType::XGlobalHeader | tar::EntryType::GNULongName => {}
            other => bail!("the archive contains a {other:?} entry ({}), which isn't allowed", path.display()),
        }
    }
    Ok(())
}

fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let meta = std::fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                out.push(p.clone());
                stack.push(p);
            } else if meta.is_file() {
                out.push(p);
            }
        }
    }
    Ok(out)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in walk(from)? {
        let rel = entry.strip_prefix(from)?;
        let dest = to.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest)?;
        } else {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&entry, &dest)?;
        }
    }
    Ok(())
}

/// A sensible default file name: `mercury-<profile>-<date>.tar.gz`.
pub fn default_file_name(profile: &str) -> String {
    format!("mercury-{profile}-{}.tar.gz", chrono::Local::now().format("%Y-%m-%d"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_import_roundtrip_strips_keys() {
        let d = tempfile::tempdir().unwrap();
        let root = Paths::under(d.path().join("a"));
        root.ensure().unwrap();
        let mut shared = Config::default();
        let mut p = ProviderConfig::openai("deepinfra", "DeepInfra", "https://api.deepinfra.com/v1/openai");
        p.api_key = Some("sk-secret".into());
        p.headers.insert("X-Api-Key".into(), "also-secret".into());
        p.headers.insert("X-Title".into(), "Mercury".into());
        shared.providers.push(p);
        shared.save(&root.config_file()).unwrap();
        let work = crate::profiles::create(&root, "work").unwrap();
        let mut cfg = Config::load_profile(&work).unwrap();
        cfg.mcp_servers.insert(
            "gh".into(),
            crate::config::McpServerConfig {
                command: Some("gh-mcp".into()),
                env: [("GITHUB_TOKEN".to_string(), "ghp_x".to_string())].into(),
                ..Default::default()
            },
        );
        cfg.save_profile(&work).unwrap();
        let store = crate::session::SessionStore::open(&work.sessions_db()).unwrap();
        let s = store.create("deepinfra", "m").unwrap();
        store.append(&s.id, &crate::provider::Message::user("remember the pelican")).unwrap();
        std::fs::create_dir_all(cfg.memories_dir(&work)).unwrap();
        std::fs::write(cfg.memories_dir(&work).join("MEMORY.md"), "likes pelicans").unwrap();
        std::fs::write(cfg.soul_file(&work), "be brief").unwrap();

        let out = d.path().join("out/work.tar.gz");
        let n = export(&work, &out).unwrap();
        assert!(n >= 5, "{n}");
        let raw = {
            let mut s = String::new();
            let mut t = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(&out).unwrap()));
            for e in t.entries().unwrap() {
                let mut e = e.unwrap();
                let mut b = vec![];
                e.read_to_end(&mut b).unwrap();
                s.push_str(&String::from_utf8_lossy(&b));
            }
            s
        };
        for secret in ["sk-secret", "also-secret", "ghp_x"] {
            assert!(!raw.contains(secret), "{secret} leaked into the export");
        }

        // Import into a different install.
        let other = Paths::under(d.path().join("b"));
        other.ensure().unwrap();
        let r = import(&other, &out, None).unwrap();
        assert_eq!(r.profile, "work");
        assert_eq!(r.sessions, 1);
        assert_eq!(r.providers_added, vec!["DeepInfra"]);
        let wp = other.with_profile("work");
        let got = Config::load_profile(&wp).unwrap();
        assert_eq!(got.provider("deepinfra").unwrap().api_key, None);
        assert_eq!(got.provider("deepinfra").unwrap().headers.get("X-Title").map(String::as_str), Some("Mercury"));
        assert_eq!(got.mcp_servers["gh"].env["GITHUB_TOKEN"], "");
        let st = crate::session::SessionStore::open(&wp.sessions_db()).unwrap();
        assert_eq!(st.search("pelican", 5).unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(got.memories_dir(&wp).join("MEMORY.md")).unwrap(), "likes pelicans");
        assert_eq!(std::fs::read_to_string(got.soul_file(&wp)).unwrap(), "be brief");
        // Importing again doesn't clobber it.
        assert_eq!(import(&other, &out, None).unwrap().profile, "work-2");
        assert!(import(&other, &out, Some("work")).is_err());
    }

    #[test]
    fn hostile_archives_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let make = |name: &str, f: &dyn Fn(&mut tar::Builder<flate2::write::GzEncoder<std::fs::File>>)| {
            let path = d.path().join(name);
            let mut b =
                tar::Builder::new(flate2::write::GzEncoder::new(std::fs::File::create(&path).unwrap(), flate2::Compression::fast()));
            f(&mut b);
            b.into_inner().unwrap().finish().unwrap();
            path
        };
        let link = make("link.tar.gz", &|b| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, "memories", "/etc").unwrap();
        });
        let dest = tempfile::tempdir().unwrap();
        assert!(unpack_safely(&link, dest.path()).unwrap_err().to_string().contains("isn't allowed"));
        // `tar::Builder` refuses to write `..` paths itself, so write the raw header by hand.
        let dotdot = d.path().join("dotdot.tar.gz");
        {
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..9].copy_from_slice(b"../escape");
            h.set_size(1);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&dotdot).unwrap(), flate2::Compression::fast());
            use std::io::Write;
            gz.write_all(h.as_bytes()).unwrap();
            gz.write_all(&[b'x'; 512]).unwrap();
            gz.write_all(&[0u8; 1024]).unwrap();
            gz.finish().unwrap();
        }
        assert!(unpack_safely(&dotdot, dest.path()).unwrap_err().to_string().contains("unsafe path"));
        assert!(!dest.path().parent().unwrap().join("escape").exists());
        let root = Paths::under(d.path().join("r"));
        root.ensure().unwrap();
        let junk = d.path().join("junk.tar.gz");
        std::fs::write(&junk, b"not an archive").unwrap();
        assert!(import(&root, &junk, None).is_err());
        let empty = make("empty.tar.gz", &|_| {});
        assert!(import(&root, &empty, None).unwrap_err().to_string().contains("isn't a Mercury profile export"));
    }
}
