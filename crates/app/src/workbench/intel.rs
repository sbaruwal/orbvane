//! Language intelligence in the workbench: diagnostics, hover, go to definition and
//! completion, plus the Problems and Output panels. Talks to servers through `crate::lsp`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use language::{Highlighter, Lang};
use lsp::{CompletionItem, Encoding, Severity};
use render::{Canvas, Color, Icon, Rect, TextStyle};
use text::{Pos, Selection};

use super::controls::POPUP_RADIUS;
use super::{Focus, Hit, Workbench, ROW_H, SMALL, UI};
use crate::editor::{severity_color, Doc, Squiggle, font_size, line_height};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::servers::Event;
use crate::palette::fuzzy;

pub(super) const HOVER_DELAY: Duration = Duration::from_millis(500);
const HOVER_MAX_W: f32 = 500.0;
const HOVER_MAX_H: f32 = 320.0;
const SUGGEST_W: f32 = 430.0;
const SUGGEST_ROWS: usize = 12;
/// The suggest widget's details pane.
const DETAILS_W: f32 = 400.0;
const DETAILS_MAX_H: f32 = 320.0;

/// Where the mouse is resting, waiting for the hover delay to pass.
pub(super) struct HoverProbe {
    pub group: usize,
    pub doc: usize,
    pub pos: Pos,
    pub since: Instant,
    pub fired: bool,
}

pub(super) struct HoverState {
    group: usize,
    doc: usize,
    /// The hovered word's range; the popup stays up while the mouse is inside it.
    word: (Pos, Pos),
    pub(super) markdown: Option<String>,
    diagnostics: Vec<(Severity, String)>,
    /// While debugging: the hovered expression's value ("expr: value").
    pub(super) debug: Option<String>,
    /// Over a link: "Follow link (cmd + click)".
    link: bool,
}

pub(super) struct Completion {
    group: usize,
    doc: usize,
    /// Start of the word being completed.
    anchor: Pos,
    seq: u64,
    pub(super) items: Vec<CompletionItem>,
    /// Extensions' suggestions (kept when the server's answer replaces `items`).
    ext_items: Vec<CompletionItem>,
    encoding: Encoding,
    incomplete: bool,
    /// Word completion (not after a trigger character): the user's snippets are offered too.
    with_snippets: bool,
    /// The last item is the Emmet expansion (recomputed as the text changes).
    emmet: bool,
    /// Visible items: (index into `items`, matched char indices in the label).
    pub(super) shown: Vec<(usize, Vec<usize>)>,
    pub(super) selected: usize,
    scroll: usize,
}

