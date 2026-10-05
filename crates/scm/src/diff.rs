//! Line diff (Myers' O(ND) algorithm) and the gutter change markers derived from it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;

/// A change marker for the editor gutter, in line numbers of the *new* text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineChange {
    /// Lines `start..end` were added.
    Added { start: usize, end: usize },
    /// Lines `start..end` replaced some old lines.
    Modified { start: usize, end: usize },
    /// Lines were removed just before line `at`.
    Deleted { at: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Equal,
    Insert,
    Delete,
}

/// Beyond this many edits, give up on a minimal diff and mark the middle as modified.
const MAX_EDITS: usize = 4000;

fn hash_lines(s: &str) -> Vec<u64> {
    s.split('\n')
        .map(|l| {
            let mut h = DefaultHasher::new();
            l.strip_suffix('\r').unwrap_or(l).hash(&mut h);
            h.finish()
        })
        .collect()
}

/// Myers' shortest edit script between `a` and `b`, or None if it exceeds `MAX_EDITS`.
fn myers(a: &[u64], b: &[u64]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let offset = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    // For each d, the part of `v` (k in -d-1..=d+1) as it was before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max.min(MAX_EDITS) as isize {
        let lo = (offset - d - 1) as usize;
        trace.push(v[lo..=(offset + d + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m, offset));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(trace: &[Vec<isize>], n: isize, m: isize, offset: isize) -> Vec<Op> {
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let snap = &trace[d as usize];
        // snap[i] corresponds to k = i - d - 1.
        let at = |k: isize| snap[(k + d + 1) as usize];
        let k = x - y;
        if d == 0 {
            while x > 0 && y > 0 {
                ops.push(Op::Equal);
                x -= 1;
                y -= 1;
            }
            break;
        }
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        ops.push(if x == prev_x { Op::Insert } else { Op::Delete });
        x = prev_x;
        y = prev_y;
    }
    let _ = offset;
    ops.reverse();
    ops
}

/// One row of a side-by-side diff: a line on either side (or a filler gap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffRow {
    Equal { left: usize, right: usize },
    /// A changed line pair (old line replaced by new line).
    Changed { left: usize, right: usize },
    /// Only on the left (removed); the right shows a filler gap.
    Removed { left: usize },
    /// Only on the right (added); the left shows a filler gap.
    Added { right: usize },
}

/// Rows for a side-by-side diff of `old` (left) and `new` (right), aligned so equal lines
/// sit next to each other. Within a run of changes, removed and added lines are paired up.
pub fn side_by_side(old: &str, new: &str) -> Vec<DiffRow> {
    let (a, b) = (hash_lines(old), hash_lines(new));
    let ops = edit_script(&a, &b);
    let mut rows = Vec::with_capacity(a.len().max(b.len()));
    let (mut i, mut j) = (0usize, 0usize);
    let (mut dels, mut inss): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    let flush = |rows: &mut Vec<DiffRow>, dels: &mut Vec<usize>, inss: &mut Vec<usize>| {
        let n = dels.len().max(inss.len());
        for k in 0..n {
            rows.push(match (dels.get(k), inss.get(k)) {
                (Some(&l), Some(&r)) => DiffRow::Changed { left: l, right: r },
                (Some(&l), None) => DiffRow::Removed { left: l },
                (None, Some(&r)) => DiffRow::Added { right: r },
                (None, None) => unreachable!(),
            });
        }
        dels.clear();
        inss.clear();
    };
    for op in ops {
        match op {
            Op::Equal => {
                flush(&mut rows, &mut dels, &mut inss);
                rows.push(DiffRow::Equal { left: i, right: j });
                i += 1;
                j += 1;
            }
            Op::Delete => {
                dels.push(i);
                i += 1;
            }
            Op::Insert => {
                inss.push(j);
                j += 1;
            }
        }
    }
    flush(&mut rows, &mut dels, &mut inss);
    rows
}

/// The full edit script from `a` to `b`, with common prefix/suffix handled cheaply.
fn edit_script(a: &[u64], b: &[u64]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops = vec![Op::Equal; prefix];
    ops.extend(myers(a_mid, b_mid).unwrap_or_else(|| {
        let mut ops = vec![Op::Delete; a_mid.len()];
        ops.extend(vec![Op::Insert; b_mid.len()]);
        ops
    }));
    ops.extend(vec![Op::Equal; suffix]);
    ops
}

