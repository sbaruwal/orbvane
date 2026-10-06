//! The code editor view: one open document shown in an editor group tab.

use std::path::PathBuf;

use render::{Canvas, Rect, TextStyle};
use text::{Buffer, EditKind, Pos, Selection};
use theme::{Theme, Token};

use crate::config;
use crate::icons;
use crate::input::{Key, KeyInput};
use language::{Highlighter, Lang, Span};

/// Editor font size in pixels (`editor.fontSize`).
pub fn font_size() -> f32 {
    config::get().font_size
}

/// Editor line height in pixels (`editor.lineHeight`).
pub fn line_height() -> f32 {
    config::get().line_height
}

pub(crate) fn tab_size() -> usize {
    config::get().tab_size
}

/// One level of indentation: `editor.tabSize` spaces, or a tab without `editor.insertSpaces`.
pub(crate) fn indent_unit() -> String {
    let cfg = config::get();
    if cfg.insert_spaces { " ".repeat(cfg.tab_size) } else { "\t".into() }
}

const MINIMAP_W: f32 = 90.0;
const SCROLLBAR_W: f32 = 14.0;
const MINIMAP_LINE_H: f32 = 2.0;

pub struct Doc {
    pub buffer: Buffer,
    pub lang: Lang,
    pub highlight: Highlighter,
    /// Untitled documents get a number ("Untitled-1").
    pub untitled: Option<usize>,
    /// Display name for documents without a file (e.g. a deleted file in a diff).
    pub label: Option<String>,
    /// Bracket pairs (colors, matching), rebuilt when the text changes.
    pub brackets: crate::brackets::BracketIndex,
    /// Inlay hints from the language server (`workbench/inlays.rs`).
    pub inlays: crate::layout::Inlays,
    /// Color swatches, by line (`workbench/color_decorators.rs`).
    pub swatches: crate::layout::Inlays,
    /// Extensions' before/after decoration texts (`workbench/ext_decorations.rs`).
    pub ext_inlays: crate::layout::Inlays,
    /// The merged inline decorations, and the generations they were built from.
    decorations: (crate::layout::Inlays, (u64, u64, u64)),
    /// Code lenses from the language server (`workbench/code_lens.rs`).
    pub lenses: crate::layout::Lenses,
    /// Semantic tokens from the language server (`workbench/semantic.rs`).
    pub semantic: crate::workbench::semantic::Semantic,
    /// Folding regions from the language server (`workbench/folding_ranges.rs`).
    pub folding: crate::folding::ServerRanges,
    /// A large file: highlighted by the line
    /// lexer only, with folding, word wrap, the git gutter and language servers off.
    pub large: bool,
    longest_line: (u64, usize),
    /// Each line's display width, kept up to date through the buffer's edit log.
    widths: Vec<u32>,
    widths_seq: Option<u64>,
}

/// The thresholds for large file optimizations.
const LARGE_BYTES: usize = 20 * 1024 * 1024;
const LARGE_LINES: usize = 300_000;

impl Doc {
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let buffer = Buffer::open(&path)?;
        Ok(Self::with_buffer(buffer, None))
    }

    /// Inlay hints, color swatches and extensions' before/after texts together (swatches first
    /// where several are at a column), rebuilt when any of them changes.
    pub fn inline_decorations(&mut self) -> &crate::layout::Inlays {
        use std::sync::atomic::{AtomicU64, Ordering};
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let key = (self.inlays.generation, self.swatches.generation, self.ext_inlays.generation);
        if self.decorations.1 != key {
            let sources = [&self.swatches.lines, &self.inlays.lines, &self.ext_inlays.lines];
            let nonempty: Vec<_> = sources.iter().filter(|l| !l.is_empty()).collect();
            let lines = match nonempty.as_slice() {
                [] => Default::default(),
                [one] => (**one).clone(),
                _ => {
                    let mut all = std::collections::HashMap::new();
                    for src in sources {
                        for (line, hints) in src.iter() {
                            all.entry(*line).or_insert_with(Vec::new).extend(hints.iter().cloned());
                        }
                    }
                    for v in all.values_mut() {
                        v.sort_by_key(|h: &crate::layout::InlayHint| (h.col, h.swatch.is_none()));
                    }
                    std::sync::Arc::new(all)
                }
            };
            let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
            self.decorations = (crate::layout::Inlays { lines, generation }, key);
        }
        &self.decorations.0
    }

    pub fn untitled(n: usize) -> Self {
        Self::with_buffer(Buffer::new(), Some(n))
    }

    fn with_buffer(buffer: Buffer, untitled: Option<usize>) -> Self {
        let lang = Lang::detect(buffer.path());
        let large = buffer.len_bytes() > LARGE_BYTES || buffer.len_lines() > LARGE_LINES;
        Self {
            highlight: Highlighter::with_parser(lang, !large),
            buffer,
            lang,
            untitled,
            label: None,
            longest_line: (u64::MAX, 0),
            brackets: Default::default(),
            inlays: Default::default(),
            swatches: Default::default(),
            ext_inlays: Default::default(),
            decorations: Default::default(),
            lenses: Default::default(),
            semantic: Default::default(),
            folding: Default::default(),
            large,
            widths: Vec::new(),
            widths_seq: None,
        }
    }

    /// A pathless document with `text`, highlighted as `name`'s language.
    pub fn open_virtual(name: &str, text: &str) -> Self {
        let mut doc = Self::virtual_named(name);
        doc.buffer.insert(Selection::default(), text);
        doc.buffer.break_undo_group();
        doc
    }

    /// An empty, pathless document shown under `name` (the right side of a deleted file's diff).
    pub fn virtual_named(name: &str) -> Self {
        let mut doc = Self::with_buffer(Buffer::new(), None);
        doc.lang = Lang::detect(Some(std::path::Path::new(name)));
        doc.highlight = Highlighter::new(doc.lang);
        doc.label = Some(name.to_string());
        doc
    }

    pub fn title(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        match (self.buffer.path(), self.untitled) {
            (Some(p), _) => p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into()),
            (None, Some(n)) => format!("Untitled-{n}"),
            (None, None) => "Untitled".into(),
        }
    }

    pub fn set_path(&mut self, path: PathBuf) {
        self.buffer.set_path(path);
        self.untitled = None;
        // A file's name is its title (a placeholder's label no longer applies).
        self.label = None;
        self.lang = Lang::detect(self.buffer.path());
        self.highlight = Highlighter::new(self.lang);
    }

    fn longest_line(&mut self) -> usize {
        let v = self.buffer.version();
        if self.longest_line.0 != v {
            self.update_widths();
            self.longest_line = (v, self.widths.iter().copied().max().unwrap_or(0) as usize);
        }
        self.longest_line.1
    }

    /// Brings the line widths up to date: only lines the edits since the last time touched are
    /// measured again (a keystroke in a million-line file doesn't re-measure it).
    fn update_widths(&mut self) {
        const STALE: u32 = u32::MAX;
        let b = &self.buffer;
        let seq = b.edit_seq();
        let edits = self.widths_seq.and_then(|s| b.edits_since(s)).filter(|_| !self.widths.is_empty());
        let mut ok = edits.is_some();
        for change in edits.into_iter().flatten() {
            let text::Change::Edit(e) = change else {
                ok = false;
                break;
            };
            let (start, old_end, new_end) = (e.start.0, e.old_end.0, e.new_end.0);
            if old_end >= self.widths.len() {
                ok = false;
                break;
            }
            self.widths.splice(start..=old_end, std::iter::repeat_n(STALE, new_end - start + 1));
        }
        if !ok || self.widths.len() != b.len_lines() {
            self.widths = b.lines_from(0).take(b.len_lines()).map(|l| display_width(&l) as u32).collect();
        } else {
            for (i, w) in self.widths.iter_mut().enumerate().filter(|(_, w)| **w == STALE) {
                *w = display_width(&b.line_cow(i)) as u32;
            }
        }
        self.widths_seq = Some(seq);
    }
}

/// A diagnostic range to underline, already converted to document positions.
#[derive(Clone, Copy, Debug)]
pub struct Squiggle {
    pub start: Pos,
    pub end: Pos,
    pub severity: lsp::Severity,
}

pub fn severity_color(theme: &Theme, severity: lsp::Severity) -> theme::Color {
    theme.color(match severity {
        lsp::Severity::Error => "editorError.foreground",
        lsp::Severity::Warning => "editorWarning.foreground",
        lsp::Severity::Information => "editorInfo.foreground",
        lsp::Severity::Hint => "editorHint.foreground",
    })
}

/// Overlays drawn on top of the text: diagnostics and find matches.
#[derive(Default)]
pub struct Decorations<'a> {
    pub squiggles: &'a [Squiggle],
    /// Find matches as (start, end), sorted by position.
    pub matches: &'a [(Pos, Pos)],
    /// Index of the current find match (highlighted more strongly).
    pub current_match: Option<usize>,
    /// Git change markers for the gutter.
    pub git: &'a [scm::LineChange],
    /// Merge conflict blocks, highlighted with inline accept actions.
    pub conflicts: &'a [crate::conflicts::Conflict],
    /// The code action lightbulb: (line, whether a preferred quick fix exists).
    pub lightbulb: Option<(usize, bool)>,
    /// Snippet placeholders to highlight: (start, end, is the final cursor).
    pub snippet: &'a [(Pos, Pos, bool)],
    /// Breakpoints in the glyph margin, by line.
    pub breakpoints: &'a [(usize, BpLook)],
    /// The focused stack frame's line while debugging, and whether it's the top frame.
    pub stack_frame: Option<(usize, bool)>,
    /// No line numbers (Zen Mode).
    pub hide_line_numbers: bool,
    /// Tests in the glyph margin: (line, state).
    pub tests: &'a [(usize, crate::testing::TestState)],
    /// Test failure messages shown after their lines: (line, first line of the message).
    pub test_messages: &'a [(usize, String)],
    /// A search editor's matches (sorted).
    pub search_results: &'a [(Pos, Pos)],
    /// A merge editor's ranges in the result.
    pub merge: &'a [MergeMark],
    /// Linked editing ranges (tag names that change together).
    pub linked: &'a [(Pos, Pos)],
    /// Extensions' decorations (`window/setDecorations`).
    pub ext: &'a [ExtDeco],
}

/// A range an extension decorated, resolved to colors.
#[derive(Clone, Debug)]
pub struct ExtDeco {
    pub start: Pos,
    pub end: Pos,
    pub background: Option<theme::Color>,
    /// The text's color.
    pub color: Option<theme::Color>,
    pub border: Option<theme::Color>,
    pub underline: bool,
    pub strike: bool,
    /// The background covers the whole lines.
    pub whole_line: bool,
    pub ruler: Option<theme::Color>,
}

/// A range of a merge editor's result: its lines' background and border.
pub struct MergeMark {
    pub lines: std::ops::Range<usize>,
    pub background: render::Color,
    pub border: render::Color,
}

/// How a breakpoint is drawn in the glyph margin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BpLook {
    Normal,
    Conditional,
    Log,
    Disabled,
    /// Sent, but the adapter couldn't set it (no code there): a hollow circle.
    Unverified,
}

/// Screen geometry from the last draw, used for hit-testing mouse input.
#[derive(Clone, Copy, Default)]
pub struct Geom {
    pub text: Rect,
    pub minimap: Rect,
    pub scrollbar: Rect,
    pub slider: Rect,
    pub cw: f32,
    pub minimap_first: usize,
    /// Where the code action lightbulb was drawn.
    pub lightbulb: Option<Rect>,
    /// Where the peek view's rows are (the workbench draws the view there).
    pub peek: Option<Rect>,
    /// The glyph margin (left of the line numbers), where clicks toggle breakpoints.
    pub glyph: Rect,
}

pub struct EditorState {
    pub doc: usize,
    pub sel: Selection,
    pub scroll_y: f32,
    pub scroll_x: f32,
    pub geom: Geom,
    /// Scroll the caret into view on the next draw.
    pub reveal: bool,
    /// Scroll so the caret sits a third of the way down on the next draw (jumps).
    center: bool,
    /// Set for a diff tab (read-only side-by-side comparison of this document).
    pub diff: Option<Box<crate::diff_view::DiffState>>,
    /// The Welcome page's tab.
    pub welcome: bool,
    /// A preview tab: reused by the next file opened with a single click (italic title).
    pub preview: bool,
    /// Secondary cursors (multi-cursor editing); `sel` is the primary one.
    pub extra: Vec<Selection>,
    /// ⌘D started from an empty selection, so it matches whole words only.
    pub word_occurrences: bool,
    /// Screen rows (word wrap, folds), rebuilt in `draw` and after edits.
    pub layout: crate::layout::Layout,
    /// Collapsed regions.
    pub folds: crate::folding::Folds,
    /// Fold chevrons drawn last frame: (rect, region start line), for clicks.
    pub fold_controls: Vec<(Rect, usize)>,
    /// Sticky scroll's lines as drawn: (rect, buffer line), for clicks.
    pub sticky: Vec<(Rect, usize)>,
    /// Code lenses as drawn: (rect, the lens's index in the server's list), for clicks.
    pub lens_hits: Vec<(Rect, usize)>,
    /// Color swatches drawn last frame: where, and the (line, column) of their color.
    pub swatch_hits: Vec<(Rect, Pos)>,
    /// The peek view's place: rows under a line (line, rows).
    pub peek: Option<(usize, usize)>,
    /// A Markdown preview tab of the document (drawn instead of the editor).
    pub markdown: Option<Box<crate::workbench::markdown_view::Preview>>,
    /// An image file's tab (drawn instead of the editor).
    pub image: Option<Box<crate::workbench::image_view::ImagePreview>>,
    /// A search editor: its header's state (the document holds the results).
    pub search: Option<Box<crate::workbench::search_editor_view::SearchEditorState>>,
    /// A merge editor: this tab's document is the result.
    pub merge: Option<Box<crate::workbench::merge_view::MergeState>>,
    /// Merge conflict actions drawn last frame: (rect, conflict, resolution), for clicks.
    pub conflict_actions: Vec<(Rect, crate::conflicts::Conflict, crate::conflicts::Resolution)>,
}

/// Where a cursor goes after its edit, relative to the start of the inserted text.
#[derive(Clone, Copy, Debug)]
enum After {
    /// After the inserted text.
    End,
    /// This many chars into it.
    Offset(usize),
    /// Select chars `.0..1` of it.
    Select(usize, usize),
}

/// The columns char `c` takes when it starts at display column `col`: a tab reaches the next
/// tab stop, a wide (East Asian) char takes 2, a combining mark 0.
pub(crate) fn cells(c: char, col: usize) -> usize {
    if c == '\t' { tab_size() - col % tab_size() } else { render::char_cells(c) }
}

