//! The Outline view: a tab of the secondary side bar listing the active editor's symbols (from the
//! language server's `textDocument/documentSymbol`) as a tree. Clicking a symbol reveals it;
//! with Follow Cursor the symbol at the cursor is selected and scrolled into view. With the
//! keyboard (`Focus::Outline`) the arrows move the selection and reveal each symbol, Enter goes
//! to it, and typing highlights the symbols whose names match.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use lsp::Encoding;
use render::{Canvas, Icon, Rect, TextStyle};
use text::{Buffer, Pos};

use super::preferences::PopupAction;
use super::{Focus, Hit, PopupItem, Workbench, ROW_H, SMALL, UI};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::palette::fuzzy;

/// How long typing pauses before the symbols are asked for again.
const DELAY: Duration = Duration::from_millis(300);
/// How long to wait before asking again when the server couldn't answer (still loading).
const RETRY: Duration = Duration::from_secs(1);
/// A request unanswered this long is given up on.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub(super) enum Sort {
    #[default]
    Position,
    Name,
    Category,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Status {
    /// No editor that could have symbols.
    #[default]
    NoEditor,
    /// The editor's language has no server offering symbols.
    NoProvider,
    Loading,
    Ready,
}

/// A symbol, in editor positions.
struct Symbol {
    name: String,
    detail: String,
    kind: u32,
    range: (Pos, Pos),
    /// Where to put the cursor (the start of the name).
    at: Pos,
    children: Vec<Symbol>,
}

/// A visible row of the tree.
pub(super) struct Row {
    depth: usize,
    /// The names from the root, identifying the symbol across refreshes.
    key: String,
    name: String,
    detail: String,
    kind: u32,
    at: Pos,
    has_children: bool,
}

pub(super) struct Outline {
    pub(super) follow_cursor: bool,
    pub(super) sort: Sort,
    status: Status,
    /// The document shown and the version its symbols are from.
    doc: Option<usize>,
    symbols: Vec<Symbol>,
    symbols_version: u64,
    /// The (document, version) wanted, when to ask for it, and the request in flight.
    wanted: Option<(usize, u64)>,
    due: Option<Instant>,
    in_flight: Option<Instant>,
    /// Keys of collapsed symbols.
    collapsed: HashSet<String>,
    scroll: f32,
    rows: Vec<Row>,
    /// The body drawn last frame (for scrolling).
    body: Rect,
    /// The symbol at the cursor last frame (Follow Cursor reveals it when it changes).
    followed: Option<String>,
    /// The selected row's key.
    selected: Option<String>,
    /// What's been typed to find symbols (empty: no find box).
    query: String,
}

impl Default for Outline {
    fn default() -> Self {
        Self {
            follow_cursor: true,
            sort: Sort::Position,
            status: Status::NoEditor,
            doc: None,
            symbols: Vec::new(),
            symbols_version: 0,
            wanted: None,
            due: None,
            in_flight: None,
            collapsed: HashSet::new(),
            scroll: 0.0,
            rows: Vec::new(),
            body: Rect::default(),
            followed: None,
            selected: None,
            query: String::new(),
        }
    }
}

fn convert(b: &Buffer, s: &lsp::DocumentSymbol, enc: Encoding) -> Symbol {
    let pos = |p: lsp::Position| b.clamp(Pos::new(p.line as usize, enc.from_lsp(&b.line(p.line as usize), p.character)));
    Symbol {
        name: s.name.clone(),
        detail: s.detail.clone(),
        kind: s.kind,
        range: (pos(s.range.start), pos(s.range.end)),
        at: pos(s.selection_range.start),
        children: s.children.iter().map(|c| convert(b, c, enc)).collect(),
    }
}

/// The innermost symbol containing `pos`, as its key.
fn symbol_at(symbols: &[Symbol], pos: Pos, prefix: &str) -> Option<String> {
    let s = symbols.iter().find(|s| s.range.0 <= pos && pos <= s.range.1)?;
    let key = format!("{prefix}/{}", s.name);
    symbol_at(&s.children, pos, &key).or(Some(key))
}

