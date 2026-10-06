//! Recently opened folders: File > Open Recent (a native submenu `main.rs` rebuilds from
//! `recent_menu`) and "File: Open Recent..." (⌃R) in the quick input.

use std::path::{Path, PathBuf};

use super::{session, Effect, Workbench};
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