/// Width in columns of `s` with tabs expanded to tab stops.
pub(crate) fn display_width(s: &str) -> usize {
    if s.is_ascii() && !s.contains('\t') {
        return s.len();
    }
    s.chars().fold(0, |col, c| col + cells(c, col))
}

/// Recolors bytes `a..z` in sorted, non-overlapping color spans (splitting the one it's in).
fn overlay(spans: &mut Vec<(usize, usize, theme::Color)>, a: usize, z: usize, color: theme::Color) {
    let mut out = Vec::with_capacity(spans.len() + 2);
    for &(s, e, c) in spans.iter() {
        if e <= a || s >= z {
            out.push((s, e, c));
            continue;
        }
        if s < a {
            out.push((s, a, c));
        }
        if e > z {
            out.push((z, e, c));
        }
    }
    out.push((a, z, color));
    out.sort_by_key(|s| s.0);
    *spans = out;
}

/// Expands tabs and moves highlight spans (byte ranges in `s`) to match the expanded text.
pub(crate) fn expand_tabs_spans(s: &str, spans: &[Span]) -> (String, Vec<Span>) {
    if !s.contains('\t') {
        return (s.to_string(), spans.to_vec());
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut col = 0;
    for c in s.chars() {
        map.extend(std::iter::repeat_n(out.len(), c.len_utf8()));
        if c == '\t' {
            let n = tab_size() - col % tab_size();
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += cells(c, col);
        }
    }
    map.push(out.len());
    let spans = spans.iter().map(|&(a, b, t)| (map[a.min(s.len())], map[b.min(s.len())], t)).collect();
    (out, spans)
}

pub(crate) fn col_to_display(line: &str, col: usize) -> usize {
    line.chars().take(col).fold(0, |d, c| d + cells(c, d))
}

pub(crate) fn display_to_col(line: &str, target: f32) -> usize {
    let mut d = 0usize;
    for (i, c) in line.chars().enumerate() {
        let w = cells(c, d);
        if target < d as f32 + w as f32 / 2.0 {
            return i;
        }
        d += w;
    }
    line.chars().count()
}

fn closing_pair(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

impl EditorState {
    pub fn new(doc: usize) -> Self {
        Self { doc, sel: Selection::default(), scroll_y: 0.0, scroll_x: 0.0, geom: Geom::default(), reveal: false, center: false, diff: None, welcome: false, extra: Vec::new(), word_occurrences: false, conflict_actions: Vec::new(), layout: Default::default(), folds: Default::default(), fold_controls: Vec::new(), sticky: Vec::new(), lens_hits: Vec::new(), swatch_hits: Vec::new(), peek: None, markdown: None, image: None, search: None, merge: None, preview: false }
    }

    /// Diff tabs, previews and the Welcome page aren't text editors.
    pub fn is_special(&self) -> bool {
        self.diff.is_some() || self.welcome || self.markdown.is_some() || self.image.is_some()
    }

    pub fn line_col(&self) -> (usize, usize) {
        (self.sel.head.line + 1, self.sel.head.col + 1)
    }

    fn set_head(&mut self, pos: Pos, extend: bool, keep_goal: bool) {
        let goal = if keep_goal { self.sel.goal_col } else { None };
        self.sel = if extend {
            Selection { anchor: self.sel.anchor, head: pos, goal_col: goal }
        } else {
            Selection { anchor: pos, head: pos, goal_col: goal }
        };
        self.extra.clear();
        self.word_occurrences = false;
        self.reveal = true;
    }

    /// Every selection, the primary one first.
    pub fn selections(&self) -> Vec<Selection> {
        std::iter::once(self.sel).chain(self.extra.iter().copied()).collect()
    }

    /// Replaces all cursors with one selection.
    pub fn set_selection(&mut self, sel: Selection) {
        self.sel = sel;
        self.extra.clear();
        self.word_occurrences = false;
    }

    /// Sets every selection (the primary first), merging ones that overlap.
    pub fn set_selections(&mut self, sels: Vec<Selection>) {
        if sels.is_empty() {
            return;
        }
        let mut order: Vec<(Selection, bool)> = sels.into_iter().enumerate().map(|(i, s)| (s, i == 0)).collect();
        order.sort_by_key(|(s, _)| s.ordered());
        let mut merged: Vec<(Selection, bool)> = Vec::with_capacity(order.len());
        for (s, primary) in order {
            if let Some((last, last_primary)) = merged.last_mut() {
                let (la, lz) = last.ordered();
                let (a, z) = s.ordered();
                let touching = a == lz && (s.is_empty() || last.is_empty());
                if a < lz || touching || (a, z) == (la, lz) {
                    let end = lz.max(z);
                    *last = if last.anchor <= last.head {
                        Selection { anchor: la, head: end, goal_col: last.goal_col }
                    } else {
                        Selection { anchor: end, head: la, goal_col: last.goal_col }
                    };
                    *last_primary |= primary;
                    continue;
                }
            }
            merged.push((s, primary));
        }
        let primary = merged.iter().position(|(_, p)| *p).unwrap_or(0);
        self.sel = merged[primary].0;
        self.extra = merged.into_iter().enumerate().filter(|(i, _)| *i != primary).map(|(_, (s, _))| s).collect();
        self.reveal = true;
    }

    /// Moves (or extends) every selection with `f`, which maps a selection to its new head.
    fn move_all(&mut self, extend: bool, keep_goal: bool, mut f: impl FnMut(Selection) -> Pos) {
        self.word_occurrences = false;
        let sels = self
            .selections()
            .into_iter()
            .map(|s| {
                let head = f(s);
                let goal = if keep_goal { s.goal_col } else { None };
                Selection { anchor: if extend { s.anchor } else { head }, head, goal_col: goal }
            })
            .collect();
        self.set_selections(sels);
    }

    /// Moves every caret `lines` up or down, keeping each one's goal column.
    /// Moves every cursor by `rows` screen rows (wrapped lines take several), keeping each
    /// cursor's goal column.
    fn vertical(&mut self, doc: &Doc, rows: isize, extend: bool) {
        let b = &doc.buffer;
        self.refresh_layout(b);
        let layout = &self.layout;
        let sels = self
            .selections()
            .into_iter()
            .map(|s| {
                let head = s.head;
                let goal = s.goal_col.unwrap_or_else(|| layout.x_of(head, b));
                let pos = layout.move_rows(head, rows, goal, b);
                Selection { anchor: if extend { s.anchor } else { pos }, head: pos, goal_col: Some(goal) }
            })
            .collect();
        self.set_selections(sels);
    }

    /// Applies one edit per selection (in `selections()` order) as a single undo step, and
    /// places each cursor as its `After` says.
    fn apply(&mut self, doc: &mut Doc, kind: EditKind, mut edits: Vec<(Pos, Pos, String, After)>) {
        let before = self.selections();
        let b = &mut doc.buffer;
        for e in &mut edits {
            if e.1 < e.0 {
                std::mem::swap(&mut e.0, &mut e.1);
            }
        }
        // Clip ranges that overlap an earlier one (e.g. word deletes from adjacent carets).
        let mut order: Vec<usize> = (0..edits.len()).collect();
        order.sort_by_key(|&i| edits[i].0);
        let mut prev_end: Option<Pos> = None;
        for &i in &order {
            if let Some(end) = prev_end {
                if edits[i].0 < end {
                    edits[i].0 = end.min(edits[i].1);
                }
            }
            prev_end = Some(prev_end.map_or(edits[i].1, |p| p.max(edits[i].1)));
        }
        let refs: Vec<(Pos, Pos, &str)> = edits.iter().map(|(a, z, t, _)| (*a, *z, t.as_str())).collect();
        let starts = b.edit(&before, &refs, kind);
        let sels = edits
            .iter()
            .zip(&starts)
            .map(|((_, _, text, after), &start)| match *after {
                After::End => Selection::caret(b.pos_of(start + text.chars().count())),
                After::Offset(n) => Selection::caret(b.pos_of(start + n)),
                After::Select(a, z) => Selection { anchor: b.pos_of(start + a), head: b.pos_of(start + z), goal_col: None },
            })
            .collect();
        self.set_selections(sels);
    }

    pub fn visible_lines(&self) -> usize {
        (self.geom.text.h / line_height()).floor().max(1.0) as usize
    }

    /// Brings folds and screen rows up to date with the buffer (after edits between frames).
    pub fn refresh_layout(&mut self, b: &Buffer) {
        self.folds.sync(b);
        let hidden = self.folds.hidden(b);
        self.layout.update(b, self.layout.wrap(), &hidden, self.folds.generation());
    }

    /// Handles a key press. Returns false if the key wasn't used.
    pub fn key(&mut self, doc: &mut Doc, k: &KeyInput) -> bool {
        self.refresh_layout(&doc.buffer);
        let used = self.key_inner(doc, k);
        // ←/→ step over folded regions instead of landing inside (which would open them).
        if matches!(k.key, Key::Left | Key::Right) {
            let b = &doc.buffer;
            let layout = &self.layout;
            let skip = |p: Pos| -> Pos {
                if !layout.is_hidden(p.line) {
                    return p;
                }
                let mut l = p.line;
                if k.key == Key::Right {
                    while l + 1 < b.len_lines() && layout.is_hidden(l) {
                        l += 1;
                    }
                    if layout.is_hidden(l) { b.end() } else { Pos::new(l, 0) }
                } else {
                    while l > 0 && layout.is_hidden(l) {
                        l -= 1;
                    }
                    Pos::new(l, b.line_len(l))
                }
            };
            let sels = self.selections().into_iter().map(|s| Selection { anchor: skip(s.anchor), head: skip(s.head), goal_col: s.goal_col }).collect();
            self.set_selections(sels);
        }
        used
    }

    fn key_inner(&mut self, doc: &mut Doc, k: &KeyInput) -> bool {
        let extend = k.shift;
        match &k.key {
            Key::Left | Key::Right if !extend && !k.cmd && !k.alt && self.selections().iter().any(|s| !s.is_empty()) => {
                // Collapse each selection to its start or end.
                let left = k.key == Key::Left;
                let b = &doc.buffer;
                self.move_all(false, false, |s| {
                    if s.is_empty() {
                        if left { b.left(s.head) } else { b.right(s.head) }
                    } else {
                        let (a, z) = s.ordered();
                        if left { a } else { z }
                    }
                });
            }
            Key::Left => {
                let b = &doc.buffer;
                self.move_all(extend, false, |s| {
                    let h = s.head;
                    if k.cmd {
                        let first = b.first_non_blank(h.line);
                        Pos::new(h.line, if h.col == first { 0 } else { first })
                    } else if k.alt {
                        b.word_left(h)
                    } else {
                        b.left(h)
                    }
                });
            }
            Key::Right => {
                let b = &doc.buffer;
                self.move_all(extend, false, |s| {
                    let h = s.head;
                    if k.cmd {
                        Pos::new(h.line, b.line_len(h.line))
                    } else if k.alt {
                        b.word_right(h)
                    } else {
                        b.right(h)
                    }
                });
            }
            Key::Up if k.cmd => self.move_all(extend, false, |_| Pos::new(0, 0)),
            Key::Down if k.cmd => {
                let end = doc.buffer.end();
                self.move_all(extend, false, |_| end)
            }
            Key::Up => self.vertical(doc, -1, extend),
            Key::Down => self.vertical(doc, 1, extend),
            Key::PageUp => {
                let n = self.visible_lines() as isize;
                self.scroll_y = (self.scroll_y - n as f32 * line_height()).max(0.0);
                self.vertical(doc, -n, extend);
            }
            Key::PageDown => {
                let n = self.visible_lines() as isize;
                self.scroll_y += n as f32 * line_height();
                self.vertical(doc, n, extend);
            }
            Key::Home => self.move_all(extend, false, |s| Pos::new(s.head.line, 0)),
            Key::End => {
                let b = &doc.buffer;
                self.move_all(extend, false, |s| Pos::new(s.head.line, b.line_len(s.head.line)));
            }
            Key::Escape => {
                // Esc drops the extra cursors first, then collapses the selection.
                if !self.extra.is_empty() {
                    self.extra.clear();
                } else if !self.sel.is_empty() {
                    self.set_selection(Selection::caret(self.sel.head));
                } else {
                    return false;
                }
                self.reveal = true;
            }
            Key::Backspace => {
                let b = &doc.buffer;
                let edits = self
                    .selections()
                    .into_iter()
                    .map(|s| {
                        let h = s.head;
                        let (a, z) = if !s.is_empty() {
                            s.ordered()
                        } else if k.cmd {
                            (Pos::new(h.line, 0), h)
                        } else if k.alt {
                            (b.word_left(h), h)
                        } else {
                            // Delete an empty auto-closed pair together: (|)
                            let line: Vec<char> = b.line(h.line).chars().collect();
                            if h.col > 0 && h.col < line.len() && closing_pair(line[h.col - 1]) == Some(line[h.col]) {
                                (Pos::new(h.line, h.col - 1), Pos::new(h.line, h.col + 1))
                            } else {
                                (b.left(h), h)
                            }
                        };
                        (a, z, String::new(), After::End)
                    })
                    .collect();
                let kind = if self.selections().iter().all(|s| s.is_empty()) && !k.cmd && !k.alt { EditKind::Delete } else { EditKind::Other };
                self.apply(doc, kind, edits);
            }
            Key::Delete => {
                let b = &doc.buffer;
                let edits = self
                    .selections()
                    .into_iter()
                    .map(|s| {
                        let (a, z) = if !s.is_empty() {
                            s.ordered()
                        } else if k.alt {
                            (s.head, b.word_right(s.head))
                        } else {
                            (s.head, b.right(s.head))
                        };
                        (a, z, String::new(), After::End)
                    })
                    .collect();
                let kind = if self.selections().iter().all(|s| s.is_empty()) && !k.alt { EditKind::Delete } else { EditKind::Other };
                self.apply(doc, kind, edits);
            }
            Key::Enter => {
                let b = &doc.buffer;
                let rust = doc.lang == Lang::Rust;
                let edits = self
                    .selections()
                    .into_iter()
                    .map(|s| {
                        let (h, z) = s.ordered();
                        let line = b.line(h.line);
                        let indent = b.leading_whitespace(h.line);
                        let before: Vec<char> = line.chars().take(h.col).collect();
                        let after = if s.is_empty() { line.chars().nth(h.col) } else { None };
                        let opens = before.iter().rev().find(|c| !c.is_whitespace()).is_some_and(|c| matches!(c, '{' | '(' | '[' | ':'))
                            && !(rust && before.last() == Some(&':'));
                        let mut text = format!("\n{indent}");
                        if opens {
                            text.push_str(&indent_unit());
                        }
                        let caret = text.chars().count();
                        // Put a closing bracket on its own line: {|} -> {\n    |\n}
                        if opens && matches!(after, Some('}') | Some(')') | Some(']')) {
                            text.push_str(&format!("\n{indent}"));
                        }
                        (h, z, text, After::Offset(caret))
                    })
                    .collect();
                self.apply(doc, EditKind::Other, edits);
                doc.buffer.break_undo_group();
            }
            Key::Tab => {
                let multiline = self.selections().iter().any(|s| s.anchor.line != s.head.line);
                if multiline || k.shift {
                    self.indent_lines(doc, !k.shift);
                } else {
                    let b = &doc.buffer;
                    let spaces = config::get().insert_spaces;
                    let edits = self
                        .selections()
                        .into_iter()
                        .map(|s| {
                            let (a, z) = s.ordered();
                            let text = if spaces {
                                let col = col_to_display(&b.line(a.line), a.col);
                                " ".repeat(tab_size() - col % tab_size())
                            } else {
                                "\t".to_string()
                            };
                            (a, z, text, After::End)
                        })
                        .collect();
                    self.apply(doc, EditKind::Other, edits);
                }
            }
            Key::Char(_) if k.cmd || k.ctrl => return false,
            Key::Char(_) | Key::Space => {
                let Some(text) = &k.text else { return false };
                self.type_text(doc, text);
            }
            _ => return false,
        }
        self.reveal = true;
        true
    }

    /// Types `text` at every cursor, with auto-closing pairs and stepping over closers.
    pub fn type_text(&mut self, doc: &mut Doc, text: &str) {
        let b = &doc.buffer;
        let mut chars = text.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        let auto_close = config::get().auto_close;
        let mut typing = true;
        let edits: Vec<(Pos, Pos, String, After)> = self
            .selections()
            .into_iter()
            .map(|s| {
                let (a, z) = s.ordered();
                let h = s.head;
                let line: Vec<char> = b.line(h.line).chars().collect();
                let next = line.get(h.col).copied();
                if let Some(c) = single {
                    // Typing a closing char that's already there just steps over it.
                    if s.is_empty() && matches!(c, ')' | ']' | '}' | '"' | '\'' | '`') && next == Some(c) {
                        return (h, h, String::new(), After::Offset(1));
                    }
                    if let Some(close) = closing_pair(c) {
                        if !s.is_empty() {
                            // Wrap the selection, keeping the wrapped text selected (on one line).
                            typing = false;
                            let inner = b.text_in(&s);
                            let n = inner.chars().count();
                            let after = if inner.contains('\n') { After::Offset(1 + n) } else { After::Select(1, 1 + n) };
                            return (a, z, format!("{c}{inner}{close}"), after);
                        }
                        let prev = if h.col > 0 { line.get(h.col - 1).copied() } else { None };
                        let next_ok = match auto_close {
                            config::AutoClose::Never => false,
                            config::AutoClose::BeforeWhitespace => next.is_none_or(char::is_whitespace),
                            config::AutoClose::Always => {
                                next.is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';'))
                            }
                        };
                        let quote = matches!(c, '"' | '\'' | '`');
                        let prev_ok = !quote || prev.is_none_or(|p| !p.is_alphanumeric());
                        // Rust lifetimes and chars: don't auto-close ' in Rust.
                        let lifetime = c == '\'' && doc.lang == Lang::Rust;
                        if next_ok && prev_ok && !lifetime {
                            typing = false;
                            return (h, h, format!("{c}{close}"), After::Offset(1));
                        }
                    }
                }
                if !s.is_empty() || text.chars().any(char::is_whitespace) {
                    typing = false;
                }
                (a, z, text.to_string(), After::End)
            })
            .collect();
        self.apply(doc, if typing { EditKind::Insert } else { EditKind::Other }, edits);
    }

    /// Lines covered by any selection (a selection ending at column 0 doesn't include that line).
    fn selected_lines(&self) -> Vec<usize> {
        let mut lines: Vec<usize> = self
            .selections()
            .iter()
            .flat_map(|s| {
                let (a, z) = s.ordered();
                let last = if z.col == 0 && z.line > a.line { z.line - 1 } else { z.line };
                a.line..=last
            })
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// Replaces whole lines in one undo step, shifting cursors on those lines by the change in
    /// length (cursors in a removed prefix end up at column 0).
    fn replace_lines(&mut self, doc: &mut Doc, new: Vec<(usize, String)>) {
        let b = &mut doc.buffer;
        let before = self.selections();
        let deltas: std::collections::HashMap<usize, isize> =
            new.iter().map(|(l, t)| (*l, t.chars().count() as isize - b.line_len(*l) as isize)).collect();
        let edits: Vec<(Pos, Pos, &str)> =
            new.iter().map(|(l, t)| (Pos::new(*l, 0), Pos::new(*l, b.line_len(*l)), t.as_str())).collect();
        b.edit(&before, &edits, EditKind::Other);
        b.break_undo_group();
        let shift = |p: Pos| match deltas.get(&p.line) {
            Some(d) if p.col > 0 || *d > 0 => b.clamp(Pos::new(p.line, (p.col as isize + d).max(0) as usize)),
            _ => p,
        };
        let sels = before.iter().map(|s| Selection { anchor: shift(s.anchor), head: shift(s.head), goal_col: None }).collect();
        self.set_selections(sels);
    }

    fn indent_lines(&mut self, doc: &mut Doc, indent: bool) {
        let unit = indent_unit();
        let b = &doc.buffer;
        let new: Vec<(usize, String)> = self
            .selected_lines()
            .into_iter()
            .filter_map(|line| {
                let text = b.line(line);
                if indent {
                    (!text.is_empty()).then(|| (line, format!("{unit}{text}")))
                } else {
                    let strip = text.chars().take(tab_size()).take_while(|c| *c == ' ').count();
                    let strip = if strip == 0 && text.starts_with('\t') { 1 } else { strip };
                    (strip > 0).then(|| (line, text.chars().skip(strip).collect()))
                }
            })
            .collect();
        self.replace_lines(doc, new);
    }

    pub fn toggle_comment(&mut self, doc: &mut Doc) {
        let Some(token) = doc.lang.def().line_comment else { return };
        let b = &doc.buffer;
        let lines: Vec<(usize, String)> = self.selected_lines().into_iter().map(|l| (l, b.line(l))).collect();
        let non_blank: Vec<&String> = lines.iter().map(|(_, t)| t).filter(|l| !l.trim().is_empty()).collect();
        if non_blank.is_empty() {
            return;
        }
        let all_commented = non_blank.iter().all(|l| l.trim_start().starts_with(token));
        let min_indent = non_blank.iter().map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        let new = lines
            .into_iter()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(l, text)| {
                let new = if all_commented {
                    let indent = text.len() - text.trim_start().len();
                    let rest = &text[indent + token.len()..];
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    format!("{}{}", &text[..indent], rest)
                } else {
                    format!("{}{} {}", &text[..min_indent], token, &text[min_indent..])
                };
                (l, new)
            })
            .collect();
        self.replace_lines(doc, new);
    }

    /// Undo or redo, restoring every cursor.
    pub fn undo(&mut self, doc: &mut Doc, redo: bool) {
        let current = self.selections();
        let b = &mut doc.buffer;
        let restored = if redo { b.redo(&current) } else { b.undo(&current) };
        if let Some(sels) = restored {
            let sels = sels
                .into_iter()
                .map(|s| Selection { anchor: b.clamp(s.anchor), head: b.clamp(s.head), goal_col: None })
                .collect();
            self.set_selections(sels);
        }
    }

    /// Keeps every cursor inside the text (another view may have edited it).
    pub fn clamp_selections(&mut self, doc: &Doc) {
        let b = &doc.buffer;
        let clamp = |s: &mut Selection| {
            s.anchor = b.clamp(s.anchor);
            s.head = b.clamp(s.head);
        };
        clamp(&mut self.sel);
        self.extra.iter_mut().for_each(clamp);
    }

    /// Text for the clipboard: each selection on its own line; with no selection, whole lines.
    /// Returns the text and the ranges it came from (for cut).
    pub fn copy_ranges(&self, doc: &Doc) -> (String, Vec<Selection>) {
        let b = &doc.buffer;
        let mut sels = self.selections();
        sels.sort_by_key(|s| s.ordered());
        if sels.iter().all(|s| s.is_empty()) {
            // Copy/cut the whole line of each cursor.
            let mut lines: Vec<usize> = sels.iter().map(|s| s.head.line).collect();
            lines.dedup();
            let ranges: Vec<Selection> = lines
                .iter()
                .map(|&line| {
                    let end = if line + 1 < b.len_lines() { Pos::new(line + 1, 0) } else { Pos::new(line, b.line_len(line)) };
                    Selection { anchor: Pos::new(line, 0), head: end, goal_col: None }
                })
                .collect();
            let text = ranges
                .iter()
                .map(|r| {
                    let t = b.text_in(r);
                    if t.ends_with('\n') { t } else { t + "\n" }
                })
                .collect();
            return (text, ranges);
        }
        let text = sels.iter().map(|s| b.text_in(s)).collect::<Vec<_>>().join("\n");
        (text, sels)
    }

    /// Deletes what `copy_ranges` returned, returning the cut text.
    pub fn cut(&mut self, doc: &mut Doc) -> String {
        let (text, ranges) = self.copy_ranges(doc);
        let before = self.selections();
        let b = &mut doc.buffer;
        let edits: Vec<(Pos, Pos, &str)> = ranges.iter().map(|r| (r.ordered().0, r.ordered().1, "")).collect();
        let starts = b.edit(&before, &edits, EditKind::Other);
        b.break_undo_group();
        let sels = starts.into_iter().map(|i| Selection::caret(b.pos_of(i))).collect();
        self.set_selections(sels);
        text
    }

    /// Pastes at every cursor. When the clipboard has one line per cursor, each cursor gets
    /// its own line.
    pub fn paste(&mut self, doc: &mut Doc, text: &str) {
        let sels = self.selections();
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        let parts: Vec<&str> = trimmed.split('\n').collect();
        let spread = sels.len() > 1 && parts.len() == sels.len();
        // Spread in document order.
        let mut order: Vec<usize> = (0..sels.len()).collect();
        order.sort_by_key(|&i| sels[i].ordered());
        let mut texts = vec![text.to_string(); sels.len()];
        if spread {
            for (rank, &i) in order.iter().enumerate() {
                texts[i] = parts[rank].to_string();
            }
        }
        let edits = sels.iter().zip(texts).map(|(s, t)| (s.ordered().0, s.ordered().1, t, After::End)).collect();
        self.apply(doc, EditKind::Other, edits);
        doc.buffer.break_undo_group();
    }

    /// Accepts a completion at every cursor: the primary replaces `start..end`, the others
    /// replace as many chars before them as were typed before the primary.
    pub fn accept_completion_everywhere(&mut self, doc: &mut Doc, start: Pos, end: Pos, text: &str) {
        let prefix = if start.line == self.sel.head.line { self.sel.head.col.saturating_sub(start.col) } else { 0 };
        let edits = self
            .selections()
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                let (a, z) = if i == 0 { (start, end) } else { (Pos::new(s.head.line, s.head.col.saturating_sub(prefix)), s.head) };
                (a, z, text.to_string(), After::End)
            })
            .collect();
        self.apply(doc, EditKind::Other, edits);
        doc.buffer.break_undo_group();
    }

    pub fn select_all(&mut self, doc: &Doc) {
        self.set_selection(Selection { anchor: Pos::new(0, 0), head: doc.buffer.end(), goal_col: None });
    }

    /// The screen row under window y (may be past the last row).
    pub fn row_at(&self, y: f32) -> usize {
        ((y - self.geom.text.y + self.scroll_y) / line_height()).floor().max(0.0) as usize
    }

    /// Clicking a sticky scroll line: the cursor goes to that line, which scrolls to the top.
    pub fn go_to_sticky_line(&mut self, doc: &Doc, line: usize) {
        let text = doc.buffer.line(line);
        let indent = text.chars().take_while(|c| c.is_whitespace()).count();
        self.set_selection(Selection::caret(Pos::new(line, indent)));
        let row = self.layout.row_of(Pos::new(line, 0));
        self.scroll_y = (row as f32 * line_height()).min(self.max_scroll_y(doc));
    }

    /// Converts a window point into a document position.
    pub fn pos_at(&self, doc: &Doc, x: f32, y: f32) -> Pos {
        let g = &self.geom;
        let b = &doc.buffer;
        let row = self.row_at(y);
        if row >= self.layout.row_count(b) {
            return b.end();
        }
        let dcol = (x - g.text.x + self.scroll_x) / g.cw.max(1.0);
        self.layout.pos_at(row, dcol.max(0.0), b)
    }

    /// Like `pos_at`, but None when the point isn't over a character (past line end, below text).
    pub fn pos_at_strict(&self, doc: &Doc, x: f32, y: f32) -> Option<Pos> {
        let g = &self.geom;
        if !g.text.contains(x, y) {
            return None;
        }
        let row = self.row_at(y);
        if row >= self.layout.row_count(&doc.buffer) {
            return None;
        }
        let line = self.layout.row(row, &doc.buffer).line;
        let dcol = (x - g.text.x + self.scroll_x) / g.cw.max(1.0);
        self.layout.char_at(row, dcol, &doc.buffer).map(|col| Pos::new(line, col))
    }

    /// Window coordinates of the top-left corner of the character at `pos`.
    pub fn point_of(&self, doc: &Doc, pos: Pos) -> (f32, f32) {
        let g = &self.geom;
        let x = g.text.x - self.scroll_x + self.layout.x_of(pos, &doc.buffer) as f32 * g.cw;
        let y = g.text.y + self.layout.row_of(pos) as f32 * line_height() - self.scroll_y;
        (x, y)
    }

    /// Moves the caret to `pos` and scrolls it a third of the way down the view (go to
    /// definition, search results). The scroll happens on the next draw, once the view's
    /// size is known (a freshly opened editor hasn't been laid out yet).
    /// Puts back a selection and scroll position saved earlier (after a preview).
    pub fn restore_view(&mut self, sel: Selection, scroll_y: f32) {
        self.set_selection(sel);
        self.scroll_y = scroll_y;
        self.center = false;
        self.reveal = false;
    }

    /// Puts the caret at `pos`, scrolling it to the center only if it's off screen.
    pub fn reveal_at(&mut self, doc: &Doc, pos: Pos) {
        let pos = doc.buffer.clamp(pos);
        self.set_selection(Selection::caret(pos));
        self.refresh_layout(&doc.buffer);
        let top = self.layout.row_of(pos) as f32 * line_height();
        let visible = top >= self.scroll_y && top + line_height() <= self.scroll_y + self.geom.text.h;
        self.center = !visible;
        self.reveal = true;
    }

    pub fn jump_to(&mut self, doc: &Doc, pos: Pos) {
        self.set_selection(Selection::caret(doc.buffer.clamp(pos)));
        self.center = true;
        self.reveal = true;
    }

    pub fn click(&mut self, doc: &Doc, x: f32, y: f32, count: u32, extend: bool) {
        self.refresh_layout(&doc.buffer);
        let pos = self.pos_at(doc, x, y);
        match count {
            2 => self.set_selection(doc.buffer.word_at(pos)),
            3 => {
                let next = if pos.line + 1 < doc.buffer.len_lines() {
                    Pos::new(pos.line + 1, 0)
                } else {
                    Pos::new(pos.line, doc.buffer.line_len(pos.line))
                };
                self.set_selection(Selection { anchor: Pos::new(pos.line, 0), head: next, goal_col: None });
            }
            _ => self.set_head(pos, extend, false),
        }
    }

    pub fn drag_to(&mut self, doc: &Doc, x: f32, y: f32) {
        self.refresh_layout(&doc.buffer);
        let pos = self.pos_at(doc, x, y);
        self.set_head(pos, true, false);
    }

    pub fn scroll_by(&mut self, doc: &Doc, dx: f32, dy: f32) {
        self.scroll_y = (self.scroll_y - dy).clamp(0.0, self.max_scroll_y(doc));
        let max_x = if self.layout.is_wrapping() { 0.0 } else { ((doc.longest_line.1 as f32 + 2.0) * self.geom.cw - self.geom.text.w).max(0.0) };
        self.scroll_x = (self.scroll_x - dx).clamp(0.0, max_x);
    }

    fn max_scroll_y(&self, doc: &Doc) -> f32 {
        let rows = self.layout.row_count(&doc.buffer);
        if !config::get().scroll_beyond_last_line {
            return (rows as f32 * line_height() - self.geom.text.h).max(0.0);
        }
        // We scroll past the last line so it can reach the top of the viewport.
        rows.saturating_sub(1) as f32 * line_height()
    }

    /// Scrolls so that the scrollbar slider's top sits at `y`.
    pub fn scroll_to_slider(&mut self, doc: &Doc, slider_top: f32) {
        let g = &self.geom;
        let track = g.scrollbar.h - g.slider.h;
        if track <= 0.0 {
            return;
        }
        let t = ((slider_top - g.scrollbar.y) / track).clamp(0.0, 1.0);
        self.scroll_y = t * self.max_scroll_y(doc);
    }

    /// Scrolls so the minimap row under `y` is centered in the view.
    pub fn scroll_to_minimap(&mut self, doc: &Doc, y: f32) {
        let g = &self.geom;
        let row = (g.minimap_first as f32 + (y - g.minimap.y) / MINIMAP_LINE_H).clamp(0.0, self.layout.row_count(&doc.buffer) as f32);
        let target = row * line_height() - g.text.h / 2.0;
        self.scroll_y = target.clamp(0.0, self.max_scroll_y(doc));
    }

    fn ensure_visible(&mut self, doc: &Doc) {
        let g = self.geom;
        let h = self.sel.head;
        let top = self.layout.row_of(h) as f32 * line_height();
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if top + line_height() > self.scroll_y + g.text.h {
            self.scroll_y = top + line_height() - g.text.h;
        }
        let x = self.layout.x_of(h, &doc.buffer) as f32 * g.cw;
        let margin = g.cw * 4.0;
        if x < self.scroll_x {
            self.scroll_x = (x - margin).max(0.0);
        } else if x + margin > self.scroll_x + g.text.w {
            self.scroll_x = x + margin - g.text.w;
        }
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll_y(doc));
    }

    pub fn draw(
        &mut self,
        c: &mut Canvas,
        theme: &Theme,
        doc: &mut Doc,
        view: Rect,
        focused: bool,
        caret_on: bool,
        minimap: bool,
        hover: Option<(f32, f32)>,
        dragging_slider: bool,
        deco: &Decorations,
    ) {
        let squiggles = deco.squiggles;
        let cfg = config::get();
        let b = &doc.buffer;
        let n_lines = b.len_lines();
        let lh = line_height();
        let style = TextStyle::mono(font_size(), lh, theme.color("editor.foreground"));
        let cw = c.measure("0000000000", &style) / 10.0;

        // Layout: [gutter | text | minimap], with the scrollbar overlaying the right edge.
        let digits = n_lines.to_string().len().max(4) as f32;
        let no_numbers = cfg.line_numbers == config::LineNumbers::Off || deco.hide_line_numbers;
        let numbers_right = if no_numbers { 10.0 } else { 18.0 + digits * cw };
        let gutter_w = numbers_right + 26.0;
        let (gutter, rest) = view.cut_left(gutter_w);
        let (text_rect, minimap_rect) = if minimap { rest.cut_right(MINIMAP_W) } else { (rest, Rect::default()) };
        let scrollbar = Rect::new(view.right() - SCROLLBAR_W, view.y, SCROLLBAR_W, view.h);
        self.geom = Geom { text: text_rect, minimap: minimap_rect, scrollbar, cw, ..self.geom };

        // Rows: wrapped at the viewport (less the scrollbar) and/or `editor.wordWrapColumn`.
        let viewport_cols = ((text_rect.w - SCROLLBAR_W - cw) / cw.max(1.0)).floor().max(10.0) as usize;
        // Large files don't wrap or fold.
        let folding = cfg.folding && !doc.large;
        let wrap = match cfg.word_wrap {
            _ if doc.large => None,
            config::WordWrap::Off => None,
            config::WordWrap::Viewport => Some(viewport_cols),
            config::WordWrap::Column(n) => Some(n),
            config::WordWrap::Bounded(n) => Some(n.min(viewport_cols)),
        };
        if !folding && !self.folds.collapsed().is_empty() {
            self.folds.unfold_all();
        }
        self.folds.set_server_ranges(if doc.large { &crate::folding::ServerRanges::NONE } else { &doc.folding });
        self.folds.sync(&doc.buffer);
        // A cursor that ended up inside a folded region (find, go to definition) opens it.
        let heads: Vec<Pos> = self.selections().iter().map(|s| s.head).collect();
        self.folds.reveal(&doc.buffer, &heads);
        let hidden = self.folds.hidden(&doc.buffer);
        self.layout.set_inlays(doc.inline_decorations());
        // A merge editor's result has its own rows above conflicts instead of the server's lenses.
        let lenses = self.merge.as_ref().map_or_else(|| doc.lenses.clone(), |m| m.lenses.clone());
        self.layout.set_zones(&lenses.lines, lenses.generation);
        self.layout.set_peek(self.peek.filter(|(l, _)| *l < doc.buffer.len_lines()));
        self.layout.update(&doc.buffer, wrap.map(|w| (w, cfg.wrapping_indent)), &hidden, self.folds.generation());
        // Fold chevrons: all regions while the pointer is over the gutter, collapsed ones always.
        let show_controls = folding
            && match cfg.fold_controls {
                config::FoldControls::Always => true,
                config::FoldControls::Never => false,
                config::FoldControls::MouseOver => hover.is_some_and(|(x, y)| gutter.contains(x, y)),
            };
        let fold_ranges: Vec<crate::folding::FoldRange> =
            if show_controls || !self.folds.collapsed().is_empty() { self.folds.ranges(&doc.buffer).to_vec() } else { Vec::new() };

        // Sticky scroll works from the same regions as folding (the server's or indentation).
        let sticky_ranges: Vec<crate::folding::FoldRange> =
            if cfg.sticky_scroll && !doc.large && self.scroll_y > 0.0 { self.folds.ranges(&doc.buffer).to_vec() } else { Vec::new() };
        doc.highlight.update(&mut doc.buffer);
        if cfg.bracket_colors || cfg.match_brackets {
            doc.brackets.update(&doc.buffer, doc.lang);
        }
        let longest = doc.longest_line();
        if self.center {
            let row = self.layout.row_of(self.sel.head);
            let target = row as f32 * lh - text_rect.h / 3.0;
            self.scroll_y = target.clamp(0.0, self.max_scroll_y(doc));
            self.center = false;
        }
        if self.reveal {
            self.ensure_visible(doc);
            self.reveal = false;
        }
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll_y(doc));
        let max_x = if self.layout.is_wrapping() { 0.0 } else { ((longest as f32 + 2.0) * cw - text_rect.w).max(0.0) };
        self.scroll_x = self.scroll_x.clamp(0.0, max_x);

        let b = &doc.buffer;
        let layout = &self.layout;
        c.fill(view, theme.color("editor.background"));

        let n_rows = layout.row_count(b);
        let first_row = (self.scroll_y / lh).floor() as usize;
        let last_row = (((self.scroll_y + view.h) / lh).ceil() as usize).min(n_rows);
        let rows: Vec<crate::layout::VRow> = (first_row..last_row).map(|r| layout.row(r, b)).collect();
        // Buffer lines on screen, and their text.
        let first = rows.first().map_or(0, |r| r.line);
        let last = rows.last().map_or(0, |r| r.line + 1);
        let lines: Vec<String> = (first..last).map(|l| b.line(l)).collect();
        let text_of = |line: usize| lines[line - first].as_str();
        let y_row = |row: usize| view.y + row as f32 * lh - self.scroll_y;
        // Screen span of buffer lines `from..to` (clipped to the view): (top, height).
        let span = |from: usize, to: usize| -> Option<(f32, f32)> {
            let (s, e) = (from.max(first), to.min(last));
            if e <= s {
                return None;
            }
            let (r0, r1) = (layout.rows_of_line(s).start, layout.rows_of_line(e - 1).end);
            Some((y_row(r0), (r1 - r0) as f32 * lh))
        };
        let text_x = text_rect.x - self.scroll_x;
        // Display columns of chars `a..z` of a row, clipped to it: (x from, x to) in columns.
        // Inlay hints inside the range are covered; one at `z` isn't.
        let cols_in_row = |vr: &crate::layout::VRow, a: usize, z: usize| -> (usize, usize) {
            let text = text_of(vr.line);
            let (a, z) = (a.clamp(vr.start, vr.end), z.clamp(vr.start, vr.end));
            let from = layout.row_x(vr, text, a, true);
            (from, layout.row_x(vr, text, z, false).max(from))
        };
        let sels = self.selections();
        let head = self.sel.head;

        // Current line highlight on each cursor's line (only when it has no selection).
        let (x0, x1) = match cfg.line_highlight {
            config::LineHighlight::None => (0.0, 0.0),
            config::LineHighlight::Gutter => (view.x, gutter.right()),
            config::LineHighlight::Line => (gutter.right(), view.right()),
            config::LineHighlight::All => (view.x, view.right()),
        };
        let mut highlighted: Vec<usize> = sels.iter().filter(|s| s.is_empty()).map(|s| s.head.line).collect();
        highlighted.sort_unstable();
        highlighted.dedup();
        for &line in highlighted.iter().filter(|_| x1 > x0) {
            if let Some((y, h)) = span(line, line + 1) {
                c.bordered(
                    Rect::new(x0, y, x1 - x0, h),
                    theme.color("editor.lineHighlightBackground"),
                    theme.color("editor.lineHighlightBorder"),
                    2.0,
                    0.0,
                );
            }
        }

        // The focused stack frame's line.
        if let Some((line, top)) = deco.stack_frame {
            if let Some((y, h)) = span(line, line + 1) {
                let key = if top { "editor.stackFrameHighlightBackground" } else { "editor.focusedStackFrameHighlightBackground" };
                c.fill(Rect::new(gutter.right(), y, view.right() - gutter.right(), h), theme.color(key));
            }
        }

        // Merge conflict blocks: header and content backgrounds per side.
        for cf in deco.conflicts.iter().filter(|cf| cf.end >= first && cf.start < last) {
            let mut band = |from: usize, to: usize, key: &str| {
                if let Some((y, h)) = span(from, to) {
                    c.fill(Rect::new(gutter.right(), y, view.right() - gutter.right(), h), theme.color(key));
                }
            };
            band(cf.start, cf.start + 1, "merge.currentHeaderBackground");
            band(cf.start + 1, cf.base.unwrap_or(cf.split), "merge.currentContentBackground");
            if let Some(base) = cf.base {
                band(base, base + 1, "merge.commonHeaderBackground");
                band(base + 1, cf.split, "merge.commonContentBackground");
            }
            band(cf.split + 1, cf.end, "merge.incomingContentBackground");
            band(cf.end, cf.end + 1, "merge.incomingHeaderBackground");
        }

        // Merge editor ranges: changed lines, and a border around conflicts.
        for mark in deco.merge.iter().filter(|m| m.lines.end >= first && m.lines.start <= last) {
            let x = gutter.right() - 4.0;
            let w = view.right() - SCROLLBAR_W - x;
            if mark.lines.is_empty() {
                let row = if mark.lines.start < n_lines { layout.rows_of_line(mark.lines.start).start } else { n_rows };
                c.fill(Rect::new(x, y_row(row) - 1.0, w, 2.0), mark.border);
            } else if let Some((y, h)) = span(mark.lines.start, mark.lines.end) {
                c.fill(Rect::new(x, y, w, h), mark.background);
                c.bordered(Rect::new(x, y, w, h), render::Color::TRANSPARENT, mark.border, 1.0, 0.0);
            }
        }

        // Git change markers, between the line numbers and the text.
        let bar_x = gutter.x + numbers_right + 6.0;
        for change in deco.git {
            let (color, start, end) = match *change {
                scm::LineChange::Added { start, end } => ("editorGutter.addedBackground", start, end),
                scm::LineChange::Modified { start, end } => ("editorGutter.modifiedBackground", start, end),
                scm::LineChange::Deleted { at } => {
                    // A small wedge on the boundary where lines were removed.
                    if at >= first && at <= last {
                        let row = if at < n_lines { layout.rows_of_line(at).start } else { n_rows };
                        c.fill(Rect::new(bar_x, y_row(row) - 3.0, 6.0, 6.0), theme.color("editorGutter.deletedBackground"));
                    }
                    continue;
                }
            };
            if let Some((y, h)) = span(start, end) {
                c.fill(Rect::new(bar_x, y, 3.0, h), theme.color(color));
            }
        }

        // Line numbers, on the first row of each line.
        c.push_clip(gutter);
        for (i, vr) in rows.iter().enumerate().filter(|(_, vr)| vr.start == 0 && !vr.zone) {
            let line = vr.line;
            let active = sels.iter().any(|s| s.head.line == line);
            let label = match cfg.line_numbers {
                _ if deco.hide_line_numbers => continue,
                config::LineNumbers::Off => continue,
                config::LineNumbers::Relative if line != head.line => line.abs_diff(head.line).to_string(),
                config::LineNumbers::Interval if !active && (line + 1) % 10 != 0 && line != 0 => continue,
                _ => (line + 1).to_string(),
            };
            let color = theme.color(if active { "editorLineNumber.activeForeground" } else { "editorLineNumber.foreground" });
            let st = style.color(color);
            let w = c.measure(&label, &st);
            c.text(gutter.x + numbers_right - w, y_row(first_row + i), &label, &st);
        }
        self.fold_controls.clear();
        let chevron_x = gutter.x + numbers_right + 10.0;
        let chevron_fg = theme.color("editorGutter.foldingControlForeground");
        for (i, vr) in rows.iter().enumerate().filter(|(_, vr)| vr.start == 0 && !vr.zone) {
            let Ok(ri) = fold_ranges.binary_search_by_key(&vr.line, |r| r.start) else { continue };
            let collapsed = self.folds.is_collapsed(fold_ranges[ri].start);
            if !show_controls && !collapsed {
                continue;
            }
            let icon = if collapsed { &icons::CHEVRON_RIGHT } else { &icons::CHEVRON_DOWN };
            let r = Rect::new(chevron_x, y_row(first_row + i), 16.0, lh);
            c.icon_in(icon, r, 14.0, chevron_fg);
            self.fold_controls.push((r, vr.line));
        }
        // The glyph margin: breakpoints, a faint one under the pointer (click to add), and the
        // focused stack frame's arrow.
        let glyph = Rect::new(gutter.x, view.y, 18.0, view.h);
        self.geom.glyph = glyph;
        let hover_line = hover.filter(|&(x, y)| glyph.contains(x, y)).and_then(|(_, y)| {
            let row = ((y - view.y + self.scroll_y) / lh).floor() as usize;
            rows.get(row.checked_sub(first_row)?).map(|vr| vr.line)
        });
        for (i, vr) in rows.iter().enumerate().filter(|(_, vr)| vr.start == 0 && !vr.zone) {
            let r = Rect::new(gutter.x + 1.0, y_row(first_row + i), 16.0, lh);
            let bp = deco.breakpoints.iter().find(|b| b.0 == vr.line).map(|b| b.1);
            let test = deco.tests.iter().find(|t| t.0 == vr.line).map(|t| t.1);
            if let Some(look) = bp {
                draw_breakpoint(c, theme, r, look, 1.0);
            } else if let Some(state) = test {
                // The test decorations: run for a test without results, else its state.
                match state {
                    crate::testing::TestState::Running => icons::draw_spinner(c, r, 14.0, theme.color("testing.iconUnset")),
                    crate::testing::TestState::Unset => c.icon_in(&icons::RUN, r, 14.0, theme.color("testing.runAction")),
                    _ => {
                        let (icon, key) = icons::test_state(state);
                        c.icon_in(icon, r, 14.0, theme.color(key));
                    }
                }
            } else if hover_line == Some(vr.line) {
                draw_breakpoint(c, theme, r, BpLook::Normal, 0.4);
            }
            if let Some((_, top)) = deco.stack_frame.filter(|f| f.0 == vr.line) {
                let key = if top { "debugIcon.breakpointCurrentStackframeForeground" } else { "debugIcon.breakpointStackframeForeground" };
                c.icon_in(&icons::STACK_FRAME, r, 14.0, theme.color(key));
            }
        }
        // The code action lightbulb, in the margin left of the line numbers.
        self.geom.lightbulb = None;
        if let Some((line, autofix)) = deco.lightbulb {
            if let Some(i) = rows.iter().position(|vr| vr.line == line && vr.start == 0 && !vr.zone) {
                let r = Rect::new(gutter.x + 1.0, y_row(first_row + i), 16.0, lh);
                let (icon, color) = if autofix {
                    (&icons::LIGHT_BULB_AUTOFIX, "editorLightBulbAutoFix.foreground")
                } else {
                    (&icons::LIGHT_BULB, "editorLightBulb.foreground")
                };
                c.icon_in(icon, r, 16.0, theme.color(color));
                self.geom.lightbulb = Some(r);
            }
        }
        c.pop_clip();

        c.push_clip(text_rect);
        // Fills the part of range `a..z` on each visible row; `newline` also covers the line
        // break after a line the range continues past (selections show it).
        let range_rects = |a: Pos, z: Pos, newline: bool| -> Vec<Rect> {
            let mut out = Vec::new();
            for (i, vr) in rows.iter().enumerate() {
                if vr.zone {
                    continue;
                }
                if vr.line < a.line || vr.line > z.line {
                    continue;
                }
                let s = if vr.line == a.line { a.col } else { 0 };
                let e = if vr.line == z.line { z.col } else { usize::MAX };
                if s > vr.end || e < vr.start || (s == vr.end && vr.end != vr.start && !(newline && vr.line < z.line)) {
                    continue;
                }
                let (mut x0, x1c) = cols_in_row(vr, s, e);
                let mut x1 = x1c;
                let line_end = vr.end == text_of(vr.line).chars().count();
                if newline && vr.line < z.line && line_end {
                    x1 += 1; // show the selected line break
                }
                if vr.start > 0 && s <= vr.start {
                    x0 = x0.min(vr.indent);
                }
                if x1 > x0 {
                    out.push(Rect::new(text_x + x0 as f32 * cw, y_row(first_row + i), (x1 - x0) as f32 * cw, lh));
                }
            }
            out
        };
        let fill_range = |c: &mut Canvas, a: Pos, z: Pos, color: theme::Color, radius: f32, newline: bool| {
            for r in range_rects(a, z, newline) {
                if radius > 0.0 { c.fill_rounded(r, color, radius) } else { c.fill(r, color) }
            }
        };

        // Snippet placeholders: a background, and a thin box for empty ones (the final cursor).
        for &(a, z, fin) in deco.snippet.iter().filter(|(a, z, _)| z.line >= first && a.line < last) {
            let key = if fin { "editor.snippetFinalTabstopHighlightBackground" } else { "editor.snippetTabstopHighlightBackground" };
            fill_range(c, a, z, theme.color(key), 0.0, false);
            if a == z {
                let row = layout.row_of(a);
                if row >= first_row && row < last_row {
                    let x = text_x + cols_in_row(&rows[row - first_row], a.col, a.col).0 as f32 * cw;
                    let border = if fin { "editor.snippetFinalTabstopHighlightBorder" } else { "editor.snippetTabstopHighlightBorder" };
                    c.fill(Rect::new(x, y_row(row), 1.0, lh), theme.color(border));
                }
            }
        }

        // Linked editing ranges.
        for &(a, z) in deco.linked.iter().filter(|(a, z)| z.line >= first && a.line < last) {
            fill_range(c, a, z, theme.color("editor.linkedEditingBackground"), 0.0, false);
        }

        // Extensions' decorations: backgrounds (whole lines or ranges) and borders.
        for d in deco.ext.iter().filter(|d| d.end.line >= first && d.start.line < last) {
            if let Some(bg) = d.background {
                if d.whole_line {
                    for line in d.start.line.max(first)..=d.end.line.min(last.saturating_sub(1)) {
                        if let Some((y, h)) = span(line, line + 1) {
                            c.fill(Rect::new(text_rect.x, y, text_rect.w, h), bg);
                        }
                    }
                } else {
                    fill_range(c, d.start, d.end, bg, 0.0, false);
                }
            }
            if let Some(b) = d.border {
                for r in range_rects(d.start, d.end, false) {
                    c.fill(Rect::new(r.x, r.y, r.w, 1.0), b);
                    c.fill(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), b);
                    c.fill(Rect::new(r.x, r.y, 1.0, r.h), b);
                    c.fill(Rect::new(r.right() - 1.0, r.y, 1.0, r.h), b);
                }
            }
        }

        // A search editor's matches, like `searchEditorFindMatch` decorations.
        let first_result = deco.search_results.partition_point(|m| m.1.line < first);
        for &(a, z) in &deco.search_results[first_result..] {
            if a.line >= last {
                break;
            }
            fill_range(c, a, z, theme.color("searchEditor.findMatchBackground"), 0.0, true);
        }

        // Find matches, under the selection.
        let visible = deco.matches.partition_point(|m| m.1.line < first);
        for (i, &(a, z)) in deco.matches.iter().enumerate().skip(visible) {
            if a.line >= last {
                break;
            }
            let current = deco.current_match == Some(i);
            let color = theme.color(if current { "editor.findMatchBackground" } else { "editor.findMatchHighlightBackground" });
            fill_range(c, a, z, color, 0.0, true);
        }

        // Selection.
        let sel_color =
            theme.color(if focused { "editor.selectionBackground" } else { "editor.inactiveSelectionBackground" });
        for (sel_a, sel_z) in sels.iter().filter(|s| !s.is_empty()).map(|s| s.ordered()) {
            fill_range(c, sel_a, sel_z, sel_color, 3.0, true);
        }

        // The bracket pair next to each cursor.
        if cfg.match_brackets {
            let bg = theme.color("editorBracketMatch.background");
            let border = theme.color("editorBracketMatch.border");
            for head in sels.iter().filter(|s| s.is_empty()).map(|s| s.head) {
                let Some((a, z)) = doc.brackets.pair_at(head) else { continue };
                for p in [a, z] {
                    let row = layout.row_of(p);
                    if row < first_row || row >= last_row || layout.is_hidden(p.line) {
                        continue;
                    }
                    let (x0, x1) = cols_in_row(&rows[row - first_row], p.col, p.col + 1);
                    let r = Rect::new(text_x + x0 as f32 * cw, y_row(row), (x1 - x0).max(1) as f32 * cw, lh);
                    c.bordered(r, bg, border, 1.0, 0.0);
                }
            }
        }

        // Indent guides (continuation rows of a wrapped line keep their line's guides).
        let guide = theme.color("editorIndentGuide.background1");
        for (i, vr) in rows.iter().enumerate().filter(|_| cfg.indent_guides) {
            if vr.zone {
                continue;
            }
            let levels = indent_level(b, vr.line, text_of(vr.line));
            for lvl in 0..levels {
                let x = text_x + (lvl * tab_size()) as f32 * cw;
                if vr.start > 0 && lvl * tab_size() >= vr.indent {
                    break;
                }
                c.fill(Rect::new(x.round(), y_row(first_row + i), 1.0, lh), guide);
            }
        }

        // We cycle through the bracket colors the theme sets (transparent ones are unset).
        let bracket_palette: Vec<theme::Color> =
            (1..=6).map(|i| theme.color(&format!("editorBracketHighlight.foreground{i}"))).filter(|c| c.a > 0.0).collect();

        // Bracket pair guides: a line down the indentation between a multi-line pair's
        // brackets, in the pair's color (the theme's guide colors, else its bracket color at
        // 30%, and fully for the pair around the cursor).
        if cfg.bracket_guides != config::BracketGuides::Off && !bracket_palette.is_empty() {
            let active = doc.brackets.enclosing(head);
            let brackets = &doc.brackets.brackets;
            for br in brackets.iter().filter(|br| br.open) {
                let Some(close) = br.partner.map(|p| brackets[p].pos) else { continue };
                if close.line <= br.pos.line + 1 || close.line < first || br.pos.line >= last {
                    continue;
                }
                let is_active = active == Some((br.pos, close));
                if cfg.bracket_guides == config::BracketGuides::Active && !is_active {
                    continue;
                }
                let depth = br.depth.unwrap_or(0) % bracket_palette.len();
                let key = if is_active { format!("editorBracketPairGuide.activeBackground{}", depth + 1) } else { format!("editorBracketPairGuide.background{}", depth + 1) };
                let themed = theme.color(&key);
                let color = if themed.a > 0.0 { themed } else { bracket_palette[depth].with_alpha(if is_active { 1.0 } else { 0.3 }) };
                let close_text = b.line(close.line);
                let col = col_to_display(&close_text, close_text.chars().take_while(|c| c.is_whitespace()).count());
                let x = (text_x + col as f32 * cw).round();
                if let Some((y, h)) = span(br.pos.line + 1, close.line) {
                    c.fill(Rect::new(x, y, 1.0, h), color);
                }
            }
        }

        // Text: each row draws its slice of the line (tabs expanded, colors kept).
        let unexpected_bracket = theme.color("editorBracketHighlight.unexpectedBracket.foreground");
        let line_spans = doc.highlight.spans(&doc.buffer, first, last);
        let mut colored = Vec::new();
        let mut expanded: Vec<Option<(String, Vec<Span>)>> = vec![None; lines.len()];
        self.swatch_hits.clear();
        for (i, vr) in rows.iter().enumerate() {
            if vr.zone {
                continue;
            }
            let li = vr.line - first;
            let (display, spans) = expanded[li].get_or_insert_with(|| expand_tabs_spans(&lines[li], &line_spans[li]));
            // One char per display column in the expanded text.
            let (d0, d1) = (col_to_display(&lines[li], vr.start), col_to_display(&lines[li], vr.end));
            let byte = |d: usize| display.char_indices().nth(d).map_or(display.len(), |(b, _)| b);
            let (b0, b1) = (byte(d0), byte(d1));
            colored.clear();
            colored.extend(spans.iter().filter(|(a, z, _)| *z > b0 && *a < b1).map(|(a, z, t)| ((*a).max(b0) - b0, (*z).min(b1) - b0, theme.token(*t))));
            // Semantic tokens over the syntax colors.
            for t in doc.semantic.on_line(vr.line).iter().filter(|t| t.end > vr.start && t.start < vr.end) {
                let (ty, mods) = doc.semantic.names(t);
                let color = theme.semantic_color(ty, &mods).unwrap_or_else(|| theme.token(t.token));
                let (a, z) = (t.start.max(vr.start), t.end.min(vr.end));
                let (ba, bz) = (byte(col_to_display(&lines[li], a)) - b0, byte(col_to_display(&lines[li], z)) - b0);
                overlay(&mut colored, ba, bz, color);
            }
            // Bracket pair colors by depth (and red for a bracket with no partner).
            if cfg.bracket_colors && !bracket_palette.is_empty() {
                for br in doc.brackets.on_line(vr.line).iter().filter(|br| br.pos.col >= vr.start && br.pos.col < vr.end) {
                    let color = match br.depth {
                        Some(d) => bracket_palette[d % bracket_palette.len()],
                        None => unexpected_bracket,
                    };
                    let d = col_to_display(&lines[li], br.pos.col);
                    let at = byte(d) - b0;
                    overlay(&mut colored, at, at + 1, color);
                }
            }
            // Extensions' text colors.
            for d in deco.ext.iter().filter(|d| d.color.is_some() && d.start.line <= vr.line && d.end.line >= vr.line) {
                let a = if d.start.line == vr.line { d.start.col.max(vr.start) } else { vr.start };
                let z = if d.end.line == vr.line { d.end.col.min(vr.end) } else { vr.end };
                if a < z {
                    let (ba, bz) = (byte(col_to_display(&lines[li], a)) - b0, byte(col_to_display(&lines[li], z)) - b0);
                    overlay(&mut colored, ba, bz, d.color.unwrap());
                }
            }
            // Split at inlay hints: each piece of text starts where the layout puts its first char.
            let y = y_row(first_row + i);
            let hints = layout.inlays_on(vr.line);
            let last_row = vr.end == lines[li].chars().count();
            let in_row = |h: &&crate::layout::InlayHint| h.col >= vr.start && (h.col < vr.end || (last_row && h.col == vr.end));
            let mut cuts: Vec<usize> = hints.iter().filter(in_row).map(|h| h.col).filter(|&c| c > vr.start && c < vr.end).collect();
            cuts.insert(0, vr.start);
            cuts.push(vr.end);
            for w in cuts.windows(2) {
                let (s0, s1) = (byte(col_to_display(&lines[li], w[0])) - b0, byte(col_to_display(&lines[li], w[1])) - b0);
                if s0 >= s1 {
                    continue;
                }
                let part: Vec<(usize, usize, theme::Color)> =
                    colored.iter().filter(|(a, z, _)| *z > s0 && *a < s1).map(|(a, z, col)| ((*a).max(s0) - s0, (*z).min(s1) - s0, *col)).collect();
                let x = text_x + layout.row_x(vr, &lines[li], w[0], true) as f32 * cw;
                c.rich_text(x, y, &display[b0 + s0..b0 + s1], &part, &style);
            }
            for h in hints.iter().filter(in_row) {
                let x = text_x + layout.row_x(vr, &lines[li], h.col, false) as f32 * cw;
                draw_inlay_hint(c, theme, h, x, y, cw, lh);
                if h.swatch.is_some() {
                    self.swatch_hits.push((Rect::new(x, y, 2.0 * cw, lh), Pos::new(vr.line, h.col)));
                }
            }
        }

        // Extensions' underlines and strike-throughs.
        for d in deco.ext.iter().filter(|d| (d.underline || d.strike) && d.end.line >= first && d.start.line < last) {
            let color = d.color.unwrap_or_else(|| theme.color("editor.foreground"));
            for r in range_rects(d.start, d.end, false) {
                let y = if d.strike { r.y + (r.h / 2.0).round() } else { r.bottom() - 2.0 };
                c.fill(Rect::new(r.x, y, r.w, 1.0), color);
            }
        }

        // The peek view's rows (drawn by the workbench over them).
        self.geom.peek = match (layout.peek_row(), self.peek) {
            (Some(r), Some((_, n))) => Some(Rect::new(view.x, y_row(r), view.w - SCROLLBAR_W, n as f32 * lh)),
            _ => None,
        };

        // Code lenses, in the rows above their lines at the lines' indentation: "Run | Debug".
        self.lens_hits.clear();
        let lens_style = TextStyle::ui(font_size() * 0.9, theme.color("editorCodeLens.foreground"));
        let lens_hover = theme.color("editorLink.activeForeground");
        for (i, vr) in rows.iter().enumerate().filter(|(_, vr)| vr.zone && !vr.peek) {
            let y = y_row(first_row + i);
            let indent = lines[vr.line - first].chars().take_while(|c| c.is_whitespace()).collect::<String>();
            let mut x = text_x + col_to_display(&indent, indent.chars().count()) as f32 * cw;
            let items = lenses.items.iter().filter(|it| it.0 == vr.line);
            let sep_w = c.measure(" | ", &lens_style);
            for (k, (_, title, index)) in items.enumerate() {
                let Some(title) = title else { continue };
                if k > 0 {
                    c.text_in(Rect::new(x, y, sep_w, lh), " | ", &lens_style);
                    x += sep_w;
                }
                let w = c.measure(title, &lens_style);
                let r = Rect::new(x, y, w, lh);
                // usize::MAX: a label, not a command.
                let hovered = *index != usize::MAX && hover.is_some_and(|(hx, hy)| r.contains(hx, hy));
                c.text_in(r, title, &if hovered { lens_style.color(lens_hover) } else { lens_style });
                if *index != usize::MAX {
                    self.lens_hits.push((r, *index));
                }
                x += w;
            }
        }

        // Collapsed regions: the header gets the fold background and a "⋯" after its text.
        for &start in self.folds.collapsed() {
            // Regions nested in a folded one are hidden with it.
            if start < first || start >= last || layout.is_hidden(start) || !fold_ranges.iter().any(|r| r.start == start) {
                continue;
            }
            let end = Pos::new(start, text_of(start).chars().count());
            let row = layout.row_of(end);
            if row < first_row || row >= last_row {
                continue;
            }
            if let Some((y, h)) = span(start, start + 1) {
                c.fill(Rect::new(text_rect.x, y, text_rect.w, h), theme.color("editor.foldBackground"));
            }
            let x = text_x + (cols_in_row(&rows[row - first_row], end.col, end.col).0 + 1) as f32 * cw;
            let ph = style.color(theme.color("editor.foldPlaceholderForeground"));
            c.text(x, y_row(row), "⋯", &ph);
        }

        // Merge conflicts: "(Current Change)" / "(Incoming Change)" after the markers, and the
        // accept actions we show as a code lens.
        self.conflict_actions.clear();
        let lens_style = TextStyle::mono(font_size() * 0.9, lh, theme.color("editorCodeLens.foreground"));
        let desc_style = style.color(theme.color("descriptionForeground"));
        for cf in deco.conflicts.iter().filter(|cf| cf.end >= first && cf.start < last) {
            for (line, label) in [(cf.start, "(Current Change)"), (cf.end, "(Incoming Change)")] {
                if line < first || line >= last {
                    continue;
                }
                // After the end of the line's last row.
                let end = Pos::new(line, text_of(line).chars().count());
                let row = layout.row_of(end);
                if row < first_row || row >= last_row {
                    continue;
                }
                let y = y_row(row);
                let x = text_x + (cols_in_row(&rows[row - first_row], end.col, end.col).0 + 1) as f32 * cw;
                let w = c.text(x, y, label, &desc_style);
                if line != cf.start {
                    continue;
                }
                let mut ax = x + w + 3.0 * cw;
                let actions = [
                    ("Accept Current Change", crate::conflicts::Resolution::Current),
                    ("Accept Incoming Change", crate::conflicts::Resolution::Incoming),
                    ("Accept Both Changes", crate::conflicts::Resolution::Both),
                ];
                for (i, (title, how)) in actions.into_iter().enumerate() {
                    if i > 0 {
                        ax += c.text(ax, y, " | ", &lens_style);
                    }
                    let tw = c.measure(title, &lens_style);
                    let r = Rect::new(ax, y, tw, lh);
                    let hovered = hover.is_some_and(|(hx, hy)| r.contains(hx, hy) && text_rect.contains(hx, hy));
                    let st = if hovered { lens_style.color(theme.color("editorLink.activeForeground")) } else { lens_style };
                    c.text(ax, y, title, &st);
                    if hovered {
                        c.fill(Rect::new(ax, y + lh - 3.0, tw, 1.0), st.color);
                    }
                    self.conflict_actions.push((r.intersect(&text_rect), *cf, how));
                    ax += tw;
                }
            }
        }

        // Whitespace markers (`editor.renderWhitespace`): dots for spaces, arrows for tabs.
        if cfg.whitespace != config::Whitespace::None {
            let ws_color = theme.color("editorWhitespace.foreground");
            let arrow_style = style.color(ws_color);
            for (i, vr) in rows.iter().enumerate() {
                if vr.zone {
                    continue;
                }
                let line = vr.line;
                let text = text_of(line);
                let chars: Vec<char> = text.chars().collect();
                let trailing_from = chars.iter().rposition(|c| !c.is_whitespace()).map_or(0, |p| p + 1);
                let base = col_to_display(text, vr.start);
                let mut display = base;
                for col in vr.start..vr.end {
                    let ch = chars[col];
                    let width = cells(ch, display);
                    if ch == ' ' || ch == '\t' {
                        let lone_space = ch == ' '
                            && col > 0
                            && !chars[col - 1].is_whitespace()
                            && chars.get(col + 1).is_some_and(|n| !n.is_whitespace());
                        let p = Pos::new(line, col);
                        let in_sel = sels.iter().any(|s| {
                            let (a, z) = s.ordered();
                            p >= a && p < z
                        });
                        let show = match cfg.whitespace {
                            config::Whitespace::All => true,
                            config::Whitespace::Boundary => !lone_space,
                            config::Whitespace::Trailing => col >= trailing_from,
                            config::Whitespace::Selection => in_sel,
                            config::Whitespace::None => false,
                        };
                        if show {
                            let x = text_x + layout.row_x(vr, text, col, true) as f32 * cw;
                            let y = y_row(first_row + i);
                            if ch == ' ' {
                                let d = 2.0_f32.min(cw / 3.0);
                                c.fill_rounded(Rect::new((x + (cw - d) / 2.0).round(), (y + (lh - d) / 2.0).round(), d, d), ws_color, d / 2.0);
                            } else {
                                c.text(x, y, "→", &arrow_style);
                            }
                        }
                    }
                    display += width;
                }
            }
        }

        // Test failure messages after their lines, like the standard inline test messages.
        for (line, msg) in deco.test_messages {
            if *line < first || *line >= last {
                continue;
            }
            let len = text_of(*line).chars().count();
            let Some(i) = rows.iter().position(|vr| vr.line == *line && !vr.zone && !vr.peek && vr.end == len) else { continue };
            let vr = &rows[i];
            let y = y_row(first_row + i);
            c.fill(Rect::new(view.x, y, view.w, lh), theme.color("testing.message.error.lineBackground"));
            let x = text_x + cols_in_row(vr, len, len).0 as f32 * cw + 2.0 * cw;
            let st = TextStyle::mono(font_size() * 0.9, lh, theme.color("testing.message.error.badgeForeground"));
            let w = (c.measure(msg, &st) + 10.0).min((view.right() - x - 4.0).max(0.0));
            if w < 20.0 {
                continue;
            }
            let badge = Rect::new(x, y + 1.0, w, lh - 2.0);
            c.bordered(badge, theme.color("testing.message.error.badgeBackground"), theme.color("testing.message.error.badgeBorder"), 1.0, 3.0);
            c.push_clip(badge);
            c.text(badge.x + 5.0, y, msg, &st);
            c.pop_clip();
        }

        // Diagnostics: wavy underlines, like the standard squiggles.
        for sq in squiggles {
            if sq.end.line < first || sq.start.line >= last {
                continue;
            }
            let color = severity_color(theme, sq.severity);
            for (i, vr) in rows.iter().enumerate() {
                if vr.zone {
                    continue;
                }
                if vr.line < sq.start.line || vr.line > sq.end.line {
                    continue;
                }
                let s = if vr.line == sq.start.line { sq.start.col } else { 0 };
                let e = if vr.line == sq.end.line { sq.end.col } else { usize::MAX };
                if e < vr.start || (s >= vr.end && !(s == vr.end && vr.end == text_of(vr.line).chars().count())) {
                    continue;
                }
                let (a, mut z) = cols_in_row(vr, s, e);
                if z <= a {
                    z = a + 1; // zero-width ranges still get a short squiggle
                }
                let x0 = text_x + a as f32 * cw;
                let x1 = text_x + z as f32 * cw;
                let base = y_row(first_row + i) + lh - 3.0;
                if sq.severity == lsp::Severity::Hint {
                    // We mark a hint only where it starts, even when it spans lines.
                    if vr.line != sq.start.line || s < vr.start || (s >= vr.end && vr.end != text_of(vr.line).chars().count()) {
                        continue;
                    }
                    for k in 0..3 {
                        c.fill(Rect::new(x0 + k as f32 * 2.0, base + 1.0, 1.0, 1.0), color);
                    }
                    continue;
                }
                let mut x = x0;
                while x < x1 {
                    // Triangle wave with a 4px period and 2px height.
                    let phase = ((x - x0) % 4.0) / 4.0;
                    let dy = if phase < 0.5 { phase * 4.0 } else { (1.0 - phase) * 4.0 };
                    c.fill(Rect::new(x, base + dy, 1.0, 1.0), color);
                    x += 0.5;
                }
            }
        }

        // Carets, in the `editor.cursorStyle` shape.
        for head in sels.iter().map(|s| s.head).filter(|_| focused && caret_on) {
            let row = layout.row_of(head);
            if row < first_row || row >= last_row || head.line < first || head.line >= last {
                continue;
            }
            let text = text_of(head.line);
            let vr = rows[row - first_row];
            let x = (text_x + layout.row_x(&vr, text, head.col, false) as f32 * cw).round();
            let y = y_row(row);
            let under = text.chars().nth(head.col);
            // A block covers the char under it: a tab to its stop, a wide char two cells.
            let w = under.map_or(1, |ch| cells(ch, col_to_display(text, head.col))).max(1) as f32 * cw;
            let color = theme.color("editorCursor.foreground");
            match cfg.cursor_style {
                config::CursorStyle::Line => c.fill(Rect::new(x, y, cfg.cursor_width, lh), color),
                config::CursorStyle::LineThin => c.fill(Rect::new(x, y, 1.0, lh), color),
                config::CursorStyle::Block => {
                    c.fill(Rect::new(x, y, w, lh), color);
                    // The character under a block cursor is drawn in the cursor's background color.
                    if let Some(ch) = under.filter(|c| !c.is_whitespace()) {
                        let bg = theme.color_opt("editorCursor.background").unwrap_or_else(|| theme.color("editor.background"));
                        c.text(x, y, &ch.to_string(), &style.color(bg));
                    }
                }
                config::CursorStyle::BlockOutline => c.bordered(Rect::new(x, y, w, lh), theme::Color::TRANSPARENT, color, 1.0, 0.0),
                config::CursorStyle::Underline => c.fill(Rect::new(x, y + lh - 2.0, w, 2.0), color),
                config::CursorStyle::UnderlineThin => c.fill(Rect::new(x, y + lh - 1.0, w, 1.0), color),
            }
        }
        // Text being composed with an input method shows at the primary caret.
        if focused {
            let head = self.sel.head;
            let row = layout.row_of(head);
            let (at, after) = if row >= first_row && row < last_row && head.line >= first && head.line < last {
                let vr = &rows[row - first_row];
                let text = text_of(head.line);
                let x = text_x + layout.row_x(vr, text, head.col, false) as f32 * cw;
                let rest: String = text.chars().take(vr.end).skip(head.col).collect();
                let (rest, _) = expand_tabs_spans(&rest, &[]);
                (Rect::new(x.round(), y_row(row), 1.0, lh), rest)
            } else {
                (Rect::new(text_rect.x, text_rect.y, 1.0, lh), String::new())
            };
            crate::ime::caret(at, &style, theme.color("editor.background"), &after, text_rect);
        }
        c.pop_clip();

        // Sticky scroll: the lines that start the regions around the top of the view stay
        // pinned there, outermost first, each pushed up as its region scrolls out.
        self.sticky.clear();
        let stuck = sticky_regions(&sticky_ranges, cfg.sticky_lines, |k| (first_row + k < n_rows).then(|| layout.row(first_row + k, b).line));
        if !stuck.is_empty() {
            c.push_layer();
            // Each line sits right under the one before it at most (pushed up with it).
            let mut prev_y = f32::INFINITY;
            let mut bottom = view.y;
            let bg = theme.color("editorStickyScroll.background");
            let gutter_bg = theme.color("editorStickyScrollGutter.background");
            let hover_bg = theme.color("editorStickyScrollHover.background");
            for (k, r) in stuck.iter().enumerate() {
                let end_row = layout.rows_of_line(r.end).end.saturating_sub(1);
                let y = (view.y + k as f32 * lh).min(y_row(end_row)).min(prev_y + lh).max(view.y - lh);
                prev_y = y;
                let row_r = Rect::new(view.x, y, text_rect.right() - view.x, lh);
                let hovered = hover.is_some_and(|(x, yy)| row_r.contains(x, yy) && yy >= view.y);
                c.fill(row_r, theme.color("editor.background"));
                c.fill(row_r, if hovered { hover_bg } else { bg });
                c.fill(Rect::new(view.x, y, gutter.w, lh), gutter_bg);
                let st = style.color(theme.color("editorLineNumber.foreground"));
                let label = (r.start + 1).to_string();
                let w = c.measure(&label, &st);
                if cfg.line_numbers != config::LineNumbers::Off {
                    c.text(gutter.x + numbers_right - w, y, &label, &st);
                }
                let raw = b.line(r.start);
                let spans = doc.highlight.spans(b, r.start, r.start + 1);
                let (text, spans) = expand_tabs_spans(&raw, spans.first().map(Vec::as_slice).unwrap_or_default());
                let colored: Vec<(usize, usize, theme::Color)> = spans.iter().map(|(a, z, t)| (*a, *z, theme.token(*t))).collect();
                c.push_clip(Rect::new(text_rect.x, y, text_rect.w, lh));
                c.rich_text(text_x, y, &text, &colored, &style);
                c.pop_clip();
                self.sticky.push((row_r, r.start));
                bottom = y + lh;
            }
            // The border (when the theme sets one) and a shadow under the pinned lines.
            c.fill(Rect::new(view.x, bottom - 1.0, text_rect.right() - view.x, 1.0), theme.color("editorStickyScroll.border"));
            for i in 0..3 {
                let a = 0.3 * (1.0 - i as f32 / 3.0);
                c.fill(Rect::new(view.x, bottom + i as f32, text_rect.right() - view.x, 1.0), theme.color("editorStickyScroll.shadow").with_alpha(a));
            }
        }

        // Minimap.
        if minimap {
            self.draw_minimap(c, theme, doc, minimap_rect, hover);
        }

        // Shadow under the tab bar once the content is scrolled.
        if self.scroll_y > 0.0 {
            for i in 0..4 {
                let a = 0.35 * (1.0 - i as f32 / 4.0);
                c.fill(Rect::new(view.x, view.y + i as f32, view.w, 1.0), theme.color("scrollbar.shadow").with_alpha(a));
            }
        }

        // Vertical scrollbar.
        let content_h = self.max_scroll_y(doc) + view.h;
        if content_h > view.h + 1.0 {
            let slider_h = (view.h * view.h / content_h).max(20.0);
            let t = self.scroll_y / self.max_scroll_y(doc).max(1.0);
            let slider = Rect::new(scrollbar.x, scrollbar.y + t * (scrollbar.h - slider_h), SCROLLBAR_W, slider_h);
            self.geom.slider = slider;
            let hovered = hover.is_some_and(|(x, y)| scrollbar.contains(x, y));
            let key = if dragging_slider {
                "scrollbarSlider.activeBackground"
            } else if hovered {
                "scrollbarSlider.hoverBackground"
            } else {
                "scrollbarSlider.background"
            };
            c.fill(Rect::new(scrollbar.x, view.y, 1.0, view.h), theme.color("widget.border"));
            c.fill(slider, theme.color(key));
            // Find, then diagnostic markers in the overview ruler, then the caret marker.
            let layout = &self.layout;
            let n_rows = layout.row_count(&doc.buffer).max(1) as f32;
            let y_line = |line: usize| scrollbar.y + layout.rows_of_line(line).start as f32 / n_rows * scrollbar.h;
            let find_color = theme.color("editorOverviewRuler.findMatchForeground");
            for &(a, _) in deco.matches {
                c.fill(Rect::new(scrollbar.x + 1.0, y_line(a.line), SCROLLBAR_W / 2.0 - 1.0, 2.0), find_color);
            }
            for cf in deco.conflicts {
                let current = theme.color("editorOverviewRuler.currentContentForeground");
                let incoming = theme.color("editorOverviewRuler.incomingContentForeground");
                let (ys, ym, ye) = (y_line(cf.start), y_line(cf.split), y_line((cf.end + 1).min(n_lines.saturating_sub(1))));
                c.fill(Rect::new(scrollbar.x + 1.0, ys, SCROLLBAR_W - 1.0, (ym - ys).max(2.0)), current);
                c.fill(Rect::new(scrollbar.x + 1.0, ym, SCROLLBAR_W - 1.0, (ye - ym).max(2.0)), incoming);
            }
            for d in deco.ext {
                if let Some(color) = d.ruler {
                    c.fill(Rect::new(scrollbar.x + 1.0, y_line(d.start.line), SCROLLBAR_W - 2.0, 2.0), color);
                }
            }
            for sq in squiggles.iter().filter(|s| s.severity <= lsp::Severity::Warning) {
                let color = severity_color(theme, sq.severity);
                c.fill(Rect::new(scrollbar.x + SCROLLBAR_W / 2.0, y_line(sq.start.line), SCROLLBAR_W / 2.0, 3.0), color);
            }
            let marker_y = scrollbar.y + layout.row_of(head) as f32 / n_rows * scrollbar.h;
            c.fill(Rect::new(scrollbar.x + 1.0, marker_y, SCROLLBAR_W - 1.0, 2.0), theme.color("editorCursor.foreground").with_alpha(0.6));
        } else {
            self.geom.slider = Rect::default();
        }
    }

    fn draw_minimap(&mut self, c: &mut Canvas, theme: &Theme, doc: &mut Doc, r: Rect, hover: Option<(f32, f32)>) {
        // The minimap shows screen rows, so it follows word wrap and folding like the editor.
        let n_rows = self.layout.row_count(&doc.buffer);
        c.fill(r, theme.color("minimap.background"));
        c.push_clip(r);
        let fits = (r.h / MINIMAP_LINE_H) as usize;
        // When the document is taller than the minimap, the minimap scrolls with the editor.
        let first = if n_rows > fits {
            let t = self.scroll_y / self.max_scroll_y(doc).max(1.0);
            (t * (n_rows - fits) as f32) as usize
        } else {
            0
        };
        self.geom.minimap_first = first;
        let last = (first + fits + 1).min(n_rows);
        let rows: Vec<crate::layout::VRow> = (first..last).map(|i| self.layout.row(i, &doc.buffer)).collect();
        if let (Some(a), Some(z)) = (rows.first(), rows.last()) {
            let (first_line, last_line) = (a.line, z.line + 1);
            let all_spans = doc.highlight.spans(&doc.buffer, first_line, last_line);
            let mut expanded: Option<(usize, String, Vec<Span>)> = None;
            for (i, row) in rows.iter().enumerate() {
                if row.zone {
                    continue;
                }
                if expanded.as_ref().is_none_or(|e| e.0 != row.line) {
                    let raw = doc.buffer.line(row.line);
                    let (text, spans) = expand_tabs_spans(&raw, &all_spans[row.line - first_line]);
                    expanded = Some((row.line, text, spans));
                }
                let (_, text, spans) = expanded.as_ref().unwrap();
                let raw = doc.buffer.line(row.line);
                let (d0, d1) = (col_to_display(&raw, row.start), col_to_display(&raw, row.end));
                let y = r.y + i as f32 * MINIMAP_LINE_H;
                let x0 = r.x + 4.0 + row.indent as f32 - d0 as f32;
                // Draw runs of non-space characters as thin bars colored by token.
                let token_at = |b: usize| spans.iter().find(|(a, z, _)| *a <= b && b < *z).map_or(Token::Plain, |s| s.2);
                let mut run: Option<(usize, Token)> = None;
                let flush = |c: &mut Canvas, run: Option<(usize, Token)>, col: usize| {
                    if let Some((start, token)) = run {
                        let w = (col - start) as f32;
                        c.fill(Rect::new(x0 + start as f32, y, w, MINIMAP_LINE_H - 0.5), theme.token(token).with_alpha(0.6));
                    }
                };
                for (col, (b, ch)) in text.char_indices().enumerate().skip(d0).take(d1 - d0) {
                    if x0 + col as f32 > r.right() {
                        break;
                    }
                    if ch == ' ' {
                        flush(c, run.take(), col);
                        continue;
                    }
                    let token = token_at(b);
                    match run {
                        Some((_, t)) if t == token => {}
                        _ => {
                            flush(c, run.take(), col);
                            run = Some((col, token));
                        }
                    }
                }
                flush(c, run, d1);
            }
        }
        // Viewport slider, shown on hover by default (`editor.minimap.showSlider`).
        if config::get().minimap_slider_always || hover.is_some_and(|(x, y)| r.contains(x, y)) {
            let top = r.y + (self.scroll_y / line_height() - first as f32) * MINIMAP_LINE_H;
            let h = self.geom.text.h / line_height() * MINIMAP_LINE_H;
            c.fill(Rect::new(r.x, top, r.w, h), theme.color("minimapSlider.background"));
        }
        c.pop_clip();
    }
}

