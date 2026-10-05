//! Text storage for open documents: a rope plus snapshot-based undo/redo.
//!
//! Cursors live outside the buffer (each editor view owns its own), so every
//! editing call takes a `Selection` and returns the updated one. `edit` applies one change per
//! cursor for multi-cursor editing, and undo restores every cursor.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ropey::Rope;

/// A position in the document as (line, char column).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// A selection: `anchor` stays put while `head` (the caret) moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub head: Pos,
    /// Column the caret wants to return to when moving vertically.
    pub goal_col: Option<usize>,
}

impl Selection {
    pub fn caret(pos: Pos) -> Self {
        Self { anchor: pos, head: pos, goal_col: None }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn ordered(&self) -> (Pos, Pos) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }
}

/// A text change in byte offsets and (row, byte column) points, the form incremental
/// parsers such as tree-sitter consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start: (usize, usize),
    pub old_end: (usize, usize),
    pub new_end: (usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Edit(TextEdit),
    /// The whole text was replaced (undo/redo); consumers should start over.
    Reset,
}

/// How an edit groups with the previous one for undo: consecutive typing (or deleting)
/// coalesces into one step, anything else starts a new step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    Other,
}

struct Snapshot {
    rope: Rope,
    /// Every cursor at the time, restored by undo/redo.
    selections: Vec<Selection>,
    /// The `state` of `rope`.
    state: u64,
}

pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    /// Identifies the current contents: a fresh id for each edit, and the snapshot's id again
    /// after undo/redo, so undoing back to the saved text makes the buffer clean again.
    state: u64,
    saved_state: u64,
    version: u64,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: EditKind,
    changes: Vec<Change>,
    /// Recent changes kept for consumers that follow lines (folded regions). Unlike
    /// `changes`, reading doesn't drain it; `log_base` is the sequence number of its first entry.
    log: Vec<Change>,
    log_base: u64,
}

