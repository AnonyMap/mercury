//! Settings screens.

use egui::{RichText, Ui};

use super::{MercuryApp, Tab, View, theme};

/// Screen-reader names for widgets that have no visible label of their own.
trait Labelled {
    fn labelled(self, label: &str) -> Self;
}

impl Labelled for egui::Response {
    fn labelled(self, label: &str) -> Self {
        super::a11y_label(&self, label);
        self
    }
}
use crate::config::{ApprovalMode, ProviderKind};
use crate::memory::{MemoryStore, Target};
use crate::session::SessionStore;
use crate::skills::SkillLibrary;

pub fn show(app: &mut MercuryApp, ui: &mut Ui, tab: Tab) {
    let p = theme::palette(app.config.ui.dark);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Settings").size(22.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Back to chat").clicked() {
                app.view = View::Chat;
            }
        });
    });
    // Tabs wrap onto a second line on narrow windows or large text sizes.
    ui.horizontal_wrapped(|ui| {
        for (t, label) in [
            (Tab::Providers, "Models & providers"),
            (Tab::Nous, "Nous Portal"),
            (Tab::Tools, "Tools & safety"),
            (Tab::Memory, "Memory"),
            (Tab::Skills, "Skills"),
            (Tab::Hermes, "Profiles & Hermes"),
            (Tab::General, "General"),
        ] {
            if ui.selectable_label(tab == t, label).clicked() {
                app.view = View::Settings(t);
            }
        }
    });
    ui.separator();
    egui::ScrollArea::vertical().id_salt(("settings", tab as u8)).auto_shrink(false).show(ui, |ui| {
        ui.set_max_width(860.0);
        match tab {
            Tab::Providers => providers(app, ui, p),
            Tab::Nous => nous(app, ui, p),
            Tab::Tools => tools(app, ui, p),
            Tab::Memory => memory(app, ui, p),
            Tab::Skills => skills(app, ui, p),
            Tab::Hermes => hermes(app, ui, p),
            Tab::General => general(app, ui, p),
        }
        if app.config_dirty && !matches!(tab, Tab::Memory | Tab::Skills | Tab::Nous | Tab::Hermes) {
            ui.add_space(10.0);
            if ui.button(RichText::new("Save changes").strong()).clicked() {
                app.apply_config();
                app.toast("Settings saved.");
            }
        }
    });
}

fn section(ui: &mut Ui, p: &theme::Palette, title: &str, hint: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).size(16.0).strong());
    if !hint.is_empty() {
        ui.label(RichText::new(hint).small().color(p.muted));
    }
    ui.add_space(4.0);
}