/// An inlay hint: its label in a rounded box (editorInlayHint colors), slightly smaller than
/// the text, `width()` columns wide from `x`.
/// Sticky scroll's regions, outermost first (at most `max`): for the `k`th pinned line, the
/// region that starts above the line shown just under the pinned ones (`line_under(k)`), is
/// inside the previous pinned region and starts first.
fn sticky_regions(ranges: &[crate::folding::FoldRange], max: usize, line_under: impl Fn(usize) -> Option<usize>) -> Vec<crate::folding::FoldRange> {
    let mut stuck: Vec<crate::folding::FoldRange> = Vec::new();
    while stuck.len() < max.max(1) {
        let Some(below) = line_under(stuck.len()) else { break };
        let parent = stuck.last().copied();
        let next = ranges
            .iter()
            .filter(|r| r.start < below && r.end >= below && parent.is_none_or(|p| r.start > p.start && r.end <= p.end))
            .min_by_key(|r| r.start);
        match next {
            Some(r) => stuck.push(*r),
            None => break,
        }
    }
    stuck
}

/// The link (http, https, file or mailto URL) at char `col` of `line`, like the standard link
/// detection: it ends at whitespace, quotes or angle brackets, and closing punctuation at its
/// end isn't part of it.
pub fn link_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let text: String = chars.iter().collect();
    for scheme in ["https://", "http://", "file://", "mailto:"] {
        let mut from = 0;
        while let Some(b) = text[from..].find(scheme) {
            let start = text[..from + b].chars().count();
            let mut end = start;
            while end < chars.len() && !chars[end].is_whitespace() && !"\"'<>`".contains(chars[end]) {
                end += 1;
            }
            // Trailing punctuation belongs to the sentence (and a ")" only closes a "(").
            while end > start && ".,;:!?".contains(chars[end - 1]) {
                end -= 1;
            }
            // Closing parens without an opening one in the link are the text's.
            while end > start && chars[end - 1] == ')' && chars[start..end].iter().filter(|&&c| c == ')').count() > chars[start..end].iter().filter(|&&c| c == '(').count() {
                end -= 1;
            }
            if (start..end).contains(&col) && end - start > scheme.len() {
                return Some(chars[start..end].iter().collect());
            }
            from += b + scheme.len();
        }
    }
    None
}

