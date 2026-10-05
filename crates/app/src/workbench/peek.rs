//! The peek view (Peek References / Peek Definition / Peek Call Hierarchy / Peek Type
//! Hierarchy): opened under the cursor's line, inside the editor, with a read-only preview of
//! the selected location on the left and, on the right, the locations grouped by file or the
//! hierarchy tree (callers or callees, supertypes or subtypes, expanded on demand). ↑/↓ pick, →/← expand and collapse calls, Enter
//! opens, Escape closes. The editor leaves rows for it (`EditorState::peek`).

use std::path::{Path, PathBuf};

use language::{Highlighter, Lang};
use lsp::Encoding;
use serde_json::Value;
use render::{Canvas, Rect, TextStyle};
use text::{Buffer, Pos, Selection};

use super::{Focus, Hit, Workbench, ROW_H, SMALL, UI};
use crate::editor::{expand_tabs_spans, font_size, line_height};
use crate::icons;

/// Rows the peek view takes in the editor.
const PEEK_ROWS: usize = 14;
const HEADER_H: f32 = 26.0;

/// A location in the peek view: a range in a file.
pub(super) struct PeekLoc {
    pub path: PathBuf,
    pub start: Pos,
    pub end: Pos,
    /// Its line's text, for the list.
    pub text: String,
}

/// A file shown in the preview (a copy with its own highlighter, so the open documents stay
/// untouched).
struct Preview {
    path: PathBuf,
    buffer: Buffer,
    highlight: Highlighter,
}

/// A function in the call tree, or a type in the type tree.
struct CallNode {
    /// The server's CallHierarchyItem or TypeHierarchyItem (sent back to ask for its calls
    /// or types).
    item: Value,
    name: String,
    detail: String,
    depth: usize,
    /// Its calls once fetched (indices into the nodes).
    children: Option<Vec<usize>>,
    expanded: bool,
}

/// Peek Call Hierarchy's tree: callers (incoming) or callees of the root, node 0. With `types`,
/// Peek Type Hierarchy's: supertypes (`incoming`) or subtypes.
struct CallTree {
    types: bool,
    incoming: bool,
    nodes: Vec<CallNode>,
    /// Numbers the requests; answers to an older tree are dropped.
    seq: u64,
    /// A file the server knows (to reach it).
    server_path: PathBuf,
}

pub(super) struct Peek {
    pub group: usize,
    pub doc: usize,
    /// "references" or "definitions".
    what: &'static str,
    /// Where each row goes: a location, or with `calls`, the place each node points at.
    pub locs: Vec<PeekLoc>,
    calls: Option<CallTree>,
    pub selected: usize,
    preview: Option<Preview>,
    /// Where the list and preview were drawn, for clicks.
    list: Rect,
    list_scroll: usize,
}

/// A row of the list: a file (with its count), a location (index into `locs`), or a call
/// tree node (index into the nodes and `locs`).
enum ListRow {
    File(PathBuf, usize),
    Loc(usize),
    Call(usize),
}

impl Peek {
    fn rows(&self) -> Vec<ListRow> {
        let mut out = Vec::new();
        if let Some(t) = &self.calls {
            fn walk(t: &CallTree, i: usize, out: &mut Vec<ListRow>) {
                out.push(ListRow::Call(i));
                if t.nodes[i].expanded {
                    for &c in t.nodes[i].children.iter().flatten() {
                        walk(t, c, out);
                    }
                }
            }
            walk(t, 0, &mut out);
            return out;
        }
        let mut i = 0;
        while i < self.locs.len() {
            let path = self.locs[i].path.clone();
            let n = self.locs[i..].iter().take_while(|l| l.path == path).count();
            out.push(ListRow::File(path, n));
            out.extend((i..i + n).map(ListRow::Loc));
            i += n;
        }
        out
    }
}

