//! Screen rows of an editor: which piece of which buffer line each row shows. With word wrap
//! a long line takes several rows, and folded lines take none; otherwise rows and lines are
//! the same (and nothing is stored). Everything that converts between positions and screen
//! coordinates goes through here, so wrapping and folding stay in one place.

use std::collections::HashMap;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use text::{Buffer, Pos};

use crate::editor::{col_to_display, display_to_col, tab_size};

/// A screen row: chars `start..end` of buffer line `line`, drawn `indent` columns in
/// (continuation rows of a wrapped line line up with its indentation). A `zone` row is the
/// space above `line` for its code lenses: it shows no text and the caret never goes there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub indent: usize,
    pub zone: bool,
    /// A zone row of the peek view under `line` (not a code lens row).
    pub peek: bool,
}

/// How continuation rows are indented (`editor.wrappingIndent`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WrapIndent {
    None,
    #[default]
    Same,
    Indent,
    DeepIndent,
}

/// An inlay hint (a type or parameter name the language server shows inline): it takes
/// columns before char `col` of its line. The caret at `col` sits before it.
#[derive(Clone, Debug, PartialEq)]
pub struct InlayHint {
    pub col: usize,
    pub label: String,
    /// A parameter name (`a:`) rather than a type.
    pub parameter: bool,
    pub pad_left: bool,
    pub pad_right: bool,
    /// A color swatch (`workbench/color_decorators.rs`) instead of a label: the color, and
    /// the column where its text ends.
    pub swatch: Option<(theme::Color, usize)>,
    /// An extension's before/after decoration text, drawn plainly in this color.
    pub color: Option<theme::Color>,
}

impl InlayHint {
    /// Columns it takes: a swatch takes two.
    pub fn width(&self) -> usize {
        if self.swatch.is_some() {
            return 2;
        }
        self.label.chars().count() + self.pad_left as usize + self.pad_right as usize
    }

    pub fn swatch(col: usize, end: usize, color: theme::Color) -> Self {
        Self { col, label: String::new(), parameter: false, pad_left: false, pad_right: false, swatch: Some((color, end)), color: None }
    }
}

/// A document's inlay hints by line (each line's sorted by column), with a generation that
/// changes whenever they do.
#[derive(Clone, Default)]
pub struct Inlays {
    pub lines: Arc<HashMap<usize, Vec<InlayHint>>>,
    pub generation: u64,
}

/// A document's code lenses: the lines that have them (sorted), and each lens as (line, its
/// title once known, its index in the server's list). `generation` changes with them.
#[derive(Clone, Default)]
pub struct Lenses {
    pub lines: Arc<Vec<usize>>,
    pub items: Arc<Vec<(usize, Option<String>, usize)>>,
    pub generation: u64,
}

impl Inlays {
    pub fn on_line(&self, line: usize) -> &[InlayHint] {
        self.lines.get(&line).map_or(&[], Vec::as_slice)
    }
}

#[derive(Default)]
pub struct Layout {
    /// What the rows were built for: (buffer version, wrap, fold generation). None: identity.
    key: Option<(u64, Option<(usize, WrapIndent)>, u64)>,
    /// Rows, when wrapping or folding. Empty means one row per line.
    rows: Vec<VRow>,
    /// First row of each line; a hidden line maps to the last row of the line folding it.
    line_row: Vec<usize>,
    /// Whether each line is hidden (folded), when folding.
    hidden: Vec<bool>,
    wrap: Option<(usize, WrapIndent)>,
    folds: (Vec<RangeInclusive<usize>>, u64),
    inlays: Inlays,
    /// Lines with a code lens row above them, and a generation that changes with them.
    zones: (Arc<Vec<usize>>, u64),
    /// Each line's lens row, when it has one (else usize::MAX).
    zone_row: Vec<usize>,
    /// The peek view: rows under a line (line, rows), and a generation that changes with it.
    peek: (Option<(usize, usize)>, u64),
    /// The first row of the peek view, if it's shown.
    peek_row: Option<usize>,
}

