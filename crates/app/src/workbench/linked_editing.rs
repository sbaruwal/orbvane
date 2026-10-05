//! Linked editing (`editor.linkedEditing`, and Start Linked Editing, ⇧⌘F2): ranges
//! that change together, like an element's opening and closing tag names. When the cursor
//! rests in one, the language server (`textDocument/linkedEditingRange`) or, for JSX, the
//! syntax tree gives the ranges. They're highlighted, and what is typed in the one with the
//! cursor is copied to the others as part of the same undo step. Leaving them, Escape, or
//! typing something the ranges can't hold (the word pattern, e.g. a space) ends it.

use std::time::{Duration, Instant};

use lsp::Encoding;
use regex::Regex;
use text::{Change, Pos, Selection};

use super::Workbench;
use crate::config;

/// How long the cursor rests before asking.
const DELAY: Duration = Duration::from_millis(100);

/// What a JSX tag name may hold (typescript-language-server's pattern).
const TAG_PATTERN: &str = r"[a-zA-Z0-9:\-._$]*";

/// The default word definition, for servers that don't send a pattern.
const WORD_PATTERN: &str = r#"(-?\d*\.\d\w*)|([^`~!@#$%^&*()\-=+\[{\]}\\|;:'",.<>/?\s]+)"#;

#[derive(Default)]
pub(super) struct LinkedEditing {
    /// Where the ranges were last asked for: (document, buffer version, cursor).
    asked: Option<(usize, u64, Pos)>,
    seq: u64,
    due: Option<Instant>,
    /// Start Linked Editing was run: on until the ranges end, whatever the setting.
    forced: bool,
    active: Option<Active>,
}

struct Active {
    doc: usize,
    /// Byte ranges; the first has the cursor, and the others copy its text.
    ranges: Vec<(usize, usize)>,
    pattern: Option<Regex>,
    /// The buffer's `edit_seq` the ranges are up to date with.
    seq: u64,
}

/// Moves byte range `r` through an edit that replaced `start..old_end` with text ending at
/// `new_end`. Typing at either edge grows the range.
/// False if the edit cuts across one of its edges.
fn map_range(r: &mut (usize, usize), start: usize, old_end: usize, new_end: usize) -> bool {
    let (a, b) = *r;
    let moved = |p: usize| (p + new_end).saturating_sub(old_end);
    if start >= a && old_end <= b {
        *r = (a, moved(b));
    } else if old_end <= a {
        *r = (moved(a), moved(b));
    } else if start < b {
        return false;
    }
    true
}

/// Whether `text` is all one match of `pattern`, as we check the reference range.
fn fits(pattern: &Option<Regex>, text: &str) -> bool {
    let Some(p) = pattern else { return true };
    text.is_empty() || p.find(text).is_some_and(|m| m.start() == 0 && m.end() == text.len())
}

impl Workbench {
    /// The active editor's cursor, where linked ranges can be: (document, version, cursor).
    fn linked_spot(&self) -> Option<(usize, u64, Pos)> {
        let ed = self.active_editor().filter(|e| !e.is_special() && e.merge.is_none() && e.extra.is_empty())?;
        let doc = self.docs[ed.doc].as_ref().filter(|d| !d.large)?;
        Some((ed.doc, doc.buffer.version(), ed.sel.head))
    }

    fn linked_clear(&mut self) {
        self.linked.active = None;
        self.linked.forced = false;
    }

    /// Copies edits to the ranges, follows the cursor and asks for ranges once it rests.
    /// Called every frame, before drawing.
    pub(super) fn linked_editing_tick(&mut self) {
        self.linked_sync();
        let enabled = config::get().linked_editing || self.linked.forced;
        let spot = self.linked_spot().filter(|_| enabled);
        // Leaving the reference range (or the editor) ends it.
        if let Some(a) = &self.linked.active {
            let inside = spot.is_some_and(|(doc, _, pos)| {
                let ed = self.active_editor();
                let b = &self.docs[doc].as_ref().unwrap().buffer;
                let (ra, rb) = a.ranges[0];
                let within = |p: Pos| (ra..=rb).contains(&b.byte_of(p));
                doc == a.doc && within(pos) && ed.is_some_and(|e| within(e.sel.anchor))
            });
            if !inside {
                self.linked_clear();
            }
        }
        if spot.is_none() {
            (self.linked.asked, self.linked.due) = (None, None);
            return;
        }
        if spot != self.linked.asked {
            self.linked.asked = spot;
            self.linked.seq += 1;
            self.linked.due = Some(Instant::now() + DELAY);
        }
        if self.linked.due.is_some_and(|t| Instant::now() >= t) {
            self.linked.due = None;
            self.linked_request();
        }
    }