impl Workbench {
    /// Opens the peek view under the cursor's line for `locations` (sorted by file, then
    /// position), with the one nearest the cursor selected.
    pub(super) fn open_peek(&mut self, locations: Vec<lsp::Location>, encoding: Encoding, what: &'static str) {
        let Some((g, doc)) = self.groups.get(self.active_group).and_then(|gr| gr.tabs.get(gr.active)).map(|t| (self.active_group, t.doc)) else { return };
        let Some(ed) = self.active_editor() else { return };
        let (line, here) = (ed.sel.head.line, ed.sel.head);
        let here_path = self.docs[doc].as_ref().and_then(|d| d.buffer.path().map(Path::to_path_buf));
        let mut locs: Vec<PeekLoc> = Vec::new();
        let mut texts: Vec<(PathBuf, Vec<String>)> = Vec::new();
        let mut sorted = locations;
        sorted.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
        for loc in sorted {
            if !texts.iter().any(|(p, _)| *p == loc.path) {
                let open = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(loc.path.as_path()));
                let lines = match open {
                    Some(d) => (0..d.buffer.len_lines()).map(|l| d.buffer.line(l)).collect(),
                    None => std::fs::read_to_string(&loc.path).unwrap_or_default().lines().map(String::from).collect(),
                };
                texts.push((loc.path.clone(), lines));
            }
            let lines = &texts.iter().find(|(p, _)| *p == loc.path).unwrap().1;
            let pos = |p: lsp::Position| {
                let text = lines.get(p.line as usize).map(String::as_str).unwrap_or_default();
                Pos::new(p.line as usize, encoding.from_lsp(text, p.character))
            };
            let text = lines.get(loc.range.start.line as usize).cloned().unwrap_or_default();
            locs.push(PeekLoc { start: pos(loc.range.start), end: pos(loc.range.end), path: loc.path, text });
        }
        if locs.is_empty() {
            return;
        }
        // Start at the location under the cursor, else the first.
        let selected = locs.iter().position(|l| Some(&l.path) == here_path.as_ref() && l.start.line == here.line).unwrap_or(0);
        self.close_peek();
        self.show_peek(Peek { group: g, doc, what, locs, calls: None, selected, preview: None, list: Rect::default(), list_scroll: 0 }, line);
    }

    /// Opens `peek` under `line` of the active editor.
    fn show_peek(&mut self, peek: Peek, line: usize) {
        let g = peek.group;
        self.peek = Some(peek);
        let active = self.groups[g].active;
        if let Some(ed) = self.groups[g].tabs.get_mut(active) {
            ed.peek = Some((line, PEEK_ROWS));
            ed.reveal = true;
        }
        self.focus = Focus::Peek;
    }

    pub(super) fn close_peek(&mut self) {
        let Some(p) = self.peek.take() else { return };
        for ed in self.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()).filter(|t| t.doc == p.doc) {
            ed.peek = None;
        }
        if self.focus == Focus::Peek {
            self.focus = Focus::Editor;
        }
    }

    /// Opens the selected location in the editor and closes the view.
    fn peek_open_selected(&mut self) {
        let Some(p) = &self.peek else { return };
        let Some(loc) = p.locs.get(p.selected) else { return };
        let (path, pos) = (loc.path.clone(), loc.start);
        self.close_peek();
        self.goto_location(&path, pos);
    }

    pub(super) fn peek_key(&mut self, k: &crate::input::KeyInput) {
        use crate::input::Key;
        let Some(p) = &mut self.peek else {
            self.focus = Focus::Editor;
            return;
        };
        // The rows that can be selected, in order.
        let order: Vec<usize> = p.rows().iter().filter_map(|r| match r {
            ListRow::Loc(i) | ListRow::Call(i) => Some(*i),
            ListRow::File(..) => None,
        }).collect();
        let at = order.iter().position(|&i| i == p.selected).unwrap_or(0);
        match k.key {
            Key::Escape => self.close_peek(),
            Key::Enter => self.peek_open_selected(),
            Key::Up => p.selected = order[at.saturating_sub(1)],
            Key::Down => p.selected = order[(at + 1).min(order.len() - 1)],
            Key::Right | Key::Left if p.calls.is_some() => {
                let i = p.selected;
                let t = p.calls.as_mut().unwrap();
                let expand = k.key == Key::Right;
                if expand == t.nodes[i].expanded {
                    // Right on an open node goes to its first call; Left on a closed one to its parent.
                    if expand {
                        if let Some(&c) = t.nodes[i].children.as_ref().and_then(|c| c.first()) {
                            p.selected = c;
                        }
                    } else if let Some(parent) = t.nodes.iter().position(|n| n.children.as_ref().is_some_and(|c| c.contains(&i))) {
                        p.selected = parent;
                    }
                } else {
                    self.toggle_call_node(i);
                }
            }
            _ => {}
        }
    }

    pub(super) fn click_peek_row(&mut self, i: usize, count: u32) {
        self.focus = Focus::Peek;
        let Some(p) = &mut self.peek else { return };
        let rows = p.rows();
        match rows.get(i) {
            Some(ListRow::Loc(l)) => {
                p.selected = *l;
                if count >= 2 {
                    self.peek_open_selected();
                }
            }
            Some(ListRow::File(path, _)) => {
                if let Some(l) = p.locs.iter().position(|l| &l.path == path) {
                    p.selected = l;
                }
            }
            Some(ListRow::Call(n)) => {
                let n = *n;
                p.selected = n;
                if count >= 2 {
                    self.peek_open_selected();
                }
            }
            None => {}
        }
    }

    // ------------------------------------------------------------ call hierarchy

    /// Peek Call Hierarchy (⌥⇧H): asks for the function at the cursor.
    pub(super) fn show_call_hierarchy(&mut self) {
        let Some(ed) = self.active_editor() else { return };
        let (pos, doc_id) = (ed.sel.head, ed.doc);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        if !self.lsp.prepare_call_hierarchy(&path, &doc.buffer, pos) {
            self.set_status_message("No call hierarchy provider for this file.");
        }
    }

    /// The item at the cursor arrived: open the peek with its callers.
    pub(super) fn call_roots_arrived(&mut self, items: Vec<Value>, encoding: Encoding) {
        let Some(item) = items.into_iter().next() else {
            return self.set_status_message("No results.");
        };
        let Some((g, doc)) = self.groups.get(self.active_group).and_then(|gr| gr.tabs.get(gr.active)).map(|t| (self.active_group, t.doc)) else { return };
        let Some(line) = self.active_editor().map(|e| e.sel.head.line) else { return };
        let Some(server_path) = self.docs[doc].as_ref().and_then(|d| d.buffer.path().map(Path::to_path_buf)) else { return };
        let root_loc = self.item_loc(&item, &item["selectionRange"], encoding);
        let tree = CallTree { types: false, incoming: true, nodes: vec![call_node(item, 0)], seq: 0, server_path };
        self.close_peek();
        self.show_peek(Peek { group: g, doc, what: "calls", locs: vec![root_loc], calls: Some(tree), selected: 0, preview: None, list: Rect::default(), list_scroll: 0 }, line);
        self.toggle_call_node(0);
    }

    // ------------------------------------------------------------ type hierarchy

    /// Peek Type Hierarchy: asks for the type at the cursor.
    pub(super) fn show_type_hierarchy(&mut self) {
        let Some(ed) = self.active_editor() else { return };
        let (pos, doc_id) = (ed.sel.head, ed.doc);
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        if !self.lsp.prepare_type_hierarchy(&path, &doc.buffer, pos) {
            self.set_status_message("No type hierarchy provider for this file.");
        }
    }

    /// The type at the cursor arrived: open the peek with its subtypes.
    pub(super) fn type_roots_arrived(&mut self, items: Vec<Value>, encoding: Encoding) {
        let Some(item) = items.into_iter().next() else {
            return self.set_status_message("No results.");
        };
        let Some((g, doc)) = self.groups.get(self.active_group).and_then(|gr| gr.tabs.get(gr.active)).map(|t| (self.active_group, t.doc)) else { return };
        let Some(line) = self.active_editor().map(|e| e.sel.head.line) else { return };
        let Some(server_path) = self.docs[doc].as_ref().and_then(|d| d.buffer.path().map(Path::to_path_buf)) else { return };
        let root_loc = self.item_loc(&item, &item["selectionRange"], encoding);
        let tree = CallTree { types: true, incoming: false, nodes: vec![call_node(item, 0)], seq: 0, server_path };
        self.close_peek();
        self.show_peek(Peek { group: g, doc, what: "types", locs: vec![root_loc], calls: Some(tree), selected: 0, preview: None, list: Rect::default(), list_scroll: 0 }, line);
        self.toggle_call_node(0);
    }

    /// Node `node`'s supertypes or subtypes arrived.
    pub(super) fn types_arrived(&mut self, node: usize, supertypes: bool, seq: u64, items: Vec<Value>, encoding: Encoding) {
        let Some(p) = &self.peek else { return };
        let Some(t) = p.calls.as_ref().filter(|t| t.types && t.seq == seq && t.incoming == supertypes && node < t.nodes.len()) else { return };
        let depth = t.nodes[node].depth + 1;
        let added: Vec<(CallNode, PeekLoc)> =
            items.into_iter().map(|item| (self.item_loc(&item, &item["selectionRange"], encoding), item)).map(|(loc, item)| (call_node(item, depth), loc)).collect();
        self.add_tree_children(node, added);
    }

    /// Adds `node`'s children (calls or types) to the tree, with their locations.
    fn add_tree_children(&mut self, node: usize, added: Vec<(CallNode, PeekLoc)>) {
        let Some(p) = &mut self.peek else { return };
        let Some(t) = p.calls.as_mut() else { return };
        let mut children = Vec::new();
        for (n, loc) in added {
            children.push(t.nodes.len());
            t.nodes.push(n);
            p.locs.push(loc);
        }
        t.nodes[node].children = Some(children);
    }

    /// Where a call hierarchy item's `range` is (in its file).
    fn item_loc(&self, item: &Value, range: &Value, encoding: Encoding) -> PeekLoc {
        let path = item["uri"].as_str().and_then(lsp::uri_to_path).unwrap_or_default();
        let range = lsp::Range::parse(range).unwrap_or(lsp::Range { start: lsp::Position { line: 0, character: 0 }, end: lsp::Position { line: 0, character: 0 } });
        let text_of = |line: u32| -> String {
            match self.docs.iter().flatten().find(|d| d.buffer.path() == Some(path.as_path())) {
                Some(d) if (line as usize) < d.buffer.len_lines() => d.buffer.line(line as usize),
                Some(_) => String::new(),
                None => std::fs::read_to_string(&path).unwrap_or_default().lines().nth(line as usize).unwrap_or_default().to_string(),
            }
        };
        let pos = |p: lsp::Position| Pos::new(p.line as usize, encoding.from_lsp(&text_of(p.line), p.character));
        PeekLoc { start: pos(range.start), end: pos(range.end), text: text_of(range.start.line), path }
    }

    /// Expands (fetching its calls the first time) or collapses call tree node `i`.
    pub(super) fn toggle_call_node(&mut self, i: usize) {
        let Some(t) = self.peek.as_mut().and_then(|p| p.calls.as_mut()) else { return };
        let node = &mut t.nodes[i];
        node.expanded = !node.expanded;
        if node.expanded && node.children.is_none() {
            let (path, item, incoming, seq) = (t.server_path.clone(), node.item.clone(), t.incoming, t.seq);
            if t.types {
                self.lsp.types(&path, item, incoming, i, seq);
            } else {
                self.lsp.calls(&path, item, incoming, i, seq);
            }
        }
    }

    /// Node `node`'s calls arrived.
    pub(super) fn calls_arrived(&mut self, node: usize, incoming: bool, seq: u64, calls: Vec<Value>, encoding: Encoding) {
        let Some(p) = &self.peek else { return };
        let Some(t) = p.calls.as_ref().filter(|t| !t.types && t.seq == seq && t.incoming == incoming && node < t.nodes.len()) else { return };
        let depth = t.nodes[node].depth + 1;
        let parent_item = t.nodes[node].item.clone();
        let mut added = Vec::new();
        for call in calls {
            let item = if incoming { call["from"].clone() } else { call["to"].clone() };
            // The preview shows the call: in the caller (incoming) or in this function (outgoing).
            let range = call["fromRanges"].get(0).cloned().unwrap_or_else(|| item["selectionRange"].clone());
            let loc = if incoming { self.item_loc(&item, &range, encoding) } else { self.item_loc(&parent_item, &range, encoding) };
            added.push((call_node(item, depth), loc));
        }
        self.add_tree_children(node, added);
    }

    /// The header's toggle (and ⇧⌥H): callers ↔ callees of the same function, or supertypes ↔
    /// subtypes of the same type. False if the peek shows neither.
    pub(super) fn peek_toggle_calls(&mut self) -> bool {
        let Some(p) = &mut self.peek else { return false };
        let Some(t) = &mut p.calls else { return false };
        t.incoming = !t.incoming;
        t.seq += 1;
        t.nodes.truncate(1);
        t.nodes[0].children = None;
        t.nodes[0].expanded = false;
        p.locs.truncate(1);
        p.selected = 0;
        p.list_scroll = 0;
        self.toggle_call_node(0);
        true
    }

    pub(super) fn peek_scroll(&mut self, dy: f32) {
        if let Some(p) = &mut self.peek {
            let steps = (-dy / ROW_H).round() as isize;
            let max = p.rows().len().saturating_sub(1);
            p.list_scroll = (p.list_scroll as isize + steps).clamp(0, max as isize) as usize;
        }
    }

    /// Draws the peek view over the rows the editor of group `g` left for it (`r`).
    pub(super) fn draw_peek(&mut self, c: &mut Canvas, g: usize, r: Rect, clip: Rect) {
        let dir = match &self.peek {
            Some(p) if p.group == g => p.locs.get(p.selected).and_then(|l| l.path.parent()).map(|d| self.display_path(d)).unwrap_or_default(),
            _ => return,
        };
        let Some(p) = &mut self.peek else { return };
        let focused = self.focus == Focus::Peek && self.palette.is_none();
        let theme = &self.theme;
        let Some(sel) = p.locs.get(p.selected) else { return };
        // The preview's file: loaded when the selection moves to another file.
        if p.preview.as_ref().is_none_or(|pv| pv.path != sel.path) {
            let open = self.docs.iter().flatten().find(|d| d.buffer.path() == Some(sel.path.as_path()));
            let text = match open {
                Some(d) => d.buffer.text(),
                None => std::fs::read_to_string(&sel.path).unwrap_or_default(),
            };
            let mut buffer = Buffer::new();
            buffer.insert(Selection::default(), &text);
            let lang = Lang::detect(Some(&sel.path));
            let mut highlight = Highlighter::new(lang);
            highlight.update(&mut buffer);
            p.preview = Some(Preview { path: sel.path.clone(), buffer, highlight });
        }
        let p_has_calls = p.calls.is_some();
        c.push_layer();
        c.push_clip(clip);
        let border = theme.color("peekView.border");
        c.fill(Rect::new(r.x, r.y, r.w, 2.0), border);
        c.fill(Rect::new(r.x, r.bottom() - 2.0, r.w, 2.0), border);
        // Header: the file name, its folder, and the count.
        let (header, body) = Rect::new(r.x, r.y + 2.0, r.w, r.h - 4.0).cut_top(HEADER_H);
        c.fill(header, theme.color("peekViewTitle.background"));
        let name = sel.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let label = TextStyle::ui(UI, theme.color("peekViewTitleLabel.foreground"));
        let desc = TextStyle::ui(SMALL + 1.0, theme.color("peekViewTitleDescription.foreground"));
        let x = header.x + 10.0;
        let w = c.text_in(Rect::new(x, header.y, 400.0, header.h), &name, &label);
        c.text_in(Rect::new(x + w + 8.0, header.y, 400.0, header.h), &dir, &desc);
        // Hierarchies: "Callers of 'add'", "Supertypes of 'Shape'"..., with a button to switch.
        let count = match &p.calls {
            Some(t) => {
                let title = match (t.types, t.incoming) {
                    (false, true) => "Callers of",
                    (false, false) => "Calls from",
                    (true, true) => "Supertypes of",
                    (true, false) => "Subtypes of",
                };
                format!("{title} '{}'", t.nodes[0].name)
            }
            None => format!("{} {}", p.locs.len(), if p.locs.len() == 1 { p.what.trim_end_matches('s') } else { p.what }),
        };
        let close = Rect::new(header.right() - 28.0, header.y + 2.0, 22.0, 22.0);
        let toggle = Rect::new(close.x - 26.0, header.y + 2.0, 22.0, 22.0);
        let right = if p.calls.is_some() { toggle.x } else { close.x };
        let cw = c.measure(&count, &desc);
        c.text_in(Rect::new(right - cw - 12.0, header.y, cw + 2.0, header.h), &count, &desc);
        c.icon_in(&icons::CLOSE, close, 16.0, theme.color("icon.foreground"));
        if let Some(t) = &p.calls {
            let icon = if t.incoming { &icons::ARROW_DOWN } else { &icons::ARROW_UP };
            c.icon_in(icon, toggle, 16.0, theme.color("icon.foreground"));
        }

        // Right: the list; left: the preview.
        let (preview_r, list_r) = body.cut_right((body.w * 0.3).clamp(180.0, 360.0));
        c.fill(list_r, theme.color("peekViewResult.background"));
        let rows = p.rows();
        let visible = (list_r.h / ROW_H).floor() as usize;
        let sel_row = rows.iter().position(|row| matches!(row, ListRow::Loc(l) if *l == p.selected)).unwrap_or(0);
        if sel_row < p.list_scroll {
            p.list_scroll = sel_row;
        } else if sel_row >= p.list_scroll + visible {
            p.list_scroll = sel_row + 1 - visible;
        }
        p.list = list_r;
        let file_style = TextStyle::ui(UI, theme.color("peekViewResult.fileForeground"));
        let line_style = TextStyle::ui(UI, theme.color("peekViewResult.lineForeground"));
        let mut hits = Vec::new();
        c.push_clip(list_r);
        for (i, row) in rows.iter().enumerate().skip(p.list_scroll).take(visible + 1) {
            let y = list_r.y + (i - p.list_scroll) as f32 * ROW_H;
            let rr = Rect::new(list_r.x, y, list_r.w, ROW_H);
            match row {
                ListRow::File(path, n) => {
                    c.icon(&icons::CHEVRON_DOWN, rr.x + 4.0, y + 3.0, 16.0, theme.color("icon.foreground"));
                    let fname = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    c.text_fit(Rect::new(rr.x + 22.0, y, rr.w - 60.0, ROW_H), &fname, &file_style);
                    let badge = n.to_string();
                    let bw = c.measure(&badge, &desc) + 10.0;
                    let b = Rect::new(rr.right() - bw - 8.0, y + 3.0, bw, 16.0);
                    c.fill_rounded(b, theme.color("badge.background"), 8.0);
                    c.text_in(Rect::new(b.x + 5.0, y, bw, ROW_H), &badge, &desc.color(theme.color("badge.foreground")));
                }
                ListRow::Call(n) => {
                    let t = p.calls.as_ref().unwrap();
                    let node = &t.nodes[*n];
                    if *n == p.selected {
                        c.fill(rr, theme.color("peekViewResult.selectionBackground"));
                        if focused {
                            c.bordered(rr, render::Color::TRANSPARENT, theme.color("list.focusOutline"), 1.0, 0.0);
                        }
                    }
                    let x = rr.x + 6.0 + node.depth as f32 * 12.0;
                    // A twistie until the calls are known to be none.
                    if node.children.as_ref().is_none_or(|c| !c.is_empty()) {
                        let chevron = if node.expanded { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT };
                        c.icon(chevron, x, y + 3.0, 16.0, theme.color("icon.foreground"));
                        hits.push((Rect::new(x, y, 18.0, ROW_H), Hit::PeekTwistie(*n)));
                    }
                    let kind = node.item["kind"].as_u64().unwrap_or(12) as u32;
                    let (icon, color) = super::outline::symbol_icon(kind);
                    c.icon(icon, x + 18.0, y + 3.0, 16.0, theme.color(color));
                    let color = if *n == p.selected { theme.color("peekViewResult.selectionForeground") } else { theme.color("peekViewResult.lineForeground") };
                    let nw = c.text_fit(Rect::new(x + 38.0, y, rr.right() - x - 44.0, ROW_H), &node.name, &line_style.color(color));
                    c.text_fit(Rect::new(x + 44.0 + nw, y, (rr.right() - x - 50.0 - nw).max(0.0), ROW_H), &node.detail, &desc);
                }
                ListRow::Loc(l) => {
                    let loc = &p.locs[*l];
                    if *l == p.selected {
                        c.fill(rr, theme.color("peekViewResult.selectionBackground"));
                        if focused {
                            c.bordered(rr, render::Color::TRANSPARENT, theme.color("list.focusOutline"), 1.0, 0.0);
                        }
                    }
                    let text = loc.text.trim();
                    let lead = loc.text.chars().count() - loc.text.trim_start().chars().count();
                    // The matched part of the line, highlighted.
                    let (a, z) = (loc.start.col.saturating_sub(lead), if loc.end.line == loc.start.line { loc.end.col.saturating_sub(lead) } else { text.chars().count() });
                    let byte = |ci: usize| text.char_indices().nth(ci).map_or(text.len(), |(b, _)| b);
                    let x = rr.x + 30.0;
                    let pre_w = c.measure(&text[..byte(a)], &line_style);
                    let mid_w = c.measure(&text[byte(a)..byte(z.max(a))], &line_style);
                    c.fill(Rect::new(x + pre_w, y + 3.0, mid_w, ROW_H - 6.0), theme.color("peekViewResult.matchHighlightBackground"));
                    let color = if *l == p.selected { theme.color("peekViewResult.selectionForeground") } else { theme.color("peekViewResult.lineForeground") };
                    c.text_fit(Rect::new(x, y, rr.right() - x - 6.0, ROW_H), text, &line_style.color(color));
                }
            }
            hits.push((rr, Hit::PeekRow(i)));
        }
        c.pop_clip();

        // The preview: the file around the selected location, with the range highlighted.
        c.fill(preview_r, theme.color("peekViewEditor.background"));
        let pv = p.preview.as_mut().unwrap();
        let lh = line_height();
        let style = TextStyle::mono(font_size(), lh, theme.color("editor.foreground"));
        let cwidth = c.measure("0000000000", &style) / 10.0;
        let n_lines = pv.buffer.len_lines();
        let fits = (preview_r.h / lh).floor() as usize;
        let first = sel.start.line.saturating_sub(fits / 3).min(n_lines.saturating_sub(fits.max(1)));
        let last = (first + fits + 1).min(n_lines);
        let spans = pv.highlight.spans(&pv.buffer, first, last);
        let gutter_w = 12.0 + (n_lines.to_string().len().max(3)) as f32 * cwidth + 16.0;
        c.fill(Rect::new(preview_r.x, preview_r.y, gutter_w, preview_r.h), theme.color("peekViewEditorGutter.background"));
        c.push_clip(preview_r);
        for (k, line) in (first..last).enumerate() {
            let y = preview_r.y + k as f32 * lh;
            let raw = pv.buffer.line(line);
            let (text, sp) = expand_tabs_spans(&raw, spans.get(k).map(Vec::as_slice).unwrap_or_default());
            if line >= sel.start.line && line <= sel.end.line {
                let a = if line == sel.start.line { crate::editor::col_to_display(&raw, sel.start.col) } else { 0 };
                let z = if line == sel.end.line { crate::editor::col_to_display(&raw, sel.end.col) } else { text.chars().count() };
                let x0 = preview_r.x + gutter_w + a as f32 * cwidth;
                c.fill(Rect::new(x0, y, (z.max(a) - a) as f32 * cwidth, lh), theme.color("peekViewEditor.matchHighlightBackground"));
            }
            let num = (line + 1).to_string();
            let numst = style.color(theme.color("editorLineNumber.foreground"));
            let nw = c.measure(&num, &numst);
            c.text(preview_r.x + gutter_w - 16.0 - nw, y, &num, &numst);
            let colored: Vec<(usize, usize, render::Color)> = sp.iter().map(|(a, z, t)| (*a, *z, theme.token(*t))).collect();
            c.rich_text(preview_r.x + gutter_w, y, &text, &colored, &style);
        }
        c.pop_clip();
        c.pop_clip();
        self.hits.push((r, Hit::PeekBody));
        self.hits.push((preview_r, Hit::PeekPreview));
        // Twisties over their rows.
        hits.sort_by_key(|(_, h)| matches!(h, Hit::PeekTwistie(_)));
        self.hits.extend(hits);
        self.hits.push((close, Hit::PeekClose));
        if p_has_calls {
            self.hits.push((toggle, Hit::PeekToggleCalls));
        }
    }
}

