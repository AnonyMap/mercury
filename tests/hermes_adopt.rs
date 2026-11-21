//! Picking up from an existing Hermes install: profile discovery, copy vs in-place adoption,
//! and importing conversation history from Hermes's state.db.

mod common;

use std::path::Path;

use common::*;
use mercury::agent::Agent;
use mercury::config::{Config, LinkMode, Paths};
use mercury::hermes_import::{self, HermesProfile};
use mercury::provider::Role;

fn digest(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[test]
fn finds_profiles_from_any_entry_point() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join(".hermes");
    fake_hermes(&root);
    let names = |p: &Path| hermes_import::list_profiles(p).into_iter().map(|p| p.name).collect::<Vec<_>>();
    assert_eq!(names(&root), vec!["default", "coder"]);
    assert_eq!(names(&root.join("profiles")), vec!["default", "coder"]);
    assert_eq!(names(&root.join("profiles/coder")), vec!["coder"]);
    assert!(names(&d.path().join("nowhere")).is_empty());

    let sum = hermes_import::summarize(&HermesProfile { name: "default".into(), home: root.clone() });
    assert_eq!(sum.model.as_deref(), Some("qwen3:32b"));
    assert_eq!((sum.memory_entries, sum.user_entries, sum.skills, sum.mcp_servers), (2, 2, 1, 3));
    assert_eq!(sum.sessions, 3, "hidden sessions don't count");
    assert!(sum.has_soul && sum.nous_oauth && !sum.nous_api_key);
}

#[test]
fn copy_mode_brings_everything_and_is_idempotent() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join(".hermes");
    fake_hermes(&root);
    let db_before = digest(&root.join("state.db"));
    let paths = Paths::under(d.path().join("mercury"));
    paths.ensure().unwrap();
    // Mercury already remembers one thing; the import merges rather than overwrites.
    mercury::memory::MemoryStore::new(paths.default_memories_dir()).add(mercury::memory::Target::User, "Uses Fedora").unwrap();
    let mut cfg = Config::default();
    let profile = HermesProfile { name: "default".into(), home: root.clone() };
    let r = hermes_import::adopt(&profile, LinkMode::Copy, true, &paths, &mut cfg).unwrap();

    assert_eq!(cfg.active().unwrap().base_url, "http://192.168.1.50:11434/v1");
    assert_eq!(cfg.active().unwrap().api_key.as_deref(), Some("sk-local"));
    assert_eq!(cfg.active_model_name().as_deref(), Some("qwen3:32b"));
    assert_eq!(cfg.mcp_servers.keys().collect::<Vec<_>>(), vec!["notion", "time"], "entries without command or url are dropped");
    assert_eq!(cfg.mcp_servers["time"].args, vec!["mcp-server-time"]);
    assert_eq!(cfg.mcp_servers["notion"].headers["Authorization"], "Bearer x");
    assert_eq!(cfg.custom_personalities["reviewer"], "You are a meticulous code reviewer. Tone: direct.");
    assert_eq!(cfg.agent.personality, "mentor");
    let users = mercury::memory::MemoryStore::new(paths.default_memories_dir()).entries(mercury::memory::Target::User);
    assert_eq!(users, vec!["Uses Fedora", "Name is Aliah", "Prefers to be challenged"]);
    assert!(r.soul_copied);
    assert_eq!(std::fs::read_to_string(paths.data_dir.join("SOUL.md")).unwrap(), "You are Hermes, calm and precise.");
    assert_eq!(cfg.extra_skill_dirs, vec![root.join("skills")]);
    assert_eq!(r.sessions_imported, 2);
    assert!(r.notes.iter().any(|n| n.contains("single-use")), "{:?}", r.notes);
    let link = cfg.hermes.clone().unwrap();
    assert_eq!((link.profile.as_str(), link.mode, link.sessions_imported), ("default", LinkMode::Copy, 2));

    // Imported transcripts are valid to continue.
    let store = mercury::session::SessionStore::open(&paths.sessions_db()).unwrap();
    let list = store.list(10).unwrap();
    let fix = list.iter().find(|s| s.title == "Fix the flaky test").unwrap();
    let msgs = store.messages(&fix.id).unwrap();
    assert_eq!(msgs.iter().map(|m| m.role).collect::<Vec<_>>(), vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
    assert_eq!(msgs[1].tool_calls[0].id, "call_a");
    assert_eq!(msgs[1].reasoning.as_deref(), Some("check the test first"));
    assert!(msgs[1].content.is_none());
    let tg = list.iter().find(|s| s.title.starts_with("[telegram]")).unwrap();
    assert_eq!(tg.title, "[telegram] remind me about the dentist");
    let tmsgs = store.messages(&tg.id).unwrap();
    assert_eq!(tmsgs[1].tool_calls[0].function.arguments, r#"{"action":"add"}"#);
    assert_eq!(tmsgs[2].tool_call_id.as_deref(), Some("call_b"), "dangling call gets a placeholder result");
    assert!(!tmsgs.iter().any(|m| m.text().contains("orphan")));
    assert!(store.search("flaky", 10).unwrap().iter().any(|h| h.session_id == fix.id), "imported history is searchable");

    // Again: nothing duplicated.
    let r2 = hermes_import::adopt(&profile, LinkMode::Copy, true, &paths, &mut cfg).unwrap();
    assert_eq!(r2.sessions_imported, 0);
    assert_eq!(store.list(10).unwrap().len(), 2);
    assert_eq!(mercury::memory::MemoryStore::new(paths.default_memories_dir()).entries(mercury::memory::Target::User).len(), 3);
    assert_eq!(cfg.extra_skill_dirs.len(), 1);
    assert_eq!(digest(&root.join("state.db")), db_before, "Hermes's database is never written to");
}

#[test]
fn in_place_mode_shares_memory_skills_and_soul() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join(".hermes");
    fake_hermes(&root);
    let paths = Paths::under(d.path().join("mercury"));
    let mut cfg = Config::default();
    let profile = HermesProfile { name: "default".into(), home: root.clone() };
    hermes_import::adopt(&profile, LinkMode::InPlace, false, &paths, &mut cfg).unwrap();
    assert_eq!(cfg.memories_dir(&paths), root.join("memories"));
    assert_eq!(cfg.skills_dir(&paths), root.join("skills"));
    assert_eq!(cfg.hermes.as_ref().unwrap().sessions_imported, 0);

    let agent = Agent::new(paths.clone(), cfg.clone(), None).unwrap();
    assert!(agent.system_prompt().contains("You are Hermes, calm and precise."));
    assert!(agent.system_prompt().contains("Prefers to be challenged"));
    assert!(agent.system_prompt().contains("You are a supportive mentor."), "Hermes's active personality");
    // Writes land in the Hermes profile itself.
    agent.tools.memory.add(mercury::memory::Target::Memory, "Learned in Mercury").unwrap();
    assert!(std::fs::read_to_string(root.join("memories/MEMORY.md")).unwrap().ends_with("Learned in Mercury"));
    agent.tools.skills.create("deploy", "Deploy it", "steps").unwrap();
    assert!(root.join("skills/deploy/SKILL.md").exists());
    assert!(agent.tools.skills.get("backup-db").unwrap().writable);

    // Switching that same profile to copy mode undoes the in-place links.
    hermes_import::adopt(&profile, LinkMode::Copy, false, &paths, &mut cfg).unwrap();
    assert_eq!(cfg.memories_dir(&paths), paths.default_memories_dir());
    assert_eq!(cfg.skills_dir(&paths), paths.default_skills_dir());
}

