//! Recently opened folders: File > Open Recent (a native submenu `main.rs` rebuilds from
//! `recent_menu`) and "File: Open Recent..." (⌃R) in the quick input.

use std::path::{Path, PathBuf};

use super::{session, Effect, Focus, Workbench};
use crate::palette::{Action, Item, Palette, Picker};

/// A path with the home folder shown as `~`, like the standard recent list.
pub(super) fn tildify(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) => format!("~/{}", path.strip_prefix(&home).unwrap().display()),
        _ => path.display().to_string(),
    }
}

/// How a recent entry reads: the folder, or a workspace file with " (Workspace)".
fn recent_label(path: &Path) -> String {
    use crate::workspace as ws;
    match () {
        _ if ws::is_workspace_file(path) && ws::is_untitled(path) => "Untitled (Workspace)".to_string(),
        _ if ws::is_workspace_file(path) => format!("{} (Workspace)", tildify(&path.with_extension(""))),
        _ => tildify(path),
    }
}

/// How many recent folders the Dock menu lists.
const DOCK_RECENT: usize = 10;

/// The Dock menu's recent folders, newest first (picked: `Workbench::open_dock_recent`).
/// The recent folders the system keeps for the Dock menu while Orbvane isn't running.
pub fn recent_folders_for_system() -> Vec<PathBuf> {
    session::recent_folders().into_iter().take(DOCK_RECENT).collect()
}

pub fn dock_folders() -> Vec<String> {
    session::recent_folders().iter().take(DOCK_RECENT).map(|p| recent_label(p)).collect()
}

impl Workbench {
    /// The folders for File > Open Recent (not the one that's open).
    pub fn recent_menu(&self) -> Vec<(String, PathBuf)> {
        let current = self.workspace_id();
        session::recent_folders().into_iter().filter(|p| Some(p) != current.as_ref()).map(|p| (recent_label(&p), p)).collect()
    }

    /// Opening a folder puts it first in the recent list.
    pub(super) fn remember_folder(&mut self, folder: &Path) {
        session::add_recent(folder);
        self.effects.push(Effect::RecentChanged);
    }

    /// A folder picked in File > Open Recent.
    pub fn open_recent(&mut self, index: usize) {
        if let Some((_, path)) = self.recent_menu().into_iter().nth(index) {
            self.open_folder_by_user(&path);
        }
    }

    /// A folder picked in the Dock menu's recent list (all of them, the open one included).
    pub fn open_dock_recent(&mut self, index: usize) {
        if let Some(path) = session::recent_folders().into_iter().nth(index) {
            self.open_folder_by_user(&path);
        }
    }

    /// macOS asked to open `path`: a folder or workspace file opens like Open Recent; a file
    /// opens in this window.
    pub fn open_requested(&mut self, path: &Path) {
        if path.is_dir() || crate::workspace::is_workspace_file(path) {
            self.open_folder_by_user(path);
        } else if path.is_file() {
            self.open_file(path);
            self.focus = Focus::Editor;
        }
    }

    /// Whether this window has `path` open or inside one of its folders.
    pub fn has_path(&self, path: &Path) -> bool {
        self.workspace_id().as_deref() == Some(path) || self.folder_of(path).is_some()
    }

    pub(super) fn clear_recent(&mut self) {
        session::clear_recent();
        self.effects.push(Effect::RecentChanged);
    }

    pub(super) fn open_recent_picker(&mut self) {
        let choices: Vec<Item> = self
            .recent_menu()
            .into_iter()
            .map(|(label, path)| Item {
                label: path.file_name().map_or(label.clone(), |n| n.to_string_lossy().into_owned()),
                detail: label,
                matches: Vec::new(),
                shortcut: None,
                action: Action::OpenFolder(path),
                group: Some("recently opened".into()),
                kind: None,
            })
            .collect();
        let mut p = Palette::with_picker(Picker { placeholder: "Select to open".into(), choices });
        if p.items.is_empty() {
            p.message = Some("No recently opened folders".into());
        }
        self.palette = Some(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What macOS asks to open: a folder goes where Open Recent would send it, a file opens
    /// here, and windows say which paths they have.
    #[test]
    fn opens_what_macos_asks_for() {
        let dir = std::env::temp_dir().join(format!("orbvane-open-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.txt"), "x\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(a.clone()), &[], std::sync::Arc::new(|| {}));
        assert!(wb.has_path(&a) && wb.has_path(&a.join("x.txt")));
        assert!(!wb.has_path(&b));

        wb.take_effects();
        wb.open_requested(&b);
        let opened = wb.take_effects().into_iter().any(|e| matches!(e, Effect::OpenFolder { path, .. } if path == b));
        assert!(opened);

        wb.open_requested(&a.join("x.txt"));
        assert_eq!(wb.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf)), Some(a.join("x.txt")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
