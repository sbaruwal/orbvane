//! Code folding: regions from the language server or, without one, the indentation strategy: a region starts at a line
//! whose next lines are indented deeper and runs until the indentation comes back (so a
//! closing `}` stays visible). Collapsed regions hide their lines through the editor's
//! `Layout`, follow edits, and open again when the cursor lands inside.

use std::ops::RangeInclusive;
use std::sync::Arc;

use text::{Buffer, Change, Pos};

use crate::editor::tab_size;

/// A foldable region: line `start` stays visible, lines `start + 1..=end` can be hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRange {
    pub start: usize,
    pub end: usize,
}

impl FoldRange {
    pub fn hidden(&self) -> RangeInclusive<usize> {
        self.start + 1..=self.end
    }

    fn contains_line(&self, line: usize) -> bool {
        self.start <= line && line <= self.end
    }
}

fn indent_of(text: &str) -> Option<usize> {
    if text.trim().is_empty() {
        return None;
    }
    let mut d = 0;
    for c in text.chars() {
        match c {
            ' ' => d += 1,
            '\t' => d += tab_size() - d % tab_size(),
            _ => break,
        }
    }
    Some(d)
}

/// Indentation-based fold ranges, sorted by start. Blank lines belong to the block around them.
pub fn indent_ranges(b: &Buffer) -> Vec<FoldRange> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (indent, header line)
    let mut last_nonblank: Option<usize> = None;
    let close = |stack: &mut Vec<(usize, usize)>, until: Option<usize>, last: Option<usize>, out: &mut Vec<FoldRange>| {
        while let Some(&(k, h)) = stack.last() {
            if until.is_some_and(|ind| ind > k) {
                break;
            }
            stack.pop();
            if let Some(end) = last.filter(|&l| l > h) {
                out.push(FoldRange { start: h, end });
            }
        }
    };
    for line in 0..b.len_lines() {
        let Some(ind) = indent_of(&b.line(line)) else { continue };
        close(&mut stack, Some(ind), last_nonblank, &mut out);
        stack.push((ind, line));
        last_nonblank = Some(line);
    }
    close(&mut stack, None, last_nonblank, &mut out);
    out.sort_by_key(|r| r.start);
    out
}

/// A document's regions from its language server, with a generation that changes when they do.
#[derive(Clone, Default)]
pub struct ServerRanges {
    pub ranges: Option<Arc<Vec<FoldRange>>>,
    pub generation: u64,
}

impl ServerRanges {
    /// No regions at all (large files, where folding is off).
    pub const NONE: ServerRanges = ServerRanges { ranges: None, generation: u64::MAX };
}

/// Fold state of one editor.
#[derive(Default)]
pub struct Folds {
    /// Header lines of collapsed regions, sorted.
    collapsed: Vec<usize>,
    /// The buffer version and edit sequence the state matches.
    version: Option<u64>,
    seq: u64,
    /// Fold ranges for `ranges_key` (buffer version, server ranges generation).
    ranges: Vec<FoldRange>,
    ranges_key: Option<(u64, u64)>,
    /// The language server's regions, used instead of indentation when present.
    server: ServerRanges,
    /// Bumped when the hidden lines change (the layout rebuilds).
    generation: u64,
}

impl Folds {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_collapsed(&self, start: usize) -> bool {
        self.collapsed.binary_search(&start).is_ok()
    }

    pub fn collapsed(&self) -> &[usize] {
        &self.collapsed
    }

    /// Uses the language server's regions (None: indentation).
    pub fn set_server_ranges(&mut self, server: &ServerRanges) {
        if server.generation != self.server.generation {
            self.server = server.clone();
        }
    }

    /// The fold ranges for the buffer's current text.
    pub fn ranges(&mut self, b: &Buffer) -> &[FoldRange] {
        let key = (b.version(), self.server.generation);
        if self.ranges_key != Some(key) {
            self.ranges = match &self.server.ranges {
                Some(r) => r.iter().copied().filter(|r| r.end > r.start && r.end < b.len_lines()).collect(),
                None if self.server.generation == u64::MAX => Vec::new(),
                None => indent_ranges(b),
            };
            self.ranges_key = Some(key);
        }
        &self.ranges
    }