/// A breakpoint in the glyph margin cell `r`, like `debug-breakpoint*` icons.
fn draw_breakpoint(c: &mut Canvas, theme: &Theme, r: Rect, look: BpLook, alpha: f32) {
    let key = match look {
        BpLook::Disabled => "debugIcon.breakpointDisabledForeground",
        BpLook::Unverified => "debugIcon.breakpointUnverifiedForeground",
        _ => "debugIcon.breakpointForeground",
    };
    let mut color = theme.color(key);
    color.a *= alpha;
    let d = 10.0;
    let dot = Rect::new(r.x + (r.w - d) / 2.0, r.y + (r.h - d) / 2.0, d, d);
    match look {
        BpLook::Log => c.icon_in(&icons::LOGPOINT, r, 13.0, color),
        BpLook::Unverified => c.bordered(dot, theme::Color::TRANSPARENT, color, 1.5, d / 2.0),
        BpLook::Conditional => {
            c.fill_rounded(dot, color, d / 2.0);
            let bg = theme.color("editor.background");
            c.fill(Rect::new(dot.x + 2.5, dot.y + 3.0, d - 5.0, 1.2), bg);
            c.fill(Rect::new(dot.x + 2.5, dot.y + 5.8, d - 5.0, 1.2), bg);
        }
        _ => c.fill_rounded(dot, color, d / 2.0),
    }
}

