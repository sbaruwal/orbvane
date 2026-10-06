//! Multi-root workspaces: a window can hold several folders, listed in a
//! `.code-workspace` file (`crate::workspace`) that also carries the workspace settings.
//! Adding a folder to a window with one folder turns it into an untitled workspace; Save
//! Workspace As writes it where the user wants. Sessions and the recent list are keyed by
//! the workspace file (or the folder, for a single folder opened directly).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::Workbench;
use crate::explorer::FileTree;
use crate::workspace::{self as ws, Folder};

impl Workbench {
    /// The workspace's folders (none without a folder).
    pub(super) fn folders(&self) -> Vec<PathBuf> {
        self.tree.as_ref().map(FileTree::roots).unwrap_or_default()
    }

    /// The workspace folder `path` is in (the innermost one).
    pub(super) fn folder_of(&self, path: &Path) -> Option<PathBuf> {
        self.folders().into_iter().filter(|f| path.starts_with(f)).max_by_key(|f| f.components().count())
    }

    /// What the window has open: the workspace file, or the single folder.
    pub fn workspace_id(&self) -> Option<PathBuf> {
        self.workspace_file.clone().or_else(|| self.folder())
    }

    /// The window's name for the title bar and the Explorer: "name (Workspace)" or the folder's.
    pub(super) fn workspace_label(&self) -> Option<String> {
        match &self.workspace_file {
            Some(file) => Some(format!("{} (Workspace)", ws::name(file))),
            None => self.tree.as_ref().map(|t| t.root_name().to_string()),
        }
    }

    /// `path` for display: relative to its workspace folder, with the folder's name in front
    /// when the workspace has several ("app/src/main.rs"), like the standard labels.
    pub(super) fn display_path(&self, path: &Path) -> String {
        let Some(folder) = self.folder_of(path) else { return path.display().to_string() };
        let rel = path.strip_prefix(&folder).unwrap_or(path).to_string_lossy().into_owned();
        if self.workspace_file.is_some() && self.folders().len() > 1 {
            let names = self.tree.as_ref().map(FileTree::root_names).unwrap_or_default();
            let i = self.folders().iter().position(|f| *f == folder).unwrap_or(0);
            let name = names.get(i).cloned().unwrap_or_default();
            if rel.is_empty() { name } else { format!("{name}/{rel}") }
        } else {
            rel
        }
    }

    /// The file a `display_path` label names.
    pub(super) fn resolve_display_path(&self, label: &str) -> Option<PathBuf> {
        let folders = self.folders();
        if Path::new(label).is_absolute() {
            return Some(PathBuf::from(label));
        }
        if self.workspace_file.is_some() && folders.len() > 1 {
            let names = self.tree.as_ref().map(FileTree::root_names).unwrap_or_default();
            for (folder, name) in folders.iter().zip(&names) {
                if let Some(rest) = label.strip_prefix(name.as_str()).and_then(|r| r.strip_prefix('/')) {
                    return Some(folder.join(rest));
                }
            }
        }
        folders.first().map(|f| f.join(label))
    }

    /// Opens a `.code-workspace` file's folders (the window's other state is set up by the
    /// caller, `open_folder_raw`). False if it couldn't be read.
    pub(super) fn load_workspace_file(&mut self, file: &Path) -> bool {
        let folders = match ws::read(file) {
            Ok(f) => f.into_iter().filter(|f| f.path.is_dir()).collect::<Vec<_>>(),
            Err(e) => {
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Unable to open workspace").set_description(e).show();
                return false;
            }
        };
        if folders.is_empty() {
            self.message_dialog()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Unable to open workspace")
                .set_description(format!("{} has no folders that exist.", file.display()))
                .show();
            return false;
        }
        let roots: Vec<(PathBuf, Option<String>)> = folders.into_iter().map(|f| (f.path, f.name)).collect();
        self.tree = Some(FileTree::with_roots(&roots));
        self.workspace_file = Some(file.to_path_buf());
        self.workspace_mtime = mtime(file);
        self.settings.set_workspace_file(file);
        true
    }

    /// Everything that follows the set of folders: git repositories, the file watcher, test
    /// providers and the Go to File list.
    pub(super) fn folders_changed(&mut self) {
        let folders = self.folders();
        self.open_repos(&folders);
        self.git_branch = folders.first().and_then(|f| super::read_git_branch(f));
        self.start_watching();
        self.testing_open_folders(&folders);
        self.palette_files = None;
        self.apply_settings();
        self.ext_folders_changed();
        self.assistant_folders_changed();
    }

    /// The folders as a workspace file lists them.
    fn workspace_folders(&self) -> Vec<Folder> {
        let Some(tree) = &self.tree else { return Vec::new() };
        let own_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
        tree.roots()
            .into_iter()
            .zip(tree.root_names())
            .map(|(path, name)| {
                let name = (own_name(&path).as_deref() != Some(name.as_str())).then_some(name);
                Folder { path, name }
            })
            .collect()
    }

    fn write_workspace_file(&mut self) {
        let Some(file) = self.workspace_file.clone() else { return };
        if let Err(e) = ws::write(&file, &self.workspace_folders()) {
            self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Unable to save the workspace").set_description(e).show();
        }
        self.workspace_mtime = mtime(&file);
    }

    /// File > Add Folder to Workspace...: a window with one folder becomes an untitled
    /// workspace; a window without a folder just opens it.
    pub(super) fn add_folder_to_workspace(&mut self) {
        let Some(path) = self.file_dialog().set_title("Add Folder to Workspace").pick_folder() else { return };
        self.add_folders_to_workspace(&[path]);
    }