fn providers(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    section(
        ui,
        p,
        "Find local models",
        "Probes Ollama :11434, LM Studio :1234, llama.cpp :8080, vLLM :8000, KoboldCpp :5001, Jan :1337 and more. The network scan checks the same ports across your /24.",
    );
    ui.horizontal(|ui| {
        if ui.add_enabled(!app.discovering, egui::Button::new("Scan this machine")).clicked() {
            app.discover(false);
        }
        if ui.add_enabled(!app.discovering, egui::Button::new("Scan this machine + network")).clicked() {
            app.discover(true);
        }
        if app.discovering {
            ui.spinner();
        }
    });
    app.discovered_list(ui);

    section(ui, p, "Your providers", "");
    let active = app.config.active().map(|p| p.id.clone()).unwrap_or_default();
    let mut remove = None;
    let mut use_it = None;
    let mut refresh = None;
    let mut dirty = false;
    for prov in app.config.providers.iter_mut() {
        egui::Frame::new()
            .fill(p.surface)
            .stroke(egui::Stroke::new(1.0, if prov.id == active { p.accent } else { p.border }))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&prov.name).strong());
                    let badge = match (prov.kind, prov.is_local()) {
                        (ProviderKind::NousOauth, _) => ("Nous Portal sign-in", p.accent),
                        (_, true) => ("local", p.ok),
                        _ => ("cloud", p.muted),
                    };
                    ui.label(RichText::new(badge.0).small().color(badge.1));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(prov.id.clone());
                        }
                        if ui.small_button("List models").clicked() {
                            refresh = Some(prov.id.clone());
                        }
                        if prov.id != active && ui.small_button("Use").clicked() {
                            use_it = Some(prov.id.clone());
                        }
                    });
                });
                egui::Grid::new(("prov_grid", &prov.id)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Label");
                    dirty |= ui
                        .add(egui::TextEdit::singleline(&mut prov.name).hint_text("e.g. Basement GPU, Laptop Ollama").desired_width(420.0))
                        .labelled("Provider label")
                        .changed();
                    ui.end_row();
                    ui.label("Base URL");
                    dirty |=
                        ui.add(egui::TextEdit::singleline(&mut prov.base_url).desired_width(420.0)).labelled("Provider base URL").changed();
                    ui.end_row();
                    if prov.kind == ProviderKind::Openai {
                        ui.label("API key");
                        let mut key = prov.api_key.clone().unwrap_or_default();
                        if ui
                            .add(egui::TextEdit::singleline(&mut key).password(true).hint_text("none").desired_width(420.0))
                            .labelled("Provider API key")
                            .changed()
                        {
                            prov.api_key = Some(key).filter(|k| !k.is_empty());
                            dirty = true;
                        }
                        ui.end_row();
                        ui.label("…or env var");
                        let mut env = prov.api_key_env.clone().unwrap_or_default();
                        if ui
                            .add(egui::TextEdit::singleline(&mut env).hint_text("e.g. OPENROUTER_API_KEY").desired_width(420.0))
                            .labelled("Provider API key variable")
                            .changed()
                        {
                            prov.api_key_env = Some(env).filter(|k| !k.is_empty());
                            dirty = true;
                        }
                        ui.end_row();
                    }
                    ui.label("Default model");
                    dirty |= ui
                        .add(egui::TextEdit::singleline(&mut prov.default_model).desired_width(420.0))
                        .labelled("Provider default model")
                        .changed();
                    ui.end_row();
                    ui.label("Context window");
                    dirty |= ui
                        .add(egui::DragValue::new(&mut prov.context_window).range(0..=2_000_000).speed(512))
                        .labelled("Context window")
                        .on_hover_text("0 = use the default in General")
                        .changed();
                    ui.end_row();
                    ui.label("Tool calling");
                    dirty |= ui.checkbox(&mut prov.tools, "send tools (turn off for models that can't call functions)").changed();
                    ui.end_row();
                });
            });
        ui.add_space(6.0);
    }
    if dirty {
        app.config_dirty = true;
    }
    if let Some(id) = remove {
        app.config.remove_provider(&id);
        app.apply_config();
    }
    if let Some(id) = use_it {
        app.select_provider(&id);
    }
    if let Some(id) = refresh {
        app.fetch_models(&id);
        app.toast("Fetching models…");
    }

    section(
        ui,
        p,
        "Add an OpenAI-compatible endpoint",
        "Anything that serves /v1/chat/completions: a GPU box on your LAN, a proxy, or a hosted API.",
    );
    ui.horizontal(|ui| {
        let r = egui::ComboBox::from_id_salt("presets").selected_text("Presets").show_ui(ui, |ui| {
            for pr in crate::config::presets() {
                if ui.selectable_label(false, &pr.name).clicked() {
                    app.draft.name = pr.name.clone();
                    app.draft.url = pr.base_url.clone();
                }
            }
        });
        super::a11y_label(&r.response, "Presets");
    });
    egui::Grid::new("add_provider").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut app.draft.name).hint_text("GPU box").desired_width(420.0));
        ui.end_row();
        ui.label("Base URL");
        ui.add(egui::TextEdit::singleline(&mut app.draft.url).hint_text("http://192.168.1.20:11434/v1").desired_width(420.0));
        ui.end_row();
        ui.label("API key");
        ui.add(egui::TextEdit::singleline(&mut app.draft.key).password(true).hint_text("optional").desired_width(420.0));
        ui.end_row();
        ui.label("Model");
        ui.add(egui::TextEdit::singleline(&mut app.draft.model).hint_text("optional — pick later").desired_width(420.0));
        ui.end_row();
    });
    if ui.add_enabled(!app.draft.url.trim().is_empty(), egui::Button::new("Add endpoint")).clicked() {
        app.add_draft_provider();
    }
}