/// The icon and theme color of an LSP `SymbolKind`, like `symbol-*` icons.
pub(super) fn symbol_icon(kind: u32) -> (&'static Icon, &'static str) {
    match kind {
        1 => (&icons::FILE, "symbolIcon.fileForeground"),
        2 => (&icons::SYMBOL_MODULE, "symbolIcon.moduleForeground"),
        3 => (&icons::SYMBOL_MODULE, "symbolIcon.namespaceForeground"),
        4 => (&icons::SYMBOL_MODULE, "symbolIcon.packageForeground"),
        5 => (&icons::SYMBOL_CLASS, "symbolIcon.classForeground"),
        6 => (&icons::SYMBOL_METHOD, "symbolIcon.methodForeground"),
        7 => (&icons::SYMBOL_PROPERTY, "symbolIcon.propertyForeground"),
        8 => (&icons::SYMBOL_FIELD, "symbolIcon.fieldForeground"),
        9 => (&icons::SYMBOL_METHOD, "symbolIcon.constructorForeground"),
        10 => (&icons::SYMBOL_ENUM, "symbolIcon.enumeratorForeground"),
        11 => (&icons::SYMBOL_INTERFACE, "symbolIcon.interfaceForeground"),
        12 => (&icons::SYMBOL_METHOD, "symbolIcon.functionForeground"),
        13 => (&icons::SYMBOL_VARIABLE, "symbolIcon.variableForeground"),
        14 => (&icons::SYMBOL_CONSTANT, "symbolIcon.constantForeground"),
        15 => (&icons::SYMBOL_KEYWORD, "symbolIcon.stringForeground"),
        16 => (&icons::SYMBOL_KEYWORD, "symbolIcon.numberForeground"),
        17 => (&icons::SYMBOL_KEYWORD, "symbolIcon.booleanForeground"),
        18 => (&icons::SYMBOL_VARIABLE, "symbolIcon.arrayForeground"),
        19 => (&icons::SYMBOL_CLASS, "symbolIcon.objectForeground"),
        20 => (&icons::SYMBOL_KEYWORD, "symbolIcon.keyForeground"),
        21 => (&icons::SYMBOL_KEYWORD, "symbolIcon.nullForeground"),
        22 => (&icons::SYMBOL_ENUM_MEMBER, "symbolIcon.enumeratorMemberForeground"),
        23 => (&icons::SYMBOL_STRUCT, "symbolIcon.structForeground"),
        24 => (&icons::SYMBOL_EVENT, "symbolIcon.eventForeground"),
        25 => (&icons::SYMBOL_KEYWORD, "symbolIcon.operatorForeground"),
        26 => (&icons::SYMBOL_CLASS, "symbolIcon.typeParameterForeground"),
        _ => (&icons::SYMBOL_KEYWORD, "symbolIcon.keywordForeground"),
    }
}

impl Outline {
    /// Rebuilds the visible rows (sorted, without the children of collapsed symbols).
    fn build_rows(&mut self) {
        fn walk(out: &mut Vec<Row>, symbols: &[Symbol], depth: usize, prefix: &str, sort: Sort, collapsed: &HashSet<String>) {
            let mut order: Vec<&Symbol> = symbols.iter().collect();
            match sort {
                Sort::Position => {}
                Sort::Name => order.sort_by_key(|s| s.name.to_lowercase()),
                Sort::Category => order.sort_by_key(|s| (s.kind, s.name.to_lowercase())),
            }
            for s in order {
                let key = format!("{prefix}/{}", s.name);
                out.push(Row {
                    depth,
                    key: key.clone(),
                    name: s.name.clone(),
                    detail: s.detail.clone(),
                    kind: s.kind,
                    at: s.at,
                    has_children: !s.children.is_empty(),
                });
                if !collapsed.contains(&key) {
                    walk(out, &s.children, depth + 1, &key, sort, collapsed);
                }
            }
        }
        let mut rows = Vec::new();
        walk(&mut rows, &self.symbols, 0, "", self.sort, &self.collapsed);
        self.rows = rows;
    }