/// Moves diagnostics' lines with edits (a line removed takes its diagnostics to the edit's start).
fn shift_lines(diags: &mut [lsp::Diagnostic], edits: &[text::Change]) {
    for change in edits {
        let text::Change::Edit(e) = change else { continue };
        let (start, old_end, new_end) = (e.start.0 as u32, e.old_end.0 as u32, e.new_end.0 as u32);
        let delta = new_end as i64 - old_end as i64;
        for d in diags.iter_mut() {
            for p in [&mut d.range.start, &mut d.range.end] {
                if p.line > old_end {
                    p.line = (p.line as i64 + delta) as u32;
                } else if p.line > start {
                    p.line = start;
                }
            }
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Workbench {
    /// The folder `path`'s server runs in: the workspace folder it's in. A file outside the
    /// folders (a library's source reached with Go to Definition) goes to a server already
    /// running for its language, which knows its project's libraries, else to the first folder;
    /// only without a folder does its own directory count.
    pub(super) fn lsp_root(&self, path: &Path, lang: Lang) -> PathBuf {
        if let Some(folder) = self.folder_of(path) {
            return folder;
        }
        let running = crate::servers::Servers::key_of(lang, Path::new("/")).and_then(|(command, _)| {
            let roots: Vec<PathBuf> = self.lsp.running().into_iter().filter(|k| k.0 == command).map(|k| k.1).collect();
            // A workspace folder's server first, then any other.
            let folders = self.folders();
            roots.iter().find(|r| folders.contains(r)).or(roots.first()).cloned()
        });
        running
            .or_else(|| self.folders().into_iter().next())
            .unwrap_or_else(|| path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf))
    }

    /// Syncs documents with their servers, applies server events and fires a pending hover.
    /// Called at the start of every frame.
    pub(super) fn lsp_tick(&mut self) {
        self.missing_servers_tick();
        // Servers start for the files on screen; a file opened in the background is sent to
        // its server when it's first shown.
        let shown = self.shown_docs();
        let docs: Vec<(PathBuf, Lang, usize)> = self
            .docs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.as_ref().is_some_and(|d| !d.large)) // large files aren't sent to servers
            .filter_map(|(i, d)| Some((d.as_ref()?.buffer.path()?.to_path_buf(), d.as_ref()?.lang, i)))
            .collect();
        for (path, lang, i) in docs {
            if !shown.contains(&i) && !self.lsp.is_open(&path) {
                continue;
            }
            let root = self.lsp_root(&path, lang);
            let doc = self.docs[i].as_ref().unwrap();
            self.lsp.sync(&path, lang, &doc.buffer, &root);
        }
        let shown_paths: Vec<PathBuf> = shown.iter().filter_map(|&i| Some(self.docs.get(i)?.as_ref()?.buffer.path()?.to_path_buf())).collect();
        self.lsp.set_shown(shown_paths.iter().map(PathBuf::as_path));
        self.idle_servers_tick();
        for event in self.lsp.poll() {
            self.handle_lsp_event(event);
        }
        self.ext_diagnostics_republished();
        self.lightbulb_refresh();
        self.shift_diagnostics();
        self.format_save_tick();
        self.lightbulb_tick();
        self.sync_snippet();
        self.outline_tick();
        self.symbol_search_tick();
        self.signature_tick();
        self.inlay_tick();
        self.lens_tick();
        self.testing_tick();
        self.semantic_tick();
        self.folding_tick();
        self.fire_hover_probe();
    }

    /// Moves diagnostics with the lines they're on as the text is edited, until the server
    /// publishes new ones (some only update on save), like the standard markers.
    fn shift_diagnostics(&mut self) {
        for path in std::mem::take(&mut self.lsp.published) {
            self.diag_seq.remove(&path);
        }
        for doc in self.docs.iter().flatten() {
            let Some(path) = doc.buffer.path() else { continue };
            let seq = doc.buffer.edit_seq();
            let Some(&old) = self.diag_seq.get(path) else {
                self.diag_seq.insert(path.to_path_buf(), seq);
                continue;
            };
            if old == seq {
                continue;
            }
            self.diag_seq.insert(path.to_path_buf(), seq);
            let Some(edits) = doc.buffer.edits_since(old) else { continue };
            if let Some((_, diags)) = self.lsp.diagnostics.get_mut(path) {
                shift_lines(diags, edits);
            }
            // Extensions' diagnostics move too, so they stay put when merged again.
            for files in self.ext_languages.diagnostics.values_mut() {
                if let Some(diags) = files.get_mut(path) {
                    shift_lines(diags, edits);
                }
            }
        }
    }

    pub(super) fn close_lsp_doc(&mut self, path: &Path) {
        self.lsp.close(path);
        if self.hover.as_ref().is_some_and(|h| self.docs.get(h.doc).is_none_or(Option::is_none)) {
            self.hover = None;
        }
    }

    fn handle_lsp_event(&mut self, event: Event) {
        match event {
            Event::Hover { path, pos, markdown } => self.add_hover(&path, pos, markdown),
            Event::Definition { locations, encoding } => {
                if locations.is_empty() && self.ext_definition_fallback() {
                    return;
                }
                self.handle_definition(locations, encoding);
            }
            Event::PrepareRename { path, pos, range, error, encoding } => self.rename_prepared(&path, pos, range, error, encoding),
            Event::Edit { edit, encoding, reply } => {
                let applied = self.apply_workspace_edit(&edit, encoding);
                if let Some((key, id)) = reply {
                    self.lsp.respond(&key, id, serde_json::json!({ "applied": applied }));
                }
            }
            Event::Failed { message } => self.set_status_message(&message),
            Event::ExtResponse { key, id, result } if self.mcp_response(&key, id, &result) => {}
            Event::ExtResponse { key, id, result } => self.testing_server_event(&key, crate::testing::ServerEvent::Response { id, result: &result }),
            Event::ExtNotification { key, method, params } => {
                self.testing_server_event(&key, crate::testing::ServerEvent::Notification { method: &method, params: &params })
            }
            Event::References { locations, encoding } => self.show_references(locations, encoding),
            Event::Symbols { path, version, symbols, encoding } => {
                self.outline_symbols(&path, version, symbols, encoding);
                self.refresh_open_palette(crate::palette::Mode::Symbols);
            }
            Event::SignatureHelp { seq, help } => self.signature_help_arrived(seq, help),
            Event::SemanticTokens { path, version, data, legend, encoding } => self.semantic_arrived(&path, version, data, legend, encoding),
            Event::FoldingRanges { path, version, ranges } => self.folding_arrived(&path, version, ranges),
            Event::InlayHints { path, version, hints, encoding } => self.inlay_hints_arrived(&path, version, hints, encoding),
            Event::CodeLenses { path, version, lenses, encoding } => self.code_lenses_arrived(&path, version, lenses, encoding),
            Event::LensResolved { path, version, index, lens } => self.code_lens_resolved(&path, version, index, lens),
            Event::CallRoots { items, encoding } => self.call_roots_arrived(items, encoding),
            Event::Calls { node, incoming, seq, calls, encoding } => self.calls_arrived(node, incoming, seq, calls, encoding),
            Event::TypeRoots { items, encoding } => self.type_roots_arrived(items, encoding),
            Event::Types { node, supertypes, seq, items, encoding } => self.types_arrived(node, supertypes, seq, items, encoding),
            Event::DocumentColors { path, version, colors, encoding } => self.colors_arrived(path, version, colors, encoding),
            Event::AutoInsert { path, version, pos, snippet } => self.auto_insert_arrived(&path, version, pos, &snippet),
            Event::LinkedEditing { seq, ranges, word_pattern, encoding } => self.linked_editing_arrived(seq, ranges, word_pattern, encoding),
            Event::WorkspaceSymbols { seq, symbols, encoding } => self.workspace_symbols_arrived(seq, symbols, encoding),
            Event::CodeActions { actions, encoding, key, auto } => {
                let source = super::refactor::ActionSource::Server(key, encoding);
                let actions = actions.into_iter().map(|a| (a, source.clone())).collect();
                match auto {
                    None => self.code_action_answer(actions),
                    Some(seq) => self.lightbulb_actions(seq, actions),
                }
            }
            Event::Formatted { path, version, edits, encoding, save } => self.formatted(&path, version, edits, encoding, save),
            Event::ResolvedAction { action, encoding, key } => self.finish_code_action(action, super::refactor::ActionSource::Server(key, encoding)),
            Event::Completion { seq, items, incomplete, encoding } => {
                let Some(comp) = &mut self.completion else { return };
                if comp.seq != seq {
                    return;
                }
                comp.items = items;
                comp.items.extend(comp.ext_items.iter().cloned());
                comp.emmet = false;
                if comp.with_snippets {
                    let snippets = self.snippet_completions();
                    let comp = self.completion.as_mut().unwrap();
                    comp.items.extend(snippets);
                }
                let comp = self.completion.as_mut().unwrap();
                comp.incomplete = incomplete;
                comp.encoding = encoding;
                self.refilter_completion();
            }
        }
    }

    /// Shows a hover for `pos` of `path` (from the server or an extension), under any already
    /// shown for the same word.
    pub(super) fn add_hover(&mut self, path: &Path, pos: Pos, markdown: String) {
        let Some(h) = &mut self.hover else { return };
        let same_doc = self.docs[h.doc].as_ref().and_then(|d| d.buffer.path()) == Some(path);
        if !same_doc || pos < h.word.0 || pos > h.word.1 {
            return;
        }
        h.markdown = Some(match h.markdown.take().filter(|m| !m.trim().is_empty()) {
            Some(old) if !markdown.trim().is_empty() => format!("{old}\n\n---\n\n{markdown}"),
            Some(old) => old,
            None => markdown,
        });
    }

    /// Extensions' suggestions for completion request `seq` arrived.
    pub(super) fn add_ext_completions(&mut self, seq: u64, items: Vec<CompletionItem>) {
        let Some(comp) = &mut self.completion else { return };
        if comp.seq != seq {
            return;
        }
        let at = if comp.emmet { comp.items.len().saturating_sub(1) } else { comp.items.len() };
        comp.items.splice(at..at, items.iter().cloned());
        comp.ext_items.extend(items);
        self.refilter_completion();
    }

    /// Goes to a definition: the peek view for several (or Peek Definition), else the place.
    pub(super) fn handle_definition(&mut self, locations: Vec<lsp::Location>, encoding: Encoding) {
        // Peek Definition, or several definitions: the peek view
        // (`editor.gotoLocation.multipleDefinitions`).
        if std::mem::take(&mut self.peek_definition) || locations.len() > 1 {
            return self.open_peek(locations, encoding, "definitions");
        }
        let Some(loc) = locations.into_iter().next() else { return };
        self.open_file(&loc.path);
        self.focus = Focus::Editor;
        if let Some((ed, doc)) = self.active_mut() {
            let line = loc.range.start.line as usize;
            let col = encoding.from_lsp(&doc.buffer.line(line), loc.range.start.character);
            ed.jump_to(doc, Pos::new(line, col));
        }
    }

    /// Diagnostics for a document as editor squiggles.
    pub(super) fn squiggles_for(&self, doc: &Doc) -> Vec<Squiggle> {
        let Some(path) = doc.buffer.path() else { return Vec::new() };
        let Some((encoding, diags)) = self.lsp.diagnostics.get(path) else { return Vec::new() };
        let b = &doc.buffer;
        let last = b.len_lines().saturating_sub(1);
        let conv = |p: lsp::Position| {
            let line = (p.line as usize).min(last);
            Pos::new(line, encoding.from_lsp(&b.line(line), p.character))
        };
        let mut out: Vec<Squiggle> = diags
            .iter()
            .map(|d| Squiggle { start: conv(d.range.start), end: conv(d.range.end), severity: d.severity })
            .collect();
        // Draw the most severe last so it sits on top.
        out.sort_by(|a, b| b.severity.cmp(&a.severity));
        out
    }

    // ------------------------------------------------------------------ hover

    /// Tracks the mouse over editor text for hover. Called from `mouse_move`.
    pub(super) fn hover_mouse(&mut self, x: f32, y: f32, over: Option<Hit>) {
        if matches!(over, Some(Hit::HoverPopup)) {
            return;
        }
        let at = match over {
            Some(Hit::Editor(g)) => self.groups.get(g).and_then(|gr| gr.tabs.get(gr.active)).and_then(|ed| {
                let doc = self.docs[ed.doc].as_ref()?;
                Some((g, ed.doc, ed.pos_at_strict(doc, x, y)?))
            }),
            _ => None,
        };
        let Some((group, doc, pos)) = at else {
            self.hover = None;
            self.hover_probe = None;
            return;
        };
        if let Some(h) = &self.hover {
            if h.group == group && h.doc == doc && pos >= h.word.0 && pos < h.word.1.max(Pos::new(h.word.1.line, h.word.1.col + 1)) {
                return;
            }
            self.hover = None;
        }
        if self.hover_probe.as_ref().is_some_and(|p| p.group == group && p.doc == doc && p.pos == pos) {
            return;
        }
        self.hover_probe = Some(HoverProbe { group, doc, pos, since: Instant::now(), fired: false });
    }

    pub(super) fn dismiss_hover(&mut self) {
        self.hover = None;
        self.hover_probe = None;
    }

    pub(super) fn fire_hover_probe(&mut self) {
        let Some(probe) = &mut self.hover_probe else { return };
        if probe.fired || probe.since.elapsed() < HOVER_DELAY || self.palette.is_some() || self.drag.is_some() {
            return;
        }
        probe.fired = true;
        let (group, doc_id, pos) = (probe.group, probe.doc, probe.pos);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let sel = doc.buffer.word_at(pos);
        let word = if sel.is_empty() { (pos, Pos::new(pos.line, pos.col + 1)) } else { sel.ordered() };
        let diagnostics: Vec<(Severity, String)> = self
            .squiggles_for(doc)
            .iter()
            .zip(self.diagnostics_for(doc))
            .filter(|(s, _)| s.start <= pos && (pos < s.end || (s.start == s.end && pos == s.start)))
            .map(|(s, msg)| (s.severity, msg))
            .collect();
        // While stopped in the debugger the hovered expression's value comes first; the
        // server's hover is the fallback (`debug_hover_failed`).
        let debugging = self.debug_hover(doc_id, pos);
        let doc = self.docs[doc_id].as_ref().unwrap();
        if let Some(path) = doc.buffer.path().map(Path::to_path_buf).filter(|_| !debugging) {
            self.lsp.hover(&path, &doc.buffer, pos);
        }
        let link = self.settings.bool("editor.links") && crate::editor::link_at(&doc.buffer.line(pos.line), pos.col).is_some();
        let decorations = self.ext_decoration_hovers(doc_id, pos);
        let markdown = (!decorations.is_empty()).then(|| decorations.join("\n\n---\n\n"));
        self.hover = Some(HoverState { group, doc: doc_id, word, markdown, diagnostics, debug: None, link });
        if !debugging {
            self.ext_provide_hover(doc_id, pos);
        }
    }

    /// Diagnostic messages in the same order as `squiggles_for` returns them.
    fn diagnostics_for(&self, doc: &Doc) -> Vec<String> {
        let Some((_, diags)) = doc.buffer.path().and_then(|p| self.lsp.diagnostics.get(p)) else { return Vec::new() };
        let mut d: Vec<&lsp::Diagnostic> = diags.iter().collect();
        d.sort_by(|a, b| b.severity.cmp(&a.severity));
        d.iter()
            .map(|d| {
                let mut msg = d.message.clone();
                match (&d.source, &d.code) {
                    (Some(s), Some(c)) => msg.push_str(&format!("  {s}({c})")),
                    (Some(s), None) => msg.push_str(&format!("  {s}")),
                    _ => {}
                }
                msg
            })
            .collect()
    }

    /// The debug hover's evaluation for (`doc`, `pos`) arrived: show it, or on failure ask the
    /// language server instead.
    pub(super) fn debug_hover_arrived(&mut self, doc_id: usize, pos: Pos, value: Result<String, String>) {
        let Some(h) = &mut self.hover else { return };
        if h.doc != doc_id || pos < h.word.0 || pos > h.word.1 {
            return;
        }
        match value {
            Ok(v) => h.debug = Some(v),
            Err(_) => {
                let Some(doc) = self.docs[doc_id].as_ref() else { return };
                if let Some(path) = doc.buffer.path().map(Path::to_path_buf) {
                    self.lsp.hover(&path, &doc.buffer, pos);
                }
            }
        }
    }

    pub(super) fn hover_deadline(&self) -> Option<Instant> {
        self.hover_probe.as_ref().filter(|p| !p.fired).map(|p| p.since + HOVER_DELAY)
    }

    // ------------------------------------------------------------------ context menu

    /// The editor's right-click menu. A click outside the selection moves the caret there
    /// first, so the menu acts on what was clicked.
    pub(super) fn editor_context_menu(&mut self, g: usize, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        use crate::commands::Command;
        self.active_group = g;
        self.focus = Focus::Editor;
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        if let Some(pos) = ed.pos_at_strict(doc, x, y) {
            let inside = ed.selections().iter().any(|s| {
                let (a, b) = s.ordered();
                !s.is_empty() && a <= pos && pos <= b
            });
            if !inside {
                ed.click(doc, x, y, 1, false);
            }
        }
        let (special, has_selection) = (ed.is_special(), !ed.sel.is_empty());
        let item = |label: &str| PopupItem::Item { label: label.into(), enabled: true, checked: None };
        let run = PopupAction::Run;
        let sep = || (PopupItem::Separator, PopupAction::None);
        let mut entries = Vec::new();
        if !special {
            let peek = vec![
                (item("Peek Definition"), run(Command::PeekDefinition)),
                (item("Peek Call Hierarchy"), run(Command::ShowCallHierarchy)),
                (item("Peek Type Hierarchy"), run(Command::ShowTypeHierarchy)),
            ];
            entries.extend([
                (item("Go to Definition"), run(Command::GoToDefinition)),
                (item("Go to References"), run(Command::GoToReferences)),
                (item("Peek"), PopupAction::Submenu(peek)),
                sep(),
                (item("Rename Symbol"), run(Command::Rename)),
                (item("Quick Fix..."), run(Command::QuickFix)),
                if has_selection { (item("Format Selection"), run(Command::FormatSelection)) } else { (item("Format Document"), run(Command::FormatDocument)) },
                sep(),
                (item("Cut"), run(Command::Cut)),
            ]);
        }
        entries.push((item("Copy"), run(Command::Copy)));
        if !special {
            entries.push((item("Paste"), run(Command::Paste)));
        }
        entries.extend([sep(), (item("Command Palette..."), run(Command::CommandPalette))]);
        self.show_popup(entries, x, y);
    }

    // ------------------------------------------------------------------ go to definition

    pub(super) fn go_to_definition(&mut self, at: Option<Pos>) {
        let Some(ed) = self.active_editor() else { return };
        let pos = at.unwrap_or(ed.sel.head);
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        let (doc_id, lang, has_server) = (ed.doc, doc.lang, self.lsp.has_server(&path));
        if has_server {
            self.lsp.definition(&path, &doc.buffer, pos);
        }
        if !self.ext_provide_definition(doc_id, pos, has_server) && !has_server {
            self.explain_no_server("Go to Definition", lang);
        }
    }

    // ------------------------------------------------------------------ completion

    /// Keys the suggest widget consumes while open. Returns true if handled.
    pub(super) fn completion_key(&mut self, k: &KeyInput) -> bool {
        let Some(comp) = &mut self.completion else { return false };
        if comp.shown.is_empty() {
            if k.key == Key::Escape {
                self.completion = None;
                return true;
            }
            return false;
        }
        let n = comp.shown.len();
        let step = |sel: usize, d: isize| (sel as isize + d).rem_euclid(n as isize) as usize;
        match k.key {
            Key::Up => comp.selected = step(comp.selected, -1),
            Key::Down => comp.selected = step(comp.selected, 1),
            Key::PageUp => comp.selected = comp.selected.saturating_sub(SUGGEST_ROWS),
            Key::PageDown => comp.selected = (comp.selected + SUGGEST_ROWS).min(n - 1),
            Key::Enter | Key::Tab if !k.shift => {
                self.accept_completion();
                return true;
            }
            Key::Escape => {
                self.completion = None;
                return true;
            }
            _ => return false,
        }
        if comp.selected < comp.scroll {
            comp.scroll = comp.selected;
        } else if comp.selected >= comp.scroll + SUGGEST_ROWS {
            comp.scroll = comp.selected + 1 - SUGGEST_ROWS;
        }
        true
    }

    /// Opens, updates or closes completion after the editor handled a key.
    pub(super) fn after_editor_key(&mut self, k: &KeyInput) {
        let typed = match (&k.key, &k.text) {
            (Key::Char(_), Some(t)) if !k.cmd && !k.ctrl => Some(t.clone()),
            _ => None,
        };
        let Some(typed) = typed else {
            match k.key {
                Key::Backspace if self.completion.is_some() => self.refilter_completion(),
                Key::Up | Key::Down | Key::PageUp | Key::PageDown if self.completion.is_none() => {}
                _ => self.completion = None,
            }
            return;
        };
        self.signature_after_typing(&typed);
        let mut chars = typed.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            self.completion = None;
            return;
        };
        // editor.formatOnType: the server may adjust the line (rust-analyzer adds the `;`
        // after `let x =`, indents a `.` chain...).
        if self.settings.bool("editor.formatOnType") && !ch.is_alphanumeric() {
            if let Some(ed) = self.active_editor().filter(|e| e.extra.is_empty()) {
                let (doc_id, pos) = (ed.doc, ed.sel.head);
                if let Some(doc) = self.docs[doc_id].as_ref() {
                    if let Some(path) = doc.buffer.path().map(Path::to_path_buf) {
                        self.lsp.format_on_type(&path, &doc.buffer, pos, ch, Self::formatting_options());
                    }
                }
            }
        }
        self.auto_insert_after_typing(ch);
        if is_word_char(ch) {
            match &self.completion {
                Some(c) if !c.incomplete => self.refilter_completion(),
                Some(_) => self.trigger_completion(None, true),
                None => self.trigger_completion(None, false),
            }
            return;
        }
        // Trigger characters (in Rust and C++ ':' only as "::").
        let path = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf));
        let is_trigger = path.is_some_and(|p| self.lsp.completion_triggers(&p).iter().any(|t| t == &typed))
            || self.active_editor().is_some_and(|ed| self.ext_completion_trigger(ed.doc, &typed));
        let double_colon = self.active_doc().is_some_and(|d| matches!(d.lang, Lang::Rust | Lang::C | Lang::Cpp));
        let prev_colon = ch == ':'
            && (!double_colon || self.active_editor().zip(self.active_doc()).is_some_and(|(ed, doc)| {
                let h = ed.sel.head;
                h.col >= 2 && doc.buffer.line(h.line).chars().nth(h.col - 2) == Some(':')
            }));
        if is_trigger && (ch != ':' || prev_colon) {
            self.trigger_completion(Some(typed), false);
        } else {
            self.completion = None;
            self.emmet_after_typing();
        }
    }

    /// Requests completions at the caret. `keep_anchor` re-requests for an open widget.
    pub(super) fn trigger_completion(&mut self, trigger: Option<String>, keep_anchor: bool) {
        let group = self.active_group;
        let Some(ed) = self.active_editor() else { return };
        let (doc_id, caret) = (ed.doc, ed.sel.head);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        let line: Vec<char> = doc.buffer.line(caret.line).chars().collect();
        let mut start = caret.col.min(line.len());
        while start > 0 && is_word_char(line[start - 1]) {
            start -= 1;
        }
        let anchor = match &self.completion {
            Some(c) if keep_anchor => c.anchor,
            _ => Pos::new(caret.line, start),
        };
        self.completion_seq += 1;
        let seq = self.completion_seq;
        let with_snippets = trigger.is_none();
        if !self.lsp.has_server(&path) {
            // No language server: only the user's snippets (on words).
            let snippets = if with_snippets { self.snippet_completions() } else { Vec::new() };
            let ext = self.ext_has_completion(doc_id);
            if snippets.is_empty() && self.emmet_completion(Encoding::Utf16, "").is_none() && !ext {
                return;
            }
            self.completion = Some(Completion {
                group,
                doc: doc_id,
                anchor,
                seq,
                items: snippets,
                ext_items: Vec::new(),
                encoding: Encoding::Utf16,
                incomplete: false,
                with_snippets,
                emmet: false,
                shown: Vec::new(),
                selected: 0,
                scroll: 0,
            });
            self.ext_provide_completion(doc_id, caret, trigger.as_deref(), seq);
            return self.refilter_completion();
        }
        self.lsp.completion(&path, &doc.buffer, caret, trigger.as_deref(), seq);
        self.ext_provide_completion(doc_id, caret, trigger.as_deref(), seq);
        match &mut self.completion {
            Some(c) if keep_anchor => {
                c.ext_items.clear();
                return c.seq = seq;
            }
            _ => {
                self.completion = Some(Completion {
                    group,
                    doc: doc_id,
                    anchor,
                    seq,
                    items: Vec::new(),
                    ext_items: Vec::new(),
                    encoding: Encoding::Utf16,
                    incomplete: false,
                    with_snippets,
                    emmet: false,
                    shown: Vec::new(),
                    selected: 0,
                    scroll: 0,
                })
            }
        }
        // The Emmet expansion shows while the server works.
        self.refilter_completion();
    }

    /// HTML: asks the server for the end tag after `>` or `</`, or quotes after `=`
    /// (`html.autoClosingTags`, `html.autoCreateQuotes`).
    fn auto_insert_after_typing(&mut self, ch: char) {
        let kind = match ch {
            '>' | '/' if self.settings.bool("html.autoClosingTags") => "autoClose",
            '=' if self.settings.bool("html.autoCreateQuotes") => "autoQuote",
            _ => return,
        };
        let Some(ed) = self.active_editor().filter(|e| e.extra.is_empty() && e.sel.is_empty()) else { return };
        let (doc_id, caret) = (ed.doc, ed.sel.head);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        self.lsp.auto_insert(&path, &doc.buffer, caret, kind);
    }

    /// The server's answer to `auto_insert_after_typing`: inserted if nothing changed since.
    pub(super) fn auto_insert_arrived(&mut self, path: &Path, version: u64, pos: Pos, snippet: &str) {
        let Some(ed) = self.active_editor().filter(|e| e.extra.is_empty() && e.sel.is_empty() && e.sel.head == pos) else { return };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        if doc.buffer.version() != version || doc.buffer.path() != Some(path) {
            return;
        }
        self.insert_snippet(pos, pos, snippet);
        // Inside the new quotes: the attribute's values.
        if snippet.starts_with('"') {
            self.trigger_completion(Some("\"".into()), false);
        }
    }

    /// After a character that closes the suggest widget (`>`, `.`, `*`...): opens it with just
    /// the Emmet expansion, if the text before the caret is an abbreviation.
    fn emmet_after_typing(&mut self) {
        let group = self.active_group;
        let Some(ed) = self.active_editor() else { return };
        let (doc, caret) = (ed.doc, ed.sel.head);
        if self.emmet_completion(Encoding::Utf16, "").is_none() {
            return;
        }
        self.completion_seq += 1;
        self.completion = Some(Completion {
            group,
            doc,
            anchor: caret,
            seq: self.completion_seq,
            items: Vec::new(),
            ext_items: Vec::new(),
            encoding: Encoding::Utf16,
            incomplete: false,
            with_snippets: false,
            emmet: false,
            shown: Vec::new(),
            selected: 0,
            scroll: 0,
        });
        self.refilter_completion();
    }

    /// Filters and sorts completion items by the text typed since the anchor.
    fn refilter_completion(&mut self) {
        let Some(comp) = &self.completion else { return };
        let Some(ed) = self.groups.get(comp.group).and_then(|g| g.tabs.get(g.active)) else {
            self.completion = None;
            return;
        };
        let caret = ed.sel.head;
        if ed.doc != comp.doc || caret.line != comp.anchor.line || caret.col < comp.anchor.col {
            self.completion = None;
            return;
        }
        let Some(doc) = self.docs[comp.doc].as_ref() else { return };
        let prefix: String = doc.buffer.line(caret.line).chars().skip(comp.anchor.col).take(caret.col - comp.anchor.col).collect();
        let emmet = self.emmet_completion(comp.encoding, &prefix);
        let comp = self.completion.as_mut().unwrap();
        if comp.emmet {
            comp.items.pop();
            comp.emmet = false;
        }
        let mut first = None;
        if let Some((item, pinned)) = emmet {
            comp.items.push(item);
            comp.emmet = true;
            first = pinned.then_some(comp.items.len() - 1);
        }
        let mut scored: Vec<(i32, &str, usize, Vec<usize>)> = comp
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let (score, _) = fuzzy(&prefix, &item.filter_text)?;
                let matches = fuzzy(&prefix, &item.label).map(|m| m.1).unwrap_or_default();
                Some((score, item.sort_text.as_str(), i, matches))
            })
            .collect();
        if prefix.is_empty() {
            scored.sort_by(|a, b| a.1.cmp(b.1));
        } else {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        }
        comp.shown = scored.into_iter().map(|(_, _, i, m)| (i, m)).collect();
        if let Some(i) = first {
            // Not matched by the word being typed: shown first regardless.
            comp.shown.retain(|(j, _)| *j != i);
            comp.shown.insert(0, (i, Vec::new()));
        }
        comp.selected = 0;
        comp.scroll = 0;
    }

    fn accept_completion(&mut self) {
        let command = self.completion.as_ref().and_then(|c| Some(c.items[c.shown.get(c.selected)?.0].command.clone()?));
        self.insert_completion();
        // A CSS property opens the value suggestions next.
        if command.as_deref() == Some("editor.action.triggerSuggest") {
            self.trigger_completion(None, false);
        }
    }

    fn insert_completion(&mut self) {
        let Some(comp) = self.completion.take() else { return };
        let Some(&(index, _)) = comp.shown.get(comp.selected) else { return };
        let item = comp.items[index].clone();
        let encoding = comp.encoding;
        let group = &mut self.groups[comp.group];
        let Some(ed) = group.tabs.get_mut(group.active) else { return };
        let Some(doc) = self.docs[comp.doc].as_mut() else { return };
        let b = &mut doc.buffer;
        let conv = |b: &text::Buffer, p: lsp::Position| {
            let line = p.line as usize;
            Pos::new(line, encoding.from_lsp(&b.line(line), p.character))
        };
        let caret = ed.sel.head;
        let (start, end, text) = match &item.edit {
            // The edit's range was computed when the request was sent; extend it over
            // whatever was typed since.
            Some(edit) => {
                let end = conv(b, edit.range.end);
                let end = if end.line == caret.line && end.col < caret.col { caret } else { end };
                (conv(b, edit.range.start), end, edit.new_text.clone())
            }
            None => (comp.anchor, caret, item.insert_text.clone()),
        };
        if !ed.extra.is_empty() {
            // Multiple cursors: insert at each one, replacing the same typed prefix (the
            // additional edits are skipped then). Snippets go in as plain text.
            let text = if item.snippet { crate::snippet::parse(&text, &()).text } else { text };
            ed.accept_completion_everywhere(doc, start, end, &text);
            return;
        }
        if item.snippet {
            // Extra edits first (they're above the insertion), then the snippet.
            let mut extra: Vec<_> = item.additional_edits.iter().map(|e| (conv(b, e.range.start), conv(b, e.range.end), e)).collect();
            extra.sort_by(|a, b| b.0.cmp(&a.0));
            let first = start;
            let (mut start, mut end) = (start, end);
            for (s, e, edit) in extra.into_iter().filter(|(s, _, _)| *s < first) {
                b.insert(Selection { anchor: s, head: e, goal_col: None }, &edit.new_text);
                let added = edit.new_text.matches('\n').count() as isize - (e.line - s.line) as isize;
                start.line = (start.line as isize + added).max(0) as usize;
                end.line = (end.line as isize + added).max(0) as usize;
            }
            b.break_undo_group();
            self.insert_snippet(start, end, &text);
            return;
        }
        let mut sel = b.insert(Selection { anchor: start, head: end, goal_col: None }, &text);
        // Extra edits (usually auto-imports near the top). Apply bottom-up and shift the caret.
        let mut extra: Vec<_> = item.additional_edits.iter().map(|e| (conv(b, e.range.start), conv(b, e.range.end), e)).collect();
        extra.sort_by(|a, b| b.0.cmp(&a.0));
        for (s, e, edit) in extra {
            if s >= start {
                continue; // overlapping or after the main edit; skip to stay safe
            }
            b.insert(Selection { anchor: s, head: e, goal_col: None }, &edit.new_text);
            let added = edit.new_text.matches('\n').count() as isize - (e.line - s.line) as isize;
            sel.head.line = (sel.head.line as isize + added).max(0) as usize;
        }
        b.break_undo_group();
        ed.set_selection(Selection::caret(b.clamp(sel.head)));
        ed.reveal = true;
    }

    // ------------------------------------------------------------------ drawing

    /// Draws the hover popup and suggest widget on a layer above the editors.
    pub(super) fn draw_intel_overlays(&mut self, c: &mut Canvas) {
        c.push_layer();
        self.draw_signature(c);
        self.draw_completion(c);
        self.draw_hover(c);
    }

    fn draw_hover(&mut self, c: &mut Canvas) {
        let Some(h) = &self.hover else { return };
        if h.markdown.is_none() && h.diagnostics.is_empty() && h.debug.is_none() && !h.link {
            return;
        }
        let Some(ed) = self.groups.get(h.group).and_then(|g| g.tabs.get(g.active)) else { return };
        let Some(doc) = self.docs[h.doc].as_ref() else { return };
        if ed.doc != h.doc {
            return;
        }
        let (wx, wy) = ed.point_of(doc, h.word.0);
        let view = ed.geom.text;

        // Build the content: diagnostics first, then the server's markdown.
        let fg = self.color("editorHoverWidget.foreground");
        let text_style = TextStyle::ui(UI, fg);
        let code_style = TextStyle::mono(font_size(), line_height(), fg);
        let max_w = HOVER_MAX_W.min(self.main_rect.w - 40.0);
        let mut blocks: Vec<Block> = Vec::new();
        if let Some(v) = &h.debug {
            blocks.push(Block::Code(doc.lang, v.lines().map(String::from).collect()));
        }
        if h.link {
            blocks.push(Block::Text("Follow link (cmd + click)".into()));
        }
        for (sev, msg) in &h.diagnostics {
            blocks.push(Block::Diagnostic(*sev, msg.clone()));
        }
        if let Some(md) = &h.markdown {
            if !blocks.is_empty() {
                blocks.push(Block::Rule);
            }
            blocks.extend(parse_markdown(md));
        }
        let pad = 8.0;
        let inner_w = max_w - pad * 2.0;
        let rows = layout_blocks(c, &blocks, &text_style, inner_w);
        let content_w = rows.iter().map(|r| row_width(c, r, &text_style, &code_style)).fold(0.0f32, f32::max).min(inner_w);
        let content_h: f32 = rows.iter().map(|r| row_height(r, &text_style)).sum();
        let w = content_w + pad * 2.0;
        let h_total = (content_h + pad * 2.0).min(HOVER_MAX_H);

        // Above the word if there's room, otherwise below it.
        let y = if wy - h_total - 4.0 >= view.y { wy - h_total - 4.0 } else { wy + line_height() + 4.0 };
        let x = wx.min(self.main_rect.right() - w - 8.0).max(self.main_rect.x + 4.0);
        let r = Rect::new(x.round(), y.round(), w.round(), h_total.round());
        c.shadow(r, POPUP_RADIUS, self.color("widget.shadow"));
        c.bordered(r, self.color("editorHoverWidget.background"), self.color("editorHoverWidget.border"), 1.0, POPUP_RADIUS);
        c.push_clip(r.inset(1.0, 1.0));
        draw_rows(c, &self.theme, &rows, r, r.y + pad, pad, &text_style, &code_style);
        c.pop_clip();
        self.hits.push((r, Hit::HoverPopup));
    }

    fn draw_completion(&mut self, c: &mut Canvas) {
        let Some(comp) = &self.completion else { return };
        if comp.shown.is_empty() {
            return;
        }
        let Some(ed) = self.groups.get(comp.group).and_then(|g| g.tabs.get(g.active)) else { return };
        let Some(doc) = self.docs[comp.doc].as_ref() else { return };
        let (ax, ay) = ed.point_of(doc, comp.anchor);
        let view = ed.geom.text;
        let rows = comp.shown.len().min(SUGGEST_ROWS);
        let h = rows as f32 * ROW_H + 8.0;
        let below = ay + line_height();
        let y = if below + h <= view.bottom() || ay - h < view.y { below } else { ay - h };
        let x = (ax - 26.0).min(self.main_rect.right() - SUGGEST_W - 4.0).max(self.main_rect.x);
        let r = Rect::new(x.round(), y.round(), SUGGEST_W, h);
        c.shadow(r, POPUP_RADIUS, self.color("widget.shadow"));
        c.bordered(r, self.color("editorSuggestWidget.background"), self.color("editorSuggestWidget.border"), 1.0, POPUP_RADIUS);
        self.hits.push((r, Hit::CompletionBox));
        let fg = self.color("editorSuggestWidget.foreground");
        let hl = self.color("editorSuggestWidget.highlightForeground");
        let dim = self.color("descriptionForeground");
        let style = TextStyle::ui(UI, fg);
        let detail_style = TextStyle::ui(12.0, dim);
        c.push_clip(r.inset(1.0, 1.0));
        let mut hits = Vec::new();
        for (row, idx) in (comp.scroll..(comp.scroll + SUGGEST_ROWS).min(comp.shown.len())).enumerate() {
            let (item_idx, matches) = &comp.shown[idx];
            let item = &comp.items[*item_idx];
            let rr = Rect::new(r.x + 4.0, r.y + 4.0 + row as f32 * ROW_H, r.w - 8.0, ROW_H);
            if idx == comp.selected {
                c.fill_rounded(rr, self.color("editorSuggestWidget.selectedBackground"), 5.0);
            } else if self.hover_hit == Some(Hit::CompletionRow(idx)) {
                c.fill_rounded(rr, self.color("list.hoverBackground"), 5.0);
            }
            let (icon, color) = kind_icon(&self.theme, item.kind);
            c.icon(icon, rr.x + 6.0, rr.y + 3.0, 16.0, color);
            let spans: Vec<(usize, usize, Color)> = item
                .label
                .char_indices()
                .enumerate()
                .filter(|(ci, _)| matches.contains(ci))
                .map(|(_, (bi, ch))| (bi, bi + ch.len_utf8(), hl))
                .collect();
            let ty = rr.y + ((ROW_H - style.line_height) / 2.0).round();
            let lw = c.rich_text(rr.x + 28.0, ty, &item.label, &spans, &style);
            if let Some(ld) = &item.label_detail {
                c.text(rr.x + 28.0 + lw, ty + 1.0, ld, &detail_style);
            }
            if let Some(detail) = &item.detail {
                let dw = c.measure(detail, &detail_style).min(rr.w * 0.5);
                let dr = Rect::new(rr.right() - dw - 8.0, rr.y, dw, rr.h);
                c.push_clip(dr);
                c.text_in(dr, detail, &detail_style);
                c.pop_clip();
            }
            hits.push((rr, Hit::CompletionRow(idx)));
        }
        c.pop_clip();
        self.hits.extend(hits);
        self.draw_completion_details(c, r);
    }

    /// The details pane beside the suggest widget: the selected item's detail and
    /// documentation.
    fn draw_completion_details(&mut self, c: &mut Canvas, list: Rect) {
        let Some(comp) = &self.completion else { return };
        let Some(item) = comp.shown.get(comp.selected).map(|&(i, _)| &comp.items[i]) else { return };
        let Some(docs) = &item.documentation else { return };
        let Some(lang) = self.docs[comp.doc].as_ref().map(|d| d.lang) else { return };
        let fg = self.color("editorSuggestWidget.foreground");
        let text_style = TextStyle::ui(UI, fg);
        let code_style = TextStyle::mono(font_size(), line_height(), fg);
        let mut blocks = Vec::new();
        if let Some(detail) = &item.detail {
            blocks.push(Block::Code(lang, detail.lines().map(String::from).collect()));
        }
        blocks.extend(parse_markdown(docs));
        let pad = 8.0;
        // Right of the list when it fits, else left of it.
        let room_right = self.main_rect.right() - list.right() - 4.0;
        let room_left = list.x - self.main_rect.x - 4.0;
        let w = DETAILS_W.min(room_right.max(room_left)).max(160.0);
        let rows = layout_blocks(c, &blocks, &text_style, w - pad * 2.0);
        let content_h: f32 = rows.iter().map(|r| row_height(r, &text_style)).sum();
        let h = (content_h + pad * 2.0).min(DETAILS_MAX_H).max(list.h.min(content_h + pad * 2.0));
        let x = if room_right >= w || room_right >= room_left { list.right() - 1.0 } else { list.x - w + 1.0 };
        let r = Rect::new(x.round(), list.y, w.round(), h.round());
        c.shadow(r, POPUP_RADIUS, self.color("widget.shadow"));
        c.bordered(r, self.color("editorSuggestWidget.background"), self.color("editorSuggestWidget.border"), 1.0, POPUP_RADIUS);
        c.push_clip(r.inset(1.0, 1.0));
        draw_rows(c, &self.theme, &rows, r, r.y + pad, pad, &text_style, &code_style);
        c.pop_clip();
        self.hits.push((r, Hit::CompletionBox));
    }

    pub(super) fn click_completion(&mut self, idx: usize) {
        if let Some(comp) = &mut self.completion {
            comp.selected = idx;
        }
        self.accept_completion();
        self.focus = Focus::Editor;
    }

    pub(super) fn scroll_completion(&mut self, dy: f32) {
        if let Some(comp) = &mut self.completion {
            let max = comp.shown.len().saturating_sub(SUGGEST_ROWS);
            let steps = (-dy / ROW_H).round() as isize;
            comp.scroll = (comp.scroll as isize + steps).clamp(0, max as isize) as usize;
        }
    }

    /// The Problems panel: diagnostics grouped by file.
    pub(super) fn draw_problems(&mut self, c: &mut Canvas, body: Rect) {
        let fg = self.color("foreground");
        let dim = self.color("descriptionForeground");
        let style = TextStyle::ui(UI, fg);
        let dim_style = TextStyle::ui(12.0, dim);
        if self.lsp.diagnostics.is_empty() {
            self.empty_state(c, body, &icons::PASS, "No problems", "Nothing in the workspace needs fixing.", None);
            return;
        }
        let mut rows: Vec<ProblemRow> = Vec::new();
        let total: usize = self.lsp.diagnostics.values().map(|(_, d)| d.len()).sum();
        for (path, (_, diags)) in &self.lsp.diagnostics {
            let mut sorted: Vec<&lsp::Diagnostic> = diags.iter().filter(|d| self.problem_shown(path, d)).collect();
            if sorted.is_empty() {
                continue;
            }
            rows.push(ProblemRow::File(path.clone(), sorted.len()));
            if self.problems_filter.collapsed.contains(path) {
                continue;
            }
            sorted.sort_by_key(|d| (d.severity, d.range.start));
            for d in sorted {
                rows.push(ProblemRow::Diag(path.clone(), d.clone()));
            }
        }
        if rows.is_empty() {
            let detail = format!("None of the {total} problems match the filter.");
            self.empty_state(c, body, &icons::FILTER, "No matching problems", &detail, None);
            return;
        }
        let max = (rows.len() as f32 * ROW_H - body.h + ROW_H).max(0.0);
        self.problems_scroll = self.problems_scroll.clamp(0.0, max);
        c.push_clip(body);
        let first = (self.problems_scroll / ROW_H) as usize;
        let visible = (body.h / ROW_H).ceil() as usize + 1;
        self.problem_targets.clear();
        self.problem_files.clear();
        self.a11y_list(super::a11y::PROBLEMS_LIST, Some(super::a11y::PANEL), "Problems", body);
        let mut hits = Vec::new();
        for (i, row) in rows.iter().enumerate().skip(first).take(visible) {
            let y = body.y + i as f32 * ROW_H - self.problems_scroll;
            let rr = Rect::new(body.x, y, body.w, ROW_H);
            if self.hover_hit == Some(Hit::ProblemRow(i)) {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            match row {
                ProblemRow::File(path, n) => {
                    let open = !self.problems_filter.collapsed.contains(path);
                    c.icon(if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT }, rr.x + 8.0, y + 3.0, 16.0, fg);
                    self.problem_files.push((i, path.clone()));
                    c.icon(&icons::FILE, rr.x + 26.0, y + 3.0, 16.0, super::file_color(path));
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let mut x = rr.x + 48.0 + c.text_in(Rect::new(rr.x + 48.0, y, 300.0, ROW_H), &name, &style.weight(600));
                    let dir = path.parent().map(|p| self.display_path(p)).unwrap_or_default();
                    x += 6.0 + c.text_in(Rect::new(x + 6.0, y, 400.0, ROW_H), &dir, &dim_style);
                    self.badge(c, x + 8.0, y + 3.0, *n);
                }
                ProblemRow::Diag(path, d) => {
                    let color = severity_color(&self.theme, d.severity);
                    let icon = match d.severity {
                        Severity::Error => &icons::ERROR,
                        Severity::Warning => &icons::WARNING,
                        _ => &icons::INFO,
                    };
                    c.icon(icon, rr.x + 44.0, y + 4.0, 14.0, color);
                    let msg = d.message.lines().next().unwrap_or("");
                    let x = rr.x + 64.0 + c.text_in(Rect::new(rr.x + 64.0, y, rr.w, ROW_H), msg, &style);
                    let src = match (&d.source, &d.code) {
                        (Some(s), Some(code)) => format!("{s}({code})"),
                        (Some(s), None) => s.clone(),
                        _ => String::new(),
                    };
                    let at = format!("Ln {}, Col {}", d.range.start.line + 1, d.range.start.character + 1);
                    let loc = if src.is_empty() { at } else { format!("{src} · {at}") };
                    c.text_in(Rect::new(x + 8.0, y, rr.w, ROW_H), &loc, &dim_style);
                    self.problem_targets.push((i, path.clone(), d.range.start));
                }
            }
            hits.push((rr, Hit::ProblemRow(i)));
            let label = match row {
                ProblemRow::File(path, n) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let dir = path.parent().map(|p| self.display_path(p)).unwrap_or_default();
                    let open = !self.problems_filter.collapsed.contains(path);
                    format!("{name}, {dir}, {n} {}, {}", if *n == 1 { "problem" } else { "problems" }, if open { "expanded" } else { "collapsed" })
                }
                ProblemRow::Diag(_, d) => {
                    let kind = match d.severity {
                        Severity::Error => "Error",
                        Severity::Warning => "Warning",
                        Severity::Information => "Info",
                        Severity::Hint => "Hint",
                    };
                    let msg = d.message.lines().next().unwrap_or("");
                    format!("{kind}: {msg}, line {}, column {}", d.range.start.line + 1, d.range.start.character + 1)
                }
            };
            self.a11y_item(super::a11y::PROBLEMS_LIST, i, label, rr.intersect(&body), false);
        }
        c.pop_clip();
        self.hits.extend(hits);
    }

    pub(super) fn open_problem(&mut self, row: usize) {
        // A file row collapses or expands its problems.
        if let Some((_, path)) = self.problem_files.iter().find(|(i, _)| *i == row).cloned() {
            if !self.problems_filter.collapsed.remove(&path) {
                self.problems_filter.collapsed.insert(path);
            }
            return;
        }
        let Some((_, path, at)) = self.problem_targets.iter().find(|(i, ..)| *i == row).cloned() else { return };
        let encoding = self.lsp.diagnostics.get(&path).map_or(Encoding::Utf16, |(e, _)| *e);
        self.open_file(&path);
        self.focus = Focus::Editor;
        if let Some((ed, doc)) = self.active_mut() {
            let line = at.line as usize;
            let col = encoding.from_lsp(&doc.buffer.line(line), at.character);
            ed.jump_to(doc, Pos::new(line, col));
        }
    }

    /// A count badge (section headers, file rows): a quiet tint of the badge color. Returns
    /// its width.
    pub(super) fn badge(&self, c: &mut Canvas, x: f32, y: f32, n: usize) -> f32 {
        let label = n.to_string();
        let style = TextStyle::ui(SMALL, self.color("foreground")).weight(600);
        let w = (c.measure(&label, &style) + 10.0).max(18.0);
        let r = Rect::new(x, y, w, 16.0);
        c.fill_rounded(r, self.color("badge.background").with_alpha(0.25), 8.0);
        let tw = c.measure(&label, &style);
        c.text_in(Rect::new(x + (w - tw) / 2.0, y, tw + 1.0, 16.0), &label, &style);
        w
    }
}