pub fn nous_login_controls(app: &mut MercuryApp, ui: &mut Ui) {
    let p = theme::palette(app.config.ui.dark);
    let status = crate::nous::status(&app.paths);
    if let Some(dc) = app.nous_code.clone() {
        egui::Frame::new().fill(p.accent_soft).corner_radius(10).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
            ui.label("Approve Mercury in your browser. If asked, enter this code:");
            ui.label(RichText::new(&dc.user_code).size(26.0).monospace().strong());
            ui.horizontal(|ui| {
                if ui.button("Open the page again").clicked() {
                    let _ = open::that_detached(dc.open_url());
                }
                if ui.button("Copy code").clicked() {
                    ui.ctx().copy_text(dc.user_code.clone());
                }
                if ui.button("Cancel").clicked() {
                    app.nous_cancel();
                }
                ui.spinner();
            });
            ui.label(RichText::new(dc.open_url()).small().color(p.muted));
        });
    } else if let Some(s) = status {
        ui.horizontal(|ui| {
            ui.label(RichText::new("● Signed in").color(p.ok));
            if let Some(exp) = s.expires_at {
                ui.label(
                    RichText::new(format!(
                        "token renews automatically (current one valid until {})",
                        exp.with_timezone(&chrono::Local).format("%H:%M")
                    ))
                    .small()
                    .color(p.muted),
                );
            }
        });
        ui.horizontal(|ui| {
            if let Some(id) = app.config.providers.iter().find(|p| p.kind == ProviderKind::NousOauth).map(|p| p.id.clone())
                && app.config.active().map(|p| p.id.as_str()) != Some(id.as_str())
                && ui.button("Use Nous Portal now").clicked()
            {
                app.select_provider(&id);
            }
            if ui.button("Sign out").clicked() {
                app.nous_logout();
            }
        });
    } else if ui.button("Sign in to Nous Portal").clicked() {
        app.nous_login();
    }
    if !app.nous_status.is_empty() {
        ui.label(RichText::new(&app.nous_status).small().color(p.muted));
    }
}

fn nous(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    section(
        ui,
        p,
        "Nous Portal",
        "Uses the same OAuth device-code sign-in as Hermes. Tokens are stored in a private file (mode 600) and refreshed automatically.",
    );
    nous_login_controls(app, ui);
    if let Some(s) = crate::nous::status(&app.paths) {
        ui.label(RichText::new(format!("Inference endpoint: {}", s.inference_base_url)).small().color(p.muted));
    }
    section(
        ui,
        p,
        "Prefer an API key?",
        "Create one at portal.nousresearch.com and add the “Nous Portal (API key)” preset under Models & providers. Useful when the browser sign-in is blocked by a CAPTCHA.",
    );
    egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
        let mut dirty = false;
        egui::Grid::new("nous_adv").num_columns(2).show(ui, |ui| {
            ui.label("Portal URL");
            dirty |= ui.text_edit_singleline(&mut app.config.nous.portal_url).labelled("Portal URL field").changed();
            ui.end_row();
            ui.label("OAuth client id");
            dirty |= ui.text_edit_singleline(&mut app.config.nous.client_id).labelled("OAuth client id field").changed();
            ui.end_row();
            ui.label("Scope");
            dirty |= ui.text_edit_singleline(&mut app.config.nous.scope).labelled("Scope field").changed();
            ui.end_row();
        });
        ui.label(
            RichText::new("Mercury signs in with Hermes's public client id because it's the only one the Portal recognises today.")
                .small()
                .color(p.muted),
        );
        if dirty {
            app.config_dirty = true;
        }
        if app.config_dirty && ui.button("Save").clicked() {
            app.apply_config();
        }
    });
}