    fn max_scroll(&self) -> f32 {
        (self.rows.len() as f32 * ROW_H - self.body.h).max(0.0)
    }

    pub(super) fn scroll_by(&mut self, dy: f32) {
        self.scroll = (self.scroll - dy).clamp(0.0, self.max_scroll());
    }

    fn selected_index(&self) -> Option<usize> {
        let key = self.selected.as_ref()?;
        self.rows.iter().position(|r| &r.key == key)
    }

    /// Scrolls row `i` into view.
    fn scroll_to(&mut self, i: usize) {
        let top = i as f32 * ROW_H;
        if top < self.scroll {
            self.scroll = top;
        } else if top + ROW_H > self.scroll + self.body.h {
            self.scroll = top + ROW_H - self.body.h;
        }
    }

    /// The first row at or after `from` (wrapping around) whose name matches the query.
    fn find_match(&self, from: usize) -> Option<usize> {
        let n = self.rows.len();
        (0..n).map(|k| (from + k) % n).find(|&i| fuzzy(&self.query, &self.rows[i].name).is_some())
    }
}

/// A symbol of the active editor, flattened for Go to Symbol in Editor.
pub(super) struct FlatSymbol {
    pub name: String,
    /// The names of the symbols it's in ("impl Canvas").
    pub container: String,
    pub kind: u32,
    pub at: Pos,
}

/// Why there are no symbols to list, if so.
pub(super) enum SymbolsState {
    NoEditor,
    NoProvider,
    Loading,
    Ready,
}

/// A symbol of the breadcrumbs path, with the symbols next to it (its dropdown).
pub(super) struct Crumb {
    pub name: String,
    pub kind: u32,
    pub siblings: Vec<(String, u32, Pos)>,
    /// Its index in `siblings`.
    pub index: usize,
}

impl Workbench {
    /// The symbols containing `pos` in document `doc`, outermost first (only for the
    /// document whose symbols are loaded: the active editor's).
    pub(super) fn symbol_path(&self, doc: usize, pos: Pos) -> Vec<Crumb> {
        let o = &self.outline;
        let mut out = Vec::new();
        if o.doc != Some(doc) {
            return out;
        }
        let mut level = &o.symbols;
        while let Some(index) = level.iter().position(|s| s.range.0 <= pos && pos <= s.range.1) {
            let s = &level[index];
            let siblings = level.iter().map(|s| (s.name.clone(), s.kind, s.at)).collect();
            out.push(Crumb { name: s.name.clone(), kind: s.kind, siblings, index });
            level = &s.children;
        }
        out
    }

    /// The active editor's symbols in document order (see `SymbolsState` when empty).
    pub(super) fn editor_symbols(&self) -> (SymbolsState, Vec<FlatSymbol>) {
        fn walk(out: &mut Vec<FlatSymbol>, symbols: &[Symbol], container: &str) {
            for s in symbols {
                out.push(FlatSymbol { name: s.name.clone(), container: container.to_string(), kind: s.kind, at: s.at });
                let inner = if container.is_empty() { s.name.clone() } else { format!("{container} › {}", s.name) };
                walk(out, &s.children, &inner);
            }
        }
        let o = &self.outline;
        let current = self.outline_target().map(|t| t.0);
        let state = match o.status {
            _ if current.is_none() => SymbolsState::NoEditor,
            _ if current != o.doc => SymbolsState::Loading,
            Status::NoEditor => SymbolsState::NoEditor,
            Status::NoProvider => SymbolsState::NoProvider,
            Status::Loading => SymbolsState::Loading,
            Status::Ready => SymbolsState::Ready,
        };
        let mut out = Vec::new();
        if matches!(state, SymbolsState::Ready) {
            walk(&mut out, &o.symbols, "");
        }
        (state, out)
    }

