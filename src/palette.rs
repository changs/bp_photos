//! A command palette (Shift+⌘P), like VS Code's: type to filter, arrows + Enter to run.

use eframe::egui::{self, Key};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    OpenFile,
    OpenFolder,
    QuickExport,
    CopyToClipboard,
    GetFreePresets,
    ImportPresetFiles,
    ImportPresetFolder,
    ShowPresetsFolder,
}

/// Every command: what it does, its name in the palette, and its shortcut (see [`shortcut`]).
pub const COMMANDS: [(Command, &str, &str); 8] = [
    (Command::OpenFile, "Open File…", "Cmd+O"),
    (Command::OpenFolder, "Open Folder…", "Shift+Cmd+O"),
    (Command::QuickExport, "Quick Export (save as name-edited)", "Cmd+S"),
    (Command::CopyToClipboard, "Copy to Clipboard", "Cmd+C"),
    (Command::GetFreePresets, "Get Free Presets (750 film looks, 30 MB download)", ""),
    (Command::ImportPresetFiles, "Import Presets… (.xmp, .lrtemplate, .cube)", ""),
    (Command::ImportPresetFolder, "Import Preset Folder…", ""),
    (Command::ShowPresetsFolder, "Show Presets Folder", ""),
];

/// A shortcut label for this platform: "Shift+⌘O" on macOS, "Shift+Ctrl+O" elsewhere.
/// (Shift is spelled out: the ⇧ glyph is missing from common UI fonts.)
pub fn shortcut(keys: &str) -> String {
    keys.replace("Cmd+", if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" })
}

#[derive(Default)]
pub struct Palette {
    open: bool,
    query: String,
    selected: usize,
    /// Focus the search box on the first frame after opening.
    focus: bool,
}

/// Fuzzy match: every query character appears in order. Lower scores are better (tighter
/// matches, and matches at word starts); `None` if it doesn't match.
fn score(query: &str, name: &str) -> Option<usize> {
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let mut pos = 0;
    let mut score = 0;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = name[pos..].iter().position(|&c| c == q)?;
        let at = pos + found;
        let word_start = at == 0 || !name[at - 1].is_alphanumeric();
        score += found + if word_start { 0 } else { 2 };
        pos = at + 1;
    }
    Some(score)
}

impl Palette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
        self.focus = self.open;
    }

    /// Draws the palette if it's open; returns the command chosen this frame.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Command> {
        if !self.open {
            return None;
        }
        let mut matches: Vec<(usize, Command, &str, &str)> =
            COMMANDS.iter().filter_map(|&(cmd, name, keys)| Some((score(&self.query, name)?, cmd, name, keys))).collect();
        matches.sort_by_key(|m| m.0);
        self.selected = self.selected.min(matches.len().saturating_sub(1));

        let (up, down, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::Enter),
                i.consume_key(egui::Modifiers::NONE, Key::Escape),
            )
        });
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        if down && self.selected + 1 < matches.len() {
            self.selected += 1;
        }
        let mut chosen = enter.then(|| matches.get(self.selected).map(|m| m.1)).flatten();

        let screen = ctx.content_rect();
        let width = 480.0_f32.min(screen.width() - 32.0);
        let area = egui::Area::new(egui::Id::new("command_palette"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(screen.center().x - width / 2.0, screen.min.y + 64.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(8.0).show(ui, |ui| {
                    ui.set_width(width);
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Type a command")
                            .desired_width(f32::INFINITY)
                            .margin(egui::vec2(8.0, 6.0)),
                    );
                    if self.focus {
                        edit.request_focus();
                        self.focus = false;
                    }
                    if edit.changed() {
                        self.selected = 0;
                    }
                    ui.add_space(4.0);
                    if matches.is_empty() {
                        ui.label(egui::RichText::new("No matching commands").weak());
                    }
                    for (i, &(_, cmd, name, keys)) in matches.iter().enumerate() {
                        let selected = i == self.selected;
                        let keys = egui::RichText::new(shortcut(keys)).weak();
                        let row = ui.add(egui::Button::selectable(selected, name).right_text(keys).min_size(egui::vec2(width, 26.0)));
                        if row.hovered() {
                            self.selected = i;
                        }
                        if row.clicked() {
                            chosen = Some(cmd);
                        }
                    }
                });
            });

        let clicked_outside = area.response.clicked_elsewhere();
        if esc || chosen.is_some() || clicked_outside {
            self.open = false;
        }
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching_prefers_word_starts() {
        assert!(score("of", "Open Folder…").is_some());
        assert!(score("fold", "Open Folder…").unwrap() < score("fold", "Open File…").unwrap_or(usize::MAX));
        assert_eq!(score("xyz", "Open File…"), None);
        assert_eq!(score("", "Open File…"), Some(0));
        assert!(score("open fi", "Open File…").is_some());
    }
}
