//! Choosing files and folders: the system file dialog (desktop portal or zenity) when the desktop
//! has one, otherwise a small built-in browser — AppImages run on desktops without either, and the
//! built-in one can show hidden folders like `~/.hermes` by default.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{RichText, Ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickPurpose {
    /// A Hermes home, its profiles folder, or one profile.
    HermesFolder,
    /// An image for the custom logo.
    Logo,
    /// The agent's working directory.
    Workdir,
    /// A file to attach to the next message.
    Attachment,
    /// Where to save an export of profile number `n` (index into the profile list).
    ExportDir(usize),
    /// A profile export to import.
    ProfileArchive,
}

impl PickPurpose {
    pub fn wants_file(self) -> bool {
        matches!(self, PickPurpose::Logo | PickPurpose::Attachment | PickPurpose::ProfileArchive)
    }
    pub fn title(self) -> &'static str {
        match self {
            PickPurpose::HermesFolder => "Choose your Hermes folder",
            PickPurpose::Logo => "Choose a logo image",
            PickPurpose::Workdir => "Choose the working directory",
            PickPurpose::Attachment => "Attach a file",
            PickPurpose::ExportDir(_) => "Choose where to save the export",
            PickPurpose::ProfileArchive => "Choose a Mercury profile export (.tar.gz)",
        }
    }
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            PickPurpose::Logo => &["png", "jpg", "jpeg", "webp", "gif", "bmp", "ico"],
            PickPurpose::ProfileArchive => &["gz", "tgz"],
            // Attachments: any file; unsupported ones are explained when picked.
            _ => &[],
        }
    }
    fn filter_name(self) -> &'static str {
        match self {
            PickPurpose::ProfileArchive => "Profile exports",
            _ => "Images",
        }
    }
}

/// Does this desktop look like it can show a system file dialog?
pub fn system_dialog_available() -> bool {
    let on_path = |bin: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()));
    let has = |v: &str| std::env::var_os(v).is_some_and(|x| !x.is_empty());
    on_path("zenity") || (has("DBUS_SESSION_BUS_ADDRESS") && (has("XDG_CURRENT_DESKTOP") || has("WAYLAND_DISPLAY")))
}

/// Show the system dialog (blocking; call off the UI thread). `Err(())` means no dialog could be
/// shown: a "cancel" that comes back faster than a person can click is a failure, not a choice.
#[allow(clippy::result_unit_err)]
pub fn system_pick(purpose: PickPurpose, start: &Path) -> Result<Option<PathBuf>, ()> {
    let t = Instant::now();
    let mut d = rfd::FileDialog::new().set_title(purpose.title()).set_directory(start);
    if !purpose.extensions().is_empty() {
        d = d.add_filter(purpose.filter_name(), purpose.extensions());
    }
    let r = if purpose.wants_file() { d.pick_file() } else { d.pick_folder() };
    match r {
        Some(p) => Ok(Some(p)),
        None if t.elapsed() < Duration::from_millis(250) => Err(()),
        None => Ok(None),
    }
}