/// The changed regions between `old` and `new` (split into lines at `\n`): each is the range
/// of `old` lines replaced and the range of `new` lines replacing them (either may be empty).
pub fn hunks(old: &str, new: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let ops = edit_script(&hash_lines(old), &hash_lines(new));
    let mut out = Vec::new();
    let (mut i, mut j, mut k) = (0, 0, 0);
    while k < ops.len() {
        if ops[k] == Op::Equal {
            (i, j, k) = (i + 1, j + 1, k + 1);
            continue;
        }
        let (i0, j0) = (i, j);
        while k < ops.len() && ops[k] != Op::Equal {
            match ops[k] {
                Op::Delete => i += 1,
                _ => j += 1,
            }
            k += 1;
        }
        out.push((i0..i, j0..j));
    }
    out
}

/// `old` with parts of the changes that turn it into `new`, like the standard Stage / Revert
/// Selected Ranges. For each change (lines `o` of `old` became lines `n` of `new`; a removal's
/// `n` is empty, at the line that follows it), `pick(n)` says which lines of `n` are selected
/// (for a removal, `Some(n)` takes it). Without `invert` the selected part is applied and the
/// rest stays as in `old`; with `invert` every change is applied except the selected part.
/// When a change has as many lines on both sides, lines pair up one to one; otherwise the
/// selected lines stand for all of `o`.
pub fn apply_changes(old: &str, new: &str, invert: bool, pick: impl Fn(Range<usize>) -> Option<Range<usize>>) -> String {
    let ops = edit_script(&hash_lines(old), &hash_lines(new));
    let (ol, nl): (Vec<&str>, Vec<&str>) = (old.split('\n').collect(), new.split('\n').collect());
    let mut out: Vec<&str> = Vec::with_capacity(ol.len().max(nl.len()));
    let (mut i, mut j, mut k) = (0, 0, 0);
    while k < ops.len() {
        if ops[k] == Op::Equal {
            out.push(ol[i]);
            (i, j, k) = (i + 1, j + 1, k + 1);
            continue;
        }
        let (i0, j0) = (i, j);
        while k < ops.len() && ops[k] != Op::Equal {
            match ops[k] {
                Op::Delete => i += 1,
                _ => j += 1,
            }
            k += 1;
        }
        let (o, n) = (&ol[i0..i], &nl[j0..j]);
        let Some(sel) = pick(j0..j) else {
            out.extend_from_slice(if invert { n } else { o });
            continue;
        };
        // The selected part: lines `a..z` of the change's new side (and, paired, of its old side).
        let (a, z) = (sel.start.max(j0) - j0, sel.end.min(j).max(sel.start.max(j0)) - j0);
        let (from, to) = if invert { (n, o) } else { (o, n) };
        if n.is_empty() {
            out.extend_from_slice(to);
        } else if o.len() == n.len() {
            out.extend_from_slice(&from[..a]);
            out.extend_from_slice(&to[a..z]);
            out.extend_from_slice(&from[z..]);
        } else if invert {
            out.extend_from_slice(&n[..a]);
            out.extend_from_slice(o);
            out.extend_from_slice(&n[z..]);
        } else {
            out.extend_from_slice(&n[a..z]);
        }
    }
    out.join("\n")
}

