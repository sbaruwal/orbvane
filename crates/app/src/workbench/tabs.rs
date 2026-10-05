//! Editor tabs: preview tabs (a single click opens a file in an italic tab that the next
//! single click reuses, until it's edited or double-clicked;
//! `workbench.editor.enablePreview`), dragging tabs to reorder them or move them to
//! another group, the Close Others / to the Right / Saved / All commands and the tab's
//! context menu, and Save As, Save All and Revert File.

use std::path::Path;

use super::{Drag, Focus, Hit, Workbench};
use crate::editor::EditorState;

/// How far the pointer moves before a press on a tab becomes a drag.
const DRAG_THRESHOLD: f32 = 5.0;

impl Workbench {
    /// Opens `path` as a preview (single click in the explorer, a search result), or pinned
    /// when previews are off.
    pub(super) fn open_file_preview(&mut self, path: &Path) {
        if crate::imageio::is_image(path) {
            return self.open_image(path, true);
        }
        if crate::search_editor::is_search_file(path) {
            return self.open_saved_search(path);
        }
        if !self.settings.bool("workbench.editor.enablePreview") {
            return self.open_file(path);
        }
        let Some(doc) = self.doc_for_path(path) else { return };
        self.show_doc_preview(doc);
        if let Some(tree) = &mut self.tree {
            tree.reveal(path);
        }
    }

    /// Shows a document in the active group's preview tab (replacing the previous preview).
    pub(super) fn show_doc_preview(&mut self, doc: usize) {
        let g = self.active_group;
        let group = &self.groups[g];
        if let Some(i) = group.tabs.iter().position(|t| t.doc == doc && !t.is_special()) {
            self.groups[g].active = i;
            return;
        }
        let mut ed = EditorState::new(doc);
        ed.preview = true;
        match group.tabs.iter().position(|t| t.preview) {
            Some(i) => {
                let old = self.groups[g].tabs[i].doc;
                self.groups[g].tabs[i] = ed;
                self.groups[g].active = i;
                self.release_doc(old);
            }
            None => {
                let group = &mut self.groups[g];
                let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
                group.tabs.insert(at, ed);
                group.active = at;
            }
        }
        self.reset_caret();
    }

    /// Drops a document no tab shows any more (a replaced preview; never dirty).
    fn release_doc(&mut self, doc: usize) {
        let shown = self.groups.iter().any(|g| g.tabs.iter().any(|t| t.doc == doc));
        let dirty = self.docs.get(doc).and_then(Option::as_ref).is_some_and(|d| d.buffer.is_dirty());
        if shown || dirty {
            return;
        }
        if let Some(path) = self.docs[doc].as_ref().and_then(|d| d.buffer.path()).map(Path::to_path_buf) {
            self.close_lsp_doc(&path);
        }
        self.docs[doc] = None;
    }

    /// Keeps the active editor open ("Keep Editor", double-clicking its tab or file).
    pub(super) fn pin_active(&mut self) {
        let g = self.active_group;
        let group = &mut self.groups[g];
        if let Some(t) = group.tabs.get_mut(group.active) {
            t.preview = false;
        }
    }

    /// Preview tabs whose document was edited become normal tabs.
    pub(super) fn pin_edited_previews(&mut self) {
        for group in &mut self.groups {
            for t in group.tabs.iter_mut().filter(|t| t.preview) {
                if self.docs.get(t.doc).and_then(Option::as_ref).is_some_and(|d| d.buffer.is_dirty()) {
                    t.preview = false;
                }
            }
        }
    }

    // ------------------------------------------------------------------ dragging tabs

    pub(super) fn start_tab_drag(&mut self, g: usize, i: usize, x: f32, y: f32) {
        self.drag = Some(Drag::Tab { g, i, from: (x, y), moving: false });
    }