fn tools(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    let mut dirty = false;
    section(ui, p, "Tools the agent may use", "Turned-off tools aren't offered to the model at all.");
    let t = &mut app.config.tools;
    dirty |= ui.checkbox(&mut t.terminal, "Terminal — run shell commands").changed();
    dirty |= ui.checkbox(&mut t.files, "Files — read, write, edit, search").changed();
    dirty |= ui.checkbox(&mut t.web, "Web — fetch pages (and search, if configured below)").changed();
    dirty |= ui.checkbox(&mut t.memory, "Memory — remember facts across chats").changed();
    dirty |= ui.checkbox(&mut t.skills, "Skills — load and save reusable procedures").changed();
    dirty |= ui.checkbox(&mut t.todo, "Todo list — plan multi-step work").changed();
    dirty |= ui.checkbox(&mut t.session_search, "Session search — look through past chats").changed();
    dirty |= ui.checkbox(&mut t.delegate, "Subagents — hand sub-tasks to helpers with a fresh context (delegate_task)").changed();
    dirty |= ui.checkbox(&mut t.clarify, "Clarifying questions — let the agent ask you mid-task").changed();
    dirty |= ui.checkbox(&mut t.checkpoints, "Checkpoints — snapshot files before edits so /rollback can undo them").changed();
    ui.horizontal(|ui| {
        ui.label("Command timeout (s)");
        dirty |= ui.add(egui::DragValue::new(&mut t.terminal_timeout_secs).range(5..=3600)).labelled("Command timeout").changed();
    });

    section(ui, p, "Dangerous commands", "Things like rm -r, sudo, curl | sh, git push --force, disk writes.");
    let a = &mut app.config.approvals;
    dirty |= ui.radio_value(a, ApprovalMode::Ask, "Ask me first (recommended)").changed();
    dirty |= ui.radio_value(a, ApprovalMode::Deny, "Always refuse").changed();
    dirty |= ui.radio_value(a, ApprovalMode::Yolo, RichText::new("Run without asking (YOLO)").color(p.warn)).changed();

    section(
        ui,
        p,
        "MCP servers",
        "Model Context Protocol servers add tools (GitHub, databases, Notion…). Same format as Hermes's mcp_servers; imported automatically from a Hermes profile.",
    );
    let mut remove = None;
    for (name, srv) in app.config.mcp_servers.iter_mut() {
        ui.horizontal(|ui| {
            dirty |= ui.checkbox(&mut srv.enabled, RichText::new(name).strong()).changed();
            let what = match (&srv.command, &srv.url) {
                (Some(c), _) => format!("{c} {}", srv.args.join(" ")),
                (None, Some(u)) => u.clone(),
                _ => "(incomplete)".into(),
            };
            ui.label(RichText::new(crate::provider::truncate(&what, 70)).monospace().small().color(p.muted));
            if ui.small_button("Remove").clicked() {
                remove = Some(name.clone());
            }
        });
    }
    if let Some(n) = remove {
        app.config.mcp_servers.remove(&n);
        dirty = true;
    }
    egui::Grid::new("mcp_add").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut app.mcp_draft.name).hint_text("github").desired_width(360.0));
        ui.end_row();
        ui.label("Command");
        ui.add(
            egui::TextEdit::singleline(&mut app.mcp_draft.command)
                .hint_text("npx -y @modelcontextprotocol/server-github")
                .desired_width(360.0),
        );
        ui.end_row();
        ui.label("…or URL");
        ui.add(egui::TextEdit::singleline(&mut app.mcp_draft.url).hint_text("https://mcp.example.com/mcp").desired_width(360.0));
        ui.end_row();
    });
    ui.horizontal(|ui| {
        let d = app.mcp_draft.clone();
        let ok = !d.name.trim().is_empty() && (!d.command.trim().is_empty() || !d.url.trim().is_empty());
        if ui.add_enabled(ok, egui::Button::new("Add server")).clicked() {
            let mut parts = d.command.split_whitespace().map(str::to_string);
            let cfg = crate::config::McpServerConfig {
                command: parts.next(),
                args: parts.collect(),
                url: Some(d.url.trim().to_string()).filter(|u| !u.is_empty()),
                enabled: true,
                ..Default::default()
            };
            app.config.mcp_servers.insert(d.name.trim().to_string(), cfg);
            app.mcp_draft = super::McpDraft::default();
            dirty = true;
        }
        if !app.config.mcp_servers.is_empty() && ui.button("Check servers").on_hover_text("Saves first, then connects").clicked() {
            app.apply_config();
            app.check_mcp();
        }
    });
    if !app.mcp_status.is_empty() {
        ui.add(egui::Label::new(RichText::new(&app.mcp_status).monospace().small()).wrap());
    }

    section(ui, p, "Web search", "Self-host SearXNG on your network, or use a Brave Search API key.");
    let t = &mut app.config.tools;
    egui::Grid::new("search_grid").num_columns(2).show(ui, |ui| {
        ui.label("SearXNG URL");
        let mut s = t.searxng_url.clone().unwrap_or_default();
        if ui.add(egui::TextEdit::singleline(&mut s).hint_text("http://192.168.1.10:8888").desired_width(360.0)).changed() {
            t.searxng_url = Some(s).filter(|x| !x.trim().is_empty());
            dirty = true;
        }
        ui.end_row();
        ui.label("Brave API key");
        let mut k = t.brave_api_key.clone().unwrap_or_default();
        if ui.add(egui::TextEdit::singleline(&mut k).password(true).desired_width(360.0)).labelled("Brave API key field").changed() {
            t.brave_api_key = Some(k).filter(|x| !x.trim().is_empty());
            dirty = true;
        }
        ui.end_row();
    });

    section(ui, p, "Working directory", "Where commands run and relative paths resolve. `cd` in the terminal tool also moves it.");
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.workdir_draft).desired_width(400.0)).labelled("Working directory field");
        if ui.button("Browse…").clicked() {
            app.browse(super::browse::PickPurpose::Workdir);
        }
        if ui.button("Apply").clicked() {
            let w = app.workdir_draft.clone();
            app.set_workdir(w);
        }
    });
    if dirty {
        app.config_dirty = true;
    }
}

