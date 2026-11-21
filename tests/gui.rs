//! Headless GUI tests: the real MercuryApp driven through egui_kittest (AccessKit queries).
#![cfg(feature = "gui")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use mercury::config::{ApprovalMode, Config, Paths};
use mercury::gui::chat::ChatItem;
use mercury::gui::{MercuryApp, Tab, View};
use serde_json::json;

fn rt() -> Arc<tokio::runtime::Runtime> {
    Arc::new(tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap())
}

fn harness(app: MercuryApp) -> Harness<'static, MercuryApp> {
    Harness::builder()
        .with_size(egui::vec2(1200.0, 820.0))
        .with_max_steps(60)
        .build_ui_state(|ui, app: &mut MercuryApp| app.ui_root(ui), app)
}

fn wait(h: &mut Harness<'static, MercuryApp>, what: &str, cond: impl Fn(&MercuryApp) -> bool) {
    for _ in 0..600 {
        h.step();
        if cond(h.state()) {
            h.run_steps(3);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for: {what}; busy={} items={:#?}", h.state().busy, h.state().items);
}

fn app_with_mock(rt: &Arc<tokio::runtime::Runtime>, base: &str) -> (tempfile::TempDir, MercuryApp) {
    let (h, cfg) = common::harness(base);
    let app = MercuryApp::new(h.paths.clone(), cfg, rt.clone()).unwrap();
    (h.dir, app)
}

#[test]
fn onboarding_when_nothing_is_configured() {
    let rt = rt();
    let dir = tempfile::tempdir().unwrap();
    let app = MercuryApp::new(Paths::under(dir.path().to_path_buf()), Config::default(), rt.clone()).unwrap();
    let mut h = harness(app);
    h.run_steps(4);
    h.get_by_label("Welcome to Mercury");
    h.get_by_label("Find local models");
    h.get_by_label("Sign in to Nous Portal");
    h.get_by_label("Add an endpoint…").click();
    h.run_steps(3);
    assert_eq!(h.state().view, View::Settings(Tab::Providers));
    h.get_by_label("Add an OpenAI-compatible endpoint");
}

#[test]
fn add_endpoint_from_settings() {
    let rt = rt();
    let mock = rt.block_on(start());
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path().to_path_buf());
    let mut app = MercuryApp::new(paths.clone(), Config::default(), rt.clone()).unwrap();
    app.view = View::Settings(Tab::Providers);
    app.draft.name = "GPU box".into();
    app.draft.url = mock.base();
    app.draft.model = "test-model".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Add endpoint").click();
    wait(&mut h, "models listed", |a| a.models.get("gpu-box").map(|m| m.len() == 2).unwrap_or(false));
    let saved = Config::load(&paths.config_file()).unwrap();
    assert_eq!(saved.active_provider.as_deref(), Some("gpu-box"));
    assert_eq!(saved.active_model_name().as_deref(), Some("test-model"));
}

#[test]
fn send_message_streams_reply_into_view() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("Hello from the mock model."));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.input = "Say hi".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| {
        !a.busy && a.items.iter().any(|i| matches!(i, ChatItem::Assistant { text, streaming: false, .. } if text.contains("Hello from")))
    });
    h.run_steps(5);
    h.get_by_label_contains("Hello from the mock model.");
    // The session appears in the sidebar under its auto title.
    assert_eq!(h.get_all_by_label("Say hi").count(), 2, "user bubble + sidebar entry");
    assert_eq!(h.state().usage.total_tokens, 15);
    assert!(h.state().input.is_empty());
}

#[test]
fn approval_banner_deny_keeps_files() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(tool_call("c1", "terminal", json!({"command": "rm -rf keepme"})));
    mock.push_chat(text("Understood, leaving it."));
    let (dir, mut app) = app_with_mock(&rt, &mock.base());
    let target = dir.path().join("work/keepme");
    std::fs::create_dir_all(&target).unwrap();
    app.input = "clean up".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "approval banner", |a| a.approval.is_some());
    h.get_by_label_contains("Approve this command?");
    h.get_by_label("Deny").click();
    wait(&mut h, "turn end", |a| !a.busy);
    assert!(target.exists());
    let tool = h.state().items.iter().find_map(|i| match i {
        ChatItem::Tool { output: Some(o), is_error, .. } => Some((o.clone(), *is_error)),
        _ => None,
    });
    let (out, err) = tool.unwrap();
    assert!(err && out.contains("denied"));
}

