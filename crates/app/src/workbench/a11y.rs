//! Accessibility, the half that knows nothing about macOS: what the window shows to assistive
//! technology (VoiceOver) and the answers about it. Each frame `draw` collects the window's areas
//! and the editors' text areas as `Node`s (like `hits`); `crate::a11y` turns them into the
//! platform's accessibility elements and asks the questions below when VoiceOver does.
//! Offsets and lengths are UTF-16 code units, lines are buffer lines.

use render::Rect;
use text::{Pos, Selection};

use super::{Focus, Workbench};
use crate::editor::{line_height, Doc, EditorState};

/// The window's areas.
pub const TOOLBAR: u64 = 1;
pub const SIDEBAR: u64 = 2;
pub const EDITORS: u64 = 3;
pub const PANEL: u64 = 4;
pub const STATUS_BAR: u64 = 5;
pub const SECONDARY_SIDEBAR: u64 = 6;
/// Lists: the Explorer's files, the palette's results, Problems, Source Control's changes and
/// Search's results. Their rows are `item(list, index)`.
pub const EXPLORER_LIST: u64 = 10;
pub const PALETTE_LIST: u64 = 11;
pub const PROBLEMS_LIST: u64 = 12;
pub const SCM_LIST: u64 = 13;
pub const SEARCH_LIST: u64 = 14;
/// An editor group's text area: `TEXT_AREA + group`.
pub const TEXT_AREA: u64 = 100;

/// Row `index` of `list`.
pub fn item(list: u64, index: usize) -> u64 {
    (list << 32) | index as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Group,
    TextArea,
    List,
    /// A row of a list (read as its label).
    Item,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: u64,
    pub parent: Option<u64>,
    pub role: Role,
    pub label: String,
    /// In window coordinates.
    pub frame: Rect,
    /// A row that's selected.
    pub selected: bool,
}

/// What notifications compare between frames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub focused: Option<u64>,
    pub version: u64,
    pub selection: (usize, usize),
    /// Said without moving focus (the selected suggestion while typing).
    pub announcement: Option<String>,
}

/// How a file's git status is said.
pub fn status_word(s: scm::FileStatus) -> &'static str {
    use scm::FileStatus::*;
    match s {
        Modified => "modified",
        Added => "added",
        Deleted => "deleted",
        Renamed => "renamed",
        Copied => "copied",
        TypeChanged => "type changed",
        Untracked => "untracked",
        Conflicted => "conflict",
    }
}

impl Workbench {
    /// One of the window's areas, drawn at `frame`.
    pub(super) fn a11y_area(&mut self, id: u64, label: &str, frame: Rect) {
        self.a11y.push(Node { id, parent: None, role: Role::Group, label: label.into(), frame, selected: false });
    }

    /// Group `g`'s editor text (`frame` is where the text is drawn).
    pub(super) fn a11y_text_area(&mut self, g: usize, frame: Rect) {
        let Some((_, doc)) = self.a11y_editor_of(g) else { return };
        let label = doc.title();
        self.a11y.push(Node { id: TEXT_AREA + g as u64, parent: Some(EDITORS), role: Role::TextArea, label, frame, selected: false });
    }

    /// A list inside `parent` (None: on its own, like the palette).
    pub(super) fn a11y_list(&mut self, id: u64, parent: Option<u64>, label: &str, frame: Rect) {
        self.a11y.push(Node { id, parent, role: Role::List, label: label.into(), frame, selected: false });
    }

    /// Row `index` of `list`, as drawn.
    pub(super) fn a11y_item(&mut self, list: u64, index: usize, label: String, frame: Rect, selected: bool) {
        self.a11y.push(Node { id: item(list, index), parent: Some(list), role: Role::Item, label, frame, selected });
    }

    pub fn a11y_node(&self, id: u64) -> Option<&Node> {
        self.a11y.iter().find(|n| n.id == id)
    }

    /// The nodes under `parent` (None: the window's areas), in drawing order.
    pub fn a11y_children(&self, parent: Option<u64>) -> Vec<u64> {
        self.a11y.iter().filter(|n| n.parent == parent).map(|n| n.id).collect()
    }

    /// The node with the keyboard: the palette's selected row while it's open, the Explorer's
    /// selected file, the active editor's text.
    pub fn a11y_focused(&self) -> Option<u64> {
        let shown = |id: u64| self.a11y_node(id).is_some().then_some(id);
        if let Some(p) = &self.palette {
            return shown(item(PALETTE_LIST, p.selected)).or_else(|| shown(PALETTE_LIST));
        }
        if self.settings_active() {
            return None;
        }
        match self.focus {
            Focus::Editor => shown(TEXT_AREA + self.active_group as u64),
            Focus::Explorer => self.tree.as_ref().and_then(|t| t.selected).and_then(|i| shown(item(EXPLORER_LIST, i))).or_else(|| shown(EXPLORER_LIST)),
            _ => None,
        }
    }

    /// The selected rows of `list`.
    pub fn a11y_selected(&self, list: u64) -> Vec<u64> {
        self.a11y.iter().filter(|n| n.parent == Some(list) && n.selected).map(|n| n.id).collect()
    }