fn memory(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    let store = MemoryStore::new(app.config.memories_dir(&app.paths));
    if app.memory_draft.is_none() {
        app.memory_draft = Some((store.read_raw(Target::Memory), store.read_raw(Target::User)));
    }
    section(
        ui,
        p,
        "Memory",
        &format!(
            "Same format as Hermes (entries separated by a line with §). Stored in {}. Changes apply from the next chat.",
            store.dir().display()
        ),
    );
    let (mem, user) = app.memory_draft.as_mut().unwrap();
    ui.label(RichText::new(format!("Agent notes — MEMORY.md ({}/{} chars)", mem.chars().count(), crate::memory::MEMORY_LIMIT)).strong());
    ui.add(egui::TextEdit::multiline(mem).desired_rows(8).desired_width(f32::INFINITY).code_editor()).labelled("MEMORY.md editor");
    ui.add_space(8.0);
    ui.label(RichText::new(format!("About you — USER.md ({}/{} chars)", user.chars().count(), crate::memory::USER_LIMIT)).strong());
    ui.add(egui::TextEdit::multiline(user).desired_rows(6).desired_width(f32::INFINITY).code_editor()).labelled("USER.md editor");
    ui.horizontal(|ui| {
        if ui.button("Save memory").clicked() {
            let (m, u) = app.memory_draft.clone().unwrap();
            match store.write_raw(Target::Memory, &m).and_then(|_| store.write_raw(Target::User, &u)) {
                Ok(()) => app.toast("Memory saved."),
                Err(e) => app.toast(format!("{e:#}")),
            }
        }
        if ui.button("Reload").clicked() {
            app.memory_draft = None;
        }
    });
}

fn skills(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    let lib = SkillLibrary::new(app.paths.default_skills_dir(), app.config.extra_skill_dirs.clone());
    section(
        ui,
        p,
        "Skills",
        &format!("Folders with a SKILL.md (Hermes / agentskills.io format). Yours live in {}. Run one with /<name>.", lib.home.display()),
    );
    ui.horizontal(|ui| {
        if ui.button("Open skills folder").clicked() {
            let _ = std::fs::create_dir_all(&lib.home);
            let _ = open::that_detached(&lib.home);
        }
    });
    let list = lib.list();
    if list.is_empty() {
        ui.label(RichText::new("No skills yet. The agent saves them as it learns, or import Hermes's skills from General.").color(p.muted));
        return;
    }
    let mut delete = None;
    ui.columns(2, |cols| {
        egui::ScrollArea::vertical().id_salt("skill_list").max_height(480.0).show(&mut cols[0], |ui| {
            for s in &list {
                let sel = app.selected_skill.as_deref() == Some(s.name.as_str());
                let r = ui.selectable_label(sel, RichText::new(&s.name).strong());
                ui.label(RichText::new(&s.description).small().color(p.muted));
                if !s.writable {
                    ui.label(RichText::new("read-only (from Hermes)").small().color(p.muted));
                }
                if r.clicked() {
                    app.selected_skill = Some(s.name.clone());
                }
                ui.add_space(4.0);
            }
        });
        if let Some(name) = app.selected_skill.clone() {
            let ui = &mut cols[1];
            if let Ok(text) = lib.view(&name, None) {
                egui::ScrollArea::vertical().id_salt("skill_view").max_height(440.0).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text).monospace().small()).wrap());
                });
            }
            if lib.get(&name).map(|s| s.writable).unwrap_or(false) && ui.button("Delete skill").clicked() {
                delete = Some(name);
            }
        }
    });
    if let Some(n) = delete {
        match lib.delete(&n) {
            Ok(()) => {
                app.selected_skill = None;
                app.toast(format!("Deleted {n}"));
            }
            Err(e) => app.toast(format!("{e:#}")),
        }
    }
}