#[test]
fn approval_banner_run_executes() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(tool_call("c1", "terminal", json!({"command": "rm -rf scratch && echo gone"})));
    mock.push_chat(text("Removed."));
    let (dir, mut app) = app_with_mock(&rt, &mock.base());
    let target = dir.path().join("work/scratch");
    std::fs::create_dir_all(&target).unwrap();
    app.input = "clean up".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "approval banner", |a| a.approval.is_some());
    h.get_by_label("Run it").click();
    wait(&mut h, "turn end", |a| !a.busy);
    assert!(!target.exists());
}

#[test]
fn settings_tabs_and_yolo_toggle_persist() {
    let rt = rt();
    let mock = rt.block_on(start());
    let (_d, app) = app_with_mock(&rt, &mock.base());
    let cfg_path = app.paths.config_file();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("⚙ Settings").click();
    h.run_steps(3);
    for tab in ["Nous Portal", "Memory", "Skills", "General", "Tools & safety"] {
        h.get_by_label(tab).click();
        h.run_steps(3);
    }
    assert_eq!(h.state().view, View::Settings(Tab::Tools));
    // Below the fold at this window size: click it the way a screen reader would.
    h.get_by_label("Run without asking (YOLO)").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().config.approvals, ApprovalMode::Yolo, "radio click");
    assert!(h.state().config_dirty);
    h.get_by_label("Save changes").click_accesskit();
    h.run_steps(5);
    assert_eq!(Config::load(&cfg_path).unwrap().approvals, ApprovalMode::Yolo);
    h.get_by_label("Back to chat").click();
    h.run_steps(3);
    assert_eq!(h.state().view, View::Chat);
}

#[test]
fn slash_commands_from_the_composer() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("first answer"));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.input = "first question".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| !a.busy && a.items.len() >= 2);

    h.state_mut().input = "/help".into();
    h.run_steps(2);
    h.get_by_label("Send").click();
    h.run_steps(3);
    assert!(matches!(h.state().items.last(), Some(ChatItem::Info(t)) if t.contains("/compress")));

    let old = h.state().session.id.clone();
    h.state_mut().input = "/new".into();
    h.run_steps(2);
    h.get_by_label("Send").click();
    wait(&mut h, "new session", |a| a.session.id != old && !a.busy);
    assert!(h.state().items.is_empty());
    // The finished chat is still in the sidebar; clicking it reloads the transcript.
    h.get_by_label("first question").click();
    wait(&mut h, "reloaded", |a| a.session.id == old);
    assert!(h.state().items.iter().any(|i| matches!(i, ChatItem::Assistant { text, .. } if text == "first answer")));
}

#[test]
fn unreachable_provider_shows_error_card() {
    let rt = rt();
    let (_d, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    app.input = "hello?".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "error", |a| !a.busy && a.items.iter().any(|i| matches!(i, ChatItem::Error(_))));
    h.run_steps(3);
    h.get_by_label_contains("Can't reach");
}