/// Change markers for turning `old` into `new`, as we draw them in the gutter.
pub fn line_changes(old: &str, new: &str) -> Vec<LineChange> {
    let (a, b) = (hash_lines(old), hash_lines(new));
    // Trim the common prefix and suffix; typical edits leave little in between.
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    let ops = myers(a_mid, b_mid).unwrap_or_else(|| {
        // Too different to diff cheaply: everything in the middle changed.
        let mut ops = vec![Op::Delete; a_mid.len()];
        ops.extend(vec![Op::Insert; b_mid.len()]);
        ops
    });

    // Group runs of inserts/deletes between equal lines.
    let mut out = Vec::new();
    let mut new_line = prefix;
    let (mut ins, mut del) = (0usize, 0usize);
    let mut run_start = new_line;
    let flush = |out: &mut Vec<LineChange>, start: usize, ins: usize, del: usize| match (ins, del) {
        (0, 0) => {}
        (0, _) => out.push(LineChange::Deleted { at: start }),
        (_, 0) => out.push(LineChange::Added { start, end: start + ins }),
        _ => out.push(LineChange::Modified { start, end: start + ins }),
    };
    for op in ops {
        match op {
            Op::Equal => {
                flush(&mut out, run_start, ins, del);
                ins = 0;
                del = 0;
                new_line += 1;
                run_start = new_line;
            }
            Op::Insert => {
                ins += 1;
                new_line += 1;
            }
            Op::Delete => del += 1,
        }
    }
    flush(&mut out, run_start, ins, del);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_added_modified_deleted() {
        let old = "a\nb\nc\nd\ne\n";
        assert_eq!(line_changes(old, old), vec![]);
        assert_eq!(line_changes(old, "a\nb\nX\nY\nc\nd\ne\n"), vec![LineChange::Added { start: 2, end: 4 }]);
        assert_eq!(line_changes(old, "a\nB\nc\nd\ne\n"), vec![LineChange::Modified { start: 1, end: 2 }]);
        assert_eq!(line_changes(old, "a\nd\ne\n"), vec![LineChange::Deleted { at: 1 }]);
        assert_eq!(
            line_changes(old, "new\na\nb\nC\nd\n"),
            vec![
                LineChange::Added { start: 0, end: 1 },
                LineChange::Modified { start: 3, end: 4 },
                LineChange::Deleted { at: 5 },
            ]
        );
    }

    #[test]
    fn myers_is_minimal() {
        let a: Vec<u64> = "ABCABBA".bytes().map(u64::from).collect();
        let b: Vec<u64> = "CBABAC".bytes().map(u64::from).collect();
        let ops = myers(&a, &b).unwrap();
        let edits = ops.iter().filter(|o| **o != Op::Equal).count();
        assert_eq!(edits, 5); // the classic example from Myers' paper
        // Replaying the script turns a into b.
        let (mut i, mut j, mut out) = (0, 0, Vec::new());
        for op in ops {
            match op {
                Op::Equal => {
                    out.push(a[i]);
                    i += 1;
                    j += 1;
                }
                Op::Delete => i += 1,
                Op::Insert => {
                    out.push(b[j]);
                    j += 1;
                }
            }
        }
        assert_eq!(out, b);
    }

    #[test]
    fn side_by_side_alignment() {
        let rows = side_by_side("a\nb\nc\nd\n", "a\nB\nX\nc\n");
        assert_eq!(
            rows,
            vec![
                DiffRow::Equal { left: 0, right: 0 },
                DiffRow::Changed { left: 1, right: 1 },
                DiffRow::Added { right: 2 },
                DiffRow::Equal { left: 2, right: 3 },
                DiffRow::Removed { left: 3 },
                DiffRow::Equal { left: 4, right: 4 },
            ]
        );
        // New files diff against empty text; the trailing empty line is common to both.
        assert_eq!(side_by_side("", "x\n"), vec![DiffRow::Added { right: 0 }, DiffRow::Equal { left: 0, right: 1 }]);
    }

    #[test]
    fn large_rewrite_falls_back() {
        let old: String = (0..3000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("new {i}\n")).collect();
        let changes = line_changes(&old, &new);
        assert_eq!(changes, vec![LineChange::Modified { start: 0, end: 3000 }]);
    }

    #[test]
    fn applies_only_chosen_changes() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let on = |lines: Range<usize>| move |r: Range<usize>| Some(r.start.max(lines.start)..r.end.min(lines.end)).filter(|s| s.start < s.end || r.is_empty() && lines.contains(&r.start));
        // Take the change on line 1 only.
        assert_eq!(apply_changes(old, new, false, on(1..2)), "a\nB\nc\nd\n");
        // Take the addition at the end only.
        assert_eq!(apply_changes(old, new, false, on(4..5)), "a\nb\nc\nd\ne\n");
        // Everything, and nothing.
        assert_eq!(apply_changes(old, new, false, Some), new);
        assert_eq!(apply_changes(old, new, false, |_| None), old);
        // Inverted: everything but the selection.
        assert_eq!(apply_changes(old, new, true, on(1..2)), "a\nb\nc\nd\ne\n");
        assert_eq!(apply_changes(old, new, true, |_| None), new);
        // A removal: its range is empty.
        assert_eq!(apply_changes("a\nb\nc", "a\nc", false, Some), "a\nc");
        assert_eq!(apply_changes("a\nb\nc", "a\nc", true, Some), "a\nb\nc");
    }

    #[test]
    fn applies_the_selected_lines_of_a_change() {
        // Two lines added together; select only the second.
        let old = "a\nz";
        let new = "a\nb\nc\nz";
        assert_eq!(apply_changes(old, new, false, |_| Some(2..3)), "a\nc\nz");
        assert_eq!(apply_changes(old, new, true, |_| Some(2..3)), "a\nb\nz");
        // Two lines changed, one to one: select the first.
        let (old, new) = ("a\nb\nc\nz", "a\nB\nC\nz");
        assert_eq!(apply_changes(old, new, false, |_| Some(1..2)), "a\nB\nc\nz");
        assert_eq!(apply_changes(old, new, true, |_| Some(1..2)), "a\nb\nC\nz");
        // Two lines became three: the selected part replaces both old lines.
        let (old, new) = ("a\nb\nc\nz", "a\nX\nY\nW\nz");
        assert_eq!(apply_changes(old, new, false, |_| Some(2..3)), "a\nY\nz");
        assert_eq!(apply_changes(old, new, true, |_| Some(2..3)), "a\nX\nb\nc\nW\nz");
    }
}