/// How many changes `edits_since` can look back.
const LOG_CAP: usize = 4096;

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            state: 0,
            saved_state: 0,
            version: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: EditKind::Other,
            changes: Vec::new(),
            log: Vec::new(),
            log_base: 0,
        }
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut buffer = Self::new();
        buffer.rope = Rope::from_str(&text);
        buffer.record(Change::Reset);
        buffer.path = Some(path.to_path_buf());
        Ok(buffer)
    }

    pub fn save(&mut self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Err(io::Error::new(io::ErrorKind::Other, "buffer has no path"));
        };
        let mut file = io::BufWriter::new(fs::File::create(path)?);
        self.rope.write_to(&mut file)?;
        self.saved_state = self.state;
        // Saving ends the undo group, so undo can come back to the saved text.
        self.last_edit = EditKind::Other;
        Ok(())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
    }

    /// Marks the current contents as matching the file on disk (after a reload).
    pub fn mark_saved(&mut self) {
        self.saved_state = self.state;
        // Saving ends the undo group, so undo can come back to the saved text.
        self.last_edit = EditKind::Other;
    }

    pub fn is_dirty(&self) -> bool {
        self.state != self.saved_state
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Line contents without the trailing line break.
    pub fn line(&self, line: usize) -> String {
        self.line_cow(line).into_owned()
    }

    /// Lines from `first` on, without their line breaks (walking the rope once, which is much
    /// faster than looking each line up).
    pub fn lines_from(&self, first: usize) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
        let first = first.min(self.rope.len_lines());
        self.rope.lines_at(first).map(|slice| {
            let trim = |s: &str| s.trim_end_matches(['\n', '\r']).len();
            match slice.as_str() {
                Some(s) => std::borrow::Cow::Borrowed(&s[..trim(s)]),
                None => {
                    let mut s = slice.to_string();
                    s.truncate(trim(&s));
                    std::borrow::Cow::Owned(s)
                }
            }
        })
    }

    /// Line `line` without its line break, borrowed from the rope when it lies in one chunk (it
    /// almost always does), so scanning many lines doesn't allocate.
    pub fn line_cow(&self, line: usize) -> std::borrow::Cow<'_, str> {
        if line >= self.rope.len_lines() {
            return std::borrow::Cow::Borrowed("");
        }
        let slice = self.rope.line(line);
        let trim = |s: &str| s.trim_end_matches(['\n', '\r']).len();
        match slice.as_str() {
            Some(s) => std::borrow::Cow::Borrowed(&s[..trim(s)]),
            None => {
                let mut s = slice.to_string();
                s.truncate(trim(&s));
                std::borrow::Cow::Owned(s)
            }
        }
    }

    /// Number of chars on a line, excluding the line break.
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return 0;
        }
        let slice = self.rope.line(line);
        let mut len = slice.len_chars();
        while len > 0 {
            let c = slice.char(len - 1);
            if c == '\n' || c == '\r' { len -= 1 } else { break }
        }
        len
    }

    pub fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.len_lines().saturating_sub(1));
        Pos::new(line, pos.col.min(self.line_len(line)))
    }

    fn to_char(&self, pos: Pos) -> usize {
        let pos = self.clamp(pos);
        self.rope.line_to_char(pos.line) + pos.col
    }

    fn to_pos(&self, idx: usize) -> Pos {
        let idx = idx.min(self.rope.len_chars());
        let line = self.rope.char_to_line(idx);
        Pos::new(line, idx - self.rope.line_to_char(line))
    }

    /// Byte offset and (row, byte column) of a char index.
    fn point(&self, idx: usize) -> (usize, (usize, usize)) {
        let byte = self.rope.char_to_byte(idx);
        let line = self.rope.char_to_line(idx);
        (byte, (line, byte - self.rope.line_to_byte(line)))
    }

    fn remove_chars(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        let (start_byte, start) = self.point(range.start);
        let (old_end_byte, old_end) = self.point(range.end);
        self.rope.remove(range);
        self.record(Change::Edit(TextEdit {
            start_byte,
            old_end_byte,
            new_end_byte: start_byte,
            start,
            old_end,
            new_end: start,
        }));
    }

    fn insert_chars(&mut self, idx: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        let (start_byte, start) = self.point(idx);
        self.rope.insert(idx, text);
        let (new_end_byte, new_end) = self.point(idx + text.chars().count());
        self.record(Change::Edit(TextEdit {
            start_byte,
            old_end_byte: start_byte,
            new_end_byte,
            start,
            old_end: start,
            new_end,
        }));
    }

    fn record(&mut self, change: Change) {
        self.changes.push(change);
        if self.log.len() >= LOG_CAP {
            let drop = LOG_CAP / 2;
            self.log.drain(..drop);
            self.log_base += drop as u64;
        }
        self.log.push(change);
    }

    /// Sequence number of the next change (pass it to `edits_since` later).
    pub fn edit_seq(&self) -> u64 {
        self.log_base + self.log.len() as u64
    }

    /// The changes made since `seq` (from `edit_seq`), or None if they're no longer known.
    pub fn edits_since(&self, seq: u64) -> Option<&[Change]> {
        let i = seq.checked_sub(self.log_base)? as usize;
        self.log.get(i..)
    }

    /// Drains the edits made since the last call.
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn line_to_byte(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() { self.rope.len_bytes() } else { self.rope.line_to_byte(line) }
    }

    /// The chunk containing `byte`, and the chunk's starting byte offset.
    pub fn chunk_at_byte(&self, byte: usize) -> (&str, usize) {
        let (chunk, start, _, _) = self.rope.chunk_at_byte(byte);
        (chunk, start)
    }

    /// Iterates the text of a byte range as string chunks.
    pub fn byte_chunks(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = &str> {
        self.rope.byte_slice(range).chunks()
    }

    pub fn text_in(&self, sel: &Selection) -> String {
        let (a, b) = sel.ordered();
        self.rope.slice(self.to_char(a)..self.to_char(b)).to_string()
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    fn checkpoint(&mut self, kind: EditKind, sel: Selection) {
        self.checkpoint_all(kind, &[sel]);
    }

    fn checkpoint_all(&mut self, kind: EditKind, sels: &[Selection]) {
        // Coalesce consecutive typing/deleting into one undo step.
        if kind == EditKind::Other || kind != self.last_edit {
            self.undo.push(Snapshot { rope: self.rope.clone(), selections: sels.to_vec(), state: self.state });
            if self.undo.len() > 1000 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = kind;
        self.version += 1;
        self.state = self.version;
    }

    /// Ends the current undo group so the next edit starts a new one.
    pub fn break_undo_group(&mut self) {
        self.last_edit = EditKind::Other;
    }

    /// Replaces the selection with `text`, returning the caret after it.
    pub fn insert(&mut self, sel: Selection, text: &str) -> Selection {
        let kind = if text.chars().any(|c| c.is_whitespace()) || !sel.is_empty() {
            EditKind::Other
        } else {
            EditKind::Insert
        };
        self.checkpoint(kind, sel);
        let (a, b) = sel.ordered();
        let start = self.to_char(a);
        let end = self.to_char(b);
        self.remove_chars(start..end);
        self.insert_chars(start, text);
        Selection::caret(self.to_pos(start + text.chars().count()))
    }

    /// Deletes the selection, or the char before the caret if it is empty.
    pub fn backspace(&mut self, sel: Selection) -> Selection {
        if !sel.is_empty() {
            return self.delete_range(sel);
        }
        let idx = self.to_char(sel.head);
        if idx == 0 {
            return sel;
        }
        self.checkpoint(EditKind::Delete, sel);
        self.remove_chars(idx - 1..idx);
        Selection::caret(self.to_pos(idx - 1))
    }

    /// Deletes the selection, or the char after the caret if it is empty.
    pub fn delete_forward(&mut self, sel: Selection) -> Selection {
        if !sel.is_empty() {
            return self.delete_range(sel);
        }
        let idx = self.to_char(sel.head);
        if idx >= self.rope.len_chars() {
            return sel;
        }
        self.checkpoint(EditKind::Delete, sel);
        self.remove_chars(idx..idx + 1);
        Selection::caret(self.to_pos(idx))
    }

    pub fn delete_range(&mut self, sel: Selection) -> Selection {
        let (a, b) = sel.ordered();
        let (start, end) = (self.to_char(a), self.to_char(b));
        if start == end {
            return Selection::caret(a);
        }
        self.checkpoint(EditKind::Other, sel);
        self.remove_chars(start..end);
        Selection::caret(self.to_pos(start))
    }

    /// Replaces a whole line's contents (used by comment toggling / indentation).
    pub fn replace_line(&mut self, line: usize, text: &str, sel: Selection) {
        self.checkpoint(EditKind::Other, sel);
        let start = self.rope.line_to_char(line);
        let end = start + self.line_len(line);
        self.remove_chars(start..end);
        self.insert_chars(start, text);
    }

    /// Applies several non-overlapping edits (replace `from..to` with text) as one undo step,
    /// recording `before` as the cursors to restore on undo. Returns the char index where each
    /// edit's text starts in the new text, in the order given.
    pub fn edit(&mut self, before: &[Selection], edits: &[(Pos, Pos, &str)], kind: EditKind) -> Vec<usize> {
        let ranges: Vec<(usize, usize)> = edits
            .iter()
            .map(|(a, b, _)| {
                let (a, b) = (self.to_char(*a), self.to_char(*b));
                (a.min(b), a.max(b))
            })
            .collect();
        // Where each edit starts once the edits before it have changed the text.
        let starts: Vec<usize> = ranges
            .iter()
            .map(|&(start, _)| {
                let shift: isize = ranges
                    .iter()
                    .zip(edits)
                    .filter(|((s, _), _)| *s < start)
                    .map(|((s, e), (_, _, text))| text.chars().count() as isize - (e - s) as isize)
                    .sum();
                (start as isize + shift) as usize
            })
            .collect();
        if edits.iter().zip(&ranges).all(|((_, _, t), (s, e))| t.is_empty() && s == e) {
            return starts;
        }
        self.checkpoint_all(kind, before);
        self.apply_ranges(&ranges, edits);
        starts
    }

    /// Applies edits that follow from the last one (linked editing's copies of what was
    /// typed) as part of its undo step, so undo removes both. Typing still coalesces after.
    pub fn edit_in_last_step(&mut self, edits: &[(Pos, Pos, &str)]) {
        if self.undo.is_empty() {
            self.edit(&[], edits, EditKind::Other);
            return;
        }
        let ranges: Vec<(usize, usize)> = edits
            .iter()
            .map(|(a, b, _)| {
                let (a, b) = (self.to_char(*a), self.to_char(*b));
                (a.min(b), a.max(b))
            })
            .collect();
        self.redo.clear();
        self.version += 1;
        self.state = self.version;
        self.apply_ranges(&ranges, edits);
    }

    /// Replaces each char range with its edit's text, from the end of the text backwards so
    /// earlier offsets stay valid.
    fn apply_ranges(&mut self, ranges: &[(usize, usize)], edits: &[(Pos, Pos, &str)]) {
        let mut order: Vec<usize> = (0..edits.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(ranges[i].0));
        for i in order {
            let (start, end) = ranges[i];
            self.remove_chars(start..end);
            self.insert_chars(start, edits[i].2);
        }
    }

    /// The byte offset of a position (clamped to the text).
    pub fn byte_of(&self, pos: Pos) -> usize {
        self.rope.char_to_byte(self.to_char(pos))
    }

    /// The position of a byte offset (clamped, rounded down to a char boundary).
    pub fn pos_of_byte(&self, byte: usize) -> Pos {
        self.to_pos(self.rope.byte_to_char(byte.min(self.rope.len_bytes())))
    }

    /// The char index of a position (clamped to the text).
    pub fn char_index(&self, pos: Pos) -> usize {
        self.to_char(pos)
    }

    /// The position of a char index.
    pub fn pos_of(&self, idx: usize) -> Pos {
        self.to_pos(idx)
    }

    /// Undoes the last step, returning the cursors from before it.
    pub fn undo(&mut self, current: &[Selection]) -> Option<Vec<Selection>> {
        let snap = self.undo.pop()?;
        self.redo.push(Snapshot { rope: std::mem::replace(&mut self.rope, snap.rope), selections: current.to_vec(), state: self.state });
        self.state = snap.state;
        self.record(Change::Reset);
        self.last_edit = EditKind::Other;
        self.version += 1;
        Some(snap.selections)
    }

    pub fn redo(&mut self, current: &[Selection]) -> Option<Vec<Selection>> {
        let snap = self.redo.pop()?;
        self.undo.push(Snapshot { rope: std::mem::replace(&mut self.rope, snap.rope), selections: current.to_vec(), state: self.state });
        self.state = snap.state;
        self.record(Change::Reset);
        self.last_edit = EditKind::Other;
        self.version += 1;
        Some(snap.selections)
    }

    // --- Cursor movement helpers -------------------------------------------------

    pub fn left(&self, pos: Pos) -> Pos {
        let idx = self.to_char(pos);
        self.to_pos(idx.saturating_sub(1))
    }

    pub fn right(&self, pos: Pos) -> Pos {
        let idx = self.to_char(pos);
        self.to_pos((idx + 1).min(self.rope.len_chars()))
    }

    pub fn word_left(&self, pos: Pos) -> Pos {
        let mut idx = self.to_char(pos);
        while idx > 0 && self.rope.char(idx - 1).is_whitespace() {
            idx -= 1;
        }
        let word = idx > 0 && is_word(self.rope.char(idx - 1));
        while idx > 0 {
            let c = self.rope.char(idx - 1);
            if c.is_whitespace() || is_word(c) != word {
                break;
            }
            idx -= 1;
        }
        self.to_pos(idx)
    }

    pub fn word_right(&self, pos: Pos) -> Pos {
        let len = self.rope.len_chars();
        let mut idx = self.to_char(pos);
        while idx < len && self.rope.char(idx).is_whitespace() {
            idx += 1;
        }
        let word = idx < len && is_word(self.rope.char(idx));
        while idx < len {
            let c = self.rope.char(idx);
            if c.is_whitespace() || is_word(c) != word {
                break;
            }
            idx += 1;
        }
        self.to_pos(idx)
    }

    /// Selects the word under `pos` (double-click behavior).
    pub fn word_at(&self, pos: Pos) -> Selection {
        let line = self.line(pos.line);
        let chars: Vec<char> = line.chars().collect();
        let col = pos.col.min(chars.len());
        let probe = if col < chars.len() { col } else { col.saturating_sub(1) };
        if chars.is_empty() || !is_word(chars[probe]) {
            return Selection::caret(pos);
        }
        let mut start = probe;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        let mut end = probe;
        while end < chars.len() && is_word(chars[end]) {
            end += 1;
        }
        Selection { anchor: Pos::new(pos.line, start), head: Pos::new(pos.line, end), goal_col: None }
    }

    pub fn first_non_blank(&self, line: usize) -> usize {
        self.line(line).chars().take_while(|c| *c == ' ' || *c == '\t').count()
    }

    pub fn leading_whitespace(&self, line: usize) -> String {
        self.line(line).chars().take_while(|c| *c == ' ' || *c == '\t').collect()
    }

    pub fn end(&self) -> Pos {
        self.to_pos(self.rope.len_chars())
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_in_the_last_step_undo_with_it() {
        let mut b = Buffer::new();
        b.insert(Selection::default(), "<a></a>");
        b.break_undo_group();
        b.insert(Selection::caret(Pos::new(0, 2)), "b");
        b.edit_in_last_step(&[(Pos::new(0, 7), Pos::new(0, 7), "b")]);
        b.insert(Selection::caret(Pos::new(0, 3)), "c");
        b.edit_in_last_step(&[(Pos::new(0, 9), Pos::new(0, 9), "c")]);
        assert_eq!(b.text(), "<abc></abc>");
        // One step: the typing and its copies.
        b.undo(&[]);
        assert_eq!(b.text(), "<a></a>");
        b.redo(&[]);
        assert_eq!(b.text(), "<abc></abc>");
    }

    #[test]
    fn insert_and_undo() {
        let mut b = Buffer::new();
        let mut sel = Selection::default();
        for c in "hello".chars() {
            sel = b.insert(sel, &c.to_string());
        }
        sel = b.insert(sel, "\n");
        sel = b.insert(sel, "x");
        assert_eq!(b.text(), "hello\nx");
        assert_eq!(sel.head, Pos::new(1, 1));
        let sels = b.undo(&[sel]).unwrap();
        assert_eq!(b.text(), "hello\n");
        b.undo(&sels).unwrap();
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn undo_back_to_saved_is_clean() {
        let mut b = Buffer::new();
        let sel = b.insert(Selection::default(), "a");
        b.mark_saved();
        assert!(!b.is_dirty());
        let sel2 = b.insert(sel, "b");
        assert!(b.is_dirty());
        let sels = b.undo(&[sel2]).unwrap();
        assert!(!b.is_dirty());
        b.redo(&sels).unwrap();
        assert!(b.is_dirty());
        b.undo(&[sel2]).unwrap();
        assert!(!b.is_dirty());
        // Undoing past the save is dirty too.
        b.undo(&[sel]).unwrap();
        assert!(b.is_dirty());
        // A new edit after undoing never matches the saved text's id.
        b.redo(&[]).unwrap();
        b.insert(sel, "c");
        b.undo(&[]).unwrap();
        assert!(!b.is_dirty());
    }

    #[test]
    fn multi_edit_is_one_undo_step() {
        let mut b = Buffer::new();
        b.insert(Selection::default(), "ab\ncd\nef");
        b.break_undo_group();
        let before = [Selection::caret(Pos::new(0, 1)), Selection::caret(Pos::new(2, 0))];
        // Replace "b" with "XY", insert "!" at the start of line 2, and delete "c".
        let starts = b.edit(
            &before,
            &[(Pos::new(2, 0), Pos::new(2, 0), "!"), (Pos::new(0, 1), Pos::new(0, 2), "XY"), (Pos::new(1, 0), Pos::new(1, 1), "")],
            EditKind::Other,
        );
        assert_eq!(b.text(), "aXY\nd\n!ef");
        // Starts are in the new text: "!" moved by +1 (XY for b) and -1 (c deleted).
        assert_eq!(starts, vec![6, 1, 4]);
        assert_eq!(b.pos_of(starts[0]), Pos::new(2, 0));
        assert_eq!(b.undo(&[]).unwrap(), before.to_vec());
        assert_eq!(b.text(), "ab\ncd\nef");
    }

    #[test]
    fn records_byte_edits() {
        let mut b = Buffer::new();
        let sel = b.insert(Selection::default(), "héllo\nx");
        b.take_changes();
        b.backspace(sel);
        let changes = b.take_changes();
        // "é" is two bytes, so "x" starts at byte 7 on row 1, column 0.
        assert_eq!(
            changes,
            vec![Change::Edit(TextEdit {
                start_byte: 7,
                old_end_byte: 8,
                new_end_byte: 7,
                start: (1, 0),
                old_end: (1, 1),
                new_end: (1, 0),
            })]
        );
    }

    #[test]
    fn word_motion() {
        let mut b = Buffer::new();
        b.insert(Selection::default(), "let foo_bar = 1;");
        assert_eq!(b.word_right(Pos::new(0, 0)), Pos::new(0, 3));
        assert_eq!(b.word_right(Pos::new(0, 3)), Pos::new(0, 11));
        assert_eq!(b.word_left(Pos::new(0, 11)), Pos::new(0, 4));
    }
}