    /// Moves the dragged tab to the position under the pointer (in its group or another).
    pub(super) fn drag_tab(&mut self, x: f32, y: f32) {
        let Some(Drag::Tab { g, i, from, moving }) = self.drag else { return };
        if !moving && (x - from.0).abs() < DRAG_THRESHOLD && (y - from.1).abs() < DRAG_THRESHOLD {
            return;
        }
        // Where to: another tab (once past its middle, so tabs of different widths don't
        // swap back and forth), or the empty end of a tab bar.
        let target = self.hits.iter().rev().find(|(r, h)| r.contains(x, y) && matches!(h, Hit::Tab(..) | Hit::TabBar(_)));
        let to = match target {
            Some((r, Hit::Tab(tg, j))) if (*tg, *j) != (g, i) => {
                let past_middle = if *tg != g || *j > i { x > r.x + r.w / 2.0 } else { x < r.x + r.w / 2.0 };
                past_middle.then_some((*tg, *j))
            }
            Some((_, Hit::TabBar(tg))) if *tg != g || i + 1 != self.groups[g].tabs.len() => {
                let end = self.groups[*tg].tabs.len();
                Some((*tg, if *tg == g { end - 1 } else { end }))
            }
            _ => None,
        };
        let Some((tg, j)) = to else {
            self.drag = Some(Drag::Tab { g, i, from, moving: true });
            return;
        };
        let (ng, ni) = self.move_tab(g, i, tg, j);
        self.drag = Some(Drag::Tab { g: ng, i: ni, from, moving: true });
    }

    /// Moves tab `i` of group `g` to index `j` of group `tg`. Returns its new place.
    fn move_tab(&mut self, g: usize, i: usize, tg: usize, j: usize) -> (usize, usize) {
        let tab = self.groups[g].tabs.remove(i);
        let mut tg = tg;
        if g != tg {
            let src = &mut self.groups[g];
            src.active = src.active.min(src.tabs.len().saturating_sub(1));
            if src.tabs.is_empty() && self.groups.len() > 1 {
                self.groups.remove(g);
                if tg > g {
                    tg -= 1;
                }
            }
        }
        let dst = &mut self.groups[tg];
        let j = j.min(dst.tabs.len());
        dst.tabs.insert(j, tab);
        dst.active = j;
        self.active_group = tg;
        self.focus = if self.settings_active() { Focus::Settings } else { Focus::Editor };
        (tg, j)
    }

    /// Closes tabs `which` of group `g` (indices), last first, asking about unsaved changes.
    /// Stops when one is cancelled.
    fn close_tabs(&mut self, g: usize, mut which: Vec<usize>) -> bool {
        which.sort_unstable();
        for i in which.into_iter().rev() {
            if g >= self.groups.len() || !self.close_tab(g, i) {
                return false;
            }
        }
        true
    }

    /// View: Close Other Editors in Group (⌥⌘T).
    pub(super) fn close_others(&mut self) {
        let g = self.active_group;
        let Some(gr) = self.groups.get(g) else { return };
        let keep = gr.active;
        let which = (0..gr.tabs.len()).filter(|&i| i != keep).collect();
        self.close_tabs(g, which);
    }

    /// View: Close Editors to the Right in Group.
    pub(super) fn close_to_the_right(&mut self) {
        let g = self.active_group;
        let Some(gr) = self.groups.get(g) else { return };
        let which = (gr.active + 1..gr.tabs.len()).collect();
        self.close_tabs(g, which);
    }

    /// View: Close Saved Editors in Group (⌘K U): the ones without unsaved changes.
    pub(super) fn close_saved(&mut self) {
        let g = self.active_group;
        let Some(gr) = self.groups.get(g) else { return };
        let which = (0..gr.tabs.len())
            .filter(|&i| {
                let t = &gr.tabs[i];
                t.settings || t.diff.is_some() || self.docs[t.doc].as_ref().is_none_or(|d| !d.buffer.is_dirty())
            })
            .collect();
        self.close_tabs(g, which);
    }

    /// View: Close All Editors in Group (⌘K W).
    pub(super) fn close_group_editors(&mut self) {
        let g = self.active_group;
        let n = self.groups.get(g).map_or(0, |gr| gr.tabs.len());
        self.close_tabs(g, (0..n).collect());
    }