    /// The document the outline should show: the active editor's, if it's a file.
    fn outline_target(&self) -> Option<(usize, u64)> {
        let ed = self.active_editor().filter(|e| !e.is_special())?;
        let doc = self.docs[ed.doc].as_ref()?;
        doc.buffer.path()?;
        Some((ed.doc, doc.buffer.version()))
    }

    /// Follows the active editor and its edits, asking for symbols after a pause (for the
    /// outline, Go to Symbol and the breadcrumbs). Called every frame.
    pub(super) fn outline_tick(&mut self) {
        let now = Instant::now();
        let target = self.outline_target();
        let o = &mut self.outline;
        if target.map(|t| t.0) != o.doc {
            o.doc = target.map(|t| t.0);
            o.symbols.clear();
            o.symbols_version = 0;
            o.collapsed.clear();
            o.scroll = 0.0;
            o.followed = None;
            o.selected = None;
            o.query.clear();
            o.wanted = None;
            o.in_flight = None;
            o.status = if target.is_some() { Status::Loading } else { Status::NoEditor };
        }
        let Some((doc_id, version)) = target else { return };
        if o.wanted != Some((doc_id, version)) {
            // A new document is asked about at once, edits after a pause.
            let first = o.wanted.is_none();
            o.wanted = Some((doc_id, version));
            o.due = Some(if first { now } else { now + DELAY });
        }
        if o.in_flight.is_some_and(|t| now >= t + TIMEOUT) {
            o.in_flight = None;
            o.due = Some(now);
        }
        if o.in_flight.is_some() || !o.due.is_some_and(|t| now >= t) {
            return;
        }
        o.due = None;
        let doc = self.docs[doc_id].as_ref().unwrap();
        let path = doc.buffer.path().unwrap().to_path_buf();
        if !self.lsp.has_server(&path) {
            self.outline.status = Status::NoProvider;
        } else if !self.lsp.is_running(&path) {
            self.outline.due = Some(now + RETRY); // still starting
        } else if self.lsp.document_symbols(&path, &doc.buffer) {
            self.outline.in_flight = Some(now);
        } else {
            self.outline.status = Status::NoProvider;
        }
    }

    pub(super) fn outline_deadline(&self) -> Option<Instant> {
        self.outline.due
    }

    /// The server's symbols for `path` at buffer `version` (None: it couldn't answer yet; ask
    /// again soon).
    pub(super) fn outline_symbols(&mut self, path: &Path, version: u64, symbols: Option<Vec<lsp::DocumentSymbol>>, encoding: Encoding) {
        let Some(doc_id) = self.outline.doc else { return };
        let Some(doc) = self.docs[doc_id].as_ref().filter(|d| d.buffer.path() == Some(path)) else { return };
        let o = &mut self.outline;
        o.in_flight = None;
        match symbols {
            Some(_) if version < o.symbols_version => {} // older than what's shown
            Some(symbols) => {
                o.symbols_version = version;
                o.symbols = symbols.iter().map(|s| convert(&doc.buffer, s, encoding)).collect();
                o.status = Status::Ready;
            }
            None => o.due = Some(Instant::now() + RETRY),
        }
    }

    pub(super) fn outline_scroll(&mut self, dy: f32) {
        self.outline.scroll_by(dy);
    }

    /// A click on a row selects it and reveals the symbol (centered if it's off screen); a
    /// double-click also moves the focus to the editor.
    pub(super) fn outline_click(&mut self, i: usize, count: u32) {
        self.outline.query.clear();
        self.outline_select(i, true);
        self.focus = if count >= 2 { Focus::Editor } else { Focus::Outline };
    }

    /// Selects row `i`, scrolling it into view, and with `reveal` shows the symbol in the
    /// editor (keeping the focus where it is).
    fn outline_select(&mut self, i: usize, reveal: bool) {
        let o = &mut self.outline;
        let Some(row) = o.rows.get(i) else { return };
        let at = row.at;
        o.selected = Some(row.key.clone());
        o.scroll_to(i);
        if reveal {
            if let Some((ed, doc)) = self.active_mut() {
                ed.reveal_at(doc, at);
            }
        }
    }