#[test]
fn nous_api_key_profile() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join(".hermes");
    fake_hermes(&root);
    let paths = Paths::under(d.path().join("mercury"));
    let mut cfg = Config::default();
    let coder = hermes_import::list_profiles(&root).into_iter().find(|p| p.name == "coder").unwrap();
    hermes_import::adopt(&coder, LinkMode::Copy, true, &paths, &mut cfg).unwrap();
    let p = cfg.active().unwrap();
    assert_eq!(p.id, "nous-api");
    assert_eq!(p.api_key.as_deref(), Some("sk-nous"));
    assert_eq!(cfg.active_model_name().as_deref(), Some("Hermes-4-70B"));
}

#[tokio::test]
async fn continue_an_imported_hermes_conversation() {
    let mock = start().await;
    mock.push_chat(text("Picking up where we left off."));
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join(".hermes");
    fake_hermes(&root);
    let (h, mut cfg) = harness(&mock.base());
    let profile = HermesProfile { name: "default".into(), home: root.clone() };
    hermes_import::adopt(&profile, LinkMode::Copy, true, &h.paths, &mut cfg).unwrap();
    // Keep the mock as the active provider for this test.
    cfg.set_active("mock", Some("test-model".into()));
    let store = mercury::session::SessionStore::open(&h.paths.sessions_db()).unwrap();
    let id = store.list(10).unwrap().into_iter().find(|s| s.title == "Fix the flaky test").unwrap().id;
    let agent = Agent::new(h.paths.clone(), cfg, Some(&id)).unwrap();
    let (agent, out) = turn(agent, "Did that fix hold?", true, None).await;
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let sent = mock.requests()[0]["messages"].as_array().unwrap().clone();
    // system + 4 imported + new user message
    assert_eq!(sent.len(), 6);
    assert_eq!(sent[2]["tool_calls"][0]["id"], "call_a");
    assert_eq!(sent[3]["tool_call_id"], "call_a");
    assert_eq!(agent.messages.len(), 6);
}