#[test]
fn every_item_kind_renders_in_both_themes() {
    // egui_kittest panics on any glyph the bundled fonts can't draw, so this guards every
    // icon and status string Mercury itself puts on screen.
    let rt = rt();
    for dark in [true, false] {
        let (_d, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
        app.config.ui.dark = dark;
        app.items = vec![
            ChatItem::User("hello".into()),
            ChatItem::Assistant {
                text: "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```".into(),
                reasoning: "thinking".into(),
                streaming: false,
            },
            ChatItem::Assistant { text: "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |".into(), reasoning: String::new(), streaming: false },
            ChatItem::Tool {
                id: "1".into(),
                name: "terminal".into(),
                summary: "$ ls".into(),
                args: "{}".into(),
                output: None,
                is_error: false,
            },
            ChatItem::Tool {
                id: "2".into(),
                name: "terminal".into(),
                summary: "$ ls".into(),
                args: "{}".into(),
                output: Some("ok".into()),
                is_error: false,
            },
            ChatItem::Tool {
                id: "3".into(),
                name: "terminal".into(),
                summary: "$ rm".into(),
                args: "{}".into(),
                output: Some("Error: denied".into()),
                is_error: true,
            },
            ChatItem::Notice("Stopped.".into()),
            ChatItem::Error("Can't reach http://127.0.0.1:9/v1".into()),
            ChatItem::Info(mercury::commands::help_text()),
            ChatItem::Assistant { text: String::new(), reasoning: String::new(), streaming: true },
        ];
        app.config.approvals = ApprovalMode::Yolo;
        let mut h = harness(app);
        h.run_steps(6);
        h.get_by_label_contains("YOLO");
        assert_eq!(h.get_all_by_label("Title").count(), 2, "duplicate markdown renders without id clashes");
    }
}

#[test]
fn text_size_controls_and_shortcuts() {
    let rt = rt();
    let (_d, app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    let cfg_path = app.paths.config_file();
    let mut h = harness(app);
    h.run_steps(3);
    assert_eq!(h.state().scale(), 1.0);
    // Zoom re-lays out the window, so click through AccessKit rather than stale pixel positions.
    h.get_by_label("A+").click_accesskit();
    h.run_steps(2);
    h.get_by_label("A+").click_accesskit();
    h.run_steps(2);
    assert!((h.state().scale() - 1.2).abs() < 1e-4);
    h.get_by_label("120%");
    assert!((Config::load(&cfg_path).unwrap().ui.font_scale - 1.2).abs() < 1e-4, "persisted");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Minus);
    h.run_steps(2);
    assert!((h.state().scale() - 1.1).abs() < 1e-4);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Equals);
    h.run_steps(2);
    assert!((h.state().scale() - 1.2).abs() < 1e-4);
    h.get_by_label("120%").click_accesskit();
    h.run_steps(2);
    assert_eq!(h.state().scale(), 1.0);
    for _ in 0..30 {
        let s = h.state().scale();
        h.state_mut().set_scale(s + 0.1);
    }
    assert_eq!(h.state().scale(), mercury::config::MAX_SCALE, "clamped");
    h.run_steps(3);
}

#[test]
fn custom_logo_from_settings() {
    let rt = rt();
    let (dir, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    let cfg_path = app.paths.config_file();
    let img = dir.path().join("my-logo.png");
    image::RgbaImage::from_pixel(30, 30, image::Rgba([10, 200, 120, 255])).save(&img).unwrap();
    app.view = View::Settings(Tab::General);
    let mut h = harness(app);
    h.run_steps(3);
    assert!(h.query_by_label("Custom image").is_none(), "no custom option until an image is chosen");
    h.state_mut().logo_path_draft = img.display().to_string();
    h.run_steps(2);
    h.get_by_label("Use this image").click_accesskit();
    h.run_steps(3);
    let saved = Config::load(&cfg_path).unwrap();
    assert_eq!(saved.ui.logo, mercury::config::Logo::Custom);
    let copy = saved.ui.custom_logo.unwrap();
    assert!(copy.starts_with(&h.state().paths.data_dir) && copy.exists(), "Mercury keeps its own copy");
    // Switch back to the orb and back again with the radios.
    h.get_by_label("Orb").click_accesskit();
    h.run_steps(2);
    assert_eq!(h.state().config.ui.logo, mercury::config::Logo::Orb);
    h.get_by_label("Custom image").click_accesskit();
    h.run_steps(2);
    assert_eq!(h.state().config.ui.logo, mercury::config::Logo::Custom);
    // A bad file leaves the logo alone.
    std::fs::write(dir.path().join("x.txt"), "not an image").unwrap();
    h.state_mut().logo_path_draft = dir.path().join("x.txt").display().to_string();
    h.run_steps(2);
    h.get_by_label("Use this image").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().config.ui.custom_logo.as_deref(), Some(copy.as_path()));
    h.get_by_label("Back to chat").click();
    h.run_steps(3);
}

