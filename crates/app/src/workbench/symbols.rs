//! Quick access modes of the palette that need the editor or the language servers, like
//! `@` Go to Symbol in Editor (⇧⌘O), `#` Go to Symbol in Workspace (⌘T) and `:`
//! Go to Line/Column (⌃G). Moving through `@` and `:` previews the place in the editor, and
//! Escape puts the editor back.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lsp::Encoding;
use text::{Pos, Selection};

use super::outline::SymbolsState;
use super::Workbench;
use crate::palette::{fuzzy, Action, Item, Mode, Palette};

/// How long typing pauses before the workspace is searched.
const SEARCH_DELAY: Duration = Duration::from_millis(150);
/// At most this many workspace matches are listed.
const MAX_WORKSPACE: usize = 300;

#[derive(Default)]
pub(super) struct SymbolSearch {
    /// The query of request `seq`, and when to send it.
    query: String,
    seq: u64,
    due: Option<Instant>,
    /// Matches for `seq` so far (several servers can answer).
    results: Vec<Item>,
    answered: bool,
    /// The editor before previews: (group, selection, scroll), restored on Escape.
    origin: Option<(usize, Selection, f32)>,
}

impl Workbench {
    /// Fills the palette for the `@`, `#` and `:` modes.
    pub(super) fn refresh_quick_access(&mut self, p: &mut Palette) {
        match p.mode() {
            Some(Mode::Symbols) => self.editor_symbol_items(p),
            Some(Mode::WorkspaceSymbols) => self.workspace_symbol_items(p),
            Some(Mode::Line) => self.line_items(p),
            _ => {}
        }
    }