    /// The tab's context menu (right-click).
    pub(super) fn tab_context_menu(&mut self, g: usize, i: usize, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        use crate::commands::Command;
        self.active_group = g;
        self.groups[g].active = i;
        let tab = &self.groups[g].tabs[i];
        let has_path = !tab.is_special() && self.docs[tab.doc].as_ref().is_some_and(|d| d.buffer.path().is_some());
        let preview = tab.preview;
        let n = self.groups[g].tabs.len();
        let item = |label: &str, enabled: bool| PopupItem::Item { label: label.into(), enabled, checked: None };
        let run = PopupAction::Run;
        let sep = || (PopupItem::Separator, PopupAction::None);
        let mut entries = vec![
            (item("Close", true), run(Command::CloseEditor)),
            (item("Close Others", n > 1), run(Command::CloseOtherEditors)),
            (item("Close to the Right", i + 1 < n), run(Command::CloseEditorsToTheRight)),
            (item("Close Saved", true), run(Command::CloseSavedEditors)),
            (item("Close All", true), run(Command::CloseAllEditors)),
            sep(),
            (item("Copy Path", has_path), run(Command::CopyFilePath)),
            (item("Copy Relative Path", has_path), run(Command::CopyRelativeFilePath)),
            sep(),
            (item("Select for Compare", has_path), run(Command::SelectForCompare)),
            (item("Compare with Selected", has_path && self.compare_left.is_some()), run(Command::CompareWithSelected)),
            sep(),
            (item("Reveal in Finder", has_path), run(Command::RevealInFinder)),
            (item("Reveal in Explorer View", has_path), run(Command::RevealInExplorer)),
            sep(),
        ];
        if preview {
            entries.push((item("Keep Open", true), run(Command::KeepEditor)));
        }
        entries.push((item("Split Right", true), run(Command::SplitEditor)));
        self.show_popup(entries, x, y);
    }

    /// File: Reveal Active File in Explorer View.
    pub(super) fn reveal_in_explorer(&mut self) {
        let Some(path) = self.active_doc().and_then(|d| d.buffer.path()).map(Path::to_path_buf) else { return };
        self.view = super::View::Explorer;
        self.sidebar_visible = true;
        self.explorer_open = true;
        if let Some(tree) = &mut self.tree {
            tree.reveal(&path);
        }
        self.scroll_explorer_to_selection();
        self.focus = Focus::Explorer;
    }