    /// Keys while the outline has focus (tree keys, and typing to find).
    pub(super) fn outline_key(&mut self, k: &KeyInput) {
        let o = &mut self.outline;
        let n = o.rows.len();
        let sel = o.selected_index();
        let page = ((o.body.h / ROW_H) as usize).max(1);
        match &k.key {
            Key::Escape if !o.query.is_empty() => o.query.clear(),
            Key::Escape => self.focus = Focus::Editor,
            _ if n == 0 => {}
            Key::Up => self.outline_select(sel.map_or(0, |i| i.saturating_sub(1)), true),
            Key::Down => self.outline_select(sel.map_or(0, |i| (i + 1).min(n - 1)), true),
            Key::PageUp => self.outline_select(sel.map_or(0, |i| i.saturating_sub(page)), true),
            Key::PageDown => self.outline_select(sel.map_or(0, |i| (i + page).min(n - 1)), true),
            Key::Home => self.outline_select(0, true),
            Key::End => self.outline_select(n - 1, true),
            Key::Right | Key::Left => {
                let Some(i) = sel else { return self.outline_select(0, true) };
                let row = &o.rows[i];
                let open = row.has_children && !o.collapsed.contains(&row.key);
                match (k.key == Key::Right, row.has_children, open) {
                    (true, true, false) | (false, true, true) => self.outline_toggle_row(i),
                    (true, true, true) => self.outline_select(i + 1, true),
                    (false, _, _) => {
                        let depth = row.depth;
                        if let Some(p) = (0..i).rev().find(|&j| o.rows[j].depth < depth) {
                            self.outline_select(p, true);
                        }
                    }
                    _ => {}
                }
            }
            Key::Enter | Key::Space if k.text.as_deref() != Some(" ") || o.query.is_empty() => {
                if let Some(i) = sel {
                    self.outline_select(i, true);
                    if k.key == Key::Enter {
                        self.focus = Focus::Editor;
                    }
                }
            }
            Key::Backspace => {
                o.query.pop();
                self.outline_find(sel.unwrap_or(0));
            }
            _ if !(k.cmd || k.ctrl) && k.text.as_ref().is_some_and(|t| !t.chars().any(char::is_control)) => {
                o.query.push_str(k.text.as_deref().unwrap_or_default());
                self.outline_find(sel.unwrap_or(0));
            }
            _ => {}
        }
    }

    /// Moves the selection to the first symbol from row `from` on that matches the query.
    fn outline_find(&mut self, from: usize) {
        if self.outline.query.is_empty() {
            return;
        }
        if let Some(i) = self.outline.find_match(from) {
            self.outline_select(i, true);
        }
    }

    pub(super) fn outline_toggle_row(&mut self, i: usize) {
        let Some(key) = self.outline.rows.get(i).map(|r| r.key.clone()) else { return };
        if !self.outline.collapsed.remove(&key) {
            self.outline.collapsed.insert(key);
        }
        self.outline.build_rows();
    }

    /// The header buttons: Collapse All, and the "..." menu.
    pub(super) fn outline_action(&mut self, action: usize) {
        match action {
            0 => {
                fn all(out: &mut HashSet<String>, symbols: &[Symbol], prefix: &str) {
                    for s in symbols.iter().filter(|s| !s.children.is_empty()) {
                        let key = format!("{prefix}/{}", s.name);
                        all(out, &s.children, &key);
                        out.insert(key);
                    }
                }
                let mut keys = HashSet::new();
                all(&mut keys, &self.outline.symbols, "");
                self.outline.collapsed = keys;
                self.outline.scroll = 0.0;
            }
            _ => {
                let o = &self.outline;
                let item = |label: &str, checked: bool| PopupItem::Item { label: label.into(), enabled: true, checked: Some(checked) };
                let entries = vec![
                    (item("Follow Cursor", o.follow_cursor), PopupAction::OutlineFollowCursor),
                    (PopupItem::Separator, PopupAction::None),
                    (item("Sort By: Position", o.sort == Sort::Position), PopupAction::OutlineSort(Sort::Position)),
                    (item("Sort By: Name", o.sort == Sort::Name), PopupAction::OutlineSort(Sort::Name)),
                    (item("Sort By: Category", o.sort == Sort::Category), PopupAction::OutlineSort(Sort::Category)),
                ];
                let (x, y) = self.outline_menu_at;
                self.show_popup(entries, x, y);
            }
        }
    }