fn general(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    let mut dirty = false;
    section(ui, p, "Appearance", "");
    ui.horizontal(|ui| {
        dirty |= ui.radio_value(&mut app.config.ui.dark, true, "Dark").changed();
        dirty |= ui.radio_value(&mut app.config.ui.dark, false, "Light").changed();
    });
    ui.horizontal(|ui| {
        ui.label("Text size");
        let mut pct = app.scale() * 100.0;
        let r = ui.add(egui::Slider::new(&mut pct, 75.0..=300.0).step_by(5.0).show_value(false)).labelled("Text size");
        ui.label(format!("{pct:.0}%"));
        if r.drag_stopped() || (r.changed() && !r.dragged()) {
            app.set_scale(pct / 100.0);
        }
        if ui.button("Fit my screen").on_hover_text("Pick a size from your monitor's resolution").clicked() {
            let w = ui.ctx().input(|i| i.viewport().monitor_size.map(|s| s.x)).unwrap_or(1920.0);
            app.set_scale(crate::config::auto_scale(w));
        }
    });
    ui.label(
        RichText::new("Shortcuts: Ctrl + / Ctrl - / Ctrl 0, or Ctrl + mouse wheel. The A+ / A- buttons sit in the top bar.")
            .small()
            .color(p.muted),
    );
    ui.horizontal(|ui| {
        ui.label("Logo");
        for (logo, label) in [(crate::config::Logo::Orb, "Orb"), (crate::config::Logo::Custom, "Custom image")] {
            if logo == crate::config::Logo::Custom && app.config.ui.custom_logo.is_none() {
                continue;
            }
            let tex = app.logo_texture(ui.ctx(), logo);
            ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(36.0, 36.0)));
            if ui.radio(app.config.ui.logo == logo, label).clicked() && app.config.ui.logo != logo {
                app.config.ui.logo = logo;
                let _ = app.config.save_profile(&app.paths);
            }
            ui.add_space(10.0);
        }
    });
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut app.logo_path_draft)
                .hint_text("/path/to/logo.png (PNG, JPEG, WebP, GIF, BMP, ICO)")
                .desired_width(360.0),
        );
        if ui.button("Browse…").clicked() {
            app.browse(super::browse::PickPurpose::Logo);
        }
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.add_enabled(!app.logo_path_draft.trim().is_empty(), egui::Button::new("Use this image")).clicked() || enter)
            && !app.logo_path_draft.trim().is_empty()
        {
            let path =
                crate::tools::files::resolve(&dirs::home_dir().unwrap_or_default(), &super::browse::clean_path(&app.logo_path_draft));
            app.set_custom_logo(&path);
        }
    });
    ui.label(
        RichText::new(
            "Or drop an image file onto this window. It's shown in the top bar, on the empty-chat screen and as the window icon.",
        )
        .small()
        .color(p.muted),
    );
    dirty |= ui.checkbox(&mut app.config.ui.show_reasoning, "Expand model reasoning by default").changed();

    section(ui, p, "Agent", "");
    egui::Grid::new("agent_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Personality");
        let r = egui::ComboBox::from_id_salt("personality").selected_text(app.config.agent.personality.clone()).show_ui(ui, |ui| {
            for (n, _) in crate::agent::PERSONALITIES {
                dirty |= ui.selectable_value(&mut app.config.agent.personality, n.to_string(), *n).changed();
            }
        });
        super::a11y_label(&r.response, "Personality");
        ui.end_row();
        ui.label("Max tool iterations");
        dirty |=
            ui.add(egui::DragValue::new(&mut app.config.agent.max_iterations).range(1..=500)).labelled("Max tool iterations").changed();
        ui.end_row();
        ui.label("Default context window");
        dirty |= ui
            .add(egui::DragValue::new(&mut app.config.agent.default_context_window).range(2048..=2_000_000).speed(1024))
            .labelled("Default context window")
            .changed();
        ui.end_row();
        ui.label("Compress at");
        ui.horizontal(|ui| {
            dirty |= ui
                .add(egui::Slider::new(&mut app.config.agent.compression_threshold, 0.3..=0.95).show_value(false))
                .labelled("Compress at")
                .changed();
            ui.label(format!("{:.0}% of the context window", app.config.agent.compression_threshold * 100.0));
        });
        ui.end_row();
        ui.label("Temperature");
        let mut on = app.config.agent.temperature.is_some();
        ui.horizontal(|ui| {
            if ui.checkbox(&mut on, "override").changed() {
                app.config.agent.temperature = on.then_some(0.7);
                dirty = true;
            }
            if let Some(t) = app.config.agent.temperature.as_mut() {
                dirty |= ui.add(egui::Slider::new(t, 0.0..=2.0).show_value(false)).labelled("Temperature").changed();
                ui.label(format!("{t:.2}"));
            }
        });
        ui.end_row();
    });

    section(ui, p, "About", "");
    ui.label(format!("Mercury {}", env!("CARGO_PKG_VERSION")));
    ui.label(
        RichText::new(format!("Config: {}\nData: {}", app.paths.config_file().display(), app.paths.data_dir.display()))
            .small()
            .color(p.muted),
    );
    if dirty {
        app.config_dirty = true;
    }
}