#[test]
fn clarify_banner_answers_the_model() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(tool_call("q1", "clarify", json!({"question": "Which database?", "choices": ["Postgres", "SQLite"]})));
    mock.push_chat(text("Going with SQLite."));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.input = "set up storage".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "question", |a| a.clarify.is_some());
    h.get_by_label("Which database?");
    h.get_by_label("SQLite").click();
    wait(&mut h, "turn end", |a| !a.busy);
    assert_eq!(mock.requests()[1]["messages"][3]["content"], "The user answered: SQLite");
}

#[test]
fn pick_up_hermes_profiles_from_dropdowns_into_separate_profiles() {
    let rt = rt();
    let (dir, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    let root = dir.path().join(".hermes");
    fake_hermes(&root);
    // Give the `coder` profile its own history so we can tell the profiles' chats apart.
    {
        let c = rusqlite::Connection::open(root.join("profiles/coder/state.db")).unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, model TEXT, started_at REAL, title TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, timestamp REAL);
             INSERT INTO sessions VALUES ('c1','cli','m',1700000000,'Refactor the parser');
             INSERT INTO messages(session_id,role,content,timestamp) VALUES ('c1','user','split the lexer out',0);",
        )
        .unwrap();
    }
    app.hermes_path = root.display().to_string();
    app.view = View::Settings(Tab::Hermes);
    let mut h = harness(app);
    h.run_steps(3);
    // Profiles are listed without pressing anything.
    assert_eq!(h.state().hermes_profiles.len(), 2);
    h.get_by_label_contains("qwen3:32b via custom · 3 conversations · memory 2+2 entries · 1 skill · 3 MCP servers");

    // Import Hermes `default` → Mercury `default`.
    h.get_by_label("Import “default” into profile “default”").click_accesskit();
    wait(&mut h, "import default", |a| !a.hermes_busy && a.hermes_result.is_some());
    assert!(matches!(&h.state().hermes_result, Some(Ok(m)) if m.contains("2 conversations")), "{:?}", h.state().hermes_result);
    assert_eq!(h.state().paths.profile, "default");
    assert!(h.state().sessions.iter().any(|s| s.title == "Fix the flaky test"));

    // Choose `coder` in the dropdown and import it: it becomes its own Mercury profile.
    h.state_mut().hermes_selected = 1;
    h.run_steps(2);
    h.get_by_label("Import “coder” into profile “coder”").click_accesskit();
    wait(&mut h, "import coder", |a| !a.hermes_busy && a.paths.profile == "coder");
    let titles: Vec<String> = h.state().sessions.iter().map(|s| s.title.clone()).collect();
    assert_eq!(titles, vec!["Refactor the parser"], "only coder's chats are shown");
    assert_eq!(h.state().config.active_model_name().as_deref(), Some("Hermes-4-70B"));
    assert_eq!(h.state().profiles, vec!["default", "coder"]);

    // Back to default from the sidebar dropdown: its chats are back, coder's are not.
    h.state_mut().switch_profile("default", false);
    h.run_steps(3);
    let titles: Vec<String> = h.state().sessions.iter().map(|s| s.title.clone()).collect();
    assert!(titles.contains(&"Fix the flaky test".to_string()) && !titles.contains(&"Refactor the parser".to_string()), "{titles:?}");
    assert_eq!(h.state().config.active_model_name().as_deref(), Some("qwen3:32b"), "each profile keeps its model");
    assert_eq!(Config::load(&h.state().paths.config_file()).unwrap().active_profile, None, "default is remembered");
}

