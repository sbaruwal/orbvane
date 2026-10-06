//! Refactoring through language servers: applying workspace edits (to open documents, and to
//! files that aren't open, which are saved like `files.refactoring.autoSave`), and
//! Rename Symbol (F2) with the inline rename box.

use std::path::{Path, PathBuf};

use lsp::{Encoding, WorkspaceEdit};
use render::{Canvas, Rect, TextStyle};
use text::{EditKind, Pos, Selection};

use super::{Focus, Hit, Workbench, UI};
use crate::editor::Doc;
use crate::input::{Key, KeyInput};
use crate::widgets::{FieldEvent, TextField};

/// How long format on save waits for the formatter before saving unformatted.
const FORMAT_ON_SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The inline rename box.
pub(super) struct RenameWidget {
    group: usize,
    doc: usize,
    path: PathBuf,
    /// Where the rename was asked for (sent with the request).
    pos: Pos,
    /// The symbol's extent; the box sits over it.
    range: (Pos, Pos),
    old_name: String,
    field: TextField,
}

/// Converts server edits for one document to editor positions.
fn to_editor_edits(doc: &Doc, edits: &[lsp::TextEdit], encoding: Encoding) -> Vec<(Pos, Pos, String)> {
    let b = &doc.buffer;
    let last = b.len_lines().saturating_sub(1);
    let conv = |p: lsp::Position| {
        let line = (p.line as usize).min(last);
        if p.line as usize > last {
            return b.end();
        }
        b.clamp(Pos::new(line, encoding.from_lsp(&b.line(line), p.character)))
    };
    let mut out: Vec<(Pos, Pos, String)> = edits.iter().map(|e| (conv(e.range.start), conv(e.range.end), e.new_text.clone())).collect();
    out.sort_by_key(|e| e.0);
    out
}

/// Where `p` (a char index) moves when char ranges `edits` (start, end, new length) are
/// replaced: shifted by the edits before it; inside an edit, clamped to its new text.
fn map_index(p: usize, edits: &[(usize, usize, usize)]) -> usize {
    let mut shift: isize = 0;
    for &(a, z, n) in edits {
        if z <= p {
            shift += n as isize - (z - a) as isize;
        } else if a < p {
            return (a as isize + shift) as usize + (p - a).min(n);
        }
    }
    (p as isize + shift).max(0) as usize
}

/// Where a code action came from: a language server (with its position encoding), or an
/// extension's provider (UTF-8 positions; its command runs in the extension).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ActionSource {
    Server(crate::servers::ServerKey, Encoding),
    Extension(String),
}

pub(super) type Action = (lsp::CodeAction, ActionSource);

/// Quick Fix waiting for answers: where it was asked (group, document, cursor), how many
/// answers are still to come, and the actions so far.
pub(super) struct ActionRequest {
    pub g: usize,
    pub doc: usize,
    pub head: Pos,
    pub waiting: usize,
    pub actions: Vec<Action>,
}

