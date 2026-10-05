//! Multi-cursor commands: add cursors above/below, at line ends, at the next/all occurrences
//! of the selection, with ⌥-click, and column (box) selection with ⇧⌥-drag.

use text::{Pos, Selection};

use crate::editor::{col_to_display, display_to_col, Doc, EditorState};

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Occurrences of `needle` in `text` as char ranges, optionally whole words only.
fn occurrences(text: &str, needle: &str, whole_word: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let n = needle.chars().count();
    let mut out = Vec::new();
    // Byte offsets come back in order, so count chars incrementally.
    let (mut last_byte, mut last_char) = (0, 0);
    for (byte, _) in text.match_indices(needle) {
        last_char += text[last_byte..byte].chars().count();
        last_byte = byte;
        if whole_word {
            let before = text[..byte].chars().next_back();
            let after = text[byte + needle.len()..].chars().next();
            if before.is_some_and(is_word) || after.is_some_and(is_word) {
                continue;
            }
        }
        out.push((last_char, last_char + n));
    }
    out
}

impl EditorState {
    /// ⌥-click: adds a cursor, or removes the one already there.
    pub fn toggle_cursor(&mut self, doc: &Doc, x: f32, y: f32) {
        let pos = self.pos_at(doc, x, y);
        let mut sels = self.selections();
        if let Some(i) = sels.iter().position(|s| s.is_empty() && s.head == pos) {
            if sels.len() > 1 {
                sels.remove(i);
                self.set_selections(sels);
            }
            return;
        }
        sels.insert(0, Selection::caret(pos));
        self.set_selections(sels);
    }

    /// Adds a cursor on the line above (or below) every cursor, at the same visual column.
    pub fn insert_cursor_vertical(&mut self, doc: &Doc, down: bool) {
        let b = &doc.buffer;
        let sels = self.selections();
        let mut added = Vec::new();
        for s in &sels {
            let line = if down { s.head.line + 1 } else { s.head.line.wrapping_sub(1) };
            if line >= b.len_lines() {
                continue;
            }
            let goal = s.goal_col.unwrap_or_else(|| col_to_display(&b.line(s.head.line), s.head.col));
            let pos = Pos::new(line, display_to_col(&b.line(line), goal as f32));
            added.push(Selection { anchor: pos, head: pos, goal_col: Some(goal) });
        }
        // The new cursor furthest in the direction of travel becomes the primary.
        added.sort_by_key(|s| s.head);
        let Some(primary) = (if down { added.last() } else { added.first() }).copied() else { return };
        let mut all = vec![primary];
        all.extend(sels);
        all.extend(added.into_iter().filter(|s| *s != primary));
        self.set_selections(all);
    }

    /// ⌥⇧I: a cursor at the end of every line of each selection.
    pub fn cursors_at_line_ends(&mut self, doc: &Doc) {
        let b = &doc.buffer;
        let mut carets = Vec::new();
        for s in self.selections() {
            let (a, z) = s.ordered();
            if a.line == z.line {
                carets.push(Selection::caret(Pos::new(a.line, b.line_len(a.line))));
                continue;
            }
            let last = if z.col == 0 { z.line - 1 } else { z.line };
            for line in a.line..=last {
                carets.push(Selection::caret(Pos::new(line, b.line_len(line))));
            }
        }
        self.set_selections(carets);
    }

    /// The text ⌘D searches for, and whether only whole words match. With an empty primary
    /// selection, the word under the caret gets selected first (then whole words match).
    fn occurrence_query(&mut self, doc: &Doc) -> Option<(String, bool)> {
        if self.sel.is_empty() && self.extra.is_empty() {
            let word = doc.buffer.word_at(self.sel.head);
            if word.is_empty() {
                return None;
            }
            self.set_selection(word);
            self.word_occurrences = true;
            return None;
        }
        let text = doc.buffer.text_in(&self.sel);
        (!text.is_empty()).then_some((text, self.word_occurrences))
    }