#[test]
fn chats_are_separate_per_profile() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("work answer"));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.new_profile = "work".into();
    app.create_profile();
    assert_eq!(app.paths.profile, "work");
    app.input = "work question".into();
    let mut h = harness(app);
    h.run_steps(3);
    // The sidebar dropdown shows the open profile.
    h.get_by_label("Choose profile");
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| !a.busy && a.sessions.len() == 1);
    h.state_mut().switch_profile("default", false);
    h.run_steps(3);
    assert!(h.state().sessions.is_empty(), "default has no chats");
    assert!(h.state().store_search("work question").is_empty(), "search is per profile too");
    h.state_mut().switch_profile("work", false);
    h.run_steps(3);
    assert_eq!(h.state().sessions[0].title, "work question");
    // Providers are shared: label one in `work`, see it in `default`.
    h.state_mut().config.providers[0].name = "Basement GPU".into();
    h.state_mut().apply_config();
    h.state_mut().switch_profile("default", false);
    h.run_steps(3);
    assert_eq!(h.state().config.providers[0].name, "Basement GPU");
}

#[test]
fn provider_labels_are_editable() {
    let rt = rt();
    let mock = rt.block_on(start());
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    let mut second = mercury::config::ProviderConfig::openai("ollama-2", "Ollama (this machine)", "http://192.168.1.40:11434/v1");
    second.default_model = "llama3".into();
    app.config.upsert_provider(second);
    app.view = View::Settings(Tab::Providers);
    let cfg_path = app.paths.config_file();
    let mut h = harness(app);
    h.run_steps(3);
    assert_eq!(h.get_all_by_label("Provider label").count(), 2, "every provider has a label field");
    h.state_mut().config.providers[1].name = "Living-room Ollama".into();
    h.state_mut().config_dirty = true;
    h.run_steps(2);
    h.get_by_label("Save changes").click_accesskit();
    h.run_steps(3);
    assert_eq!(Config::load(&cfg_path).unwrap().providers[1].name, "Living-room Ollama");
    h.get_by_label("Back to chat").click();
    h.run_steps(3);
}

#[test]
fn built_in_browser_picks_the_hermes_folder() {
    let rt = rt();
    let (dir, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    fake_hermes(&dir.path().join(".hermes"));
    app.view = View::Settings(Tab::Hermes);
    app.browser = Some(mercury::gui::browse::Browser::new(mercury::gui::browse::PickPurpose::HermesFolder, dir.path().to_path_buf()));
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Choose your Hermes folder");
    h.get_by_label("🗀  .hermes").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().browser.as_ref().unwrap().dir, dir.path().join(".hermes"));
    h.get_by_label("Use this folder").click_accesskit();
    h.run_steps(3);
    assert!(h.state().browser.is_none());
    assert_eq!(h.state().hermes_path, dir.path().join(".hermes").display().to_string());
    assert_eq!(h.state().hermes_profiles.len(), 2, "picking the folder lists its profiles");
}

#[test]
fn pasted_paths_are_accepted() {
    // Paste arrives as an Event::Paste from the platform clipboard; the fields must take it,
    // including paths with quotes or a leading ~ as file managers copy them.
    let rt = rt();
    let (dir, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    let root = dir.path().join(".hermes");
    fake_hermes(&root);
    app.view = View::Settings(Tab::Hermes);
    app.hermes_path.clear();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Hermes folder path").click();
    h.run_steps(1);
    h.event(egui::Event::Paste(format!("'{}'", root.display())));
    h.run_steps(2);
    assert!(!h.state().hermes_path.is_empty(), "the paste landed in the field");
    h.get_by_label("Find profiles").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().hermes_profiles.len(), 2, "quoted paste still resolves");
    assert_eq!(h.state().hermes_path, root.display().to_string(), "the field shows the cleaned path");
    // Dragging a folder from a file manager gives a file:// URI.
    h.state_mut().hermes_path = format!("file://{}", root.join("profiles").join("coder").display());
    h.get_by_label("Find profiles").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().hermes_profiles.len(), 1);
    assert_eq!(h.state().hermes_profiles[0].0.name, "coder");
}

