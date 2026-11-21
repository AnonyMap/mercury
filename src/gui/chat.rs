//! The chat view model and how it renders.

use egui::{Align, Layout, RichText, Ui};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use super::theme::Palette;
use crate::provider::{Message, Role};

#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    User(String),
    Assistant { text: String, reasoning: String, streaming: bool },
    Tool { id: String, name: String, summary: String, args: String, output: Option<String>, is_error: bool },
    Notice(String),
    Error(String),
    Info(String),
}

/// Rebuild the view from a stored transcript.
pub fn from_messages(msgs: &[Message]) -> Vec<ChatItem> {
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::System => {}
            Role::User => {
                if m.text().starts_with("[Summary of our earlier conversation]") {
                    out.push(ChatItem::Info("Earlier messages were summarised to save context.".into()));
                }
                out.push(ChatItem::User(user_label(m.text(), m.images.len())));
            }
            Role::Assistant => {
                if !m.text().is_empty() || m.reasoning.is_some() {
                    out.push(ChatItem::Assistant {
                        text: m.text().to_string(),
                        reasoning: m.reasoning.clone().unwrap_or_default(),
                        streaming: false,
                    });
                }
                for c in &m.tool_calls {
                    let args = crate::tools::parse_args(&c.function.arguments).unwrap_or(serde_json::Value::Null);
                    out.push(ChatItem::Tool {
                        id: c.id.clone(),
                        name: c.function.name.clone(),
                        summary: crate::tools::summarize_call(&c.function.name, &args),
                        args: serde_json::to_string_pretty(&args).unwrap_or_default(),
                        output: None,
                        is_error: false,
                    });
                }
            }
            Role::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                if let Some(ChatItem::Tool { output, is_error, .. }) =
                    out.iter_mut().rev().find(|i| matches!(i, ChatItem::Tool { id: tid, .. } if *tid == id))
                {
                    *is_error = m.text().starts_with("Error:");
                    *output = Some(m.text().to_string());
                }
            }
        }
    }
    out
}

/// How a user message reads in the transcript (images are noted, not shown).
pub fn user_label(text: &str, images: usize) -> String {
    match images {
        0 => text.to_string(),
        1 => format!("{text}\n\n[1 image attached]"),
        n => format!("{text}\n\n[{n} images attached]"),
    }
}

/// The searchable text of an item.
fn item_text(item: &ChatItem) -> String {
    match item {
        ChatItem::User(t) | ChatItem::Notice(t) | ChatItem::Error(t) | ChatItem::Info(t) => t.clone(),
        ChatItem::Assistant { text, reasoning, .. } => format!("{text}\n{reasoning}"),
        ChatItem::Tool { summary, args, output, .. } => format!("{summary}\n{args}\n{}", output.as_deref().unwrap_or("")),
    }
}

/// Indices of items containing `query` (case-insensitive). Empty query: none.
pub fn find_matches(items: &[ChatItem], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    items.iter().enumerate().filter(|(_, it)| item_text(it).to_lowercase().contains(&q)).map(|(i, _)| i).collect()
}

/// Find-in-chat state for one frame of rendering.
#[derive(Default)]
pub struct Highlight<'a> {
    pub matches: &'a [usize],
    pub current: Option<usize>,
    /// Scroll the current match into view (set for one frame after it changes).
    pub scroll: bool,
}

/// Append streamed text to the live assistant item, creating it if needed.
pub fn push_delta(items: &mut Vec<ChatItem>, text: &str, reasoning: bool) {
    if !matches!(items.last(), Some(ChatItem::Assistant { streaming: true, .. })) {
        items.push(ChatItem::Assistant { text: String::new(), reasoning: String::new(), streaming: true });
    }
    if let Some(ChatItem::Assistant { text: t, reasoning: r, .. }) = items.last_mut() {
        if reasoning { r.push_str(text) } else { t.push_str(text) }
    }
}

/// Finalise the live assistant item with the committed message.
pub fn commit_assistant(items: &mut Vec<ChatItem>, m: &Message) {
    let live = items.iter().rposition(|i| matches!(i, ChatItem::Assistant { streaming: true, .. }));
    match live {
        Some(i) => {
            if m.text().is_empty() && m.reasoning.is_none() {
                items.remove(i);
            } else {
                items[i] = ChatItem::Assistant {
                    text: m.text().to_string(),
                    reasoning: m.reasoning.clone().unwrap_or_default(),
                    streaming: false,
                };
            }
        }
        None if !m.text().is_empty() => items.push(ChatItem::Assistant {
            text: m.text().to_string(),
            reasoning: m.reasoning.clone().unwrap_or_default(),
            streaming: false,
        }),
        None => {}
    }
}

pub fn render(ui: &mut Ui, items: &[ChatItem], p: &Palette, cache: &mut CommonMarkCache, show_reasoning: bool, hl: &Highlight<'_>) {
    let width = ui.available_width();
    for (idx, item) in items.iter().enumerate() {
        let r = ui.scope(|ui| render_item(ui, idx, item, p, cache, show_reasoning, width)).response;
        if hl.matches.contains(&idx) {
            let current = hl.current == Some(idx);
            let stroke = egui::Stroke::new(if current { 2.5 } else { 1.0 }, if current { p.accent } else { p.warn });
            ui.painter().rect_stroke(r.rect.expand(3.0), 6.0, stroke, egui::StrokeKind::Outside);
            if current && hl.scroll {
                r.scroll_to_me(Some(Align::Center));
            }
        }
        ui.add_space(8.0);
    }
}