/// Display width of each prefix of `chars` (with tabs at tab stops): `disp[i]` is the column
/// where char `i` starts.
fn prefix_display(chars: &[char]) -> Vec<usize> {
    let mut out = Vec::with_capacity(chars.len() + 1);
    let mut d = 0;
    out.push(0);
    for &c in chars {
        d += if c == '\t' { tab_size() - d % tab_size() } else { 1 };
        out.push(d);
    }
    out
}

/// Splits a line into rows of at most `width` columns, breaking after whitespace when
/// possible.
fn wrap_line(line: usize, text: &str, width: usize, indent_mode: WrapIndent, hints: &[InlayHint], out: &mut Vec<VRow>) {
    let chars: Vec<char> = text.chars().collect();
    let mut disp = prefix_display(&chars);
    // Inlay hints take room on the row of the char after them.
    for h in hints {
        for d in disp.iter_mut().skip(h.col + 1) {
            *d += h.width();
        }
    }
    let total = *disp.last().unwrap();
    if total <= width || chars.is_empty() {
        out.push(VRow { line, start: 0, end: chars.len(), indent: 0, zone: false, peek: false });
        return;
    }
    let lead = chars.iter().take_while(|c| **c == ' ' || **c == '\t').count();
    let base = disp[lead];
    let indent = match indent_mode {
        WrapIndent::None => 0,
        WrapIndent::Same => base,
        WrapIndent::Indent => base + tab_size(),
        WrapIndent::DeepIndent => base + 2 * tab_size(),
    };
    // Continuation rows keep at least half the width for text.
    let indent = if indent > width / 2 { 0 } else { indent };
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        let avail = if first { width } else { width - indent };
        let row_indent = if first { 0 } else { indent };
        // The furthest end that fits.
        let mut end = start;
        while end < chars.len() && disp[end + 1] - disp[start] <= avail {
            end += 1;
        }
        if end == start {
            end = start + 1; // a single char wider than the row
        }
        if end < chars.len() {
            // Prefer breaking after whitespace (the space stays at the end of this row).
            if let Some(b) = (start + 1..=end).rev().find(|&i| chars[i - 1].is_whitespace() && !chars[i].is_whitespace()) {
                if b > start + (end - start) / 4 {
                    end = b;
                }
            }
        }
        out.push(VRow { line, start, end, indent: row_indent, zone: false, peek: false });
        start = end;
        first = false;
    }
}

impl Layout {
    /// Rebuilds the rows if the buffer, the wrap settings or the folds changed. `wrap`: the
    /// wrap width in columns, or None. `hidden`: folded line ranges (sorted), with a
    /// generation number that changes whenever they do.
    pub fn update(&mut self, b: &Buffer, wrap: Option<(usize, WrapIndent)>, hidden: &[RangeInclusive<usize>], generation: u64) {
        self.wrap = wrap.map(|(w, i)| (w.max(10), i));
        if self.folds.1 != generation || self.folds.0.as_slice() != hidden {
            self.folds = (hidden.to_vec(), generation);
        }
        if self.wrap.is_none() && hidden.is_empty() && self.zones.0.is_empty() && self.peek.0.is_none() {
            self.peek_row = None;
            self.key = None;
            self.rows = Vec::new();
            self.line_row = Vec::new();
            self.hidden = Vec::new();
            return;
        }
        let key = (b.version(), self.wrap, generation ^ self.inlays.generation.rotate_left(32) ^ self.zones.1.rotate_left(16) ^ self.peek.1.rotate_left(48));
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.rows.clear();
        self.line_row.clear();
        self.hidden.clear();
        self.zone_row.clear();
        self.peek_row = None;
        let zones = self.zones.0.clone();
        let mut folds = hidden.iter().peekable();
        for line in 0..b.len_lines() {
            while folds.peek().is_some_and(|r| *r.end() < line) {
                folds.next();
            }
            if folds.peek().is_some_and(|r| r.contains(&line)) {
                self.line_row.push(self.rows.len().saturating_sub(1));
                self.hidden.push(true);
                self.zone_row.push(usize::MAX);
                continue;
            }
            if zones.binary_search(&line).is_ok() {
                self.zone_row.push(self.rows.len());
                self.rows.push(VRow { line, start: 0, end: 0, indent: 0, zone: true, peek: false });
            } else {
                self.zone_row.push(usize::MAX);
            }
            self.line_row.push(self.rows.len());
            self.hidden.push(false);
            match self.wrap {
                Some((width, indent)) => wrap_line(line, &b.line(line), width, indent, self.inlays.on_line(line), &mut self.rows),
                None => self.rows.push(VRow { line, start: 0, end: b.line_len(line), indent: 0, zone: false, peek: false }),
            }
            if let Some((_, n)) = self.peek.0.filter(|(l, _)| *l == line) {
                self.peek_row = Some(self.rows.len());
                let end = b.line_len(line);
                self.rows.extend((0..n).map(|_| VRow { line, start: end, end, indent: 0, zone: true, peek: true }));
            }
        }
    }