    /// Draws the outline's body (the header is drawn with the Explorer's sections).
    pub(super) fn draw_outline(&mut self, c: &mut Canvas, r: Rect) {
        self.outline.body = r;
        self.hits.push((r, Hit::OutlineBody));
        let fg = self.color_or("sideBar.foreground", "foreground");
        let message = match self.outline.status {
            Status::NoEditor => Some("There are no editors open that can provide outline information.".to_string()),
            Status::NoProvider => Some("The active editor cannot provide outline information.".to_string()),
            Status::Loading => None,
            Status::Ready if self.outline.symbols.is_empty() => {
                let name = self.active_doc().and_then(|d| d.buffer.path()).and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                Some(format!("No symbols found in document '{}'", name.unwrap_or_default()))
            }
            Status::Ready => None,
        };
        if let Some(message) = message {
            let style = TextStyle::ui(UI, fg);
            c.push_clip(r);
            let lines = super::intel::wrap(c, &message, &style, r.w - 40.0);
            for (i, line) in lines.iter().enumerate() {
                c.text(r.x + 20.0, r.y + 8.0 + i as f32 * 18.0, line, &style);
            }
            c.pop_clip();
            return;
        }
        self.outline.build_rows();

        // Follow Cursor: highlight the symbol at the cursor; reveal it when it changes.
        let head = self.active_editor().map(|e| e.sel.head);
        let current = head.filter(|_| self.outline.follow_cursor).and_then(|h| symbol_at(&self.outline.symbols, h, ""));
        if current != self.outline.followed {
            self.outline.followed = current.clone();
            if current.is_some() {
                self.outline.selected = current.clone();
            }
            if let Some(key) = &current {
                // Open the collapsed symbols around it, then scroll it into view.
                let o = &mut self.outline;
                o.collapsed.retain(|k| !key.starts_with(&format!("{k}/")));
                o.build_rows();
                if let Some(i) = o.rows.iter().position(|row| &row.key == key) {
                    let top = i as f32 * ROW_H;
                    if top < o.scroll || top + ROW_H > o.scroll + r.h {
                        o.scroll = top - (r.h - ROW_H) / 2.0;
                    }
                }
            }
        }
        self.outline.scroll = self.outline.scroll.clamp(0.0, self.outline.max_scroll());

        let hover = self.hover_hit;
        let focused = self.focus == Focus::Outline && self.palette.is_none();
        let theme = &self.theme;
        let o = &self.outline;
        let highlight = theme.color("list.highlightForeground");
        let indent = crate::config::get().tree_indent;
        let style = TextStyle::ui(UI, fg);
        let detail_style = TextStyle::ui(SMALL, theme.color("descriptionForeground"));
        let first = (o.scroll / ROW_H) as usize;
        let visible = (r.h / ROW_H).ceil() as usize + 1;
        let mut hits = Vec::new();
        c.push_clip(r);
        for i in first..(first + visible).min(o.rows.len()) {
            let row = &o.rows[i];
            let y = r.y + i as f32 * ROW_H - o.scroll;
            let rr = Rect::new(r.x, y, r.w, ROW_H);
            if o.selected.as_ref() == Some(&row.key) {
                let key = if focused { "list.activeSelectionBackground" } else { "list.inactiveSelectionBackground" };
                c.fill_rounded(super::row_pill(rr), theme.color(key), super::ROW_RADIUS);
            } else if hover == Some(Hit::OutlineRow(i)) || hover == Some(Hit::OutlineTwistie(i)) {
                c.fill_rounded(super::row_pill(rr), theme.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let x = rr.x + 8.0 + row.depth as f32 * indent;
            if row.has_children {
                let open = !o.collapsed.contains(&row.key);
                let icon = if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT };
                c.icon(icon, x, y + 3.0, 16.0, fg);
            }
            let (icon, color) = symbol_icon(row.kind);
            c.icon(icon, x + 18.0, y + 3.0, 16.0, theme.color(color));
            let name_x = x + 40.0;
            // The characters matching what's been typed, highlighted.
            let spans: Vec<(usize, usize, render::Color)> = fuzzy(&o.query, &row.name)
                .filter(|_| !o.query.is_empty())
                .map(|(_, m)| row.name.char_indices().enumerate().filter(|(ci, _)| m.contains(ci)).map(|(_, (b, ch))| (b, b + ch.len_utf8(), highlight)).collect())
                .unwrap_or_default();
            let name_w = c.rich_text(name_x, y + ((ROW_H - style.line_height) / 2.0).round(), &row.name, &spans, &style);
            if !row.detail.is_empty() {
                let dx = name_x + name_w + 6.0;
                if dx < rr.right() - 20.0 {
                    c.text_in(Rect::new(dx, y, rr.right() - dx - 4.0, ROW_H), &row.detail, &detail_style);
                }
            }
            hits.push((rr.intersect(&r), Hit::OutlineRow(i)));
            if row.has_children {
                hits.push((Rect::new(x, y, 18.0, ROW_H).intersect(&r), Hit::OutlineTwistie(i)));
            }
        }
        c.pop_clip();
        self.hits.extend(hits);
        if !o.query.is_empty() {
            // The find box, at the top right like the standard tree find widget; its border turns
            // red when nothing matches.
            let w = (r.w - 16.0).min(220.0);
            let bx = Rect::new(r.right() - w - 8.0, r.y + 4.0, w, ROW_H + 4.0);
            let found = o.find_match(0).is_some();
            let border = if found { theme.color("focusBorder") } else { theme.color("inputValidation.errorBorder") };
            c.push_layer(); // above the rows' text
            c.fill(bx.inset(-2.0, -2.0), theme.color("widget.shadow"));
            c.bordered(bx, theme.color("input.background"), border, 1.0, super::controls::FIELD_RADIUS);
            let query_style = TextStyle::ui(UI, theme.color("input.foreground"));
            c.push_clip(bx);
            c.text_fit(Rect::new(bx.x + 6.0, bx.y, bx.w - 12.0, bx.h), &o.query, &query_style);
            c.pop_clip();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, line: usize, children: Vec<Symbol>) -> Symbol {
        let at = Pos::new(line, 0);
        Symbol { name: name.into(), detail: String::new(), kind: 12, range: (at, Pos::new(line + 1, 0)), at, children }
    }

    #[test]
    fn find_wraps_and_skips_collapsed_children() {
        let mut o = Outline::default();
        o.symbols = vec![sym("Canvas", 0, vec![sym("width", 1, vec![]), sym("height", 2, vec![])]), sym("draw", 3, vec![])];
        o.build_rows();
        assert_eq!(o.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Canvas", "width", "height", "draw"]);
        o.query = "h".into();
        // From "height" on, the next match is "height" itself; from "draw" it wraps to "width".
        assert_eq!(o.find_match(2), Some(2));
        assert_eq!(o.find_match(3), Some(1));
        o.collapsed.insert("/Canvas".into());
        o.build_rows();
        assert_eq!(o.rows.len(), 2);
        assert_eq!(o.find_match(0), None);
        o.query = "dr".into();
        assert_eq!(o.find_match(0), Some(1));
    }
}