fn call_node(item: Value, depth: usize) -> CallNode {
    CallNode {
        name: item["name"].as_str().unwrap_or_default().to_string(),
        detail: item["detail"].as_str().unwrap_or_default().to_string(),
        item,
        depth,
        children: None,
        expanded: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeyInput};

    fn key(k: Key) -> KeyInput {
        KeyInput { key: k, text: None, cmd: false, shift: false, alt: false, ctrl: false }
    }

    #[test]
    fn peek_picks_and_opens_a_location() {
        let dir = std::env::temp_dir().join(format!("orbvane-peek-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&b, "four\nfive\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
        wb.open_file(&a);
        let loc = |path: &Path, line: u32| lsp::Location {
            path: path.to_path_buf(),
            range: lsp::Range { start: lsp::Position { line, character: 0 }, end: lsp::Position { line, character: 3 } },
        };
        wb.open_peek(vec![loc(&b, 1), loc(&a, 2), loc(&a, 0)], Encoding::Utf16, "references");
        let p = wb.peek.as_ref().unwrap();
        // Sorted by file, then position; the one on the cursor's line is selected.
        assert_eq!(p.locs.iter().map(|l| (l.path.file_name().unwrap().to_str().unwrap(), l.start.line)).collect::<Vec<_>>(), [("a.txt", 0), ("a.txt", 2), ("b.txt", 1)]);
        assert_eq!(p.selected, 0);
        assert_eq!(p.locs[2].text, "five");
        assert_eq!(wb.active_editor().unwrap().peek, Some((0, PEEK_ROWS)));
        assert!(wb.focus == Focus::Peek);
        wb.peek_key(&key(Key::Down));
        wb.peek_key(&key(Key::Down));
        wb.peek_key(&key(Key::Enter));
        assert!(wb.peek.is_none());
        assert_eq!(wb.active_doc().and_then(|d| d.buffer.path()), Some(b.as_path()));
        assert_eq!(wb.active_editor().unwrap().sel.head, Pos::new(1, 0));
        assert!(wb.groups.iter().flat_map(|g| &g.tabs).all(|t| t.peek.is_none()));

        // A call tree: the callers of a function, expanded as they arrive.
        let uri = lsp::path_to_uri(&a);
        let item = |name: &str, line: u32| serde_json::json!({ "name": name, "kind": 12, "uri": uri,
            "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 3 } },
            "selectionRange": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 3 } } });
        wb.open_file(&a);
        wb.call_roots_arrived(vec![item("one", 0)], Encoding::Utf16);
        let calls = vec![serde_json::json!({ "from": item("two", 1), "fromRanges": [{ "start": { "line": 1, "character": 1 }, "end": { "line": 1, "character": 2 } }] })];
        wb.calls_arrived(0, true, 0, calls, Encoding::Utf16);
        let p = wb.peek.as_ref().unwrap();
        assert_eq!(p.rows().len(), 2);
        assert_eq!((p.locs[1].start, p.locs[1].text.as_str()), (Pos::new(1, 1), "two"));
        wb.peek_key(&key(Key::Down));
        assert_eq!(wb.peek.as_ref().unwrap().selected, 1);
        // An answer for the other direction (after switching back and forth) is dropped.
        wb.peek_toggle_calls();
        wb.calls_arrived(0, true, 0, vec![], Encoding::Utf16);
        assert!(wb.peek.as_ref().unwrap().calls.as_ref().unwrap().nodes[0].children.is_none());

        // A type tree: subtypes first, supertypes after switching; call answers don't apply.
        wb.close_peek();
        wb.open_file(&a);
        wb.type_roots_arrived(vec![item("one", 0)], Encoding::Utf16);
        let t = wb.peek.as_ref().unwrap().calls.as_ref().unwrap();
        assert!(t.types && !t.incoming);
        wb.calls_arrived(0, false, 0, vec![serde_json::json!({ "to": item("x", 2) })], Encoding::Utf16);
        assert!(wb.peek.as_ref().unwrap().calls.as_ref().unwrap().nodes[0].children.is_none());
        wb.types_arrived(0, false, 0, vec![item("two", 1), item("three", 2)], Encoding::Utf16);
        let p = wb.peek.as_ref().unwrap();
        assert_eq!(p.rows().len(), 3);
        assert_eq!((p.locs[2].start, p.locs[2].text.as_str()), (Pos::new(2, 0), "three"));
        assert!(wb.peek_toggle_calls());
        let t = wb.peek.as_ref().unwrap().calls.as_ref().unwrap();
        assert!(t.incoming && t.seq == 1 && t.nodes.len() == 1);
        wb.types_arrived(0, true, 1, vec![], Encoding::Utf16);
        assert_eq!(wb.peek.as_ref().unwrap().calls.as_ref().unwrap().nodes[0].children, Some(vec![]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