    /// Uses a document's inlay hints (rows are rebuilt on the next `update` if they changed).
    pub fn set_inlays(&mut self, inlays: &Inlays) {
        if inlays.generation != self.inlays.generation {
            self.inlays = inlays.clone(); // part of the rows' key
        }
    }

    /// Gives these lines (sorted) a code lens row above them; `generation` changes with them.
    pub fn set_zones(&mut self, lines: &Arc<Vec<usize>>, generation: u64) {
        if generation != self.zones.1 {
            self.zones = (lines.clone(), generation);
        }
    }

    /// Shows the peek view as `rows` rows under `line` (None: hides it).
    pub fn set_peek(&mut self, peek: Option<(usize, usize)>) {
        if peek != self.peek.0 {
            self.peek = (peek, self.peek.1 + 1);
        }
    }

    /// The first row of the peek view, when it's shown.
    pub fn peek_row(&self) -> Option<usize> {
        self.peek_row
    }

    /// The code lens row above `line`, if it has one.
    pub fn zone_row(&self, line: usize) -> Option<usize> {
        self.zone_row.get(line).copied().filter(|&r| r != usize::MAX)
    }

    pub fn inlays_on(&self, line: usize) -> &[InlayHint] {
        self.inlays.on_line(line)
    }

    /// The display column of char `col` of `line` from the line start, with inlay hints:
    /// `after` counts a hint at `col` (where the char is drawn); the caret sits before it.
    pub fn line_x(&self, line: usize, text: &str, col: usize, after: bool) -> usize {
        let hints: usize = self.inlays.on_line(line).iter().filter(|h| h.col < col || (after && h.col == col)).map(InlayHint::width).sum();
        col_to_display(text, col) + hints
    }

    /// Like `line_x`, within row `vr` (including its indent).
    pub fn row_x(&self, vr: &VRow, text: &str, col: usize, after: bool) -> usize {
        vr.indent + self.line_x(vr.line, text, col, after) - self.line_x(vr.line, text, vr.start, false)
    }

    /// The char of row `row` under display column `x` (None over an inlay hint, the indent or
    /// past the end).
    pub fn char_at(&self, row: usize, x: f32, b: &Buffer) -> Option<usize> {
        let vr = self.row(row, b);
        if vr.zone {
            return None;
        }
        let text = b.line(vr.line);
        (vr.start..vr.end).find(|&c| {
            let (from, to) = (self.row_x(&vr, &text, c, true) as f32, self.row_x(&vr, &text, c + 1, false) as f32);
            from <= x && x < to
        })
    }