fn draw_inlay_hint(c: &mut Canvas, theme: &Theme, h: &crate::layout::InlayHint, x: f32, y: f32, cw: f32, lh: f32) {
    if let Some((color, _)) = h.swatch {
        // `.colorpicker-color-decoration`: a 0.8em square with a 0.1em border, which
        // its stylesheet (not the theme) makes #eee on dark themes and #000 on light ones.
        let em = font_size();
        let size = (0.8 * em).round();
        let r = Rect::new((x + (2.0 * cw - size) / 2.0).round(), (y + (lh - size) / 2.0).round(), size, size);
        let border = if theme.is_dark() { theme::Color::rgba8(0xee, 0xee, 0xee, 0xff) } else { theme::Color::rgba8(0, 0, 0, 0xff) };
        let bw = (0.1 * em).round().max(1.0);
        c.fill(r, border);
        let inner = Rect::new(r.x + bw, r.y + bw, r.w - 2.0 * bw, r.h - 2.0 * bw);
        c.fill(inner, theme.color("editor.background"));
        c.fill(inner, color);
        return;
    }
    // An extension's before/after text: plain text in its color.
    if let Some(color) = h.color {
        let st = TextStyle::mono(font_size(), lh, color);
        c.text(x + if h.pad_left { cw } else { 0.0 }, y, &h.label, &st);
        return;
    }
    let (fg_key, bg_key) = if h.parameter {
        ("editorInlayHint.parameterForeground", "editorInlayHint.parameterBackground")
    } else {
        ("editorInlayHint.typeForeground", "editorInlayHint.typeBackground")
    };
    let fg = theme.color_opt(fg_key).filter(|c| c.a > 0.0).unwrap_or_else(|| theme.color("editorInlayHint.foreground"));
    let bg = theme.color_opt(bg_key).filter(|c| c.a > 0.0).unwrap_or_else(|| theme.color("editorInlayHint.background"));
    let x0 = x + if h.pad_left { cw } else { 0.0 };
    let w = display_width(&h.label) as f32 * cw;
    let r = Rect::new(x0, y + 1.0, w, lh - 2.0);
    c.fill_rounded(r, bg, 3.0);
    let st = TextStyle::mono(font_size() * 0.9, lh, fg);
    let tw = c.measure(&h.label, &st);
    c.text(x0 + ((w - tw) / 2.0).max(0.0), y, &h.label, &st);
}

