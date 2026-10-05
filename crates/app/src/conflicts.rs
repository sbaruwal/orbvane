//! Merge conflicts in the editor: finding `<<<<<<<`/`=======`/`>>>>>>>` blocks, and resolving
//! them (Accept Current / Incoming / Both Changes), like the standard merge-conflict extension.

use text::{EditKind, Pos, Selection};

use crate::editor::{Doc, EditorState};

/// One conflict block, as line numbers of its markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The `<<<<<<<` line.
    pub start: usize,
    /// The `|||||||` line (diff3 style, with the common ancestor).
    pub base: Option<usize>,
    /// The `=======` line.
    pub split: usize,
    /// The `>>>>>>>` line.
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Current,
    Incoming,
    Both,
}

impl Conflict {
    pub fn current(&self) -> std::ops::Range<usize> {
        self.start + 1..self.base.unwrap_or(self.split)
    }

    pub fn incoming(&self) -> std::ops::Range<usize> {
        self.split + 1..self.end
    }
}

fn is_marker(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// Finds the conflict blocks in `lines` (each without its line break). Incomplete blocks are
/// ignored.
pub fn find<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Conflict> {
    let mut out = Vec::new();
    let mut open: Option<(usize, Option<usize>, Option<usize>)> = None;
    for (i, line) in lines.into_iter().enumerate() {
        if is_marker(line, "<<<<<<<") {
            open = Some((i, None, None));
        } else if let Some((start, base, split)) = &mut open {
            if is_marker(line, "|||||||") && split.is_none() {
                *base = Some(i);
            } else if is_marker(line, "=======") && split.is_none() {
                *split = Some(i);
            } else if is_marker(line, ">>>>>>>") {
                if let Some(split) = *split {
                    out.push(Conflict { start: *start, base: *base, split, end: i });
                }
                open = None;
            }
        }
    }
    out
}

/// Whether a file's text still contains conflict markers.
pub fn has_conflicts(text: &str) -> bool {
    !find(text.lines()).is_empty()
}

/// Conflicts of a document, cached per buffer version.
#[derive(Default)]
pub struct ConflictCache {
    version: Option<u64>,
    pub conflicts: Vec<Conflict>,
}

impl ConflictCache {
    pub fn update(&mut self, doc: &Doc) -> &[Conflict] {
        let version = doc.buffer.version();
        if self.version != Some(version) {
            self.version = Some(version);
            let text = doc.buffer.text();
            // Cheap check first: most files have no markers at all.
            self.conflicts = if text.contains("<<<<<<<") { find(text.lines()) } else { Vec::new() };
        }
        &self.conflicts
    }
}

impl EditorState {
    /// Replaces a conflict block with the chosen side(s), as one undo step.
    pub fn resolve_conflict(&mut self, doc: &mut Doc, conflict: Conflict, how: Resolution) {
        let b = &doc.buffer;
        let lines = |r: std::ops::Range<usize>| r.map(|l| format!("{}\n", b.line(l))).collect::<String>();
        let text = match how {
            Resolution::Current => lines(conflict.current()),
            Resolution::Incoming => lines(conflict.incoming()),
            Resolution::Both => lines(conflict.current()) + &lines(conflict.incoming()),
        };
        let start = Pos::new(conflict.start, 0);
        let end = if conflict.end + 1 < b.len_lines() { Pos::new(conflict.end + 1, 0) } else { b.end() };
        // At the end of a file without a final newline, don't add one.
        let text = if end == b.end() && !b.text().ends_with('\n') { text.trim_end_matches('\n').to_string() } else { text };
        let before = self.selections();
        doc.buffer.edit(&before, &[(start, end, &text)], EditKind::Other);
        doc.buffer.break_undo_group();
        self.set_selection(Selection::caret(start));
        self.reveal = true;
    }

    /// Resolves every conflict in the document the same way (one undo step each, last first).
    pub fn resolve_all_conflicts(&mut self, doc: &mut Doc, how: Resolution) -> usize {
        let conflicts = find(doc.buffer.text().lines());
        for c in conflicts.iter().rev() {
            self.resolve_conflict(doc, *c, how);
        }
        conflicts.len()
    }

    /// Moves the caret to the next (or previous) conflict, wrapping around.
    pub fn go_to_conflict(&mut self, doc: &Doc, forward: bool) -> bool {
        let conflicts = find(doc.buffer.text().lines());
        let line = self.sel.head.line;
        let pick = if forward {
            conflicts.iter().find(|c| c.start > line).or(conflicts.first())
        } else {
            conflicts.iter().rev().find(|c| c.start < line).or(conflicts.last())
        };
        let Some(c) = pick else { return false };
        self.jump_to(doc, Pos::new(c.start, 0));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "a\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> topic\nb\n<<<<<<< HEAD\nx\n||||||| base\nw\n=======\ny\n>>>>>>> topic\n";

    #[test]
    fn finds_conflicts() {
        let found = find(TEXT.lines());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], Conflict { start: 1, base: None, split: 3, end: 5 });
        assert_eq!(found[1], Conflict { start: 7, base: Some(9), split: 11, end: 13 });
        assert_eq!(found[1].current(), 8..9);
        assert_eq!(found[1].incoming(), 12..13);
        // Not markers: longer runs, or text right after the marker.
        assert!(find("<<<<<<<<\n=======\n>>>>>>>\n".lines()).is_empty());
        assert!(!has_conflicts("<<<<<<<x\n=======\n>>>>>>> y\n"));
    }

    #[test]
    fn resolves_conflicts() {
        let mut doc = Doc::open_virtual("a.txt", TEXT);
        let mut ed = EditorState::new(0);
        let first = find(TEXT.lines())[0];
        ed.resolve_conflict(&mut doc, first, Resolution::Incoming);
        assert!(doc.buffer.text().starts_with("a\ntheirs\nb\n<<<<<<<"));
        assert_eq!(ed.resolve_all_conflicts(&mut doc, Resolution::Both), 1);
        assert_eq!(doc.buffer.text(), "a\ntheirs\nb\nx\ny\n");
        // Each resolution is its own undo step.
        doc.buffer.undo(&ed.selections());
        assert!(doc.buffer.text().contains("||||||| base"));
    }
}
