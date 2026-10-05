//! Staging, unstaging and reverting only the selected lines, like the standard Git: Stage /
//! Unstage / Revert Selected Ranges. Only the selected lines of each change (run of changed
//! lines) are applied; the rest of the file's changes are left alone.

use std::ops::Range;
use std::path::{Path, PathBuf};

use scm::{DiffRow, Op};
use text::{Pos, Selection};

use super::Workbench;

/// The selected part of a change covering lines `r` (empty for a removal before line
/// `r.start`, which is taken whole when a line next to it is selected): the lines from the
/// first selected one to the last.
fn selected_part(r: &Range<usize>, lines: &[usize]) -> Option<Range<usize>> {
    if r.is_empty() {
        return lines.iter().any(|&l| l == r.start || l + 1 == r.start).then(|| r.clone());
    }
    let first = *lines.iter().find(|l| r.contains(l))?;
    let last = *lines.iter().rev().find(|l| r.contains(l))?;
    Some(first..last + 1)
}

impl Workbench {
    /// The active file (in the repository), its text, and the lines its selections cover.
    fn selected_lines(&self) -> Option<(PathBuf, PathBuf, String, Vec<usize>)> {
        let root = self.repo.as_ref()?.root.clone();
        let ed = self.active_editor().filter(|e| !e.is_special())?;
        let doc = self.docs[ed.doc].as_ref()?;
        let path = doc.buffer.path()?.to_path_buf();
        if !path.starts_with(&root) {
            return None;
        }
        let mut lines: Vec<usize> = ed
            .selections()
            .iter()
            .flat_map(|s| {
                let (a, z) = s.ordered();
                // A selection ending at the start of a line doesn't include that line.
                let last = if z.col == 0 && z.line > a.line { z.line - 1 } else { z.line };
                a.line..=last
            })
            .collect();
        lines.sort_unstable();
        lines.dedup();
        Some((root, path, doc.buffer.text(), lines))
    }

    /// The staged text of `path` (HEAD's, or none for a new file).
    fn staged_text(root: &Path, path: &Path) -> String {
        scm::show(root, "", path).or_else(|| scm::show(root, "HEAD", path)).unwrap_or_default()
    }

    /// Git: Stage Selected Ranges.
    pub(super) fn stage_selected_ranges(&mut self) {
        let Some((root, path, working, lines)) = self.selected_lines() else { return };
        let index = Self::staged_text(&root, &path);
        let staged = scm::apply_changes(&index, &working, false, |r| selected_part(&r, &lines));
        if staged == index {
            return self.set_status_message("No changes in the selection to stage.");
        }
        self.git_run(Op::StageContents { path, contents: staged });
    }

    /// Git: Unstage Selected Ranges: the staged changes on the selected lines go back to HEAD.
    pub(super) fn unstage_selected_ranges(&mut self) {
        let Some((root, path, working, lines)) = self.selected_lines() else { return };
        let index = Self::staged_text(&root, &path);
        let head = scm::show(&root, "HEAD", &path).unwrap_or_default();
        // The selection is in the working file; find the same lines in the staged text.
        let index_lines: Vec<usize> = scm::side_by_side(&index, &working)
            .into_iter()
            .filter_map(|row| match row {
                DiffRow::Equal { left, right } | DiffRow::Changed { left, right } if lines.contains(&right) => Some(left),
                _ => None,
            })
            .collect();
        let unstaged = scm::apply_changes(&head, &index, true, |r| selected_part(&r, &index_lines));
        if unstaged == index {
            return self.set_status_message("No staged changes in the selection.");
        }
        self.git_run(Op::StageContents { path, contents: unstaged });
    }

    /// Git: Revert Selected Ranges: the selected lines go back to their staged version (an
    /// edit to the document, which can be undone).
    pub(super) fn revert_selected_ranges(&mut self) {
        let Some((root, path, working, lines)) = self.selected_lines() else { return };
        let index = Self::staged_text(&root, &path);
        let reverted = scm::apply_changes(&index, &working, true, |r| selected_part(&r, &lines));
        if reverted == working {
            return self.set_status_message("No changes in the selection to revert.");
        }
        let Some((ed, doc)) = self.active_mut() else { return };
        let before = ed.selections();
        let all = (Pos::new(0, 0), doc.buffer.end(), reverted.as_str());
        doc.buffer.edit(&before, &[all], text::EditKind::Other);
        let head = doc.buffer.clamp(ed.sel.head);
        ed.set_selection(Selection::caret(head));
    }
}

#[cfg(test)]
mod tests {
    use super::selected_part;

    #[test]
    fn selection_picks_part_of_a_change() {
        assert_eq!(selected_part(&(2..6), &[3]), Some(3..4));
        assert_eq!(selected_part(&(2..6), &[1, 3, 4, 9]), Some(3..5));
        assert_eq!(selected_part(&(2..4), &[4]), None);
        // A removal before line 5 is taken from line 4 or 5.
        assert_eq!(selected_part(&(5..5), &[4]), Some(5..5));
        assert_eq!(selected_part(&(5..5), &[5]), Some(5..5));
        assert_eq!(selected_part(&(5..5), &[7]), None);
    }
}