/// Indent level for guides; blank lines take the level of the next non-blank line.
fn indent_level(b: &Buffer, line: usize, text: &str) -> usize {
    let spaces = |s: &str| display_width(&s.chars().take_while(|c| *c == ' ' || *c == '\t').collect::<String>());
    if !text.trim().is_empty() {
        return spaces(text).div_ceil(tab_size());
    }
    let prev = (line.saturating_sub(100)..line).rev().map(|l| b.line(l)).find(|l| !l.trim().is_empty());
    let next = (line + 1..(line + 100).min(b.len_lines())).map(|l| b.line(l)).find(|l| !l.trim().is_empty());
    match (prev, next) {
        (Some(p), Some(n)) => spaces(&p).min(spaces(&n)).div_ceil(tab_size()),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wide chars take two columns, combining marks none, tabs reach the next stop.
    #[test]
    fn columns_count_wide_chars() {
        assert_eq!(display_width("日本"), 4);
        assert_eq!(display_width("a日b"), 4);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(col_to_display("a日b", 2), 3);
        assert_eq!(col_to_display("한국어 x", 4), 7);
        assert_eq!(cells('\t', 1), tab_size() - 1);
        assert_eq!(cells('日', 0), 2);
        // A click lands on the nearer side of a wide char.
        assert_eq!(display_to_col("a日b", 1.9), 1);
        assert_eq!(display_to_col("a日b", 2.1), 2);
        assert_eq!(display_to_col("a日b", 3.4), 2);
        assert_eq!(display_to_col("a日b", 3.6), 3);
    }

    #[test]
    fn finds_links() {
        let line = "// See https://code.visualstudio.com/docs (and http://x.io/a_(b)), or mailto:me@x.io.";
        assert_eq!(link_at(line, 10).as_deref(), Some("https://code.visualstudio.com/docs"));
        assert_eq!(link_at(line, 50).as_deref(), Some("http://x.io/a_(b)"));
        assert_eq!(link_at(line, 75).as_deref(), Some("mailto:me@x.io"));
        assert_eq!(link_at(line, 3), None);
        assert_eq!(link_at("let s = \"https://a.b/c\";", 12).as_deref(), Some("https://a.b/c"));
    }

    #[test]
    fn sticky_scroll_picks_enclosing_regions() {
        use crate::folding::FoldRange;
        // impl (0..=20) > fn (2..=10) > if (4..=8), and fn (12..=19).
        let ranges = [FoldRange { start: 0, end: 20 }, FoldRange { start: 2, end: 10 }, FoldRange { start: 4, end: 8 }, FoldRange { start: 12, end: 19 }];
        let starts = |top: usize| sticky_regions(&ranges, 5, |k| Some(top + k)).iter().map(|r| r.start).collect::<Vec<_>>();
        // Scrolled so line 6 is at the top: impl, fn and if are pinned (lines 6..8 hide under them).
        assert_eq!(starts(6), [0, 2, 4]);
        // Line 1 at the top: impl is pinned over it and the fn line (2) shows under it.
        assert_eq!(starts(1), [0]);
        // Line 2 at the top is hidden under impl, so the fn line is pinned too.
        assert_eq!(starts(2), [0, 2]);
        // In the second fn.
        assert_eq!(starts(14), [0, 12]);
        // At most `max` lines.
        assert_eq!(sticky_regions(&ranges, 2, |k| Some(6 + k)).len(), 2);
        assert!(starts(21).is_empty());
    }

    fn doc(text: &str) -> Doc {
        let mut d = Doc::untitled(1);
        d.buffer.insert(Selection::default(), text);
        d.buffer.break_undo_group();
        d
    }

    fn key(key: Key) -> KeyInput {
        KeyInput { key, text: None, cmd: false, shift: false, alt: false, ctrl: false }
    }

    fn typed(s: &str) -> KeyInput {
        KeyInput { text: Some(s.into()), ..key(Key::Char(s.into())) }
    }

    fn carets(ed: &mut EditorState, positions: &[(usize, usize)]) {
        ed.set_selections(positions.iter().map(|&(l, c)| Selection::caret(Pos::new(l, c))).collect());
    }

    #[test]
    fn types_and_deletes_at_every_cursor() {
        let mut d = doc("one\ntwo\nthree");
        let mut ed = EditorState::new(0);
        carets(&mut ed, &[(0, 3), (1, 3), (2, 5)]);
        for ch in ["!", "?"] {
            ed.key(&mut d, &typed(ch));
        }
        assert_eq!(d.buffer.text(), "one!?\ntwo!?\nthree!?");
        ed.key(&mut d, &key(Key::Backspace));
        assert_eq!(d.buffer.text(), "one!\ntwo!\nthree!");
        // Auto-closing pairs and stepping over the closer, per cursor.
        ed.key(&mut d, &typed("("));
        assert_eq!(d.buffer.text(), "one!()\ntwo!()\nthree!()");
        ed.key(&mut d, &typed(")"));
        assert_eq!(ed.selections().iter().map(|s| s.head.col).collect::<Vec<_>>(), vec![6, 6, 8]);
        // One undo step removes the pairs at all three cursors, and restores the cursors.
        ed.undo(&mut d, false);
        assert_eq!(d.buffer.text(), "one!\ntwo!\nthree!");
        assert_eq!(ed.selections().len(), 3);
    }

    #[test]
    fn cursors_on_one_line_and_merging() {
        let mut d = doc("a b c");
        let mut ed = EditorState::new(0);
        carets(&mut ed, &[(0, 1), (0, 3), (0, 5)]);
        ed.key(&mut d, &typed("x"));
        assert_eq!(d.buffer.text(), "ax bx cx");
        // Moving to the line start merges all cursors into one.
        ed.key(&mut d, &key(Key::Home));
        assert_eq!(ed.selections().len(), 1);
        // Enter at several carets keeps indentation.
        let mut d = doc("    a\n    b");
        carets(&mut ed, &[(0, 5), (1, 5)]);
        ed.key(&mut d, &key(Key::Enter));
        assert_eq!(d.buffer.text(), "    a\n    \n    b\n    ");
    }

    #[test]
    fn paste_spreads_lines_and_cut_takes_lines() {
        let mut d = doc("x\ny\nz");
        let mut ed = EditorState::new(0);
        carets(&mut ed, &[(0, 1), (1, 1), (2, 1)]);
        ed.paste(&mut d, "1\n2\n3");
        assert_eq!(d.buffer.text(), "x1\ny2\nz3");
        ed.paste(&mut d, "-");
        assert_eq!(d.buffer.text(), "x1-\ny2-\nz3-");
        // With empty selections, copy and cut take whole lines.
        carets(&mut ed, &[(0, 0), (2, 0)]);
        assert_eq!(ed.copy_ranges(&d).0, "x1-\nz3-\n");
        assert_eq!(ed.cut(&mut d), "x1-\nz3-\n");
        assert_eq!(d.buffer.text(), "y2-\n");
    }

    #[test]
    fn add_cursors_below_keep_column() {
        let d = doc("abcdef\nab\nabcdef");
        let mut ed = EditorState::new(0);
        carets(&mut ed, &[(0, 4)]);
        ed.insert_cursor_vertical(&d, true);
        ed.insert_cursor_vertical(&d, true);
        let heads: Vec<Pos> = {
            let mut s: Vec<Pos> = ed.selections().iter().map(|s| s.head).collect();
            s.sort();
            s
        };
        // The short middle line clamps, the last line gets the original column back.
        assert_eq!(heads, vec![Pos::new(0, 4), Pos::new(1, 2), Pos::new(2, 4)]);
        assert_eq!(ed.sel.head, Pos::new(2, 4));
    }

    #[test]
    fn column_selection() {
        let mut d = doc("abcdef\nab\nabcdef");
        let mut ed = EditorState::new(0);
        // A 10px-wide character grid starting at (0, 0).
        ed.geom.text = render::Rect::new(0.0, 0.0, 500.0, 500.0);
        ed.geom.cw = 10.0;
        let lh = line_height();
        let from = ed.column_at(&d, 20.0, 1.0);
        ed.column_select(&d, from, 40.0, 2.0 * lh + 1.0);
        let mut sels: Vec<(Pos, Pos)> = ed.selections().iter().map(|s| (s.anchor, s.head)).collect();
        sels.sort();
        // Columns 2..4 on each line; the short line clamps to its end.
        assert_eq!(
            sels,
            vec![(Pos::new(0, 2), Pos::new(0, 4)), (Pos::new(1, 2), Pos::new(1, 2)), (Pos::new(2, 2), Pos::new(2, 4))]
        );
        ed.key(&mut d, &typed("_"));
        assert_eq!(d.buffer.text(), "ab_ef\nab_\nab_ef");
    }

    #[test]
    fn occurrences() {
        let mut d = doc("foo bar foo_x foo");
        let mut ed = EditorState::new(0);
        carets(&mut ed, &[(0, 1)]);
        ed.add_next_occurrence(&d, false); // selects the word
        assert_eq!(d.buffer.text_in(&ed.sel), "foo");
        ed.add_next_occurrence(&d, false); // whole words: skips foo_x
        assert_eq!(ed.sel.ordered().0, Pos::new(0, 14));
        assert_eq!(ed.selections().len(), 2);
        ed.key(&mut d, &typed("z"));
        assert_eq!(d.buffer.text(), "z bar foo_x z");
        let mut ed = EditorState::new(0);
        ed.set_selection(Selection { anchor: Pos::new(0, 0), head: Pos::new(0, 1), goal_col: None });
        ed.select_all_occurrences(&d);
        assert_eq!(ed.selections().len(), 2);
    }

    #[test]
    fn comment_and_indent_all_selected_lines() {
        let mut d = Doc::open_virtual("a.rs", "a\nb\nc");
        let mut ed = EditorState::new(0);
        carets(&mut ed, &[(0, 1), (2, 1)]);
        ed.toggle_comment(&mut d);
        assert_eq!(d.buffer.text(), "// a\nb\n// c");
        assert_eq!(ed.selections().iter().map(|s| s.head.col).collect::<Vec<_>>(), vec![4, 4]);
        ed.undo(&mut d, false);
        assert_eq!(d.buffer.text(), "a\nb\nc");
        // Indent every line touched by a selection, as one step.
        ed.set_selections(vec![
            Selection { anchor: Pos::new(0, 0), head: Pos::new(1, 1), goal_col: None },
            Selection::caret(Pos::new(2, 0)),
        ]);
        ed.key(&mut d, &key(Key::Tab));
        assert_eq!(d.buffer.text(), "    a\n    b\n    c");
        ed.key(&mut d, &KeyInput { shift: true, ..key(Key::Tab) });
        assert_eq!(d.buffer.text(), "a\nb\nc");
    }
}

/// Timings for a million-line file: `cargo test --release -p app large_file_costs -- --ignored
/// --nocapture`.
#[cfg(test)]
mod perf_probe {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn large_file_costs() {
        let dir = std::env::temp_dir().join("orbvane-perf");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.rs");
        let line = "    let value = compute(alpha, beta) + other_function(gamma); // comment\n";
        std::fs::write(&path, line.repeat(1_000_000)).unwrap();
        let t = Instant::now();
        let mut doc = Doc::open(path.clone()).unwrap();
        eprintln!("open: {:?}", t.elapsed());
        let t = Instant::now();
        doc.highlight.update(&mut doc.buffer);
        eprintln!("highlight update: {:?}", t.elapsed());
        let t = Instant::now();
        let _ = doc.highlight.spans(&doc.buffer, 999_900, 999_950);
        eprintln!("spans at end: {:?}", t.elapsed());
        let t = Instant::now();
        let _ = doc.longest_line();
        eprintln!("longest line: {:?}", t.elapsed());
        let t = Instant::now();
        doc.brackets.update(&doc.buffer, doc.lang);
        eprintln!("brackets: {:?}", t.elapsed());
        let t = Instant::now();
        let _ = crate::folding::indent_ranges(&doc.buffer);
        eprintln!("indent ranges: {:?}", t.elapsed());
        let t = Instant::now();
        let _ = doc.buffer.text();
        eprintln!("text(): {:?}", t.elapsed());
        let t = Instant::now();
        doc.buffer.insert(Selection::caret(Pos::new(500_000, 4)), "x");
        doc.highlight.update(&mut doc.buffer);
        let _ = doc.highlight.spans(&doc.buffer, 500_000, 500_050);
        eprintln!("edit + highlight update + spans: {:?}", t.elapsed());
        let t = Instant::now();
        let _ = doc.longest_line();
        eprintln!("longest line after edit: {:?}", t.elapsed());
        eprintln!("large: {}", doc.large);
    }
}