enum ProblemRow {
    File(PathBuf, usize),
    Diag(PathBuf, lsp::Diagnostic),
}

pub(super) enum Block {
    Text(String),
    Code(Lang, Vec<String>),
    Diagnostic(Severity, String),
    Rule,
}

pub(super) enum Row {
    Text(String),
    Code(String, Vec<language::Span>),
    Diag(Option<Severity>, String),
    Gap(f32),
    Rule,
}

/// Lays out markdown blocks as rows `inner_w` wide (paragraphs wrapped, code highlighted).
pub(super) fn layout_blocks(c: &mut Canvas, blocks: &[Block], text_style: &TextStyle, inner_w: f32) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for block in blocks {
        match block {
            Block::Text(t) => {
                for line in wrap(c, t, text_style, inner_w) {
                    rows.push(Row::Text(line));
                }
                rows.push(Row::Gap(4.0));
            }
            Block::Diagnostic(sev, msg) => {
                for (i, line) in wrap(c, msg, text_style, inner_w - 20.0).into_iter().enumerate() {
                    rows.push(Row::Diag(if i == 0 { Some(*sev) } else { None }, line));
                }
                rows.push(Row::Gap(4.0));
            }
            Block::Code(lang, lines) => {
                let mut hl = Highlighter::new(*lang);
                let mut buf = text::Buffer::new();
                buf.insert(Selection::default(), &lines.join("\n"));
                hl.update(&mut buf);
                let spans = hl.spans(&buf, 0, lines.len());
                for (line, spans) in lines.iter().zip(spans) {
                    rows.push(Row::Code(line.clone(), spans));
                }
                rows.push(Row::Gap(4.0));
            }
            Block::Rule => rows.push(Row::Rule),
        }
    }
    while matches!(rows.last(), Some(Row::Gap(_))) {
        rows.pop();
    }
    rows
}

