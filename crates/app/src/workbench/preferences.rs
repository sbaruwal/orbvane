//! Settings and themes: applying `settings.json`, the color theme picker, opening the settings
//! files, native popup menus, keyboard chords, auto save and the save participants (trim
//! trailing whitespace, final newlines).

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use settings::Scope;
use text::{Pos, Selection};
use theme::{Theme, ThemeInfo, ThemeKind};

use super::{Effect, Focus, PopupItem, Workbench};
use crate::config::{self, AutoSave, Config};
use crate::editor::Doc;
use crate::input::{Key, KeyInput};
use crate::palette::{Action, Item, Palette, Picker};

/// What picking an entry of a native popup menu does.
#[derive(Clone, Debug)]
pub(crate) enum PopupAction {
    None,
    SetValue(Scope, &'static str, Value),
    Reset(Scope, &'static str),
    CopySettingId(&'static str),
    CopySettingJson(&'static str),
    Run(crate::commands::Command),
    /// Apply the code action at this index of `Workbench::code_actions`.
    CodeAction(usize),
    /// Put the cursor in the active editor here (a breadcrumbs symbol).
    GotoHere(text::Pos),
    /// Show a sidebar view (the switcher's overflow menu).
    ShowView(super::View),
    /// Outline "..." menu: toggle Follow Cursor, or sort.
    OutlineFollowCursor,
    OutlineSort(super::outline::Sort),
    /// The Problems filter menu's toggles.
    ProblemsToggle(super::problems::ProblemsToggle),
    /// A breakpoint's context menu entry, for (file, line).
    Breakpoint(std::path::PathBuf, usize, super::debug::BpMenu),
    /// An action on a test.
    Test(crate::testing::Key, super::testing_view::TestAction),
    /// Show this Output channel.
    OutputChannel(String),
    /// A gear menu action on an extension.
    Extension(String, super::extensions_view::ExtMenu),
    /// Run a command (an extension's, from its menus) with these arguments.
    ExtCommand(String, Vec<Value>),
    /// The Assistant's agent menu.
    Agent(super::AgentAction),
    /// The entry is a submenu with these entries.
    Submenu(Vec<(PopupItem, PopupAction)>),
}

#[derive(Default)]
pub(crate) struct AutoSaveState {
    /// Per document: the version last seen and when it changed.
    edits: HashMap<usize, (u64, Instant)>,
    /// The document of the focused editor last frame (for `onFocusChange`).
    focused_doc: Option<usize>,
}

impl AutoSaveState {
    /// Drops what's kept per document (by index in `Workbench::docs`, which is being cleared).
    pub(crate) fn forget_docs(&mut self) {
        self.edits.clear();
        self.focused_doc = None;
    }
}

impl Workbench {
    /// Re-reads the effective settings into the config, theme and fonts.
    pub(super) fn apply_settings(&mut self) {
        config::set(Config::from_store(&self.settings));
        let theme = self.settings.string("workbench.colorTheme");
        if theme != self.theme.name {
            self.set_theme(&theme);
        }
        self.font_family = self.settings.string("editor.fontFamily");
        self.ui_mono = self.settings.string("workbench.interfaceFont") != "system";
        self.ext_settings_changed();
    }

    /// Built-in themes, then extensions' themes and the user's theme files
    /// (`<user data>/themes/*.json`).
    pub(super) fn themes(&self) -> Vec<ThemeInfo> {
        let mut themes = theme::builtin_themes();
        themes.extend(crate::contributions::themes());
        themes.extend(theme::user_themes(&settings::user_data_dir().join("themes")));
        themes
    }

    /// Switches to the theme called `name` (without saving the setting). Unknown or broken
    /// themes fall back to the default.
    pub(super) fn set_theme(&mut self, name: &str) {
        let loaded = self.themes().into_iter().find(|t| t.name.eq_ignore_ascii_case(name)).and_then(|info| Theme::load(&info).ok());
        let theme = match loaded {
            Some(t) => t,
            None if self.theme.name == theme::DEFAULT_THEME => return,
            None => Theme::default_theme(),
        };
        let dark = theme.is_dark();
        self.theme = theme;
        self.effects.push(Effect::SetAppearance { dark });
    }

    /// Writes a setting and applies it. Problems (a broken settings file) are shown in a dialog.
    pub(super) fn update_setting(&mut self, scope: Scope, key: &str, value: Option<Value>) {
        if let Err(e) = self.settings.set(scope, key, value) {
            self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Unable to write settings").set_description(e).show();
            return;
        }
        self.apply_settings();
        if let Some(path) = self.settings.path(scope).map(Path::to_path_buf) {
            self.reload_clean_doc(&path);
        }
    }

    /// Picks up edits to settings files made outside the settings editor.
    pub(super) fn reload_settings(&mut self) {
        if self.settings.reload_if_changed() {
            self.apply_settings();
        }
    }

    /// Replaces an open, unmodified document's text with the file on disk.
    pub(super) fn reload_clean_doc(&mut self, path: &Path) {
        let Some(doc) = self.docs.iter_mut().flatten().find(|d| d.buffer.path() == Some(path)) else { return };
        if doc.buffer.is_dirty() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let text = crate::search_editor::document_text(doc.lang, text.replace("\r\n", "\n"));
        if text == doc.buffer.text() {
            return;
        }
        let all = Selection { anchor: Pos::new(0, 0), head: doc.buffer.end(), goal_col: None };
        doc.buffer.insert(all, &text);
        doc.buffer.mark_saved();
    }

    /// Opens a settings file in an editor, creating it (`{}`) if needed.
    pub(super) fn open_settings_json(&mut self, scope: Scope) {
        let Some(path) = self.settings.path(scope).map(Path::to_path_buf) else {
            self.message_dialog()
                .set_level(rfd::MessageLevel::Info)
                .set_title("No folder is open")
                .set_description("Open a folder to use workspace settings.")
                .show();
            return;
        };
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, "{\n}\n");
        }
        self.open_file(&path);
        self.focus = Focus::Editor;
    }

    // ------------------------------------------------------------------ theme picker

    pub(super) fn open_theme_picker(&mut self) {
        let current = self.theme.name.clone();
        let group = |kind: ThemeKind| {
            match kind {
                ThemeKind::Light => "light themes",
                ThemeKind::Dark => "dark themes",
                ThemeKind::HighContrastDark | ThemeKind::HighContrastLight => "high contrast themes",
            }
            .to_string()
        };
        // We list light, then dark, then high contrast themes, alphabetically in each.
        let mut themes = self.themes();
        let order = |k: ThemeKind| match k {
            ThemeKind::Light => 0,
            ThemeKind::Dark => 1,
            _ => 2,
        };
        themes.sort_by(|a, b| order(a.kind).cmp(&order(b.kind)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let choices: Vec<Item> = themes
            .iter()
            .map(|t| Item {
                label: t.name.clone(),
                detail: String::new(),
                matches: Vec::new(),
                shortcut: None,
                action: Action::Theme(t.name.clone()),
                group: Some(group(t.kind)),
                kind: None,
            })
            .collect();
        let mut p = Palette::with_picker(Picker { placeholder: "Select Color Theme (Up/Down Keys to Preview)".into(), choices });
        p.selected = p.items.iter().position(|i| i.label == current).unwrap_or(0);
        p.scroll_to_selection();
        self.theme_before_picker = Some(current);
        self.palette = Some(p);
    }

    /// Shows the theme under the picker's selection without saving it.
    pub(super) fn preview_picked_theme(&mut self) {
        if self.theme_before_picker.is_none() {
            return;
        }
        if let Some(Action::Theme(name)) = self.palette.as_ref().and_then(|p| p.selected_action()) {
            if name != self.theme.name {
                self.set_theme(&name);
            }
        }
    }

    /// The palette was dismissed without choosing: undo any theme preview.
    pub(super) fn cancel_palette(&mut self) {
        self.palette = None;
        self.cancel_quick_access();
        if let Some(name) = self.theme_before_picker.take() {
            if name != self.theme.name {
                self.set_theme(&name);
            }
        }
    }

    pub(super) fn pick_theme(&mut self, name: &str) {
        self.theme_before_picker = None;
        self.set_theme(name);
        self.update_setting(Scope::User, "workbench.colorTheme", Some(Value::String(name.to_string())));
    }

    // ------------------------------------------------------------------ native popup menus

    pub(super) fn show_popup(&mut self, entries: Vec<(PopupItem, PopupAction)>, x: f32, y: f32) {
        // Submenus are flattened: actions are listed in the order `main.rs` numbers the items.
        fn flatten(entries: Vec<(PopupItem, PopupAction)>, actions: &mut Vec<PopupAction>) -> Vec<PopupItem> {
            entries
                .into_iter()
                .map(|(item, action)| match (item, action) {
                    (PopupItem::Item { label, .. }, PopupAction::Submenu(children)) => {
                        PopupItem::Submenu { label, items: flatten(children, actions) }
                    }
                    (item, action) => {
                        actions.push(action);
                        item
                    }
                })
                .collect()
        }
        let mut actions = Vec::new();
        let items = flatten(entries, &mut actions);
        self.popup = actions;
        self.effects.push(Effect::Popup { items, x, y });
    }

    /// An entry of the last popup menu was picked.
    pub fn popup_selected(&mut self, index: usize) {
        let Some(action) = self.popup.get(index).cloned() else { return };
        self.popup.clear();
        match action {
            PopupAction::None | PopupAction::Submenu(_) => {}
            PopupAction::Run(cmd) => self.run(cmd),
            PopupAction::CodeAction(i) => self.run_code_action(i),
            PopupAction::GotoHere(pos) => self.goto_here(pos),
            PopupAction::ShowView(v) => self.show_view(v),
            PopupAction::OutlineFollowCursor => self.outline.follow_cursor = !self.outline.follow_cursor,
            PopupAction::OutlineSort(sort) => self.outline.sort = sort,
            PopupAction::Breakpoint(path, line, action) => self.breakpoint_menu_action(path, line, action),
            PopupAction::ProblemsToggle(t) => self.problems_toggle(t),
            PopupAction::Test(key, action) => self.test_popup(key, action),
            PopupAction::OutputChannel(name) => self.show_output_channel(&name, false),
            PopupAction::Agent(action) => self.agent_action(action),
            PopupAction::Extension(id, action) => self.extension_menu_action(&id, action),
            PopupAction::ExtCommand(command, args) => self.ext_execute(&command, args, None),
            PopupAction::SetValue(scope, key, value) => {
                if key == "workbench.colorTheme" {
                    if let Some(name) = value.as_str() {
                        self.set_theme(name);
                    }
                }
                self.update_setting(scope, key, Some(value));
            }
            PopupAction::Reset(scope, key) => self.update_setting(scope, key, None),
            PopupAction::CopySettingId(key) => {
                if let Some(cb) = &mut self.clipboard {
                    let _ = cb.set_text(key.to_string());
                }
            }
            PopupAction::CopySettingJson(key) => {
                let value = serde_json::to_string(&self.settings.get(key)).unwrap_or_default();
                if let Some(cb) = &mut self.clipboard {
                    let _ = cb.set_text(format!("\"{key}\": {value}"));
                }
            }
        }
    }

    // ------------------------------------------------------------------ chords

    /// Handles keyboard chords (⌘K ⌘T). Returns true if the key was used.
    pub(super) fn chord_key(&mut self, k: &KeyInput) -> bool {
        if let Some(first) = self.chord.take() {
            if matches!(k.key, Key::Other) {
                self.chord = Some(first); // a lone modifier press
                return true;
            }
            match k.chord_command(&first) {
                Some(cmd) => {
                    self.status_message = None;
                    self.run(cmd)
                }
                None => {
                    self.status_message =
                        Some((format!("The key combination ({}, {}) is not a command.", first.label(), k.label()), Instant::now()));
                }
            }
            return true;
        }
        // The terminal keeps ⌘K for clearing.
        if self.focus != Focus::Terminal && k.starts_chord() {
            self.status_message = Some((format!("({}) was pressed. Waiting for second key of chord...", k.label()), Instant::now()));
            self.chord = Some(k.clone());
            return true;
        }
        false
    }

    /// Shows a short message in the status bar for a few seconds.
    pub(super) fn set_status_message(&mut self, text: &str) {
        self.status_message = Some((text.to_string(), Instant::now()));
    }

    /// The status bar text for chords, while it should still show.
    pub(super) fn status_text(&self) -> Option<&str> {
        let (text, at) = self.status_message.as_ref()?;
        (self.chord.is_some() || at.elapsed() < Duration::from_secs(3)).then_some(text.as_str())
    }

    // ------------------------------------------------------------------ saving

    /// The save participants: trim trailing whitespace, insert or trim final newlines.
    pub(super) fn before_save(doc: &mut Doc) {
        let cfg = config::get();
        let b = &mut doc.buffer;
        if cfg.trim_trailing_whitespace {
            for line in (0..b.len_lines()).rev() {
                let text = b.line(line);
                let trimmed = text.trim_end_matches([' ', '\t']).chars().count();
                let len = text.chars().count();
                if trimmed < len {
                    b.delete_range(Selection { anchor: Pos::new(line, trimmed), head: Pos::new(line, len), goal_col: None });
                }
            }
        }
        if cfg.trim_final_newlines {
            // Keep one line break after the last non-empty line.
            let n = b.len_lines();
            if let Some(last) = (0..n).rev().find(|&l| !b.line(l).is_empty()) {
                if last + 2 < n {
                    b.delete_range(Selection { anchor: Pos::new(last + 1, 0), head: b.end(), goal_col: None });
                }
            }
        }
        if cfg.insert_final_newline {
            let end = b.end();
            if end.col > 0 {
                b.insert(Selection::caret(end), "\n");
            }
        }
        b.break_undo_group();
    }

    /// Saves a document that has a file, without dialogs. Returns false on failure.
    pub(super) fn save_doc_quietly(&mut self, doc_id: usize) -> bool {
        let Some(doc) = self.docs.get_mut(doc_id).and_then(|d| d.as_mut()) else { return false };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return false };
        if !doc.buffer.is_dirty() {
            return true;
        }
        if let Some(saved) = self.search_config_of(doc_id).and_then(|_| self.save_search_editor(doc_id, false)) {
            return saved;
        }
        let Some(doc) = self.docs.get_mut(doc_id).and_then(|d| d.as_mut()) else { return false };
        Self::before_save(doc);
        if doc.buffer.save().is_err() {
            return false;
        }
        self.lsp.saved(&path);
        self.after_save(&path);
        true
    }

    /// Reacts to a saved file: settings files are re-read, git status refreshed.
    pub(super) fn after_save(&mut self, path: &Path) {
        if [Scope::User, Scope::Workspace].iter().any(|s| self.settings.path(*s) == Some(path)) {
            self.reload_settings();
        }
        if path == crate::keymap::path() {
            self.reload_keymap(true);
        }
        self.ext_saved(path);
        self.refresh_scm();
    }

    fn save_all_dirty_files(&mut self) {
        for id in 0..self.docs.len() {
            if self.docs[id].as_ref().is_some_and(|d| d.buffer.is_dirty() && d.buffer.path().is_some()) {
                self.save_doc_quietly(id);
            }
        }
    }

    /// Runs auto save for the current frame.
    pub(super) fn auto_save_tick(&mut self) {
        let cfg = config::get();
        match cfg.auto_save {
            AutoSave::Off | AutoSave::OnWindowChange => {}
            AutoSave::AfterDelay => {
                let delay = Duration::from_millis(cfg.auto_save_delay_ms);
                let now = Instant::now();
                let mut due = Vec::new();
                for (id, doc) in self.docs.iter().enumerate() {
                    let Some(doc) = doc.as_ref().filter(|d| d.buffer.is_dirty() && d.buffer.path().is_some()) else {
                        self.auto_save.edits.remove(&id);
                        continue;
                    };
                    let v = doc.buffer.version();
                    let entry = self.auto_save.edits.entry(id).or_insert((v, now));
                    if entry.0 != v {
                        *entry = (v, now);
                    } else if now - entry.1 >= delay {
                        due.push(id);
                    }
                }
                for id in due {
                    self.auto_save.edits.remove(&id);
                    if !self.save_doc_quietly(id) {
                        // Retry after another delay rather than on every frame.
                        let v = self.docs[id].as_ref().map_or(0, |d| d.buffer.version());
                        self.auto_save.edits.insert(id, (v, Instant::now()));
                    }
                }
            }
            AutoSave::OnFocusChange => {
                let focused = (self.focus == Focus::Editor && self.palette.is_none())
                    .then(|| self.active_editor().filter(|e| e.diff.is_none() && !e.settings).map(|e| e.doc))
                    .flatten();
                if let Some(prev) = self.auto_save.focused_doc.filter(|p| Some(*p) != focused) {
                    self.save_doc_quietly(prev);
                }
                self.auto_save.focused_doc = focused;
            }
        }
    }

    /// When auto save next needs to run without input.
    pub(super) fn auto_save_deadline(&self) -> Option<Instant> {
        let cfg = config::get();
        if cfg.auto_save != AutoSave::AfterDelay {
            return None;
        }
        let delay = Duration::from_millis(cfg.auto_save_delay_ms);
        self.auto_save.edits.values().map(|(_, t)| *t + delay).min()
    }

    /// The window lost focus: `onFocusChange` and `onWindowChange` save everything.
    pub fn window_blurred(&mut self) {
        if matches!(config::get().auto_save, AutoSave::OnFocusChange | AutoSave::OnWindowChange) {
            self.save_all_dirty_files();
        }
    }
}