/// A path as people paste it: surrounding whitespace and quotes (file managers add them for paths
/// with spaces) and `file://` URIs (what dragging from a file manager gives) are accepted.
pub fn clean_path(s: &str) -> String {
    let t = s.trim().trim_matches(['"', '\'']).trim();
    match t.strip_prefix("file://") {
        Some(rest) => percent_decode(rest.strip_prefix("localhost").unwrap_or(rest)),
        None => t.to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where a picker should open for `purpose`, given what the field currently holds.
pub fn start_dir(purpose: PickPurpose, current: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let current = &clean_path(current);
    let cur = crate::tools::files::resolve(&home, current);
    let from_field = if cur.is_dir() { Some(cur.clone()) } else { cur.parent().filter(|p| p.is_dir()).map(Path::to_path_buf) };
    match (purpose, from_field) {
        (_, Some(p)) if !current.trim().is_empty() => p,
        (PickPurpose::HermesFolder, _) => crate::hermes_import::hermes_home().filter(|h| h.is_dir()).unwrap_or(home),
        _ => home,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// The built-in browser.
pub struct Browser {
    pub purpose: PickPurpose,
    pub dir: PathBuf,
    pub path_text: String,
    pub show_hidden: bool,
    pub entries: Vec<Entry>,
    pub error: Option<String>,
}

pub enum BrowserAction {
    None,
    Cancel,
    Chosen(PathBuf),
}

/// Directory listing: folders first, then (in file mode) files with an allowed extension.
pub fn list_dir(dir: &Path, show_hidden: bool, files: bool, exts: &[&str]) -> std::io::Result<Vec<Entry>> {
    let mut v: Vec<Entry> = std::fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let path = e.path();
            let is_dir = path.is_dir();
            let keep = is_dir
                || (files
                    && path
                        .extension()
                        .and_then(|x| x.to_str())
                        .is_some_and(|x| exts.is_empty() || exts.iter().any(|e| e.eq_ignore_ascii_case(x))));
            keep.then_some(Entry { name, path, is_dir })
        })
        .collect();
    v.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
    Ok(v)
}

impl Browser {
    pub fn new(purpose: PickPurpose, start: PathBuf) -> Self {
        let mut b = Self {
            purpose,
            dir: PathBuf::new(),
            path_text: String::new(),
            // Hermes lives in a hidden folder; show those when looking for it.
            show_hidden: purpose == PickPurpose::HermesFolder,
            entries: vec![],
            error: None,
        };
        b.go(start);
        b
    }

    pub fn go(&mut self, dir: PathBuf) {
        match list_dir(&dir, self.show_hidden, self.purpose.wants_file(), self.purpose.extensions()) {
            Ok(e) => {
                self.entries = e;
                self.path_text = dir.display().to_string();
                self.dir = dir;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", dir.display())),
        }
    }

    pub fn show(&mut self, ui: &mut Ui, muted: egui::Color32) -> BrowserAction {
        let mut action = BrowserAction::None;
        ui.horizontal(|ui| {
            if ui.button("Up").clicked()
                && let Some(parent) = self.dir.parent()
            {
                self.go(parent.to_path_buf());
            }
            if ui.button("Home").clicked() {
                self.go(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
            }
            let r = ui.add(egui::TextEdit::singleline(&mut self.path_text).desired_width(ui.available_width() - 60.0));
            super::a11y_label(&r, "Folder path");
            if ui.button("Go").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                let p = crate::tools::files::resolve(&self.dir, &self.path_text);
                if p.is_file() && self.purpose.wants_file() {
                    action = BrowserAction::Chosen(p);
                } else {
                    self.go(p);
                }
            }
        });
        if ui.checkbox(&mut self.show_hidden, "Show hidden folders").changed() {
            self.go(self.dir.clone());
        }
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(egui::Color32::from_rgb(0xef, 0x6f, 0x6c)));
        }
        let mut go_to = None;
        egui::ScrollArea::vertical().id_salt("browser_list").max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
            if self.entries.is_empty() {
                ui.label(RichText::new("(empty)").color(muted));
            }
            for e in &self.entries {
                let label = if e.is_dir { format!("🗀  {}", e.name) } else { format!("    {}", e.name) };
                if ui.selectable_label(false, label).clicked() {
                    if e.is_dir {
                        go_to = Some(e.path.clone());
                    } else {
                        action = BrowserAction::Chosen(e.path.clone());
                    }
                }
            }
        });
        if let Some(d) = go_to {
            self.go(d);
        }
        ui.separator();
        ui.horizontal(|ui| {
            if !self.purpose.wants_file() && ui.button(RichText::new("Use this folder").strong()).clicked() {
                action = BrowserAction::Chosen(self.dir.clone());
            }
            if ui.button("Cancel").clicked() {
                action = BrowserAction::Cancel;
            }
            ui.label(RichText::new(self.dir.display().to_string()).small().color(muted));
        });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_and_start_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".hermes")).unwrap();
        std::fs::create_dir_all(d.path().join("Zeta")).unwrap();
        std::fs::create_dir_all(d.path().join("alpha")).unwrap();
        std::fs::write(d.path().join("logo.PNG"), "x").unwrap();
        std::fs::write(d.path().join("notes.txt"), "x").unwrap();
        let names = |v: Vec<Entry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        assert_eq!(names(list_dir(d.path(), false, false, &[]).unwrap()), vec!["alpha", "Zeta"]);
        assert_eq!(names(list_dir(d.path(), true, false, &[]).unwrap()), vec![".hermes", "alpha", "Zeta"]);
        assert_eq!(names(list_dir(d.path(), false, true, PickPurpose::Logo.extensions()).unwrap()), vec!["alpha", "Zeta", "logo.PNG"]);

        let typed = d.path().join("alpha").display().to_string();
        assert_eq!(start_dir(PickPurpose::Workdir, &typed), d.path().join("alpha"));
        let file = d.path().join("logo.PNG").display().to_string();
        assert_eq!(start_dir(PickPurpose::Logo, &file), d.path());
        assert_eq!(start_dir(PickPurpose::Workdir, &format!("  '{typed}' ")), d.path().join("alpha"), "pasted with quotes");

        assert_eq!(clean_path(" \"/home/me/My Stuff\" "), "/home/me/My Stuff");
        assert_eq!(clean_path("file:///home/me/My%20Stuff/.hermes"), "/home/me/My Stuff/.hermes");
        assert_eq!(clean_path("file://localhost/tmp/x%2"), "/tmp/x%2");
        assert_eq!(clean_path("~/.hermes"), "~/.hermes");

        let mut b = Browser::new(PickPurpose::HermesFolder, d.path().to_path_buf());
        assert!(b.show_hidden, "hidden folders are shown when looking for ~/.hermes");
        assert_eq!(b.entries[0].name, ".hermes");
        b.go(d.path().join("missing"));
        assert!(b.error.is_some());
        assert_eq!(b.dir, d.path(), "a bad path leaves the browser where it was");
    }
}