    /// What to say without moving focus: the selected suggestion while the list is open.
    fn a11y_announcement(&self) -> Option<String> {
        let comp = self.completion.as_ref()?;
        let item = comp.shown.get(comp.selected).map(|&(i, _)| &comp.items[i])?;
        Some(match &item.detail {
            Some(d) if !d.is_empty() => format!("{}, {d}", item.label),
            _ => item.label.clone(),
        })
    }

    /// Group `g`'s active editor when it's a text editor.
    fn a11y_editor_of(&self, g: usize) -> Option<(&EditorState, &Doc)> {
        let group = self.groups.get(g)?;
        let ed = group.tabs.get(group.active).filter(|e| !e.is_special())?;
        let doc = self.docs.get(ed.doc)?.as_ref()?;
        Some((ed, doc))
    }

    fn a11y_editor(&self, id: u64) -> Option<(&EditorState, &Doc)> {
        let g = id.checked_sub(TEXT_AREA)? as usize;
        self.a11y_editor_of(g)
    }

    /// The whole text.
    pub fn a11y_value(&self, id: u64) -> String {
        self.a11y_editor(id).map(|(_, d)| d.buffer.text()).unwrap_or_default()
    }

    pub fn a11y_length(&self, id: u64) -> usize {
        self.a11y_editor(id).map_or(0, |(_, d)| d.buffer.len_utf16())
    }

    /// The primary selection: (start, length).
    pub fn a11y_selection(&self, id: u64) -> (usize, usize) {
        let Some((ed, d)) = self.a11y_editor(id) else { return (0, 0) };
        let (a, z) = ed.sel.ordered();
        let start = d.buffer.utf16_of(a);
        (start, d.buffer.utf16_of(z) - start)
    }

    /// The line `offset` is on.
    pub fn a11y_line_of(&self, id: u64, offset: usize) -> usize {
        self.a11y_editor(id).map_or(0, |(_, d)| d.buffer.pos_of_utf16(offset).line)
    }

    /// Line `line`: (start, length), its line break included.
    pub fn a11y_line_range(&self, id: u64, line: usize) -> (usize, usize) {
        let Some((_, d)) = self.a11y_editor(id) else { return (0, 0) };
        let b = &d.buffer;
        if line >= b.len_lines() {
            return (b.len_utf16(), 0);
        }
        let start = b.utf16_of(Pos::new(line, 0));
        let end = if line + 1 < b.len_lines() { b.utf16_of(Pos::new(line + 1, 0)) } else { b.len_utf16() };
        (start, end - start)
    }

    pub fn a11y_string(&self, id: u64, start: usize, len: usize) -> String {
        self.a11y_editor(id).map(|(_, d)| d.buffer.text_utf16(start, start + len)).unwrap_or_default()
    }

    /// Where a range is drawn (window coordinates): its first row, from its start to its end
    /// (or the text's right edge when it continues on later rows).
    pub fn a11y_range_frame(&self, id: u64, start: usize, len: usize) -> Rect {
        let Some((ed, d)) = self.a11y_editor(id) else { return Rect::default() };
        let (a, z) = (d.buffer.pos_of_utf16(start), d.buffer.pos_of_utf16(start + len));
        let (x0, y0) = ed.point_of(d, a);
        let (x1, y1) = ed.point_of(d, z);
        let right = if (y1 - y0).abs() < 0.5 { x1.max(x0 + 1.0) } else { ed.geom.text.right() };
        Rect::new(x0, y0, right - x0, line_height())
    }

    /// The lines on screen: (start, length).
    pub fn a11y_visible(&self, id: u64) -> (usize, usize) {
        let Some((ed, d)) = self.a11y_editor(id) else { return (0, 0) };
        let b = &d.buffer;
        let rows = ed.layout.row_count(b).max(1);
        let first = ((ed.scroll_y / line_height()).floor().max(0.0) as usize).min(rows - 1);
        let last = (((ed.scroll_y + ed.geom.text.h) / line_height()).ceil() as usize).clamp(first, rows - 1);
        let (l0, l1) = (ed.layout.row(first, b).line, ed.layout.row(last, b).line);
        let start = b.utf16_of(Pos::new(l0, 0));
        let end = if l1 + 1 < b.len_lines() { b.utf16_of(Pos::new(l1 + 1, 0)) } else { b.len_utf16() };
        (start, end - start)
    }

    /// Assistive technology moved the caret or selected text.
    pub fn a11y_select(&mut self, id: u64, start: usize, len: usize) {
        let Some(g) = id.checked_sub(TEXT_AREA).map(|g| g as usize) else { return };
        let Some((_, d)) = self.a11y_editor_of(g) else { return };
        let (anchor, head) = (d.buffer.pos_of_utf16(start), d.buffer.pos_of_utf16(start + len));
        let group = &mut self.groups[g];
        let ed = &mut group.tabs[group.active];
        ed.set_selection(Selection { anchor, head, goal_col: None });
        ed.reveal = true;
    }

    /// What notifications compare from frame to frame.
    pub fn a11y_state(&self) -> State {
        let announcement = self.a11y_announcement();
        let Some(id) = self.a11y_focused() else { return State { announcement, ..State::default() } };
        let version = self.a11y_editor(id).map_or(0, |(_, d)| d.buffer.version());
        State { focused: Some(id), version, selection: self.a11y_selection(id), announcement }
    }
}