fn hermes(app: &mut MercuryApp, ui: &mut Ui, p: &theme::Palette) {
    use crate::config::LinkMode;
    app.ensure_hermes_scanned();

    // ---- Mercury profiles -------------------------------------------------------------
    section(
        ui,
        p,
        "Mercury profiles",
        "Each profile is its own agent: separate chats, memory, skills, SOUL.md, model and MCP servers. Providers and appearance are shared. Switch from the sidebar.",
    );
    let current = app.paths.profile.clone();
    let mut switch = None;
    let mut delete = None;
    for name in app.profiles.clone() {
        ui.horizontal(|ui| {
            let label = if name == current { RichText::new(format!("{name}  (open)")).strong() } else { RichText::new(&name) };
            ui.label(label);
            let link = app.config.hermes.as_ref().filter(|_| name == current).map(|l| format!("from Hermes “{}”", l.profile));
            if let Some(l) = link {
                ui.label(RichText::new(l).small().color(p.muted));
            }
            if name != current && ui.small_button(format!("Open {name}")).clicked() {
                switch = Some(name.clone());
            }
            if name == current || name == crate::config::DEFAULT_PROFILE {
                return;
            }
            if app.confirm_delete.as_deref() == Some(&name) {
                let chats = SessionStore::open(&app.paths.with_profile(&name).sessions_db())
                    .and_then(|s| s.list(10_000))
                    .map(|v| v.len())
                    .unwrap_or(0);
                ui.label(RichText::new(format!("Delete {name}, its {chats} chats, memory and skills?")).color(p.warn));
                if ui.small_button(format!("Yes, delete {name}")).clicked() {
                    delete = Some(name.clone());
                    app.confirm_delete = None;
                }
                if ui.small_button("Keep").clicked() {
                    app.confirm_delete = None;
                }
            } else if ui.small_button(format!("Delete {name}")).clicked() {
                app.confirm_delete = Some(name.clone());
            }
        });
    }
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut app.new_profile).hint_text("new profile name, e.g. work").desired_width(240.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.add_enabled(!app.new_profile.trim().is_empty(), egui::Button::new("Create profile")).clicked() || enter)
            && !app.new_profile.trim().is_empty()
        {
            app.create_profile();
        }
    });
    if let Some(n) = switch {
        app.switch_profile(&n, false);
    }
    if let Some(n) = delete {
        app.delete_profile(&n);
    }

    // ---- Hermes ------------------------------------------------------------------------
    section(
        ui,
        p,
        "Pick up from Hermes",
        "Choose your Hermes folder (~/.hermes), then which Hermes profile to bring over: models and keys, MCP servers, personalities, memory, SOUL.md, skills and conversations. Each Hermes profile becomes a Mercury profile with its own chats.",
    );
    ui.horizontal(|ui| {
        ui.label("Hermes folder");
        let r = ui.add(egui::TextEdit::singleline(&mut app.hermes_path).hint_text("~/.hermes").desired_width(340.0));
        super::a11y_label(&r, "Hermes folder path");
        if ui.button("Browse…").clicked() {
            app.browse(super::browse::PickPurpose::HermesFolder);
        }
        if ui.button("Find profiles").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
            app.scan_hermes();
        }
    });
    if let Some(Err(e)) = &app.hermes_result {
        ui.label(RichText::new(e).color(p.danger));
    }
    if app.hermes_profiles.is_empty() {
        return;
    }
    app.hermes_selected = app.hermes_selected.min(app.hermes_profiles.len() - 1);
    let names: Vec<String> = app.hermes_profiles.iter().map(|(p, _)| p.name.clone()).collect();
    egui::Grid::new("hermes_pick").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("Hermes profile");
        let r = egui::ComboBox::from_id_salt("hermes_profile").selected_text(&names[app.hermes_selected]).width(240.0).show_ui(ui, |ui| {
            for (i, n) in names.iter().enumerate() {
                ui.selectable_value(&mut app.hermes_selected, i, n);
            }
        });
        super::a11y_label(&r.response, "Hermes profile");
        ui.end_row();

        let (prof, sum) = app.hermes_profiles[app.hermes_selected].clone();
        ui.label("");
        let model = match (&sum.provider, &sum.model) {
            (Some(pr), Some(m)) => format!("{m} via {pr}"),
            (None, Some(m)) => m.clone(),
            (Some(pr), None) => pr.clone(),
            _ => "no model set".into(),
        };
        let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        ui.vertical(|ui| {
            ui.label(RichText::new(prof.home.display().to_string()).small().color(p.muted));
            ui.label(format!(
                "{model} · {} · memory {}+{} entries · {} · {}{}",
                plural(sum.sessions, "conversation", "conversations"),
                sum.memory_entries,
                sum.user_entries,
                plural(sum.skills, "skill", "skills"),
                plural(sum.mcp_servers, "MCP server", "MCP servers"),
                if sum.has_soul { " · SOUL.md" } else { "" }
            ));
            if sum.nous_oauth && !sum.nous_api_key {
                ui.label(
                    RichText::new("Signed in to Nous Portal there. That login can't be shared safely (single-use refresh tokens), so sign in once more under Nous Portal.")
                        .small()
                        .color(p.warn),
                );
            }
        });
        ui.end_row();

        ui.label("Into Mercury profile");
        let same = crate::profiles::sanitize(&prof.name);
        let same_label = if app.profiles.contains(&same) { same.clone() } else { format!("{same} (new)") };
        let shown = if app.hermes_target.is_empty() { same_label.clone() } else { app.hermes_target.clone() };
        let r = egui::ComboBox::from_id_salt("hermes_target").selected_text(shown).width(240.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut app.hermes_target, String::new(), same_label);
            for n in app.profiles.clone() {
                if n != same {
                    ui.selectable_value(&mut app.hermes_target, n.clone(), n);
                }
            }
        });
        super::a11y_label(&r.response, "Into Mercury profile");
        ui.end_row();

        ui.label("How");
        ui.vertical(|ui| {
            ui.radio_value(&mut app.hermes_mode, LinkMode::Copy, "Copy it (Hermes keeps working separately)");
            ui.radio_value(&mut app.hermes_mode, LinkMode::InPlace, "Use it in place (share its memory, skills and SOUL.md; don't run Hermes on it at the same time)");
            ui.checkbox(&mut app.hermes_sessions, "Bring conversation history (read-only; Hermes's database is never modified)");
        });
        ui.end_row();
    });
    let (prof, _) = app.hermes_profiles[app.hermes_selected].clone();
    let target = if app.hermes_target.is_empty() { crate::profiles::sanitize(&prof.name) } else { app.hermes_target.clone() };
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !app.hermes_busy,
                egui::Button::new(RichText::new(format!("Import “{}” into profile “{target}”", prof.name)).strong()),
            )
            .clicked()
        {
            let (mode, t) = (app.hermes_mode, app.hermes_target.clone());
            app.adopt_hermes(prof.clone(), mode, &t);
        }
        if app.hermes_busy {
            ui.spinner();
            ui.label("Importing…");
        }
    });
    if let Some(Ok(m)) = &app.hermes_result {
        ui.label(RichText::new(m).color(p.ok));
    }
    ui.label(RichText::new("Running it again later only brings over what's new.").small().color(p.muted));
}