    /// Asks the language server for the ranges at the cursor, or finds them in the syntax tree.
    fn linked_request(&mut self) {
        let Some((id, _, pos)) = self.linked.asked else { return };
        let seq = self.linked.seq;
        let Some(doc) = self.docs[id].as_mut() else { return };
        if let Some(path) = doc.buffer.path().map(std::path::Path::to_path_buf) {
            if self.lsp.linked_editing_ranges(&path, &doc.buffer, pos, seq) {
                return;
            }
        }
        doc.highlight.update(&mut doc.buffer);
        let head = doc.buffer.byte_of(pos);
        match doc.highlight.linked_tags(head) {
            Some(tags) => self.linked_set(id, head, tags.to_vec(), Some(TAG_PATTERN)),
            None if !self.linked.forced => self.linked.active = None,
            None => self.linked_clear(),
        }
    }

    /// The server's ranges for request `seq`.
    pub(super) fn linked_editing_arrived(&mut self, seq: u64, ranges: Option<Vec<lsp::Range>>, word_pattern: Option<String>, encoding: Encoding) {
        let Some((id, version, pos)) = self.linked.asked.filter(|_| seq == self.linked.seq) else { return };
        let Some(doc) = self.docs[id].as_ref().filter(|d| d.buffer.version() == version) else { return };
        let b = &doc.buffer;
        let byte = |p: lsp::Position| {
            let line = (p.line as usize).min(b.len_lines().saturating_sub(1));
            b.byte_of(Pos::new(line, encoding.from_lsp(&b.line(line), p.character)))
        };
        let ranges: Vec<(usize, usize)> = ranges.unwrap_or_default().iter().map(|r| (byte(r.start), byte(r.end))).collect();
        let head = b.byte_of(pos);
        self.linked_set(id, head, ranges, Some(word_pattern.as_deref().unwrap_or(WORD_PATTERN)));
    }

    /// Starts linked editing on `ranges` if one of them has the cursor (byte `head`).
    fn linked_set(&mut self, doc: usize, head: usize, mut ranges: Vec<(usize, usize)>, pattern: Option<&str>) {
        let Some(i) = ranges.iter().position(|&(a, b)| a <= head && head <= b).filter(|_| ranges.len() > 1) else {
            return self.linked_clear();
        };
        let reference = ranges.remove(i);
        ranges.insert(0, reference);
        // Anchored like the standard check: the match has to start at the range's start.
        let pattern = pattern.and_then(|p| Regex::new(&format!("^(?:{p})")).ok());
        let seq = self.docs[doc].as_ref().map_or(0, |d| d.buffer.edit_seq());
        self.linked.active = Some(Active { doc, ranges, pattern, seq });
    }