    /// Whether `line` is hidden inside a folded region.
    pub fn is_hidden(&self, line: usize) -> bool {
        self.hidden.get(line).copied().unwrap_or(false)
    }

    /// The wrap settings of the last `update`.
    pub fn wrap(&self) -> Option<(usize, WrapIndent)> {
        self.wrap
    }

    pub fn is_wrapping(&self) -> bool {
        self.wrap.is_some()
    }

    fn identity(&self) -> bool {
        self.key.is_none()
    }

    pub fn row_count(&self, b: &Buffer) -> usize {
        if self.identity() { b.len_lines() } else { self.rows.len() }
    }

    pub fn row(&self, i: usize, b: &Buffer) -> VRow {
        if self.identity() {
            return VRow { line: i, start: 0, end: b.line_len(i), indent: 0, zone: false, peek: false };
        }
        self.rows[i.min(self.rows.len().saturating_sub(1))]
    }

    /// The rows showing `line` (a hidden line: the last row of the line folding it).
    pub fn rows_of_line(&self, line: usize) -> Range<usize> {
        if self.identity() {
            return line..line + 1;
        }
        let start = self.line_row.get(line).copied().unwrap_or(self.rows.len().saturating_sub(1));
        if self.is_hidden(line) {
            return start..start + 1;
        }
        // The next visible line's first row (its lens row, if it has one).
        let end = (line + 1..self.line_row.len()).find(|&l| !self.is_hidden(l)).map_or(self.rows.len(), |l| self.zone_row(l).unwrap_or(self.line_row[l]));
        // The peek view under the line isn't part of it.
        let end = self.peek_row.filter(|&p| p > start && p < end).unwrap_or(end);
        start..end.max(start + 1)
    }

    /// The row showing `pos` (a position at a wrap point shows at the start of the next row).
    pub fn row_of(&self, pos: Pos) -> usize {
        let rows = self.rows_of_line(pos.line);
        if self.identity() {
            return rows.start;
        }
        if self.is_hidden(pos.line) {
            return rows.start;
        }
        rows.clone().find(|&r| pos.col < self.rows[r].end).unwrap_or(rows.end - 1)
    }

    /// The display column of `pos` within its row (including the row's indent).
    pub fn x_of(&self, pos: Pos, b: &Buffer) -> usize {
        if self.is_hidden(pos.line) {
            return 0;
        }
        let text = b.line(pos.line);
        let vr = self.row(self.row_of(pos), b);
        self.row_x(&vr, &text, pos.col, false)
    }

    /// The position at display column `x` (fractional: rounds to the nearest boundary) of row
    /// `row`.
    pub fn pos_at(&self, row: usize, x: f32, b: &Buffer) -> Pos {
        let vr = self.row(row, b);
        if vr.zone {
            return Pos::new(vr.line, 0);
        }
        let text = b.line(vr.line);
        let col = if self.inlays.on_line(vr.line).is_empty() {
            let start_x = col_to_display(&text, vr.start) as f32;
            display_to_col(&text, start_x + (x - vr.indent as f32).max(0.0))
        } else {
            // The nearest caret position; over a hint, the position the hint is at.
            let dist = |c: usize| {
                let (a, z) = (self.row_x(&vr, &text, c, false) as f32, self.row_x(&vr, &text, c, true) as f32);
                if x < a { a - x } else if x > z { x - z } else { 0.0 }
            };
            (vr.start..=vr.end).min_by(|&p, &q| dist(p).total_cmp(&dist(q))).unwrap_or(vr.start)
        };
        // Past the end of a wrapped row lands on its last char, not the next row's first.
        let last_row = vr.end == b.line_len(vr.line);
        let max = if last_row || vr.end == vr.start { vr.end } else { vr.end - 1 };
        Pos::new(vr.line, col.clamp(vr.start, max))
    }