    /// The next occurrence after the primary selection that isn't already selected (wrapping).
    fn next_occurrence(&self, doc: &Doc, query: &str, whole_word: bool, backwards: bool) -> Option<Selection> {
        let b = &doc.buffer;
        let text = b.text();
        let found = occurrences(&text, query, whole_word);
        let taken: Vec<(usize, usize)> =
            self.selections().iter().map(|s| (b.char_index(s.ordered().0), b.char_index(s.ordered().1))).collect();
        let (from, to) = (b.char_index(self.sel.ordered().0), b.char_index(self.sel.ordered().1));
        let free = |m: &&(usize, usize)| !taken.contains(m);
        let pick = if backwards {
            found.iter().rev().filter(free).find(|m| m.1 <= from).or_else(|| found.iter().rev().find(free))
        } else {
            found.iter().filter(free).find(|m| m.0 >= to).or_else(|| found.iter().find(free))
        };
        pick.map(|&(a, z)| Selection { anchor: b.pos_of(a), head: b.pos_of(z), goal_col: None })
    }

    /// ⌘D: selects the word, then adds the next occurrence as a new (primary) selection.
    pub fn add_next_occurrence(&mut self, doc: &Doc, backwards: bool) {
        let Some((query, whole_word)) = self.occurrence_query(doc) else {
            self.reveal = true;
            return;
        };
        if let Some(next) = self.next_occurrence(doc, &query, whole_word, backwards) {
            let mut sels = vec![next];
            sels.extend(self.selections());
            self.set_selections(sels);
        }
    }

    /// ⌘K ⌘D: moves the last added selection to the next occurrence (skipping this one).
    pub fn move_to_next_occurrence(&mut self, doc: &Doc) {
        let Some((query, whole_word)) = self.occurrence_query(doc) else { return };
        if let Some(next) = self.next_occurrence(doc, &query, whole_word, false) {
            let mut sels = vec![next];
            sels.extend(self.extra.iter().copied());
            self.set_selections(sels);
        }
    }

    /// ⇧⌘L: selects every occurrence of the selection (or of the word under the caret).
    pub fn select_all_occurrences(&mut self, doc: &Doc) {
        let (query, whole_word) = if self.sel.is_empty() {
            let word = doc.buffer.word_at(self.sel.head);
            if word.is_empty() {
                return;
            }
            (doc.buffer.text_in(&word), true)
        } else {
            (doc.buffer.text_in(&self.sel), false)
        };
        let b = &doc.buffer;
        let primary_start = b.char_index(self.sel.ordered().0);
        let found = occurrences(&b.text(), &query, whole_word);
        if found.is_empty() {
            return;
        }
        // Keep the primary where it was.
        let mut sels: Vec<Selection> =
            found.iter().map(|&(a, z)| Selection { anchor: b.pos_of(a), head: b.pos_of(z), goal_col: None }).collect();
        if let Some(i) = found.iter().position(|m| m.0 <= primary_start && primary_start <= m.1) {
            sels.swap(0, i);
        }
        self.set_selections(sels);
        self.word_occurrences = whole_word;
    }

    /// ⇧⌥-drag: a box selection from `from` (line, visual column) to the point under the mouse.
    pub fn column_select(&mut self, doc: &Doc, from: (usize, f32), x: f32, y: f32) {
        let b = &doc.buffer;
        let g = &self.geom;
        let row = self.row_at(y).min(self.layout.row_count(b).saturating_sub(1));
        let line = self.layout.row(row, b).line;
        let col = ((x - g.text.x + self.scroll_x) / g.cw.max(1.0)).max(0.0).round();
        let (l0, l1) = (from.0.min(line), from.0.max(line));
        let mut sels: Vec<Selection> = (l0..=l1)
            .map(|l| {
                let text = b.line(l);
                Selection {
                    anchor: Pos::new(l, display_to_col(&text, from.1)),
                    head: Pos::new(l, display_to_col(&text, col)),
                    goal_col: None,
                }
            })
            .collect();
        // The line under the mouse is the primary.
        let primary = line - l0;
        sels.swap(0, primary);
        self.set_selections(sels);
    }

    /// The (line, visual column) under a point, for starting a column selection.
    pub fn column_at(&self, doc: &Doc, x: f32, y: f32) -> (usize, f32) {
        let pos = self.pos_at(doc, x, y);
        let col = ((x - self.geom.text.x + self.scroll_x) / self.geom.cw.max(1.0)).max(0.0).round();
        (pos.line, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_occurrences() {
        let text = "let foo = foo_bar + foo; // fóo foo";
        assert_eq!(occurrences(text, "foo", false).len(), 4);
        let whole = occurrences(text, "foo", true);
        assert_eq!(whole.len(), 3);
        // Char (not byte) offsets: "fóo" has a two-byte char before the last "foo".
        assert_eq!(whole.last(), Some(&(32, 35)));
    }
}