pub(super) fn row_height(r: &Row, text_style: &TextStyle) -> f32 {
    match r {
        Row::Text(_) | Row::Diag(..) => text_style.line_height,
        Row::Code(..) => line_height(),
        Row::Gap(g) => *g,
        Row::Rule => 9.0,
    }
}

pub(super) fn row_width(c: &mut Canvas, r: &Row, text_style: &TextStyle, code_style: &TextStyle) -> f32 {
    match r {
        Row::Text(t) => c.measure(t, text_style),
        Row::Diag(_, t) => c.measure(t, text_style) + 20.0,
        Row::Code(t, _) => c.measure(t, code_style),
        _ => 0.0,
    }
}

/// Draws rows inside the popup `r`, from `y`, `pad` in from its left edge.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_rows(c: &mut Canvas, theme: &theme::Theme, rows: &[Row], r: Rect, y: f32, pad: f32, text_style: &TextStyle, code_style: &TextStyle) {
    let mut cy = y;
    for row in rows {
        match row {
            Row::Text(t) => c.text(r.x + pad, cy, t, text_style),
            Row::Diag(sev, t) => {
                if let Some(sev) = sev {
                    let icon = match sev {
                        Severity::Error => &icons::ERROR,
                        Severity::Warning => &icons::WARNING,
                        _ => &icons::INFO,
                    };
                    c.icon(icon, r.x + pad, cy + 2.0, 14.0, severity_color(theme, *sev));
                }
                c.text(r.x + pad + 20.0, cy, t, text_style)
            }
            Row::Code(t, spans) => {
                let colored: Vec<(usize, usize, Color)> = spans.iter().map(|(a, b, tok)| (*a, *b, theme.token(*tok))).collect();
                c.rich_text(r.x + pad, cy, t, &colored, code_style)
            }
            Row::Rule => {
                c.fill(Rect::new(r.x, cy + 4.0, r.w, 1.0), theme.color("editorHoverWidget.border"));
                0.0
            }
            Row::Gap(_) => 0.0,
        };
        cy += row_height(row, text_style);
    }
}

