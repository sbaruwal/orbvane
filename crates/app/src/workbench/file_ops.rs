//! File operations in the Explorer: New File / New Folder and Rename edit
//! inline in the tree, Delete moves to the Trash (after asking), Cut / Copy / Paste and
//! dragging items onto folders copy and move, and Copy Path / Reveal in Finder. Editors of
//! renamed files follow them.

use std::path::{Path, PathBuf};

use render::{Canvas, Rect, TextStyle};

use super::{Focus, Hit, View, Workbench, ROW_H, UI};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::TextField;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum EditKind {
    NewFile,
    NewFolder,
    /// Renaming this file or folder.
    Rename(PathBuf),
}

/// The name being typed in the tree.
pub(super) struct ExplorerEdit {
    pub kind: EditKind,
    /// The folder the new item goes in (for a rename, the item's folder).
    pub dir: PathBuf,
    pub field: TextField,
    /// Why the name can't be used (drawn under the field).
    pub error: Option<String>,
}

/// Files copied or cut in the Explorer, for Paste.
pub(super) struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

/// A name for a copy of `name` in `dir` that doesn't exist yet: "a copy.txt", "a copy 2.txt"...
fn copy_name(dir: &Path, name: &str) -> PathBuf {
    let path = Path::new(name);
    let (stem, ext) = match (path.file_stem(), path.extension()) {
        (Some(s), Some(e)) if !name.starts_with('.') || name.matches('.').count() > 1 => (s.to_string_lossy().into_owned(), format!(".{}", e.to_string_lossy())),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|n| if n == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {n}{ext}") })
        .map(|n| dir.join(n))
        .find(|p| !p.exists())
        .unwrap()
}

/// Copies a file or a whole folder.
fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// The checks for a new name typed in the Explorer (None: fine).
fn name_error(dir: &Path, name: &str, renaming: Option<&Path>) -> Option<String> {
    if name.trim().is_empty() {
        return Some("A file or folder name must be provided.".into());
    }
    if name.starts_with('/') {
        return Some("A file or folder name cannot start with a slash.".into());
    }
    if name.split('/').any(|part| part == "." || part == "..") {
        return Some(format!("The name **{name}** is not valid as a file or folder name. Please choose a different name."));
    }
    let target = dir.join(name);
    // Renaming "a" to "A" on a case-insensitive disk finds "a" itself.
    let same_item = renaming.is_some_and(|old| old.to_string_lossy().eq_ignore_ascii_case(&target.to_string_lossy()));
    if target.exists() && !same_item {
        return Some(format!("A file or folder **{name}** already exists at this location. Please choose a different name."));
    }
    if name != name.trim() {
        return Some("Leading or trailing whitespace detected in file or folder name.".into());
    }
    None
}

impl Workbench {
    /// The folder of the selected row (itself if it's a folder), else the root.
    fn explorer_target_dir(&self) -> Option<PathBuf> {
        let tree = self.tree.as_ref()?;
        let row = tree.selected.and_then(|i| tree.rows.get(i));
        Some(match row {
            Some(r) if r.is_dir => r.path.clone(),
            Some(r) => r.path.parent().map_or_else(|| tree.root_path().to_path_buf(), Path::to_path_buf),
            None => tree.root_path().to_path_buf(),
        })
    }

    /// The selected item's path, if any (not the root).
    pub(super) fn explorer_selected_path(&self) -> Option<PathBuf> {
        let tree = self.tree.as_ref()?;
        tree.selected.and_then(|i| tree.rows.get(i)).map(|r| r.path.clone())
    }

    /// New File / New Folder: an input row at the top of the target folder.
    pub(super) fn explorer_new(&mut self, folder: bool) {
        let Some(dir) = self.explorer_target_dir() else { return };
        self.view = View::Explorer;
        self.sidebar_visible = true;
        self.explorer_open = true;
        if let Some(tree) = &mut self.tree {
            if let Some(i) = tree.rows.iter().position(|r| r.path == dir) {
                tree.set_expanded(i, true);
            }
        }
        let kind = if folder { EditKind::NewFolder } else { EditKind::NewFile };
        self.explorer_edit = Some(ExplorerEdit { kind, dir, field: TextField::default(), error: None });
        self.focus = Focus::Explorer;
    }

    /// Rename (Enter or F2 in the Explorer): the selected row's name becomes editable, with
    /// the name before the extension selected.
    pub(super) fn explorer_rename(&mut self) {
        let Some(path) = self.explorer_selected_path().filter(|p| !self.folders().contains(p)) else { return };
        let Some(dir) = path.parent().map(Path::to_path_buf) else { return };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut field = TextField::default();
        field.set_text(&name);
        let stem = match name.rfind('.') {
            Some(i) if i > 0 && !path.is_dir() => name[..i].chars().count(),
            _ => name.chars().count(),
        };
        field.select_range(0, stem);
        self.explorer_edit = Some(ExplorerEdit { kind: EditKind::Rename(path), dir, field, error: None });
        self.focus = Focus::Explorer;
    }

    /// Keys while a name is being typed. Returns false when there's no edit.
    pub(super) fn explorer_edit_key(&mut self, k: &KeyInput) -> bool {
        let Some(edit) = &mut self.explorer_edit else { return false };
        match k.key {
            Key::Escape => self.explorer_edit = None,
            Key::Enter => self.commit_explorer_edit(),
            _ => {
                edit.field.key(k);
                let renaming = match &edit.kind {
                    EditKind::Rename(p) => Some(p.as_path()),
                    _ => None,
                };
                edit.error = if edit.field.text.is_empty() { None } else { name_error(&edit.dir, &edit.field.text, renaming) };
            }
        }
        true
    }

    /// Enter: create or rename. An empty name (or an unchanged one) just ends the edit.
    pub(super) fn commit_explorer_edit(&mut self) {
        let Some(edit) = self.explorer_edit.take() else { return };
        let name = edit.field.text.clone();
        let renaming = match &edit.kind {
            EditKind::Rename(p) => Some(p.clone()),
            _ => None,
        };
        let unchanged = renaming.as_ref().is_some_and(|p| p.file_name().is_some_and(|n| n.to_string_lossy() == name));
        if name.is_empty() || unchanged {
            return;
        }
        if let Some(e) = name_error(&edit.dir, &name, renaming.as_deref()).filter(|e| !e.starts_with("Leading")) {
            // Keep editing.
            self.explorer_edit = Some(ExplorerEdit { error: Some(e), ..edit });
            return;
        }
        let target = edit.dir.join(&name);
        let result = match &edit.kind {
            EditKind::NewFile => target
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map(|_| ())),
            EditKind::NewFolder => std::fs::create_dir_all(&target),
            EditKind::Rename(old) => std::fs::rename(old, &target),
        };
        if let Err(e) = result {
            return self.file_op_error(&format!("Unable to {} '{name}' ({e}).", if renaming.is_some() { "rename" } else { "create" }));
        }
        if let Some(old) = &renaming {
            self.paths_moved(old, &target);
        }
        self.refresh_explorer_and_reveal(&target);
        if edit.kind == EditKind::NewFile {
            self.open_file(&target);
            self.pin_active();
            self.focus = Focus::Editor;
        }
    }

    /// Re-reads the tree now (not waiting for FSEvents) and selects `path`.
    fn refresh_explorer_and_reveal(&mut self, path: &Path) {
        self.palette_files = None;
        if let Some(tree) = &mut self.tree {
            tree.refresh();
            tree.reveal(path);
        }
        self.scroll_explorer_to_selection();
    }

    /// Something at `old` is now at `new` (renamed or moved): open editors, language servers
    /// and breakpoints follow it.
    fn paths_moved(&mut self, old: &Path, new: &Path) {
        let moved = |p: &Path| -> Option<PathBuf> { p.strip_prefix(old).ok().map(|rest| if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(rest) }) };
        let mut closed = Vec::new();
        for doc in self.docs.iter_mut().flatten() {
            let Some(path) = doc.buffer.path().map(Path::to_path_buf) else { continue };
            if let Some(to) = moved(&path) {
                closed.push(path);
                doc.set_path(to);
            }
        }
        for path in closed {
            self.close_lsp_doc(&path);
        }
        let bps: Vec<PathBuf> = self.debug.breakpoints.keys().filter(|p| moved(p).is_some()).cloned().collect();
        for p in bps {
            if let (Some(list), Some(to)) = (self.debug.breakpoints.remove(&p), moved(&p)) {
                self.debug.breakpoints.insert(to, list);
            }
        }
    }

    /// Delete (⌘⌫): moves the selected item to the Trash after asking.
    pub(super) fn explorer_delete(&mut self) {
        // A workspace folder is removed from the workspace, never deleted from here.
        let Some(path) = self.explorer_selected_path().filter(|p| !self.folders().contains(p)) else { return };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let question = if path.is_dir() { format!("Are you sure you want to delete '{name}' and its contents?") } else { format!("Are you sure you want to delete '{name}'?") };
        let confirmed = cfg!(test)
            || self
                .message_dialog()
                .set_level(rfd::MessageLevel::Warning)
                .set_title(&question)
                .set_description("You can restore this file from the Trash.")
                .set_buttons(rfd::MessageButtons::OkCancelCustom("Move to Trash".into(), "Cancel".into()))
                .show()
                == rfd::MessageDialogResult::Custom("Move to Trash".into());
        if !confirmed {
            return;
        }
        if let Err(e) = crate::trash::move_to_trash(&path) {
            return self.file_op_error(&format!("Unable to move '{name}' to the Trash ({e})."));
        }
        // Select the row that takes its place.
        let index = self.tree.as_ref().and_then(|t| t.selected);
        self.palette_files = None;
        if let Some(tree) = &mut self.tree {
            tree.refresh();
            tree.selected = index.map(|i| i.min(tree.rows.len().saturating_sub(1))).filter(|_| !tree.rows.is_empty());
        }
    }

    /// ⌘X / ⌘C / ⌘V / ⌘A while the Explorer has focus: the name field's while typing a name,
    /// else files.
    pub(super) fn explorer_clipboard_command(&mut self, cmd: crate::commands::Command) {
        use crate::commands::Command;
        if let Some(edit) = &mut self.explorer_edit {
            match cmd {
                Command::SelectAll => edit.field.select_all(),
                Command::Paste => {
                    if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                        if let Some(edit) = &mut self.explorer_edit {
                            edit.field.insert(&text);
                        }
                    }
                }
                _ => {
                    let text = if cmd == Command::Cut { edit.field.cut() } else { edit.field.copy() };
                    if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
                        let _ = cb.set_text(text);
                    }
                }
            }
            return;
        }
        match cmd {
            Command::Cut => self.explorer_copy(true),
            Command::Copy => self.explorer_copy(false),
            Command::Paste => self.explorer_paste(),
            _ => {}
        }
    }

    /// Copy / Cut in the Explorer.
    pub(super) fn explorer_copy(&mut self, cut: bool) {
        if let Some(path) = self.explorer_selected_path() {
            self.file_clipboard = Some(FileClipboard { paths: vec![path], cut });
        }
    }

    /// Paste in the Explorer: copies (as "name copy" when the name is taken) or moves the
    /// clipboard's items into the selected folder.
    pub(super) fn explorer_paste(&mut self) {
        let Some(dir) = self.explorer_target_dir() else { return };
        let Some(clip) = self.file_clipboard.take() else { return };
        let mut last = None;
        for src in &clip.paths {
            let Some(name) = src.file_name() else { continue };
            if clip.cut && src.parent() == Some(dir.as_path()) {
                continue; // already there
            }
            if clip.cut && dir.starts_with(src) {
                self.file_op_error(&format!("Cannot move '{}' into itself.", name.to_string_lossy()));
                continue;
            }
            let dest = if dir.join(name).exists() { copy_name(&dir, &name.to_string_lossy()) } else { dir.join(name) };
            let result = if clip.cut { std::fs::rename(src, &dest) } else { copy_all(src, &dest) };
            match result {
                Ok(()) => {
                    if clip.cut {
                        self.paths_moved(src, &dest);
                    }
                    last = Some(dest);
                }
                Err(e) => self.file_op_error(&format!("Unable to paste '{}' ({e}).", name.to_string_lossy())),
            }
        }
        // A cut is pasted once; a copy can be pasted again.
        if !clip.cut {
            self.file_clipboard = Some(clip);
        }
        if let Some(p) = last {
            self.refresh_explorer_and_reveal(&p);
        }
    }

    /// Dropping Explorer row `from` onto row `onto` (or the empty space: the root) moves it
    /// into that folder (a file's folder), after asking.
    pub(super) fn explorer_drop(&mut self, from: usize, onto: Option<usize>) {
        let Some(tree) = &self.tree else { return };
        let Some(src) = tree.rows.get(from).map(|r| r.path.clone()) else { return };
        let dir = match onto.and_then(|i| tree.rows.get(i)) {
            Some(r) if r.is_dir => r.path.clone(),
            Some(r) => r.path.parent().map(Path::to_path_buf).unwrap_or_default(),
            None => tree.root_path().to_path_buf(),
        };
        if src.parent() == Some(dir.as_path()) || dir.starts_with(&src) {
            return;
        }
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if dir.join(&name).exists() {
            return self.file_op_error(&format!("A file or folder with the name '{name}' already exists in the destination folder."));
        }
        let confirmed = cfg!(test)
            || self
                .message_dialog()
                .set_level(rfd::MessageLevel::Info)
                .set_title(format!("Are you sure you want to move '{name}' into '{dir_name}'?"))
                .set_buttons(rfd::MessageButtons::OkCancelCustom("Move".into(), "Cancel".into()))
                .show()
                == rfd::MessageDialogResult::Custom("Move".into());
        if !confirmed {
            return;
        }
        let dest = dir.join(&name);
        if let Err(e) = std::fs::rename(&src, &dest) {
            return self.file_op_error(&format!("Unable to move '{name}' ({e})."));
        }
        self.paths_moved(&src, &dest);
        self.refresh_explorer_and_reveal(&dest);
    }

    /// Copy Path / Copy Relative Path: of the Explorer's selection while it has focus, else
    /// of the active file.
    pub(super) fn copy_file_path(&mut self, relative: bool) {
        let path = if self.focus == Focus::Explorer { self.explorer_selected_path().or_else(|| self.folder()) } else { None };
        let Some(path) = path.or_else(|| self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf))) else { return };
        let text = match (relative, self.folder()) {
            (true, Some(root)) => path.strip_prefix(&root).map(Path::to_path_buf).unwrap_or(path),
            _ => path,
        };
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(text.to_string_lossy().into_owned());
        }
    }

    /// The file a command is about: the Explorer's selection while it has focus, else the
    /// active editor's.
    fn context_file(&self) -> Option<PathBuf> {
        let explorer = if self.focus == Focus::Explorer { self.explorer_selected_path() } else { None };
        explorer.or_else(|| self.active_editor().filter(|e| !e.is_special()).and_then(|e| self.docs[e.doc].as_ref()?.buffer.path().map(Path::to_path_buf)))
    }

    /// File: Select for Compare.
    pub(super) fn select_for_compare(&mut self) {
        self.compare_left = self.context_file().filter(|p| p.is_file());
    }

    /// File: Compare with Selected: the file picked with Select for Compare on the left.
    pub(super) fn compare_with_selected(&mut self) {
        let (Some(left), Some(right)) = (self.compare_left.clone(), self.context_file()) else { return };
        self.compare_paths(right, Some(left));
    }

    /// Opens a comparison: `left` (the active file when None) against `right`, which opens
    /// as the live, editable side.
    pub(super) fn compare_paths(&mut self, right: PathBuf, left: Option<PathBuf>) {
        let (left, right) = match left {
            Some(l) => (l, right),
            // Compare Active File With...: the active file on the left, the picked one right.
            None => match self.context_file() {
                Some(active) => (active, right),
                None => return,
            },
        };
        if !right.is_file() || !left.is_file() {
            return;
        }
        self.open_diff(crate::diff_view::DiffSpec { path: right, staged: false, revision: None, left_file: Some(left) });
        self.pin_active();
    }

    /// File: Compare Active File with Saved (⌘K D).
    pub(super) fn compare_with_saved(&mut self) {
        let Some(path) = self.context_file().filter(|p| p.is_file()) else { return };
        self.compare_paths(path.clone(), Some(path));
    }

    /// File: Compare Active File With...: a picker of the folder's files.
    pub(super) fn compare_file_with(&mut self) {
        let Some(root) = self.folder() else { return };
        if self.context_file().is_none() {
            return;
        }
        let choices = crate::explorer::walk_files(&root, 20_000)
            .into_iter()
            .map(|p| {
                let label = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let detail = p.parent().and_then(|d| d.strip_prefix(&root).ok()).map(|d| d.display().to_string()).unwrap_or_default();
                crate::palette::Item { label, detail, matches: Vec::new(), shortcut: None, action: crate::palette::Action::CompareWith(p), group: None, kind: None }
            })
            .collect();
        self.palette = Some(crate::palette::Palette::with_picker(crate::palette::Picker { placeholder: "Select file to compare with".into(), choices }));
    }

    /// Reveal in Finder (⌥⌘R).
    pub(super) fn reveal_in_finder(&mut self) {
        let path = if self.focus == Focus::Explorer { self.explorer_selected_path().or_else(|| self.folder()) } else { None };
        let Some(path) = path.or_else(|| self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf))) else { return };
        let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
    }

    /// Open in Integrated Terminal: a new terminal in the item's folder.
    pub(super) fn open_in_terminal(&mut self) {
        let Some(dir) = self.explorer_target_dir() else { return };
        self.new_terminal_in(dir);
    }

    /// The inline name field at `r`, with its validation message under it.
    pub(super) fn draw_explorer_field(&mut self, c: &mut Canvas, r: Rect, focused: bool, caret_on: bool) {
        let fg = self.color("input.foreground");
        let (bg, border) = (self.color("input.background"), self.color("focusBorder"));
        let placeholder = self.color("input.placeholderForeground");
        let selection = self.color("editor.selectionBackground");
        let error_border = self.color("inputValidation.errorBorder");
        let Some(edit) = &mut self.explorer_edit else { return };
        let has_error = edit.error.is_some();
        c.bordered(r, bg, if has_error { error_border } else { border }, 1.0, 0.0);
        edit.field.draw(c, Rect::new(r.x + 3.0, r.y, r.w - 6.0, r.h), &TextStyle::ui(UI, fg), "", placeholder, focused, caret_on, selection);
        let error = edit.error.clone();
        self.hits.push((r, Hit::ExplorerEditField));
        if let Some(msg) = error {
            // Under the field, over the rows below, like the standard validation message.
            let msg = msg.replace("**", "");
            let style = TextStyle::ui(12.0, self.color("inputValidation.errorForeground"));
            let lines = super::intel::wrap(c, &msg, &style, r.w - 12.0);
            let b = Rect::new(r.x, r.bottom(), r.w, lines.len() as f32 * 17.0 + 8.0);
            c.push_layer();
            c.bordered(b, self.color("inputValidation.errorBackground"), error_border, 1.0, 0.0);
            for (i, l) in lines.iter().enumerate() {
                c.text(b.x + 6.0, b.y + 4.0 + i as f32 * 17.0, l, &style);
            }
        }
    }

    /// New File, New Folder, Refresh and Collapse Folders on the folder's header, while the
    /// pointer is over the Explorer.
    pub(super) fn draw_explorer_actions(&mut self, c: &mut Canvas, head: Rect, body: Rect) {
        let (mx, my) = self.mouse;
        if !(head.contains(mx, my) || body.contains(mx, my)) && self.explorer_edit.is_none() {
            return;
        }
        let fg = self.color_or("sideBarSectionHeader.foreground", "sideBar.foreground");
        let actions = [&icons::NEW_FILE, &icons::NEW_FOLDER, &icons::REFRESH, &icons::COLLAPSE_ALL];
        for (k, icon) in actions.iter().enumerate() {
            let b = Rect::new(head.right() - 8.0 - (actions.len() - k) as f32 * 24.0, head.y, 22.0, ROW_H);
            if self.hovered(Hit::ExplorerAction(k)) {
                c.fill_rounded(b.inset(0.0, 2.0), self.color("toolbar.hoverBackground"), 3.0);
            }
            c.icon_in(icon, b, 16.0, fg);
            self.hits.push((b, Hit::ExplorerAction(k)));
        }
    }

    /// The Explorer's context menu for row `row` (None: the empty space, the root folder).
    pub(super) fn explorer_context_menu(&mut self, row: Option<usize>, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        use crate::commands::Command;
        if let Some(tree) = &mut self.tree {
            tree.selected = row;
        }
        self.focus = Focus::Explorer;
        let item = |label: &str, enabled: bool| PopupItem::Item { label: label.into(), enabled, checked: None };
        let run = |c: Command| PopupAction::Run(c);
        let sep = || (PopupItem::Separator, PopupAction::None);
        let on_item = row.is_some();
        let mut entries = vec![
            (item("New File...", true), run(Command::ExplorerNewFile)),
            (item("New Folder...", true), run(Command::ExplorerNewFolder)),
            sep(),
            (item("Reveal in Finder", true), run(Command::RevealInFinder)),
            (item("Open in Integrated Terminal", true), run(Command::OpenInTerminal)),
            sep(),
        ];
        if on_item {
            entries.push((item("Cut", true), run(Command::Cut)));
            entries.push((item("Copy", true), run(Command::Copy)));
        }
        entries.push((item("Paste", self.file_clipboard.is_some()), run(Command::Paste)));
        entries.push(sep());
        if on_item && self.explorer_selected_path().is_some_and(|p| p.is_file()) {
            entries.push((item("Select for Compare", true), run(Command::SelectForCompare)));
            let can = self.compare_left.as_ref().is_some_and(|l| Some(l) != self.explorer_selected_path().as_ref());
            entries.push((item("Compare with Selected", can), run(Command::CompareWithSelected)));
            entries.push(sep());
        }
        entries.push((item("Copy Path", true), run(Command::CopyFilePath)));
        entries.push((item("Copy Relative Path", true), run(Command::CopyRelativeFilePath)));
        // A multi-root workspace's folder: workspace actions instead of Rename and Delete.
        let root = row.and_then(|i| self.tree.as_ref()?.rows.get(i)).is_some_and(|r| r.root);
        if root {
            entries.push(sep());
            entries.push((item("Add Folder to Workspace...", true), run(Command::AddFolderToWorkspace)));
            entries.push((item("Remove Folder from Workspace", true), run(Command::RemoveFolderFromWorkspace)));
        } else if on_item {
            entries.push(sep());
            entries.push((item("Rename...", true), run(Command::ExplorerRename)));
            entries.push((item("Delete", true), run(Command::ExplorerDelete)));
        }
        self.show_popup(entries, x, y);
    }

    fn file_op_error(&mut self, msg: &str) {
        if cfg!(test) {
            return self.set_status_message(msg);
        }
        self.message_dialog().set_level(rfd::MessageLevel::Error).set_title(msg).show();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_names() {
        let dir = std::env::temp_dir().join(format!("orbvane-copyname-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(copy_name(&dir, "a.txt"), dir.join("a copy.txt"));
        std::fs::write(dir.join("a copy.txt"), "").unwrap();
        assert_eq!(copy_name(&dir, "a.txt"), dir.join("a copy 2.txt"));
        assert_eq!(copy_name(&dir, "Makefile"), dir.join("Makefile copy"));
        assert_eq!(copy_name(&dir, ".gitignore"), dir.join(".gitignore copy"));
        assert_eq!(copy_name(&dir, "x.tar.gz"), dir.join("x.tar copy.gz"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Types `name` into the Explorer's field and presses Enter.
    fn type_name(wb: &mut Workbench, name: &str) {
        wb.explorer_edit.as_mut().unwrap().field.set_text(name);
        wb.commit_explorer_edit();
    }

    fn select(wb: &mut Workbench, path: &Path) {
        let tree = wb.tree.as_mut().unwrap();
        tree.reveal(path);
        assert!(tree.selected.is_some(), "{} not in the tree", path.display());
    }

    #[test]
    fn explorer_operations() {
        let dir = std::env::temp_dir().join(format!("orbvane-fileops-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("src/a.txt"), "hello").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));

        // New File in src (selected), with a folder on the way; it opens.
        select(&mut wb, &dir.join("src"));
        wb.explorer_new(false);
        type_name(&mut wb, "sub/new.txt");
        assert!(dir.join("src/sub/new.txt").is_file());
        assert_eq!(wb.active_doc().and_then(|d| d.buffer.path()), Some(dir.join("src/sub/new.txt").as_path()));
        // A taken name keeps the field open with the message.
        select(&mut wb, &dir.join("src/a.txt"));
        wb.explorer_new(true);
        type_name(&mut wb, "sub");
        assert!(wb.explorer_edit.as_ref().unwrap().error.as_ref().unwrap().contains("already exists"));
        wb.explorer_edit = None;

        // Rename an open file: its editor follows.
        wb.open_file(&dir.join("src/a.txt"));
        select(&mut wb, &dir.join("src/a.txt"));
        wb.explorer_rename();
        assert_eq!(wb.explorer_edit.as_ref().unwrap().field.selected_text(), "a");
        type_name(&mut wb, "b.txt");
        assert!(dir.join("src/b.txt").is_file() && !dir.join("src/a.txt").exists());
        assert!(wb.docs.iter().flatten().any(|d| d.buffer.path() == Some(dir.join("src/b.txt").as_path())));

        // Copy + paste into the same folder makes "b copy.txt"; cut + paste into the root moves.
        select(&mut wb, &dir.join("src/b.txt"));
        wb.explorer_copy(false);
        wb.explorer_paste();
        assert_eq!(std::fs::read_to_string(dir.join("src/b copy.txt")).unwrap(), "hello");
        select(&mut wb, &dir.join("src/b copy.txt"));
        wb.explorer_copy(true);
        wb.tree.as_mut().unwrap().selected = None;
        wb.explorer_paste();
        assert!(dir.join("b copy.txt").is_file() && !dir.join("src/b copy.txt").exists());

        // Dragging a file onto a folder moves it there.
        let rows = &wb.tree.as_ref().unwrap().rows;
        let from = rows.iter().position(|r| r.path == dir.join("b copy.txt")).unwrap();
        let onto = rows.iter().position(|r| r.path == dir.join("src/sub")).unwrap();
        wb.explorer_drop(from, Some(onto));
        assert!(dir.join("src/sub/b copy.txt").is_file());

        // Delete moves to the Trash.
        select(&mut wb, &dir.join("src/sub"));
        wb.explorer_delete();
        assert!(!dir.join("src/sub").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compares_files() {
        let dir = std::env::temp_dir().join(format!("orbvane-compare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "one\ntwo\n").unwrap();
        std::fs::write(&b, "one\nthree\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
        wb.focus = Focus::Editor;
        wb.open_file(&a);
        wb.select_for_compare();
        wb.open_file(&b);
        wb.compare_with_selected();
        let label = |wb: &Workbench| wb.active_editor().and_then(|e| e.diff.as_ref()).map(|d| d.label.clone());
        assert_eq!(label(&wb).as_deref(), Some("a.txt ↔ b.txt"));
        wb.open_file(&b);
        wb.compare_with_saved();
        assert_eq!(label(&wb).as_deref(), Some("b.txt (on disk) ↔ b.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_checks() {
        let dir = std::env::temp_dir();
        assert!(name_error(&dir, "", None).is_some());
        assert!(name_error(&dir, "/x", None).is_some());
        assert!(name_error(&dir, "a/../b", None).is_some());
        assert!(name_error(&dir, " x ", None).unwrap().starts_with("Leading"));
        assert!(name_error(&dir, "surely-not-here-1234.rs", None).is_none());
    }
}