    fn range_at(&mut self, b: &Buffer, start: usize) -> Option<FoldRange> {
        let ranges = self.ranges(b);
        ranges.binary_search_by_key(&start, |r| r.start).ok().map(|i| ranges[i])
    }

    /// Moves collapsed regions along with edits, and drops ones that no longer exist.
    pub fn sync(&mut self, b: &Buffer) {
        if self.version == Some(b.version()) {
            return;
        }
        let first_sync = self.version.is_none();
        self.version = Some(b.version());
        let seq = std::mem::replace(&mut self.seq, b.edit_seq());
        if self.collapsed.is_empty() || first_sync {
            return;
        }
        if let Some(edits) = b.edits_since(seq) {
            for change in edits {
                let Change::Edit(e) = change else { continue };
                let (start, old_end, new_end) = (e.start.0, e.old_end.0, e.new_end.0);
                let delta = new_end as isize - old_end as isize;
                self.collapsed.retain_mut(|s| {
                    if *s > old_end {
                        *s = (*s as isize + delta) as usize;
                        true
                    } else {
                        // The header itself was edited in place: keep; removed/merged: drop.
                        *s <= start
                    }
                });
            }
        }
        // Keep only headers that still start a region.
        let ranges = self.ranges(b).to_vec();
        self.collapsed.retain(|s| ranges.binary_search_by_key(s, |r| r.start).is_ok());
        self.collapsed.dedup();
        self.generation += 1;
    }

    /// Lines hidden by collapsed regions, as sorted, non-overlapping inclusive ranges.
    pub fn hidden(&mut self, b: &Buffer) -> Vec<RangeInclusive<usize>> {
        let starts = self.collapsed.clone();
        let mut out: Vec<RangeInclusive<usize>> = Vec::new();
        for s in starts {
            let Some(r) = self.range_at(b, s) else { continue };
            match out.last_mut() {
                // Nested inside (or overlapping) a region already hidden.
                Some(last) if *last.end() >= r.start => {
                    let end = (*last.end()).max(r.end);
                    *last = *last.start()..=end;
                }
                _ => out.push(r.hidden()),
            }
        }
        out
    }

    fn set_collapsed(&mut self, mut starts: Vec<usize>) {
        starts.sort_unstable();
        starts.dedup();
        if starts != self.collapsed {
            self.collapsed = starts;
            self.generation += 1;
        }
    }

    /// Opens regions that hide any of `positions` (a cursor landed inside).
    pub fn reveal(&mut self, b: &Buffer, positions: &[Pos]) {
        if self.collapsed.is_empty() {
            return;
        }
        let mut keep = self.collapsed.clone();
        for &s in &self.collapsed.clone() {
            if let Some(r) = self.range_at(b, s) {
                if positions.iter().any(|p| r.hidden().contains(&p.line)) {
                    keep.retain(|k| *k != s);
                }
            }
        }
        self.set_collapsed(keep);
    }

    /// The innermost region containing `line`, preferring one that starts on it.
    fn innermost(&mut self, b: &Buffer, line: usize, collapsed: Option<bool>) -> Option<FoldRange> {
        let collapsed_starts = self.collapsed.clone();
        let is_collapsed = |r: &FoldRange| collapsed_starts.binary_search(&r.start).is_ok();
        let ranges = self.ranges(b);
        let fits = |r: &&FoldRange| r.contains_line(line) && collapsed.is_none_or(|c| is_collapsed(r) == c);
        ranges.iter().filter(fits).filter(|r| r.start == line).last().or_else(|| ranges.iter().filter(fits).last()).copied()
    }

    /// Fold (⌥⌘[): collapses the innermost open region at `line`.
    pub fn fold(&mut self, b: &Buffer, line: usize) {
        if let Some(r) = self.innermost(b, line, Some(false)) {
            let mut c = self.collapsed.clone();
            c.push(r.start);
            self.set_collapsed(c);
        }
    }

    /// Unfold (⌥⌘]): opens the collapsed region at `line`.
    pub fn unfold(&mut self, b: &Buffer, line: usize) {
        if let Some(r) = self.innermost(b, line, Some(true)) {
            let c = self.collapsed.iter().copied().filter(|s| *s != r.start).collect();
            self.set_collapsed(c);
        }
    }