/// Splits hover markdown into code blocks, paragraphs and rules. Inline markup is simplified.
pub(super) fn parse_markdown(md: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para = String::new();
    let mut code: Option<(Lang, Vec<String>)> = None;
    let flush = |para: &mut String, blocks: &mut Vec<Block>| {
        let t = para.trim();
        if !t.is_empty() {
            blocks.push(Block::Text(strip_inline(t)));
        }
        para.clear();
    };
    for line in md.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            match code.take() {
                Some((lang, lines)) => blocks.push(Block::Code(lang, lines)),
                None => {
                    flush(&mut para, &mut blocks);
                    let lang = Lang::from_name(rest.trim()).unwrap_or(Lang::PlainText);
                    code = Some((lang, Vec::new()));
                }
            }
            continue;
        }
        if let Some((_, lines)) = &mut code {
            lines.push(line.to_string());
            continue;
        }
        if line.trim() == "---" || line.trim() == "***" {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Rule);
        } else if line.trim().is_empty() {
            flush(&mut para, &mut blocks);
        } else {
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(line.trim());
        }
    }
    if let Some((lang, lines)) = code {
        blocks.push(Block::Code(lang, lines));
    }
    flush(&mut para, &mut blocks);
    blocks
}

/// Drops inline markdown syntax: emphasis markers, backticks, link targets.
fn strip_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' | '*' => {}
            '_' if chars.peek() == Some(&'_') => {
                chars.next();
            }
            // `_emphasis_` (not the `_` inside snake_case).
            '_' if !out.ends_with(char::is_alphanumeric) || !chars.peek().is_some_and(|c| c.is_alphanumeric()) => {}
            '[' => {
                let label: String = chars.by_ref().take_while(|&c| c != ']').collect();
                out.push_str(&strip_inline(&label));
                if chars.peek() == Some(&'(') {
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Greedy word wrap to `width`.
pub(super) fn wrap(c: &mut Canvas, text: &str, style: &TextStyle, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && c.measure(&candidate, style) > width {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
}

fn kind_icon(theme: &theme::Theme, kind: u32) -> (&'static Icon, Color) {
    match lsp::completion_kind_name(kind) {
        "method" | "function" | "constructor" => (&icons::SYMBOL_METHOD, theme.color("symbolIcon.methodForeground")),
        "field" | "property" | "enum member" => (&icons::SYMBOL_FIELD, theme.color("symbolIcon.fieldForeground")),
        "class" | "struct" | "interface" | "enum" | "type parameter" => {
            (&icons::SYMBOL_CLASS, theme.color("symbolIcon.classForeground"))
        }
        "variable" | "value" | "constant" => (&icons::SYMBOL_VARIABLE, theme.color("symbolIcon.fieldForeground")),
        "module" => (&icons::SYMBOL_MODULE, theme.color("symbolIcon.moduleForeground")),
        _ => (&icons::SYMBOL_KEYWORD, theme.color("symbolIcon.keywordForeground")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_emphasis() {
        assert_eq!(strip_inline("_Widely available_ in snake_case and __bold__"), "Widely available in snake_case and bold");
    }

    #[test]
    fn markdown_blocks() {
        let md = "```rust\ndemo\n```\n\n```rust\nfn helper() -> u32\n```\n\n---\n\nReturns **one**. See [`Vec`](https://x).";
        let blocks = parse_markdown(md);
        assert!(matches!(&blocks[0], Block::Code(Lang::Rust, l) if l == &vec!["demo".to_string()]));
        assert!(matches!(&blocks[1], Block::Code(Lang::Rust, l) if l[0] == "fn helper() -> u32"));
        assert!(matches!(blocks[2], Block::Rule));
        assert!(matches!(&blocks[3], Block::Text(t) if t == "Returns one. See Vec."));
    }

    fn workbench_with(file: &str, text: &str) -> (Workbench, PathBuf) {
        let dir = std::env::temp_dir().join(format!("orbvane-intel-{}-{file}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), text).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        (Workbench::new(Some(dir.clone()), &[dir.join(file)], std::sync::Arc::new(|| {})), dir)
    }

    #[test]
    fn says_when_a_language_server_is_missing() {
        if crate::servers::find_binary("zls").is_some() {
            return; // Zig's server is installed here.
        }
        let (mut wb, dir) = workbench_with("main.zig", "pub fn main() void {}\n");
        wb.lsp_tick();
        wb.lsp_tick();
        let toasts = wb.toast_list();
        let missing: Vec<_> = toasts.iter().filter(|(_, m, _)| m.starts_with("Zig needs zls")).collect();
        assert_eq!(missing.len(), 1, "{toasts:?}");
        assert!(missing[0].1.contains("\"brew install zls\""), "{}", missing[0].1);
        assert_eq!(missing[0].2, ["Install", "Copy Install Command"]);
        // Shown once a session; Go to Definition and References bring it back (still one).
        wb.lsp_tick();
        let id = missing[0].0;
        wb.close_toast(id, None);
        wb.go_to_definition(None);
        wb.find_references();
        assert_eq!(wb.toast_list().iter().filter(|(_, m, _)| m.starts_with("Zig needs zls")).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_editor_has_a_context_menu() {
        let (mut wb, dir) = workbench_with("notes.py", "x = 1\n");
        wb.take_effects();
        wb.editor_context_menu(0, 0.0, 0.0);
        let labels = |items: &[super::super::PopupItem]| -> Vec<String> {
            items.iter().map(|i| match i {
                super::super::PopupItem::Item { label, .. } | super::super::PopupItem::Submenu { label, .. } => label.clone(),
                super::super::PopupItem::Separator => "-".into(),
            }).collect()
        };
        let Some(super::super::Effect::Popup { items, .. }) = wb.take_effects().into_iter().find(|e| matches!(e, super::super::Effect::Popup { .. })) else {
            panic!("no menu")
        };
        assert_eq!(
            labels(&items),
            ["Go to Definition", "Go to References", "Peek", "-", "Rename Symbol", "Quick Fix...", "Format Document", "-", "Cut", "Copy", "Paste", "-", "Command Palette..."]
        );
        let super::super::PopupItem::Submenu { items: peek, .. } = &items[2] else { panic!("Peek isn't a submenu") };
        assert_eq!(labels(peek), ["Peek Definition", "Peek Call Hierarchy", "Peek Type Hierarchy"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