    /// File: Save As... (⇧⌘S): the active editor's contents go to a file picked in the save
    /// panel, and the editor now shows that file (the old one is left as it was on disk).
    pub(super) fn save_as(&mut self) {
        let Some(doc_id) = self.active_editor().filter(|e| !e.is_special()).map(|e| e.doc) else { return };
        if self.save_search_editor(doc_id, true).is_some() {
            return;
        }
        let Some(doc) = self.docs[doc_id].as_ref() else { return };
        let old = doc.buffer.path().map(Path::to_path_buf);
        let name = old.as_ref().and_then(|p| p.file_name()).map_or_else(|| format!("{}.txt", doc.title()), |n| n.to_string_lossy().into_owned());
        let dir = old.as_ref().and_then(|p| p.parent().map(Path::to_path_buf)).or_else(|| self.folder());
        let mut dialog = self.file_dialog().set_file_name(name);
        if let Some(dir) = dir {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else { return };
        if let Some(old) = &old {
            self.close_lsp_doc(old);
        }
        let Some(doc) = self.docs[doc_id].as_mut() else { return };
        doc.set_path(path);
        self.save_doc(doc_id);
    }

    /// Saves document `doc_id` to its file (untitled ones through the save panel). Returns
    /// false if it's still unsaved.
    pub(super) fn save_doc(&mut self, doc_id: usize) -> bool {
        if let Some(saved) = self.save_search_editor(doc_id, false) {
            return saved;
        }
        let untitled = self.docs[doc_id].as_ref().is_some_and(|d| d.buffer.path().is_none());
        if untitled {
            // The save panel names it after the editor: show that editor first.
            let at = self.groups.iter().enumerate().find_map(|(g, gr)| gr.tabs.iter().position(|t| t.doc == doc_id && !t.is_special()).map(|i| (g, i)));
            let Some((g, i)) = at else { return false };
            self.active_group = g;
            self.groups[g].active = i;
            self.run(crate::commands::Command::Save);
            return self.docs[doc_id].as_ref().is_some_and(|d| !d.buffer.is_dirty());
        }
        let Some(doc) = self.docs[doc_id].as_mut() else { return false };
        Self::before_save(doc);
        match doc.buffer.save() {
            Ok(()) => {
                if let Some(path) = doc.buffer.path().map(Path::to_path_buf) {
                    self.lsp.saved(&path);
                    self.after_save(&path);
                }
                true
            }
            Err(e) => {
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Failed to save").set_description(e.to_string()).show();
                false
            }
        }
    }

    /// File: Save All (⌥⌘S): every editor with unsaved changes.
    pub(super) fn save_all(&mut self) {
        let dirty: Vec<usize> = (0..self.docs.len()).filter(|&i| self.docs[i].as_ref().is_some_and(|d| d.buffer.is_dirty())).collect();
        let (g, a) = (self.active_group, self.groups.get(self.active_group).map(|gr| gr.active));
        for doc in dirty {
            if !self.save_doc(doc) {
                break;
            }
        }
        // Untitled files were shown to be saved; go back to where we were.
        if g < self.groups.len() {
            self.active_group = g;
            if let Some(a) = a.filter(|&a| a < self.groups[g].tabs.len()) {
                self.groups[g].active = a;
            }
        }
    }

    /// File: Revert File: drops the unsaved changes, reading the file again (an edit that can
    /// be undone).
    pub(super) fn revert_file(&mut self) {
        let Some((ed, doc)) = self.active_mut() else { return };
        let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let text = text.replace("\r\n", "\n");
        let before = ed.selections();
        let all = (text::Pos::new(0, 0), doc.buffer.end(), text.as_str());
        doc.buffer.edit(&before, &[all], text::EditKind::Other);
        doc.buffer.mark_saved();
        let head = doc.buffer.clamp(ed.sel.head);
        ed.set_selection(text::Selection::caret(head));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_save_and_revert() {
        let dir = std::env::temp_dir().join(format!("orbvane-tabs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<_> = ["a.txt", "b.txt", "c.txt", "d.txt"].iter().map(|n| dir.join(n)).collect();
        for f in &files {
            std::fs::write(f, "one\n").unwrap();
        }
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
        for f in &files {
            wb.open_file(f);
            wb.pin_active();
        }
        let names = |wb: &Workbench| -> Vec<String> {
            wb.groups[0].tabs.iter().map(|t| wb.docs[t.doc].as_ref().unwrap().title()).collect()
        };
        assert_eq!(names(&wb), ["a.txt", "b.txt", "c.txt", "d.txt"]);
        wb.groups[0].active = 2;
        wb.close_to_the_right();
        assert_eq!(names(&wb), ["a.txt", "b.txt", "c.txt"]);

        // Save All writes every changed file; Revert File brings back what's on disk.
        for g in 0..2 {
            wb.groups[0].active = g;
            let (ed, doc) = wb.active_mut().unwrap();
            ed.set_selection(text::Selection::caret(text::Pos::new(0, 0)));
            doc.buffer.insert(ed.sel, "x");
        }
        wb.save_all();
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), "xone\n");
        assert_eq!(std::fs::read_to_string(&files[1]).unwrap(), "xone\n");
        {
            let (ed, doc) = wb.active_mut().unwrap();
            doc.buffer.insert(ed.sel, "y");
            assert!(doc.buffer.is_dirty());
        }
        wb.revert_file();
        let doc = wb.active_doc().unwrap();
        assert_eq!(doc.buffer.text(), "xone\n");
        assert!(!doc.buffer.is_dirty());

        wb.close_others();
        assert_eq!(names(&wb), ["b.txt"]);
        wb.close_saved();
        assert!(wb.groups[0].tabs.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