#[test]
fn deleting_a_profile_asks_first() {
    let rt = rt();
    let (_d, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    mercury::profiles::create(&app.paths, "scratch").unwrap();
    app.refresh_profiles();
    app.view = View::Settings(Tab::Hermes);
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Delete scratch").click_accesskit();
    h.run_steps(2);
    assert!(h.state().profiles.contains(&"scratch".to_string()), "one click doesn't delete");
    h.get_by_label("Keep").click_accesskit();
    h.run_steps(2);
    h.get_by_label("Delete scratch").click_accesskit();
    h.run_steps(2);
    h.get_by_label_contains("its 0 chats");
    h.get_by_label("Yes, delete scratch").click_accesskit();
    h.run_steps(2);
    assert_eq!(h.state().profiles, vec!["default"]);
}

// ---- v0.4 ------------------------------------------------------------------------------------

#[test]
fn command_palette_reaches_settings_models_and_chats() {
    let rt = rt();
    let mock = rt.block_on(start());
    *mock.state.models.lock().unwrap() = vec!["test-model".into(), "deepseek-v4".into()];
    let (_d, app) = app_with_mock(&rt, &mock.base());
    let mut h = harness(app);
    wait(&mut h, "model list", |a| a.models.get("mock").is_some_and(|m| m.len() == 2));
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    h.run_steps(3);
    assert!(h.state().palette.is_some());
    h.get_by_label("Command palette search").type_text("tools safety");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().view, View::Settings(Tab::Tools));
    assert!(h.state().palette.is_none());

    h.get_by_label("Commands").click_accesskit();
    h.run_steps(3);
    h.get_by_label("Command palette search").type_text("deepseek");
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().config.active_model_name().as_deref(), Some("deepseek-v4"));

    // Esc closes it without doing anything.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::P);
    h.run_steps(2);
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().palette.is_none());
}

#[test]
fn sidebar_toggle_find_and_history() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("Pelicans are large water birds."));
    mock.push_chat(text("Yes, pelicans can fly."));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.input = "Tell me about pelicans".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "first reply", |a| !a.busy && a.items.len() >= 2);
    h.state_mut().input = "Can they fly?".into();
    h.run_steps(1);
    h.get_by_label("Send").click();
    wait(&mut h, "second reply", |a| !a.busy && a.items.len() >= 4);

    // Ctrl+B hides and shows the chats sidebar (persisted).
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::B);
    h.run_steps(3);
    assert!(!h.state().config.ui.show_sidebar);
    assert!(h.query_by_label("+  New chat").is_none());
    h.get_by_label("☰").click_accesskit();
    h.run_steps(3);
    h.get_by_label("+  New chat");

    // Ctrl+F finds across the conversation.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    h.run_steps(3);
    h.get_by_label("Find in chat").type_text("pelican");
    h.run_steps(3);
    h.get_by_label("1 of 3");
    h.get_by_label("Next").click_accesskit();
    h.run_steps(2);
    h.get_by_label("2 of 3");
    h.get_by_label("Previous").click_accesskit();
    h.run_steps(1);
    h.get_by_label("Previous").click_accesskit();
    h.run_steps(2);
    h.get_by_label("3 of 3");
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().find.is_none());

    // ↑ in the empty composer brings back earlier messages, newest first; ↓ goes back.
    assert!(h.state_mut().history_step(true));
    assert_eq!(h.state().input, "Can they fly?");
    assert!(h.state_mut().history_step(true));
    assert_eq!(h.state().input, "Tell me about pelicans");
    assert!(h.state_mut().history_step(false));
    assert!(h.state_mut().history_step(false));
    assert_eq!(h.state().input, "");
    h.state_mut().input = "half-typed".into();
    assert!(!h.state_mut().history_step(true), "doesn't overwrite what you're typing");
}

#[test]
fn drafts_survive_switching_chats_and_the_last_chat_reopens() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("First chat answer."));
    let (d, mut app) = app_with_mock(&rt, &mock.base());
    let paths = app.paths.clone();
    let cfg = app.config.clone();
    app.input = "first question".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| !a.busy && a.items.len() >= 2);
    let first = h.state().session.id.clone();
    h.state_mut().input = "a draft I haven't sent".into();
    h.state_mut().new_chat();
    wait(&mut h, "new chat", |a| a.session.id != first && !a.busy);
    assert!(h.state().input.is_empty());
    h.state_mut().open_session(first.clone());
    wait(&mut h, "back in the first chat", |a| a.session.id == first);
    assert_eq!(h.state().input, "a draft I haven't sent");

    // A new window opens on the last chat.
    let again = MercuryApp::new(paths, cfg, rt.clone()).unwrap();
    let mut h2 = harness(again);
    wait(&mut h2, "transcript", |a| a.items.len() >= 2);
    assert_eq!(h2.state().session.id, first);
    h2.get_by_label_contains("First chat answer.");
    drop(d);
}