    /// Follows edits made since the last frame: edits in the reference range are copied to the
    /// others; edits elsewhere move the ranges and ask again.
    fn linked_sync(&mut self) {
        let Some(active) = &self.linked.active else { return };
        let id = active.doc;
        let Some(buffer) = self.docs.get(id).and_then(|d| d.as_ref()).map(|d| &d.buffer) else { return self.linked_clear() };
        // Undo and redo replace the text: start over.
        let Some(changes) = buffer.edits_since(active.seq) else { return self.linked_clear() };
        if changes.is_empty() {
            return;
        }
        let mut ranges = active.ranges.clone();
        let mut in_reference = true;
        for change in changes {
            let Change::Edit(e) = change else { return self.linked_clear() };
            let (ra, rb) = ranges[0];
            in_reference &= e.start_byte <= rb && e.old_end_byte >= ra;
            if !ranges.iter_mut().all(|r| map_range(r, e.start_byte, e.old_end_byte, e.new_end_byte)) {
                return self.linked_clear();
            }
        }
        let seq = buffer.edit_seq();
        if !in_reference {
            let a = self.linked.active.as_mut().unwrap();
            (a.ranges, a.seq) = (ranges, seq);
            self.linked.asked = None;
            return;
        }
        let (ra, rb) = ranges[0];
        let text_of = |a: usize, b: usize| buffer.text_in(&Selection { anchor: buffer.pos_of_byte(a), head: buffer.pos_of_byte(b), goal_col: None });
        if buffer.pos_of_byte(ra).line != buffer.pos_of_byte(rb).line {
            return self.linked_clear();
        }
        let text = text_of(ra, rb);
        if !fits(&active.pattern, &text) {
            return self.linked_clear();
        }
        // Each copy replaces only what differs (the text between the common prefix and suffix).
        let mut edits = Vec::new();
        for &(a, b) in &ranges[1..] {
            let old = text_of(a, b);
            let prefix = old.bytes().zip(text.bytes()).take_while(|(x, y)| x == y).count();
            let prefix = (0..=prefix).rev().find(|&n| old.is_char_boundary(n) && text.is_char_boundary(n)).unwrap_or(0);
            let (old_rest, new_rest) = (&old[prefix..], &text[prefix..]);
            let suffix = old_rest.bytes().rev().zip(new_rest.bytes().rev()).take_while(|(x, y)| x == y).count();
            let suffix = (0..=suffix)
                .rev()
                .find(|&n| old_rest.is_char_boundary(old_rest.len() - n) && new_rest.is_char_boundary(new_rest.len() - n))
                .unwrap_or(0);
            let (from, to) = (a + prefix, b - suffix);
            let new = &new_rest[..new_rest.len() - suffix];
            if from != to || !new.is_empty() {
                edits.push((buffer.pos_of_byte(from), buffer.pos_of_byte(to), new.to_string()));
            }
        }
        if !edits.is_empty() {
            self.edit_doc(id, edits, true);
            let Some(buffer) = self.docs[id].as_ref().map(|d| &d.buffer) else { return };
            for change in buffer.edits_since(seq).unwrap_or_default() {
                if let Change::Edit(e) = change {
                    for r in &mut ranges {
                        map_range(r, e.start_byte, e.old_end_byte, e.new_end_byte);
                    }
                }
            }
        }
        let seq = self.docs[id].as_ref().map_or(seq, |d| d.buffer.edit_seq());
        let a = self.linked.active.as_mut().unwrap();
        (a.ranges, a.seq) = (ranges, seq);
    }

    /// Start Linked Editing (⇧⌘F2): linked editing at the cursor, even with the setting off.
    pub(super) fn start_linked_editing(&mut self) {
        self.linked.forced = true;
        self.linked.asked = None;
        self.linked_editing_tick();
        self.linked.due = Some(Instant::now());
        self.linked_editing_tick();
    }

    /// Escape in the editor ends linked editing. True if it was on.
    pub(super) fn linked_escape(&mut self) -> bool {
        let on = self.linked.active.is_some();
        self.linked_clear();
        on
    }

    pub(super) fn linked_deadline(&self) -> Option<Instant> {
        self.linked.due
    }

    /// The linked ranges to highlight in group `g`'s editor.
    pub(super) fn linked_highlights(&self, g: usize) -> Vec<(Pos, Pos)> {
        let Some(a) = &self.linked.active else { return Vec::new() };
        let gr = &self.groups[g];
        if g != self.active_group || gr.tabs.get(gr.active).is_none_or(|e| e.doc != a.doc) {
            return Vec::new();
        }
        let Some(b) = self.docs[a.doc].as_ref().map(|d| &d.buffer) else { return Vec::new() };
        a.ranges.iter().map(|&(x, y)| (b.pos_of_byte(x), b.pos_of_byte(y))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_follow_edits() {
        // Typing inside or at either edge grows the range; edits before move it.
        let mut r = (5, 8);
        assert!(map_range(&mut r, 8, 8, 9));
        assert_eq!(r, (5, 9));
        assert!(map_range(&mut r, 5, 5, 6));
        assert_eq!(r, (5, 10));
        assert!(map_range(&mut r, 0, 2, 0));
        assert_eq!(r, (3, 8));
        assert!(map_range(&mut r, 3, 8, 3));
        assert_eq!(r, (3, 3));
        // After it: unchanged. Across an edge: no longer valid.
        assert!(map_range(&mut r, 9, 12, 10));
        assert_eq!(r, (3, 3));
        let mut r = (5, 8);
        assert!(!map_range(&mut r, 6, 10, 6));
        assert!(!map_range(&mut r, 2, 6, 2));
    }

    #[test]
    fn word_pattern_must_cover_the_text() {
        let p = Some(Regex::new(&format!("^(?:{TAG_PATTERN})")).unwrap());
        assert!(fits(&p, "Foo.Bar"));
        assert!(fits(&p, ""));
        assert!(!fits(&p, "div "));
        assert!(fits(&None, "any thing"));
    }
}