    pub fn toggle(&mut self, b: &Buffer, line: usize) {
        match self.innermost(b, line, None) {
            Some(r) if self.is_collapsed(r.start) => self.unfold(b, r.start),
            Some(r) => self.fold(b, r.start),
            None => {}
        }
    }

    /// Toggles the region starting at `start` (a click on its gutter chevron).
    pub fn toggle_at(&mut self, start: usize) {
        let mut c = self.collapsed.clone();
        match c.binary_search(&start) {
            Ok(i) => {
                c.remove(i);
            }
            Err(_) => c.push(start),
        }
        self.set_collapsed(c);
    }

    /// Folds (or unfolds) the region at `line` and every region inside it.
    pub fn set_recursive(&mut self, b: &Buffer, line: usize, fold: bool) {
        let Some(outer) = self.innermost(b, line, None) else { return };
        let inner: Vec<usize> = self.ranges(b).iter().filter(|r| r.start >= outer.start && r.end <= outer.end).map(|r| r.start).collect();
        let mut c: Vec<usize> = self.collapsed.iter().copied().filter(|s| !inner.contains(s)).collect();
        if fold {
            c.extend(inner);
        }
        self.set_collapsed(c);
    }

    pub fn fold_all(&mut self, b: &Buffer) {
        let all = self.ranges(b).iter().map(|r| r.start).collect();
        self.set_collapsed(all);
    }

    pub fn unfold_all(&mut self) {
        self.set_collapsed(Vec::new());
    }

    /// Restores collapsed regions (session restore); ones that don't exist are dropped on sync.
    pub fn restore(&mut self, b: &Buffer, starts: Vec<usize>) {
        let valid: Vec<usize> = starts.into_iter().filter(|s| self.ranges(b).iter().any(|r| r.start == *s)).collect();
        self.version = Some(b.version());
        self.seq = b.edit_seq();
        self.set_collapsed(valid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Selection;

    fn buffer(text: &str) -> Buffer {
        let mut b = Buffer::new();
        b.insert(Selection::default(), text);
        b
    }

    const CODE: &str = "fn a() {\n    let x = 1;\n    if x {\n        go();\n\n        stop();\n    }\n}\nfn b() {}\n";

    #[test]
    fn indentation_ranges() {
        let r = indent_ranges(&buffer(CODE));
        assert_eq!(r, vec![FoldRange { start: 0, end: 6 }, FoldRange { start: 2, end: 5 }]);
    }

    #[test]
    fn fold_hide_and_follow_edits() {
        let mut b = buffer(CODE);
        let mut f = Folds::default();
        f.sync(&b);
        f.fold(&b, 3); // inside `if x {` → folds it
        assert_eq!(f.collapsed(), &[2]);
        assert_eq!(f.hidden(&b), vec![3..=5]);
        f.fold(&b, 1);
        assert_eq!(f.collapsed(), &[0, 2]);
        assert_eq!(f.hidden(&b), vec![1..=6]);
        f.unfold(&b, 0);
        assert_eq!(f.collapsed(), &[2]);

        // Two lines inserted above move the fold down.
        b.insert(Selection::caret(Pos::new(0, 0)), "// one\n// two\n");
        f.sync(&b);
        assert_eq!(f.collapsed(), &[4]);
        assert_eq!(f.hidden(&b), vec![5..=7]);

        // A cursor landing inside opens it.
        f.reveal(&b, &[Pos::new(6, 3)]);
        assert!(f.collapsed().is_empty());
    }

    #[test]
    fn fold_all_and_recursive() {
        let b = buffer(CODE);
        let mut f = Folds::default();
        f.fold_all(&b);
        assert_eq!(f.collapsed(), &[0, 2]);
        f.unfold_all();
        f.set_recursive(&b, 0, true);
        assert_eq!(f.collapsed(), &[0, 2]);
        f.set_recursive(&b, 0, false);
        assert!(f.collapsed().is_empty());
    }

    #[test]
    fn deleting_the_header_drops_the_fold() {
        let mut b = buffer(CODE);
        let mut f = Folds::default();
        f.sync(&b);
        f.toggle_at(2);
        // Join line 1 and 2 (delete the newline at the end of line 1).
        b.edit(&[Selection::default()], &[(Pos::new(1, 14), Pos::new(2, 4), "")], text::EditKind::Delete);
        f.sync(&b);
        assert!(f.collapsed().is_empty());
    }
}