    /// Moves `pos` by `delta` rows, keeping display column `goal`.
    pub fn move_rows(&self, pos: Pos, delta: isize, goal: usize, b: &Buffer) -> Pos {
        let mut target = self.row_of(pos) as isize + delta;
        // Lens rows are stepped over (moving up from a line lands on the line above).
        while target >= 0 && (target as usize) < self.row_count(b) && self.row(target as usize, b).zone {
            target += delta.signum();
        }
        if target < 0 {
            return Pos::new(0, 0);
        }
        if target as usize >= self.row_count(b) {
            return b.end();
        }
        self.pos_at(target as usize, goal as f32, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> Buffer {
        let mut b = Buffer::new();
        b.insert(text::Selection::default(), text);
        b
    }

    #[test]
    fn lens_rows_sit_above_their_lines() {
        let b = buffer("a\nfn f() {}\nb\nfn g() {}");
        let mut l = Layout::default();
        l.set_zones(&Arc::new(vec![1, 3]), 1);
        l.update(&b, None, &[], 0);
        assert_eq!(l.row_count(&b), 6);
        assert!(l.row(1, &b).zone && l.row(1, &b).line == 1);
        assert_eq!(l.row_of(Pos::new(1, 0)), 2);
        assert_eq!(l.zone_row(1), Some(1));
        // A line's rows end where the next line's lens row starts.
        assert_eq!(l.rows_of_line(0), 0..1);
        assert_eq!(l.rows_of_line(1), 2..3);
        // The caret skips lens rows; clicking one lands on its line.
        assert_eq!(l.move_rows(Pos::new(2, 0), -1, 0, &b), Pos::new(1, 0));
        assert_eq!(l.move_rows(Pos::new(0, 0), 1, 0, &b), Pos::new(1, 0));
        assert_eq!(l.pos_at(4, 3.0, &b), Pos::new(3, 0));
        assert_eq!(l.char_at(1, 0.0, &b), None);
    }

    #[test]
    fn peek_rows_follow_their_line() {
        let b = buffer("a\nb\nc");
        let mut l = Layout::default();
        l.set_peek(Some((1, 3)));
        l.update(&b, None, &[], 0);
        assert_eq!(l.row_count(&b), 6);
        assert_eq!(l.peek_row(), Some(2));
        assert!(l.row(3, &b).peek);
        assert_eq!(l.rows_of_line(1), 1..2);
        assert_eq!(l.row_of(Pos::new(2, 0)), 5);
        assert_eq!(l.move_rows(Pos::new(1, 0), 1, 0, &b), Pos::new(2, 0));
        l.set_peek(None);
        l.update(&b, None, &[], 0);
        assert_eq!(l.row_count(&b), 3);
        assert_eq!(l.peek_row(), None);
    }

    #[test]
    fn identity_without_wrap() {
        let b = buffer("one\ntwo\nthree");
        let mut l = Layout::default();
        l.update(&b, None, &[], 0);
        assert_eq!(l.row_count(&b), 3);
        assert_eq!(l.row(2, &b), VRow { line: 2, start: 0, end: 5, indent: 0, zone: false, peek: false });
        assert_eq!(l.row_of(Pos::new(1, 2)), 1);
        assert_eq!(l.pos_at(2, 3.4, &b), Pos::new(2, 3));
    }

    #[test]
    fn wraps_at_word_boundaries_with_same_indent() {
        let b = buffer("    alpha beta gamma delta\nshort");
        let mut l = Layout::default();
        l.update(&b, Some((16, WrapIndent::Same)), &[], 0);
        // "    alpha beta " | "gamma delta" (indented 4)
        assert_eq!(l.row_count(&b), 3);
        assert_eq!(l.row(0, &b), VRow { line: 0, start: 0, end: 15, indent: 0, zone: false, peek: false });
        assert_eq!(l.row(1, &b), VRow { line: 0, start: 15, end: 26, indent: 4, zone: false, peek: false });
        assert_eq!(l.row(2, &b).line, 1);
        assert_eq!(l.rows_of_line(0), 0..2);
        // "gamma" starts row 1, drawn at column 4.
        let g = Pos::new(0, 15);
        assert_eq!(l.row_of(g), 1);
        assert_eq!(l.x_of(g, &b), 4);
        assert_eq!(l.pos_at(1, 6.0, &b), Pos::new(0, 17));
        // Clicking past the end of the first row stays on it.
        assert_eq!(l.pos_at(0, 40.0, &b), Pos::new(0, 14));
        // Moving down keeps the display column.
        assert_eq!(l.move_rows(Pos::new(0, 9), 1, 9, &b), Pos::new(0, 20));
        assert_eq!(l.move_rows(Pos::new(0, 20), 1, 9, &b), Pos::new(1, 5));
    }

    #[test]
    fn hard_breaks_words_longer_than_a_row() {
        let b = buffer(&"x".repeat(25));
        let mut l = Layout::default();
        l.update(&b, Some((10, WrapIndent::Same)), &[], 0);
        let rows: Vec<(usize, usize)> = (0..l.row_count(&b)).map(|r| (l.row(r, &b).start, l.row(r, &b).end)).collect();
        assert_eq!(rows, vec![(0, 10), (10, 20), (20, 25)]);
        // Edits rebuild the rows.
        let mut b = b;
        b.insert(text::Selection::caret(Pos::new(0, 25)), "yyyyy");
        l.update(&b, Some((10, WrapIndent::Same)), &[], 0);
        assert_eq!(l.row_count(&b), 3);
        assert_eq!(l.row(2, &b).end, 30);
    }

    #[test]
    fn folded_lines_take_no_rows() {
        let b = buffer("a {\n  b\n  c\n}\nd");
        let mut l = Layout::default();
        l.update(&b, None, &[1..=2], 1);
        assert_eq!(l.row_count(&b), 3);
        assert_eq!((0..3).map(|r| l.row(r, &b).line).collect::<Vec<_>>(), vec![0, 3, 4]);
        assert_eq!(l.rows_of_line(0), 0..1);
        assert_eq!(l.row_of(Pos::new(3, 0)), 1);
        // Moving down from the header skips the folded lines.
        assert_eq!(l.move_rows(Pos::new(0, 1), 1, 1, &b), Pos::new(3, 1));
        assert!(l.is_hidden(2));
    }

    #[test]
    fn inlay_hints_take_columns() {
        let b = buffer("let total = sum(2, 3);");
        let mut l = Layout::default();
        let hint = |col: usize, label: &str| InlayHint { col, label: label.into(), parameter: false, pad_left: false, pad_right: false, swatch: None, color: None };
        let lines = HashMap::from([(0, vec![hint(9, ": i32"), hint(16, "a: ")])]);
        l.set_inlays(&Inlays { lines: Arc::new(lines), generation: 1 });
        l.update(&b, None, &[], 0);
        // The caret at a hint's column sits before it; the char there is drawn after it.
        assert_eq!(l.x_of(Pos::new(0, 9), &b), 9);
        let vr = l.row(0, &b);
        assert_eq!(l.row_x(&vr, &b.line(0), 9, true), 14);
        assert_eq!(l.x_of(Pos::new(0, 16), &b), 21);
        assert_eq!(l.x_of(Pos::new(0, 17), &b), 25);
        // Clicking on a hint lands at its column; after it, on the next chars.
        assert_eq!(l.pos_at(0, 11.0, &b), Pos::new(0, 9));
        assert_eq!(l.pos_at(0, 15.0, &b), Pos::new(0, 10));
        assert_eq!(l.char_at(0, 11.0, &b), None);
        assert_eq!(l.char_at(0, 14.5, &b), Some(9));
        // Wrapping counts them.
        l.update(&b, Some((12, WrapIndent::None)), &[], 0);
        assert!(l.row_count(&b) >= 3);
    }
}