    pub(super) fn add_folders_to_workspace(&mut self, paths: &[PathBuf]) {
        let Some(tree) = &mut self.tree else {
            if let Some(first) = paths.first() {
                self.open_folder(first);
            }
            return;
        };
        let new: Vec<&PathBuf> = paths.iter().filter(|p| p.is_dir() && !tree.roots().contains(p)).collect();
        if new.is_empty() {
            return;
        }
        for p in &new {
            tree.add_root(p, None);
        }
        if self.workspace_file.is_none() {
            // The folder's session moves to the new workspace.
            let file = ws::new_untitled();
            self.workspace_file = Some(file.clone());
            self.write_workspace_file();
            self.settings.set_workspace_file(&file);
            self.remember_folder(&file);
        } else {
            self.write_workspace_file();
        }
        self.folders_changed();
        if let (Some(tree), Some(p)) = (&mut self.tree, new.last()) {
            tree.reveal(p);
        }
    }

    /// Remove Folder from Workspace (the Explorer's context menu on a workspace folder).
    pub(super) fn remove_folder_from_workspace(&mut self, path: &Path) {
        let Some(tree) = &mut self.tree else { return };
        if tree.roots().len() < 2 || !tree.roots().iter().any(|r| r == path) {
            return;
        }
        tree.remove_root(path);
        self.write_workspace_file();
        self.folders_changed();
    }

    /// Workspaces: Remove Folder from Workspace...: the Explorer's selected folder (from its
    /// context menu), or a pick of the workspace's folders.
    pub(super) fn remove_folder_command(&mut self) {
        let folders = self.folders();
        if folders.len() < 2 {
            return self.set_status_message("The workspace has only one folder.");
        }
        if self.focus == super::Focus::Explorer {
            if let Some(p) = self.explorer_selected_path().filter(|p| folders.contains(p)) {
                return self.remove_folder_from_workspace(&p);
            }
        }
        let names = self.tree.as_ref().map(FileTree::root_names).unwrap_or_default();
        let choices = folders
            .into_iter()
            .zip(names)
            .map(|(path, name)| crate::palette::Item {
                label: name,
                detail: super::recent::tildify(&path),
                matches: Vec::new(),
                shortcut: None,
                action: crate::palette::Action::RemoveRootFolder(path),
                group: None,
                kind: None,
            })
            .collect();
        self.palette = Some(crate::palette::Palette::with_picker(crate::palette::Picker { placeholder: "Select workspace folder to remove".into(), choices }));
    }

    /// File > Save Workspace As...: writes the workspace to a `.code-workspace` file of the
    /// user's choice, which the window then has open.
    pub(super) fn save_workspace_as(&mut self) {
        if self.tree.is_none() {
            return;
        }
        let dir = self.folders().first().and_then(|f| f.parent().map(Path::to_path_buf));
        let name = format!("{}.{}", self.workspace_file.as_deref().filter(|f| !ws::is_untitled(f)).map_or_else(|| "workspace".to_string(), ws::name), ws::EXTENSION);
        let mut dialog = self.file_dialog().set_title("Save Workspace").set_file_name(&name).add_filter("Code Workspace", &[ws::EXTENSION]);
        if let Some(d) = dir {
            dialog = dialog.set_directory(d);
        }
        let Some(mut file) = dialog.save_file() else { return };
        if !ws::is_workspace_file(&file) {
            file.set_extension(ws::EXTENSION);
        }
        // Keep the workspace settings of an untitled workspace.
        let old = self.workspace_file.clone();
        let text = old.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
        let folders = self.workspace_folders();
        let text = ws::with_folders(&text, &folders, &file);
        if let Err(e) = std::fs::write(&file, text) {
            self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Unable to save the workspace").set_description(e.to_string()).show();
            return;
        }
        if let Some(old) = old.filter(|f| ws::is_untitled(f)) {
            let _ = std::fs::remove_dir_all(old.parent().unwrap_or(&old));
        }
        self.workspace_file = Some(file.clone());
        self.workspace_mtime = mtime(&file);
        self.settings.set_workspace_file(&file);
        self.apply_settings();
        self.remember_folder(&file);
    }

    /// File > Open Workspace from File...
    pub(super) fn open_workspace_from_file(&mut self) {
        let Some(file) = self.file_dialog().set_title("Open Workspace from File").add_filter("Code Workspace", &[ws::EXTENSION]).pick_file() else { return };
        self.open_folder_by_user(&file);
    }

    /// Picks up edits to the workspace file made outside (checked about once a second).
    pub(super) fn workspace_tick(&mut self) {
        let Some(file) = self.workspace_file.clone() else { return };
        if self.workspace_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.workspace_checked = Some(Instant::now());
        let now = mtime(&file);
        if now == self.workspace_mtime {
            return;
        }
        self.workspace_mtime = now;
        let Ok(folders) = ws::read(&file) else { return };
        let folders: Vec<Folder> = folders.into_iter().filter(|f| f.path.is_dir()).collect();
        if folders.is_empty() || folders == self.workspace_folders() {
            return;
        }
        let Some(tree) = &mut self.tree else { return };
        // Add first: the tree always keeps at least one folder.
        for f in &folders {
            tree.add_root(&f.path, f.name.as_deref());
        }
        for old in tree.roots() {
            if !folders.iter().any(|f| f.path == old) {
                tree.remove_root(&old);
            }
        }
        self.folders_changed();
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