fn render_item(ui: &mut Ui, idx: usize, item: &ChatItem, p: &Palette, cache: &mut CommonMarkCache, show_reasoning: bool, width: f32) {
    {
        match item {
            ChatItem::User(t) => {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    egui::Frame::new().fill(p.user_bubble).corner_radius(12).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                        ui.set_max_width(width * 0.75);
                        ui.add(egui::Label::new(RichText::new(t).color(p.text)).wrap());
                    });
                });
            }
            ChatItem::Assistant { text, reasoning, streaming } => {
                if !reasoning.is_empty() {
                    egui::CollapsingHeader::new(
                        RichText::new(if *streaming && text.is_empty() { "Thinking…" } else { "Reasoning" }).color(p.muted).small(),
                    )
                    .id_salt(("reasoning", idx))
                    .default_open(show_reasoning)
                    .show(ui, |ui| {
                        ui.label(RichText::new(reasoning).color(p.muted).small());
                    });
                }
                if !text.is_empty() {
                    // Unique id per message: identical markdown (tables, code) must not share widget ids.
                    ui.push_id(("assistant_md", idx), |ui| {
                        ui.set_max_width(width);
                        CommonMarkViewer::new().show(ui, cache, text);
                    });
                }
                if *streaming {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        if text.is_empty() && reasoning.is_empty() {
                            ui.label(RichText::new("Waiting for the model…").color(p.muted).small());
                        }
                    });
                } else if !text.is_empty() {
                    ui.push_id(("copy", idx), |ui| {
                        if ui.small_button("Copy").on_hover_text("Copy this reply as Markdown").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                    });
                }
            }
            ChatItem::Tool { name, summary, args, output, is_error, id } => {
                let (icon, color) = match (output, is_error) {
                    (None, _) => ("…", p.accent),
                    (Some(_), false) => ("✔", p.ok),
                    (Some(_), true) => ("✖", p.danger),
                };
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(width - 24.0);
                        egui::CollapsingHeader::new(RichText::new(format!("{icon}  {summary}")).color(color))
                            .id_salt(("tool", id, idx))
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.label(RichText::new(format!("{name} arguments")).small().color(p.muted));
                                ui.add(egui::Label::new(RichText::new(args).monospace().small()).wrap());
                                if let Some(o) = output {
                                    ui.separator();
                                    ui.label(RichText::new("result").small().color(p.muted));
                                    egui::ScrollArea::vertical().id_salt(("tool_out", id, idx)).max_height(260.0).show(ui, |ui| {
                                        ui.add(egui::Label::new(RichText::new(o).monospace().small()).wrap());
                                    });
                                }
                            });
                    });
            }
            ChatItem::Notice(t) => {
                ui.label(RichText::new(t).italics().small().color(p.muted));
            }
            ChatItem::Info(t) => {
                ui.add(egui::Label::new(RichText::new(t).monospace().color(p.muted)).wrap());
            }
            ChatItem::Error(t) => {
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(egui::Stroke::new(1.0, p.danger))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(width - 24.0);
                        ui.add(egui::Label::new(RichText::new(t).color(p.danger)).wrap());
                    });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCall};

    #[test]
    fn transcript_to_items() {
        let mut a = Message::assistant("");
        a.content = None;
        a.tool_calls.push(ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall { name: "terminal".into(), arguments: "{\"command\":\"ls\"}".into() },
        });
        let msgs = vec![Message::user("hi"), a, Message::tool("c1", "terminal", "Error: nope"), Message::assistant("done")];
        let items = from_messages(&msgs);
        assert_eq!(items.len(), 3);
        assert_eq!(find_matches(&items, "DONE"), vec![2]);
        assert_eq!(find_matches(&items, "nope"), vec![1], "tool output is searchable");
        assert!(find_matches(&items, "  ").is_empty());
        let with_img = from_messages(&[Message::user_with_images("see", vec!["data:x".into(), "data:y".into()])]);
        assert_eq!(with_img[0], ChatItem::User("see\n\n[2 images attached]".into()));
        assert!(matches!(&items[1], ChatItem::Tool { summary, output: Some(_), is_error: true, .. } if summary == "$ ls"));
    }

    #[test]
    fn streaming_then_commit() {
        let mut items = vec![ChatItem::User("q".into())];
        push_delta(&mut items, "think", true);
        push_delta(&mut items, "Hel", false);
        push_delta(&mut items, "lo", false);
        assert!(matches!(&items[1], ChatItem::Assistant { text, reasoning, streaming: true } if text == "Hello" && reasoning == "think"));
        commit_assistant(&mut items, &Message::assistant("Hello"));
        assert!(matches!(&items[1], ChatItem::Assistant { streaming: false, .. }));
        // A tool-call-only turn removes the empty live bubble.
        push_delta(&mut items, "", false);
        let mut m = Message::assistant("");
        m.content = None;
        commit_assistant(&mut items, &m);
        assert_eq!(items.len(), 2);
    }
}