#[test]
fn attachments_reach_the_model() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("I see a small orange square and your notes."));
    let (d, mut app) = app_with_mock(&rt, &mock.base());
    let img = d.path().join("square.png");
    image::RgbImage::from_pixel(8, 8, image::Rgb([255, 120, 0])).save(&img).unwrap();
    let notes = d.path().join("notes.md");
    std::fs::write(&notes, "- buy milk\n").unwrap();
    std::fs::write(d.path().join("data.bin"), [0u8, 159, 146, 150]).unwrap();
    app.attach_files(vec![img, notes, d.path().join("data.bin")]);
    assert_eq!(app.attachments.len(), 2, "the binary file is refused");
    app.input = "What's in these?".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Remove square.png");
    h.get_by_label("Remove notes.md");
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| !a.busy && a.items.len() >= 2);
    assert!(h.state().attachments.is_empty());
    let body = mock.requests().last().cloned().unwrap();
    let user = body["messages"].as_array().unwrap().iter().rfind(|m| m["role"] == "user").unwrap().clone();
    let text_part = user["content"][0]["text"].as_str().unwrap();
    assert!(text_part.starts_with("What's in these?") && text_part.contains("[Attached file: notes.md]") && text_part.contains("buy milk"));
    assert!(user["content"][1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
    h.get_by_label_contains("[1 image attached]");
}

#[test]
fn pin_archive_and_status_bar() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("one"));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.input = "Alpha topic".into();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| !a.busy && a.items.len() >= 2);
    // Context meter and speed come from the reply's usage.
    h.get_by_label_contains("context 15 / 32k");
    let id = h.state().session.id.clone();
    h.state_mut().set_pinned(&id, true);
    h.run_steps(3);
    h.get_by_label("Pinned");
    h.state_mut().set_archived(&id, true);
    wait(&mut h, "fresh chat after archiving the open one", |a| a.session.id != id && !a.busy);
    h.get_by_label("Archived (1)").click_accesskit();
    h.run_steps(3);
    assert!(h.state().show_archived);
    h.get_by_label("Alpha topic");
    let meta = h.state().sessions.iter().find(|s| s.id == id).cloned().unwrap();
    assert!(meta.archived && !meta.pinned, "archiving unpins");
}

#[test]
fn effort_picker_and_connection_test() {
    let rt = rt();
    let mock = rt.block_on(start());
    mock.push_chat(text("OK"));
    let (_d, app) = app_with_mock(&rt, &mock.base());
    let paths = app.paths.clone();
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Reasoning effort").click_accesskit();
    h.run_steps(2);
    h.get_by_label("low").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().config.active().unwrap().reasoning_effort.as_deref(), Some("low"));
    assert_eq!(Config::load(&paths.config_file()).unwrap().provider("mock").unwrap().reasoning_effort.as_deref(), Some("low"));

    h.state_mut().view = View::Settings(Tab::Providers);
    h.run_steps(3);
    h.get_by_label("Test connection").click_accesskit();
    wait(&mut h, "probe result", |a| a.probes.contains_key("mock"));
    h.get_by_label_contains("OK: 2 chat models listed");
    assert_eq!(mock.requests().last().unwrap()["reasoning_effort"], "low", "the test uses the provider's options");

    // Extra request fields: bad JSON shows an error and isn't applied; good JSON is.
    h.get_by_label("Provider extra request fields").scroll_to_me();
    h.run_steps(2);
    h.get_by_label("Provider extra request fields").focus();
    h.run_steps(1);
    h.get_by_label("Provider extra request fields").type_text("{oops");
    h.run_steps(2);
    h.get_by_label_contains("not valid JSON");
    assert!(h.state().config.provider("mock").unwrap().extra_body.is_empty());
    h.state_mut().provider_text.get_mut("mock").unwrap().extra = String::new();
    h.run_steps(1);
    h.get_by_label("Provider extra request fields").focus();
    h.run_steps(1);
    h.get_by_label("Provider extra request fields").type_text(r#"{"service_tier":"flex"}"#);
    h.run_steps(2);
    assert_eq!(h.state().config.provider("mock").unwrap().extra_body["service_tier"], "flex");
}