impl Workbench {
    /// Applies edits one document at a time, each as one undo step. Returns whether all of
    /// them could be applied.
    pub(super) fn apply_workspace_edit(&mut self, edit: &WorkspaceEdit, encoding: Encoding) -> bool {
        let autosave = self.settings.bool("files.refactoring.autoSave");
        let mut ok = true;
        for (path, edits) in &edit.changes {
            if edits.is_empty() {
                continue;
            }
            let open = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path.as_path())));
            match open {
                Some(id) => {
                    let was_dirty = self.docs[id].as_ref().is_some_and(|d| d.buffer.is_dirty());
                    self.apply_doc_edits(id, edits, encoding);
                    if autosave && !was_dirty {
                        self.save_doc_quietly(id);
                    }
                }
                None => ok &= self.apply_file_edits(path, edits, encoding),
            }
        }
        ok
    }

    /// Edits an open document, keeping every editor's cursors where they were relative to
    /// the text around them.
    fn apply_doc_edits(&mut self, id: usize, edits: &[lsp::TextEdit], encoding: Encoding) {
        let Some(doc) = self.docs[id].as_ref() else { return };
        let edits = to_editor_edits(doc, edits, encoding);
        self.edit_doc(id, edits, false);
    }

    /// Replaces ranges of an open document, keeping every editor's cursors where they were
    /// relative to the text around them. `in_last_step`: part of the last undo step (edits
    /// that follow from it), else a step of its own.
    pub(super) fn edit_doc(&mut self, id: usize, mut edits: Vec<(Pos, Pos, String)>, in_last_step: bool) {
        let Some(doc) = self.docs[id].as_ref() else { return };
        edits.sort_by_key(|e| e.0);
        let b = &doc.buffer;
        let ranges: Vec<(usize, usize, usize)> =
            edits.iter().map(|(a, z, t)| (b.char_index(*a), b.char_index(*z), t.chars().count())).collect();
        // Every editor's selections, as char indices, to map through the edit.
        let mut sels: Vec<(usize, usize, Vec<(usize, usize)>)> = Vec::new();
        for (g, group) in self.groups.iter().enumerate() {
            for (t, ed) in group.tabs.iter().enumerate().filter(|(_, e)| e.doc == id) {
                sels.push((g, t, ed.selections().iter().map(|s| (b.char_index(s.anchor), b.char_index(s.head))).collect()));
            }
        }
        let before = self.active_editor().filter(|e| e.doc == id).map(|e| e.selections()).unwrap_or_default();
        let refs: Vec<(Pos, Pos, &str)> = edits.iter().map(|(a, z, t)| (*a, *z, t.as_str())).collect();
        let doc = self.docs[id].as_mut().unwrap();
        if in_last_step {
            doc.buffer.edit_in_last_step(&refs);
        } else {
            doc.buffer.edit(&before, &refs, EditKind::Other);
            doc.buffer.break_undo_group();
        }
        for (g, t, old) in sels {
            let b = &self.docs[id].as_ref().unwrap().buffer;
            let new: Vec<Selection> = old
                .iter()
                .map(|&(a, h)| Selection { anchor: b.pos_of(map_index(a, &ranges)), head: b.pos_of(map_index(h, &ranges)), goal_col: None })
                .collect();
            self.groups[g].tabs[t].set_selections(new);
        }
    }

    /// Edits a file that isn't open, and saves it.
    fn apply_file_edits(&mut self, path: &Path, edits: &[lsp::TextEdit], encoding: Encoding) -> bool {
        let Ok(mut doc) = Doc::open(path.to_path_buf()) else { return false };
        let edits = to_editor_edits(&doc, edits, encoding);
        let refs: Vec<(Pos, Pos, &str)> = edits.iter().map(|(a, z, t)| (*a, *z, t.as_str())).collect();
        doc.buffer.edit(&[], &refs, EditKind::Other);
        if doc.buffer.save().is_err() {
            return false;
        }
        self.after_save(path);
        true
    }

    // ------------------------------------------------------------------ formatting

    pub(super) fn formatting_options() -> serde_json::Value {
        let cfg = crate::config::get();
        serde_json::json!({
            "tabSize": cfg.tab_size,
            "insertSpaces": cfg.insert_spaces,
            "trimTrailingWhitespace": cfg.trim_trailing_whitespace,
            "insertFinalNewline": cfg.insert_final_newline,
            "trimFinalNewlines": cfg.trim_final_newlines,
        })
    }

    /// Format Document (⇧⌥F) / Format Selection (⌘K ⌘F).
    pub(super) fn format_active(&mut self, selection: bool) {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let (doc_id, (a, z)) = (ed.doc, ed.sel.ordered());
        let Some(path) = self.docs[doc_id].as_ref().and_then(|d| d.buffer.path()).map(Path::to_path_buf) else { return };
        let capability = if selection && a != z { "documentRangeFormattingProvider" } else { "documentFormattingProvider" };
        let range = (selection && a != z).then_some((a, z));
        if !self.lsp.has_server(&path) || !self.lsp.supports(&path, capability) {
            // An extension's formatter, if there's one.
            if self.ext_provide_formatting(doc_id, range, false) {
                return;
            }
            let what = if selection { "selection" } else { "document" };
            return self.set_status_message(&format!("There is no formatter for this {what}."));
        }
        let doc = self.docs[doc_id].as_ref().unwrap();
        self.lsp.format(&path, &doc.buffer, range, Self::formatting_options(), false);
    }

    /// Format on save: asks for formatting and saves when it arrives. Returns false (save
    /// right away) when the file has no formatter.
    pub(super) fn format_then_save(&mut self, doc_id: usize) -> bool {
        if !self.settings.bool("editor.formatOnSave") || self.format_saves.contains_key(&doc_id) {
            return false;
        }
        let Some(doc) = self.docs.get(doc_id).and_then(Option::as_ref) else { return false };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return false };
        let asked = if self.lsp.has_server(&path) && self.lsp.supports(&path, "documentFormattingProvider") {
            let doc = self.docs[doc_id].as_ref().unwrap();
            self.lsp.format(&path, &doc.buffer, None, Self::formatting_options(), true)
        } else {
            self.ext_provide_formatting(doc_id, None, true)
        };
        if !asked {
            return false;
        }
        self.format_saves.insert(doc_id, std::time::Instant::now());
        true
    }

    /// Formatting edits arrived: apply them if the text hasn't changed since, then save if
    /// this was format on save.
    pub(super) fn formatted(&mut self, path: &Path, version: u64, edits: Vec<lsp::TextEdit>, encoding: Encoding, save: bool) {
        let Some(id) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) else { return };
        if save && self.format_saves.remove(&id).is_none() {
            return; // timed out and saved already
        }
        let current = self.docs[id].as_ref().is_some_and(|d| d.buffer.version() == version);
        if current && !edits.is_empty() {
            self.apply_doc_edits(id, &edits, encoding);
        }
        if save {
            self.save_doc_quietly(id);
        }
    }

    /// Saves files whose formatter hasn't answered within a second, unformatted.
    pub(super) fn format_save_tick(&mut self) {
        let late: Vec<usize> =
            self.format_saves.iter().filter(|(_, t)| t.elapsed() >= FORMAT_ON_SAVE_TIMEOUT).map(|(id, _)| *id).collect();
        for id in late {
            self.format_saves.remove(&id);
            self.save_doc_quietly(id);
        }
    }

    pub(super) fn format_save_deadline(&self) -> Option<std::time::Instant> {
        self.format_saves.values().min().map(|t| *t + FORMAT_ON_SAVE_TIMEOUT)
    }

    // ------------------------------------------------------------------ code actions

    /// Quick Fix (⌘.): asks for code actions at the selection.
    pub(super) fn quick_fix(&mut self) {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let (doc_id, range, head) = (ed.doc, ed.sel.ordered(), ed.sel.head);
        let waiting = self.request_code_actions(doc_id, range, None);
        if waiting == 0 {
            return self.set_status_message("No code actions available");
        }
        self.code_action_request = Some(ActionRequest { g: self.active_group, doc: doc_id, head, waiting, actions: Vec::new() });
    }

    /// Asks the document's server and the extensions' providers for code actions at `range`,
    /// sending the diagnostics there. Returns how many were asked.
    pub(super) fn request_code_actions(&mut self, doc_id: usize, (a, z): (Pos, Pos), auto: Option<u64>) -> usize {
        let Some(path) = self.docs[doc_id].as_ref().and_then(|d| d.buffer.path()).map(Path::to_path_buf) else { return 0 };
        // Diagnostics touching the range's lines (servers match them to their fixes).
        let (encoding, diagnostics): (Encoding, Vec<lsp::Diagnostic>) = self
            .lsp
            .diagnostics
            .get(&path)
            .map(|(enc, ds)| (*enc, ds.iter().filter(|d| d.range.start.line as usize <= z.line && d.range.end.line as usize >= a.line).cloned().collect()))
            .unwrap_or((Encoding::Utf8, Vec::new()));
        let mut asked = 0;
        if self.lsp.has_server(&path) && self.lsp.supports(&path, "codeActionProvider") {
            let doc = self.docs[doc_id].as_ref().unwrap();
            self.lsp.code_actions(&path, &doc.buffer, (a, z), diagnostics.iter().map(|d| d.raw.clone()).collect(), auto);
            asked += 1;
        }
        asked + self.ext_provide_code_actions(doc_id, (a, z), &diagnostics, encoding, auto)
    }

    /// An answer to Quick Fix; the menu opens below the cursor once all have answered.
    pub(super) fn code_action_answer(&mut self, actions: Vec<Action>) {
        let Some(req) = &mut self.code_action_request else { return };
        req.actions.extend(actions);
        req.waiting = req.waiting.saturating_sub(1);
        if req.waiting > 0 {
            return;
        }
        let ActionRequest { g, doc: doc_id, head, actions, .. } = self.code_action_request.take().unwrap();
        if actions.is_empty() {
            return self.set_status_message("No code actions available");
        }
        let Some(ed) = self.groups.get(g).and_then(|gr| gr.tabs.get(gr.active)).filter(|e| e.doc == doc_id) else { return };
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let (x, y) = ed.point_of(doc, head);
        self.code_actions_menu(actions, x, y + crate::editor::line_height());
    }

    /// Shows code actions as a native menu at (x, y): quick fixes, then refactorings, then
    /// source actions, each group with the preferred one first.
    pub(super) fn code_actions_menu(&mut self, actions: Vec<Action>, x: f32, y: f32) {
        let rank = |a: &lsp::CodeAction| {
            let group = if a.kind.starts_with("quickfix") || a.kind.is_empty() {
                0
            } else if a.kind.starts_with("refactor") {
                1
            } else {
                2
            };
            (group, !a.preferred)
        };
        let mut order: Vec<usize> = (0..actions.len()).collect();
        order.sort_by_key(|&i| rank(&actions[i].0));
        let mut entries = Vec::new();
        let mut last_group = None;
        for &i in &order {
            let a = &actions[i].0;
            let group = rank(a).0;
            if last_group.is_some_and(|g| g != group) {
                entries.push((super::PopupItem::Separator, super::preferences::PopupAction::None));
            }
            last_group = Some(group);
            let item = super::PopupItem::Item { label: a.title.clone(), enabled: a.disabled.is_none(), checked: None };
            entries.push((item, super::preferences::PopupAction::CodeAction(i)));
        }
        self.code_actions = Some(actions);
        self.show_popup(entries, x, y);
    }

    /// A code action was picked: apply its edit, then run its command (or fetch the edit
    /// first if the server left it out).
    pub(super) fn run_code_action(&mut self, i: usize) {
        let Some(mut actions) = self.code_actions.take() else { return };
        if i >= actions.len() {
            return;
        }
        let (action, source) = actions.swap_remove(i);
        if let ActionSource::Server(key, _) = &source {
            if action.edit.is_none() && action.command.is_none() {
                return self.lsp.resolve_code_action(key, &action);
            }
        }
        self.finish_code_action(action, source);
    }

    pub(super) fn finish_code_action(&mut self, action: lsp::CodeAction, source: ActionSource) {
        let encoding = match &source {
            ActionSource::Server(_, encoding) => *encoding,
            ActionSource::Extension(_) => Encoding::Utf8,
        };
        if let Some(edit) = &action.edit {
            if !self.apply_workspace_edit(edit, encoding) {
                self.set_status_message("Some edits couldn't be applied.");
            }
        }
        let Some(command) = &action.command else { return };
        let ran = match &source {
            ActionSource::Server(key, _) => self.lsp.execute_command(key, command),
            ActionSource::Extension(_) => {
                let id = command["command"].as_str().unwrap_or("").to_string();
                let args = command["arguments"].as_array().cloned().unwrap_or_default();
                self.ext_execute(&id, args, None);
                true
            }
        };
        if !ran && action.edit.is_none() {
            let name = command["title"].as_str().or(command["command"].as_str()).unwrap_or("command");
            self.set_status_message(&format!("\"{name}\" isn't supported in orbvane."));
        }
    }

    // ------------------------------------------------------------------ references

    /// Go to References (⇧F12): asks the server for every use of the symbol at the cursor.
    pub(super) fn find_references(&mut self) {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let (doc_id, pos) = (ed.doc, ed.sel.head);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        if !self.lsp.has_server(&path) {
            let lang = doc.lang;
            return self.explain_no_server("Go to References", lang);
        }
        if !self.lsp.supports(&path, "referencesProvider") {
            return self.set_status_message("No reference provider is available for this file.");
        }
        self.lsp.references(&path, &doc.buffer, pos);
    }

    /// Shows the references: one jumps straight there, several open a picker grouped by
    /// file with each line's text.
    pub(super) fn show_references(&mut self, locations: Vec<lsp::Location>, encoding: Encoding) {
        if locations.is_empty() {
            return self.set_status_message("No references found");
        }
        // Several: the peek view (`editor.gotoLocation.multipleReferences`).
        if locations.len() > 1 {
            return self.open_peek(locations, encoding, "references");
        }
        let mut items = Vec::new();
        let mut files: Vec<(PathBuf, Vec<String>)> = Vec::new();
        let mut sorted = locations;
        sorted.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
        for loc in &sorted {
            // Line text from the open document, or from disk.
            if !files.iter().any(|(p, _)| *p == loc.path) {
                let open = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(loc.path.as_path()));
                let lines: Vec<String> = match open {
                    Some(d) => (0..d.buffer.len_lines()).map(|l| d.buffer.line(l)).collect(),
                    None => std::fs::read_to_string(&loc.path).unwrap_or_default().lines().map(String::from).collect(),
                };
                files.push((loc.path.clone(), lines));
            }
            let lines = &files.iter().find(|(p, _)| *p == loc.path).unwrap().1;
            let line = loc.range.start.line as usize;
            let text = lines.get(line).cloned().unwrap_or_default();
            let col = encoding.from_lsp(&text, loc.range.start.character);
            let rel = self.display_path(&loc.path);
            items.push(crate::palette::Item {
                label: text.trim().to_string(),
                detail: format!("{}:{}", line + 1, col + 1),
                matches: Vec::new(),
                shortcut: None,
                action: crate::palette::Action::Goto(loc.path.clone(), Pos::new(line, col)),
                group: Some(rel),
                kind: None,
            });
        }
        if let [only] = items.as_slice() {
            if let crate::palette::Action::Goto(path, pos) = &only.action {
                let (path, pos) = (path.clone(), *pos);
                return self.goto_location(&path, pos);
            }
        }
        let n = items.len();
        let placeholder = format!("{n} references");
        self.palette = Some(crate::palette::Palette::with_picker(crate::palette::Picker { placeholder, choices: items }));
    }

    /// Opens `path` and puts the cursor at `pos`.
    pub(super) fn goto_location(&mut self, path: &Path, pos: Pos) {
        self.open_file(path);
        self.focus = Focus::Editor;
        if let Some((ed, doc)) = self.active_mut() {
            ed.jump_to(doc, pos);
        }
    }

    // ------------------------------------------------------------------ rename

    /// F2: asks the server whether the symbol at the cursor can be renamed.
    pub(super) fn start_rename(&mut self) {
        let g = self.active_group;
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let (doc_id, pos) = (ed.doc, ed.sel.head);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        if !self.lsp.has_server(&path) || !self.lsp.supports(&path, "renameProvider") {
            return self.set_status_message("No rename provider is available for this file.");
        }
        self.rename = None;
        self.rename_request = Some((g, doc_id, pos));
        let doc = self.docs[doc_id].as_ref().unwrap();
        self.lsp.prepare_rename(&path, &doc.buffer, pos);
    }

    /// The server answered `prepareRename`: open the rename box (or say why not).
    pub(super) fn rename_prepared(&mut self, path: &Path, pos: Pos, range: Option<(lsp::Range, Option<String>)>, error: Option<String>, encoding: Encoding) {
        let Some((g, doc_id, at)) = self.rename_request.take() else { return };
        if at != pos {
            return;
        }
        if let Some(e) = error {
            return self.set_status_message(&e);
        }
        let Some(doc) = self.docs.get(doc_id).and_then(Option::as_ref).filter(|d| d.buffer.path() == Some(path)) else { return };
        let b = &doc.buffer;
        let conv = |p: lsp::Position| b.clamp(Pos::new(p.line as usize, encoding.from_lsp(&b.line(p.line as usize), p.character)));
        let (range, placeholder) = match range {
            Some((r, placeholder)) => ((conv(r.start), conv(r.end)), placeholder),
            None => {
                let w = b.word_at(pos);
                (w.ordered(), None)
            }
        };
        let old_name = b.text_in(&Selection { anchor: range.0, head: range.1, goal_col: None });
        if old_name.is_empty() && placeholder.is_none() {
            return self.set_status_message("The element can't be renamed.");
        }
        let mut field = TextField::default();
        field.set_text(placeholder.as_deref().unwrap_or(&old_name));
        field.select_all();
        self.rename = Some(RenameWidget { group: g, doc: doc_id, path: path.to_path_buf(), pos, range, old_name, field });
        self.focus = Focus::Rename;
    }

    fn submit_rename(&mut self) {
        let Some(w) = self.rename.take() else { return };
        self.focus = Focus::Editor;
        let name = w.field.text.trim().to_string();
        if name.is_empty() || name == w.old_name {
            return;
        }
        if let Some(doc) = self.docs.get(w.doc).and_then(Option::as_ref) {
            self.lsp.rename(&w.path, &doc.buffer, w.pos, &name);
        }
    }

    pub(super) fn cancel_rename(&mut self) {
        if self.rename.take().is_some() && self.focus == Focus::Rename {
            self.focus = Focus::Editor;
        }
    }

    pub(super) fn rename_key(&mut self, k: &KeyInput) {
        match k.key {
            Key::Enter => return self.submit_rename(),
            Key::Escape => return self.cancel_rename(),
            _ => {}
        }
        let Some(w) = &mut self.rename else { return self.focus = Focus::Editor };
        if w.field.key(k) == FieldEvent::Ignored {
            if let Some(cmd) = k.command() {
                self.run(cmd);
            }
        }
    }

    pub(super) fn rename_clipboard(&mut self, cut: bool, paste: bool, select_all: bool) {
        let Some(w) = &mut self.rename else { return };
        if select_all {
            return w.field.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                if let Some(w) = &mut self.rename {
                    w.field.insert(&text);
                }
            }
            return;
        }
        let text = if cut { w.field.cut() } else { w.field.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    pub(super) fn click_rename(&mut self, x: f32, shift: bool) {
        if let Some(w) = &mut self.rename {
            self.focus = Focus::Rename;
            w.field.click(x, shift);
        }
    }

    /// Draws the rename box over the symbol, like the standard rename widget.
    pub(super) fn draw_rename(&mut self, c: &mut Canvas, g: usize, editor_rect: Rect) {
        let Some(w) = &self.rename else { return };
        if w.group != g {
            return;
        }
        let Some((ed, doc)) = self.groups[g].tabs.get(self.groups[g].active).and_then(|ed| Some((ed, self.docs[ed.doc].as_ref()?))) else { return };
        if ed.doc != w.doc {
            return;
        }
        let (x, y) = ed.point_of(doc, w.range.0);
        let style = TextStyle::ui(UI, self.color("input.foreground"));
        let text_w = c.measure(&w.field.text, &style);
        let box_w = (text_w + 30.0).max(220.0);
        let bx = Rect::new((x - 4.0).max(editor_rect.x), (y - 3.0).max(editor_rect.y), box_w, 28.0);
        let hint = "Enter to Rename";
        c.push_layer();
        c.shadow(Rect::new(bx.x, bx.y, bx.w, bx.h + 20.0), 4.0, self.color("widget.shadow"));
        c.fill(Rect::new(bx.x, bx.y, bx.w, bx.h + 20.0), self.color("editorWidget.background"));
        let input = bx.inset(3.0, 3.0);
        c.bordered(input, self.color("input.background"), self.color("focusBorder"), 1.0, 2.0);
        let (ph, sel, caret_on, focused) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"), self.caret_on(), self.focus == Focus::Rename);
        let fr = Rect::new(input.x + 4.0, input.y, input.w - 8.0, input.h);
        if let Some(w) = &mut self.rename {
            w.field.draw(c, fr, &style, "", ph, focused, caret_on, sel);
        }
        let hs = TextStyle::ui(11.0, self.color("descriptionForeground"));
        c.text_in(Rect::new(bx.x + 6.0, bx.bottom(), bx.w - 12.0, 20.0), hint, &hs);
        self.hits.push((Rect::new(bx.x, bx.y, bx.w, bx.h + 20.0), Hit::RenameBox));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_follow_edits() {
        // "let foo = foo + 1;" renaming both `foo` (4..7, 10..13) to `total` (5 chars).
        let edits = [(4, 7, 5), (10, 13, 5)];
        assert_eq!(map_index(2, &edits), 2);
        assert_eq!(map_index(8, &edits), 10);
        assert_eq!(map_index(6, &edits), 6); // inside the first `foo`
        assert_eq!(map_index(13, &edits), 17);
        assert_eq!(map_index(17, &edits), 21);
    }
}
