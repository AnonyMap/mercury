//! Command palette (Ctrl+K / Ctrl+P): one box to reach chats, commands, models, profiles and
//! settings by typing a few letters.

use egui::{Key, RichText, Ui};

use super::Tab;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    NewChat,
    OpenSession(String),
    /// Put this in the composer (slash commands that take arguments) or run it (those that don't).
    Slash(String),
    Settings(Tab),
    Model {
        provider: String,
        model: String,
    },
    Profile(String),
    ToggleSidebar,
    ToggleTheme,
    Find,
    Attach,
    TestProvider(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    pub hint: String,
    pub action: Action,
}

impl Item {
    pub fn new(label: impl Into<String>, hint: impl Into<String>, action: Action) -> Self {
        Self { label: label.into(), hint: hint.into(), action }
    }
}

/// Items whose label or hint contains every word of `query` (case-insensitive), best first:
/// label prefix matches, then label matches, then hint-only matches; original order otherwise.
pub fn filter<'a>(items: &'a [Item], query: &str) -> Vec<&'a Item> {
    let q = query.trim().to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut scored: Vec<(u8, usize, &Item)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let label = it.label.to_lowercase();
            let hay = format!("{label} {}", it.hint.to_lowercase());
            if !words.iter().all(|w| hay.contains(w)) {
                return None;
            }
            let rank = if words.is_empty() || label.starts_with(&q) {
                0
            } else if words.iter().all(|w| label.contains(w)) {
                1
            } else {
                2
            };
            Some((rank, i, it))
        })
        .collect();
    scored.sort_by_key(|(r, i, _)| (*r, *i));
    scored.into_iter().map(|(_, _, it)| it).collect()
}

pub struct Palette {
    pub query: String,
    pub selected: usize,
    pub items: Vec<Item>,
    focused: bool,
}

pub enum Outcome {
    None,
    Close,
    Run(Action),
}

impl Palette {
    pub fn new(items: Vec<Item>) -> Self {
        Self { query: String::new(), selected: 0, items, focused: false }
    }

    pub fn show(&mut self, ui: &mut Ui, muted: egui::Color32, accent: egui::Color32) -> Outcome {
        let mut out = Outcome::None;
        let (up, down, enter, esc) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::Escape),
            )
        });
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.query).hint_text("Type a command, chat, model or setting").desired_width(f32::INFINITY),
        );
        super::a11y_label(&r, "Command palette search");
        if !self.focused {
            r.request_focus();
            self.focused = true;
        }
        if r.changed() {
            self.selected = 0;
        }
        let hits = filter(&self.items, &self.query);
        if hits.is_empty() {
            ui.label(RichText::new("No matches").color(muted));
        }
        self.selected = self.selected.min(hits.len().saturating_sub(1));
        if down && !hits.is_empty() {
            self.selected = (self.selected + 1) % hits.len();
        }
        if up && !hits.is_empty() {
            self.selected = (self.selected + hits.len() - 1) % hits.len();
        }
        egui::ScrollArea::vertical().id_salt("palette_list").max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
            for (i, it) in hits.iter().enumerate().take(200) {
                let sel = i == self.selected;
                let text = RichText::new(&it.label).color(if sel { accent } else { ui.visuals().text_color() });
                let resp = ui.horizontal(|ui| {
                    let b = ui.add(egui::Button::selectable(sel, text).frame_when_inactive(false));
                    ui.label(RichText::new(&it.hint).small().color(muted));
                    b
                });
                if sel {
                    resp.inner.scroll_to_me(None);
                }
                if resp.inner.clicked() {
                    out = Outcome::Run(it.action.clone());
                }
            }
        });
        if enter && let Some(it) = hits.get(self.selected) {
            out = Outcome::Run(it.action.clone());
        }
        if esc {
            out = Outcome::Close;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_and_ranking() {
        let items = vec![
            Item::new("New chat", "Ctrl+N", Action::NewChat),
            Item::new("Settings: Models & providers", "", Action::Settings(Tab::Providers)),
            Item::new("Model: deepseek-v4", "DeepInfra", Action::Model { provider: "deepinfra".into(), model: "deepseek-v4".into() }),
            Item::new("Chat: plan the garden", "Yesterday", Action::OpenSession("s1".into())),
        ];
        assert_eq!(filter(&items, "").len(), 4);
        assert_eq!(filter(&items, "new")[0].label, "New chat");
        let hits = filter(&items, "deepinfra");
        assert_eq!(hits.len(), 1, "hint matches count");
        assert_eq!(filter(&items, "model deep")[0].label, "Model: deepseek-v4");
        assert_eq!(filter(&items, "garden")[0].action, Action::OpenSession("s1".into()));
        assert!(filter(&items, "zzz").is_empty());
        // "set" starts "Settings…" so it outranks labels that merely contain it.
        assert_eq!(filter(&items, "set")[0].action, Action::Settings(Tab::Providers));
    }
}