#[test]
fn skills_can_be_switched_off() {
    let rt = rt();
    let (_d, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    let dir = app.config.skills_dir(&app.paths).join("deploy-site");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "---\nname: deploy-site\ndescription: Deploy the site\n---\nSteps.").unwrap();
    app.view = View::Settings(Tab::Skills);
    let mut h = harness(app);
    h.run_steps(3);
    assert_eq!(h.state().skill_names(), vec!["deploy-site"]);
    h.get_by_label("Enable skill deploy-site").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().config.disabled_skills, vec!["deploy-site"]);
    assert!(h.state().skill_names().is_empty(), "not runnable as /deploy-site");
    let saved = Config::load_profile(&h.state().paths).unwrap();
    assert_eq!(saved.disabled_skills, vec!["deploy-site"]);
    h.get_by_label("Enable skill deploy-site").click_accesskit();
    h.run_steps(3);
    assert!(h.state().config.disabled_skills.is_empty());
}

#[test]
fn profiles_export_and_import_from_the_app() {
    let rt = rt();
    let (d, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    mercury::profiles::create(&app.paths, "work").unwrap();
    app.refresh_profiles();
    let out = d.path().join("exports");
    std::fs::create_dir_all(&out).unwrap();
    app.view = View::Settings(Tab::Hermes);
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("Export work");
    h.state_mut().export_profile("work", out.clone());
    wait(&mut h, "exported", |_| std::fs::read_dir(&out).map(|r| r.count() == 1).unwrap_or(false));
    let file = std::fs::read_dir(&out).unwrap().next().unwrap().unwrap().path();
    assert!(file.to_string_lossy().ends_with(".tar.gz"));
    h.state_mut().import_profile(file);
    wait(&mut h, "imported", |a| a.profiles.contains(&"work-2".to_string()));
    h.get_by_label("Open work-2");
}

#[test]
fn a_panic_in_agent_work_is_reported_not_hung() {
    let rt = rt();
    let (_d, mut app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    app.busy = true;
    app.spawn_guarded(async { panic!("boom in a background task") });
    let mut h = harness(app);
    wait(&mut h, "recovered", |a| !a.busy);
    h.get_by_label_contains("Something went wrong inside Mercury (boom in a background task)");
}

#[test]
fn yolo_toggle_in_the_status_bar() {
    let rt = rt();
    let (_d, app) = app_with_mock(&rt, "http://127.0.0.1:9/v1");
    let mut h = harness(app);
    h.run_steps(3);
    h.get_by_label("approvals: ask").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().config.approvals, ApprovalMode::Yolo);
    h.get_by_label("YOLO: commands run without approval").click_accesskit();
    h.run_steps(3);
    assert_eq!(h.state().config.approvals, ApprovalMode::Ask);
}

#[test]
fn context_meter_uses_the_window_the_server_reports() {
    let rt = rt();
    let mock = rt.block_on(start());
    *mock.state.models_json.lock().unwrap() = Some(
        json!({"data": [{"id": "test-model", "metadata": {"tags": ["chat", "vision"], "context_length": 65536, "description": "", "pricing": {}}}]}),
    );
    mock.push_chat(text("hi"));
    let (_d, mut app) = app_with_mock(&rt, &mock.base());
    app.input = "hello".into();
    let mut h = harness(app);
    wait(&mut h, "model list", |a| a.model_meta.contains_key("mock/test-model"));
    h.get_by_label("Send").click();
    wait(&mut h, "reply", |a| !a.busy && a.items.len() >= 2);
    h.get_by_label_contains("context 15 / 65k");
    // The model menu shows what the server said about the model.
    h.get_by_label("Model").click_accesskit();
    h.run_steps(3);
    h.get_by_label_contains("test-model  · 65k  · vision");
}