    fn editor_symbol_items(&mut self, p: &mut Palette) {
        let query = p.input[1..].trim().to_string();
        let (state, symbols) = self.editor_symbols();
        p.message = match state {
            SymbolsState::NoEditor => Some("To go to a symbol, first open a text editor with symbol information.".into()),
            SymbolsState::NoProvider => Some("The active text editor does not provide symbol information.".into()),
            SymbolsState::Loading => Some("Loading symbols...".into()),
            SymbolsState::Ready if symbols.is_empty() => Some("No editor symbols".into()),
            SymbolsState::Ready => Some("No matching editor symbols".into()),
        };
        let mut scored: Vec<(i32, Item)> = symbols
            .into_iter()
            .filter_map(|s| {
                let (score, matches) = fuzzy(&query, &s.name)?;
                let item = Item {
                    label: s.name,
                    detail: s.container,
                    matches,
                    shortcut: None,
                    action: Action::GotoHere(s.at),
                    group: None,
                    kind: Some(s.kind),
                };
                Some((score, item))
            })
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0));
        }
        p.items = scored.into_iter().map(|(_, i)| i).collect();
    }

    fn workspace_symbol_items(&mut self, p: &mut Palette) {
        let query = p.input[1..].trim().to_string();
        let s = &mut self.symbol_search;
        if query != s.query || s.seq == 0 {
            s.query = query.clone();
            s.seq += 1;
            s.due = Some(Instant::now() + SEARCH_DELAY);
            s.answered = false;
        }
        // Keep showing the previous matches (filtered again) until the new ones arrive.
        let mut scored: Vec<(i32, Item)> = s
            .results
            .iter()
            .filter_map(|it| {
                let (score, matches) = fuzzy(&query, &it.label)?;
                Some((score, Item { matches, action: it.action.clone(), label: it.label.clone(), detail: it.detail.clone(), shortcut: None, group: None, kind: it.kind }))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        p.items = scored.into_iter().map(|(_, i)| i).collect();
        p.message = Some(if s.answered { "No matching workspace symbols" } else { "Searching..." }.into());
    }

    /// Sends a pending workspace search. Called every frame.
    pub(super) fn symbol_search_tick(&mut self) {
        let s = &mut self.symbol_search;
        if !s.due.is_some_and(|t| Instant::now() >= t) {
            return;
        }
        s.due = None;
        let (query, seq) = (s.query.clone(), s.seq);
        if self.lsp.workspace_symbols(&query, seq) == 0 {
            self.symbol_search.answered = true;
            self.symbol_search.results.clear();
            self.refresh_open_palette(Mode::WorkspaceSymbols);
        }
    }

    pub(super) fn symbol_search_deadline(&self) -> Option<Instant> {
        self.symbol_search.due
    }

    /// A server's matches for search `seq`.
    pub(super) fn workspace_symbols_arrived(&mut self, seq: u64, symbols: Vec<lsp::WorkspaceSymbol>, encoding: Encoding) {
        if seq != self.symbol_search.seq {
            return;
        }
        // Columns come in the server's units: convert with the line's text (open buffer, or
        // the file read once).
        let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut items = Vec::new();
        for sym in symbols.into_iter().take(MAX_WORKSPACE) {
            let (line, character) = sym.range.map_or((0, 0), |r| (r.start.line as usize, r.start.character));
            let open = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(sym.path.as_path()));
            let text = match open {
                Some(d) => d.buffer.line(line).to_string(),
                None => files
                    .entry(sym.path.clone())
                    .or_insert_with(|| std::fs::read_to_string(&sym.path).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default())
                    .get(line)
                    .cloned()
                    .unwrap_or_default(),
            };
            let pos = Pos::new(line, encoding.from_lsp(&text, character));
            let rel = self.display_path(&sym.path);
            let detail = if sym.container.is_empty() { rel } else { format!("{}  {rel}", sym.container) };
            items.push(Item {
                label: sym.name,
                detail,
                matches: Vec::new(),
                shortcut: None,
                action: Action::Goto(sym.path, pos),
                group: None,
                kind: Some(sym.kind),
            });
        }
        let s = &mut self.symbol_search;
        if !s.answered {
            s.results.clear();
            s.answered = true;
        }
        s.results.extend(items);
        self.refresh_open_palette(Mode::WorkspaceSymbols);
    }

    /// Re-lists the palette if it's showing `mode` (new data arrived), keeping the selection.
    pub(super) fn refresh_open_palette(&mut self, mode: Mode) {
        let Some(mut p) = self.palette.take() else { return };
        if p.mode() == Some(mode) {
            let selected = p.selected;
            self.refresh_quick_access(&mut p);
            p.selected = selected.min(p.items.len().saturating_sub(1));
        }
        self.palette = Some(p);
    }

    fn line_items(&mut self, p: &mut Palette) {
        let Some((ed, doc)) = self.active_editor().zip(self.active_doc()) else {
            p.message = Some("Open a text editor first to go to a line.".into());
            return;
        };
        let lines = doc.buffer.len_lines();
        let mut parts = p.input[1..].trim().split([':', ',']).map(str::trim);
        let line = parts.next().and_then(|s| s.parse::<usize>().ok());
        let col = parts.next().and_then(|s| s.parse::<usize>().ok());
        match line.filter(|l| (1..=lines).contains(l)) {
            Some(line) => {
                let label = match col {
                    Some(c) => format!("Go to line {line} and character {c}."),
                    None => format!("Go to line {line}."),
                };
                let pos = doc.buffer.clamp(Pos::new(line - 1, col.unwrap_or(1).saturating_sub(1)));
                p.items = vec![Item { label, detail: String::new(), matches: Vec::new(), shortcut: None, action: Action::GotoHere(pos), group: None, kind: None }];
            }
            None if p.input[1..].trim().is_empty() => {
                let (l, c) = (ed.sel.head.line + 1, ed.sel.head.col + 1);
                p.message = Some(format!("Current Line: {l}, Character: {c}. Type a line number between 1 and {lines} to navigate to."));
            }
            None => p.message = Some(format!("Type a line number between 1 and {lines} to navigate to.")),
        }
    }

    /// Shows the selected symbol or line in the editor while the palette is open.
    pub(super) fn preview_quick_access(&mut self) {
        let Some(p) = &self.palette else { return };
        if !matches!(p.mode(), Some(Mode::Symbols | Mode::Line)) {
            return self.cancel_quick_access(); // the prefix was deleted
        }
        let Some(Action::GotoHere(pos)) = p.selected_action() else { return };
        let g = self.active_group;
        let Some((ed, doc)) = self.active_mut() else { return };
        let origin = (g, ed.sel, ed.scroll_y);
        ed.reveal_at(doc, pos);
        self.symbol_search.origin.get_or_insert(origin);
    }

    /// The palette closed without going anywhere: put the editor back.
    pub(super) fn cancel_quick_access(&mut self) {
        let Some((g, sel, scroll)) = self.symbol_search.origin.take() else { return };
        let gr = &mut self.groups[g];
        if let Some(ed) = gr.tabs.get_mut(gr.active) {
            ed.restore_view(sel, scroll);
        }
    }

    /// Enter on a symbol of the active editor or a line.
    pub(super) fn goto_here(&mut self, pos: Pos) {
        self.symbol_search.origin = None;
        self.focus = super::Focus::Editor;
        if let Some((ed, doc)) = self.active_mut() {
            ed.reveal_at(doc, pos);
        }
    }
}
