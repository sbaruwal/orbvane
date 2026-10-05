//! Session restore: the last folder and window size, and per folder the open
//! editors (with selections and scroll positions), the layout and expanded explorer folders.
//! With `files.hotExit`, quitting keeps unsaved changes (and untitled files) instead of asking,
//! and they come back dirty on the next launch.
//!
//! Stored under `<user data>/State`: `global.json`, and `workspaces/<hash>.json` per folder
//! (`empty.json` without a folder).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use text::{Pos, Selection};

use super::{Focus, Group, View, Workbench};
use crate::editor::{Doc, EditorState};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Global {
    last_folder: Option<PathBuf>,
    window: Option<WindowBounds>,
    #[serde(default)]
    recent: Vec<PathBuf>,
    /// Outline view options.
    #[serde(default)]
    outline: Option<OutlineOptions>,
}

#[derive(Serialize, Deserialize)]
struct OutlineOptions {
    follow_cursor: bool,
    sort: super::outline::Sort,
}

#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
struct WorkspaceState {
    sidebar_visible: bool,
    sidebar_width: f32,
    view: String,
    panel_visible: bool,
    panel_height: f32,
    panel_maximized: bool,
    panel_tab: usize,
    /// Expanded explorer folders, relative to the root.
    expanded: Vec<PathBuf>,
    groups: Vec<GroupState>,
    active_group: usize,
    /// Breakpoints per file.
    #[serde(default)]
    breakpoints: Vec<(PathBuf, Vec<super::debug::Breakpoint>)>,
    /// Toggle Activate Breakpoints (None: on).
    #[serde(default)]
    breakpoints_off: bool,
    #[serde(default)]
    watches: Vec<String>,
    /// Exception breakpoints turned on or off (filter id → enabled).
    #[serde(default)]
    exception_breakpoints: BTreeMap<String, bool>,
    /// The launch configuration last started.
    #[serde(default)]
    debug_config: Option<String>,
    /// The secondary side bar: shown, its width (0: the default) and tab.
    #[serde(default)]
    aux_visible: bool,
    #[serde(default)]
    aux_width: f32,
    #[serde(default)]
    aux_tab: super::aux_bar::AuxTab,
    /// The Run and Debug sections: open, and dragged heights.
    #[serde(default)]
    debug_sections: Option<Vec<(bool, Option<f32>)>>,
    /// The Assistant's open chats (`chat_store` ids) and the one shown.
    #[serde(default)]
    assistant_chats: Vec<String>,
    #[serde(default)]
    assistant_active: Option<String>,
}

#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
struct GroupState {
    tabs: Vec<TabState>,
    active: usize,
}

#[derive(Default, Serialize, Deserialize, Debug, PartialEq)]
struct TabState {
    /// The file (None: an untitled document, or the Settings editor).
    path: Option<PathBuf>,
    #[serde(default)]
    untitled: Option<usize>,
    #[serde(default)]
    settings: bool,
    #[serde(default)]
    preview: bool,
    /// Unsaved contents (hot exit).
    #[serde(default)]
    backup: Option<String>,
    /// Header lines of collapsed regions.
    #[serde(default)]
    folds: Vec<usize>,
    /// Primary selection: anchor line, col, head line, col.
    selection: [usize; 4],
    scroll: [f32; 2],
}

fn push_recent(recent: &mut Vec<PathBuf>, folder: PathBuf) {
    recent.retain(|r| *r != folder);
    recent.insert(0, folder);
    recent.truncate(20);
}

/// Recently opened folders, newest first (ones that no longer exist are left out).
pub(super) fn recent_folders() -> Vec<PathBuf> {
    let global: Global = read(&state_dir().join("global.json"));
    global.recent.into_iter().filter(|p| p.is_dir() || (crate::workspace::is_workspace_file(p) && p.is_file())).collect()
}

/// Puts `folder` first in the recent list (opening it).
pub(super) fn add_recent(folder: &Path) {
    let path = state_dir().join("global.json");
    let mut global: Global = read(&path);
    push_recent(&mut global.recent, folder.to_path_buf());
    write(&path, &global);
}

/// File > Open Recent > Clear Recently Opened.
pub(super) fn clear_recent() {
    let path = state_dir().join("global.json");
    let mut global: Global = read(&path);
    global.recent.clear();
    write(&path, &global);
}

pub(super) fn state_dir() -> PathBuf {
    settings::user_data_dir().join("State")
}

/// A stable name for a folder's (or workspace file's) state: FNV-1a of the path.
pub(super) fn workspace_key(folder: Option<&Path>) -> String {
    let Some(folder) = folder else { return "empty".into() };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in folder.to_string_lossy().bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

fn workspace_file(folder: Option<&Path>) -> PathBuf {
    state_dir().join("workspaces").join(format!("{}.json", workspace_key(folder)))
}

pub(super) fn read<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub(super) fn write<T: Serialize>(path: &Path, value: &T) {
    let Ok(text) = serde_json::to_string_pretty(value) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write then rename, so a crash never leaves half a file.
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The window size and position to start with.
pub fn window_bounds() -> Option<WindowBounds> {
    read::<Global>(&state_dir().join("global.json")).window
}

/// The folder to reopen on a launch without arguments (`window.restoreWindows`).
pub fn folder_to_restore(store: &settings::Store) -> Option<PathBuf> {
    if store.string("window.restoreWindows") == "none" {
        return None;
    }
    read::<Global>(&state_dir().join("global.json")).last_folder.filter(|f| f.is_dir() || (crate::workspace::is_workspace_file(f) && f.is_file()))
}

fn view_name(v: View) -> String {
    match v {
        View::Ext(i) => return format!("ext:{}", crate::contributions::container(i).map_or(String::new(), |c| c.id)),
        View::Explorer => "explorer",
        View::Search => "search",
        View::Scm => "scm",
        View::Debug => "debug",
        View::Extensions => "extensions",
        View::Testing => "testing",
    }
    .to_string()
}

fn view_from(name: &str) -> View {
    match name {
        "search" => View::Search,
        "scm" => View::Scm,
        "debug" => View::Debug,
        "extensions" => View::Extensions,
        "testing" => View::Testing,
        _ => match name.strip_prefix("ext:").and_then(crate::contributions::container_index) {
            Some(i) => View::Ext(i),
            None => View::Explorer,
        },
    }
}

impl Workbench {
    pub(super) fn folder(&self) -> Option<PathBuf> {
        self.tree.as_ref().map(|t| t.root_path().to_path_buf())
    }

    /// Whether quitting keeps unsaved changes instead of asking (`files.hotExit`).
    pub(super) fn hot_exit(&self) -> bool {
        self.settings.string("files.hotExit") != "off"
    }

    fn capture(&self, backups: bool) -> WorkspaceState {
        let root = self.folder();
        let groups = self
            .groups
            .iter()
            .map(|g| {
                let mut active = g.active;
                let mut tabs = Vec::new();
                for (i, ed) in g.tabs.iter().enumerate() {
                    let doc = self.docs.get(ed.doc).and_then(Option::as_ref);
                    let state = match doc {
                        _ if ed.settings => Some(TabState { settings: true, ..Default::default() }),
                        _ if ed.diff.is_some() || ed.markdown.is_some() => None,
                        Some(doc) => {
                            let dirty = doc.buffer.is_dirty() || (doc.untitled.is_some() && !doc.buffer.text().is_empty());
                            let backup = (backups && dirty).then(|| doc.buffer.text());
                            // Untitled files are only kept with their contents.
                            match (doc.buffer.path(), doc.untitled) {
                                (Some(p), _) => Some(TabState { path: Some(p.to_path_buf()), backup, ..Default::default() }),
                                (None, Some(n)) if backup.is_some() => Some(TabState { untitled: Some(n), backup, ..Default::default() }),
                                _ => None,
                            }
                        }
                        None => None,
                    };
                    match state {
                        Some(mut t) => {
                            let s = ed.sel;
                            t.selection = [s.anchor.line, s.anchor.col, s.head.line, s.head.col];
                            t.scroll = [ed.scroll_y, ed.scroll_x];
                            t.folds = ed.folds.collapsed().to_vec();
                            t.preview = ed.preview;
                            tabs.push(t);
                        }
                        None if i < g.active => active -= 1,
                        None => {}
                    }
                }
                GroupState { active: active.min(tabs.len().saturating_sub(1)), tabs }
            })
            .collect();
        let expanded = match (&self.tree, &root) {
            (Some(tree), Some(root)) => tree.expanded_paths().into_iter().map(|p| p.strip_prefix(root).map_or_else(|_| p.clone(), Path::to_path_buf)).collect(),
            _ => Vec::new(),
        };
        let (assistant_chats, assistant_active) = self.assistant_session();
        WorkspaceState {
            sidebar_visible: self.sidebar_visible,
            sidebar_width: self.sidebar_w,
            view: view_name(self.view),
            panel_visible: self.panel_visible,
            panel_height: self.panel_h,
            panel_maximized: self.panel_maximized,
            panel_tab: self.panel_tab,
            expanded,
            groups,
            active_group: self.active_group,
            breakpoints: self.debug.breakpoints.iter().map(|(p, b)| (p.clone(), b.clone())).collect(),
            breakpoints_off: !self.debug.active,
            watches: self.debug.watches.clone(),
            exception_breakpoints: self.debug.exception_choices.clone(),
            debug_config: self.debug.selected_config.clone(),
            aux_visible: self.aux.visible,
            aux_width: self.aux.width,
            aux_tab: self.aux.tab,
            debug_sections: Some(self.debug.view.open.iter().zip(&self.debug.view.heights).map(|(o, h)| (*o, *h)).collect()),
            assistant_chats,
            assistant_active,
        }
    }

    /// Saves the open folder's editors and layout (with unsaved contents if `backups`), and
    /// the window and last folder.
    pub(super) fn save_session(&self, backups: bool) {
        let folder = self.workspace_id();
        write(&workspace_file(folder.as_deref()), &self.capture(backups));
        let path = state_dir().join("global.json");
        let mut global: Global = read(&path);
        global.last_folder = folder.clone();
        global.outline = Some(OutlineOptions { follow_cursor: self.outline.follow_cursor, sort: self.outline.sort });
        if let Some(f) = folder {
            push_recent(&mut global.recent, f);
        }
        if let Some(w) = &self.window {
            let scale = w.scale_factor();
            let size = w.inner_size().to_logical::<f64>(scale);
            if let Ok(pos) = w.outer_position() {
                let pos = pos.to_logical::<f64>(scale);
                global.window = Some(WindowBounds { x: pos.x, y: pos.y, width: size.width, height: size.height, maximized: w.is_maximized() });
            }
        }
        write(&path, &global);
    }

    /// Reopens the editors and layout saved for the current folder (or for no folder).
    pub(super) fn restore_session(&mut self) {
        if self.folder().is_none() && self.settings.string("window.restoreWindows") == "folders" {
            return;
        }
        let global: Global = read(&state_dir().join("global.json"));
        if let Some(o) = global.outline {
            self.outline.follow_cursor = o.follow_cursor;
            self.outline.sort = o.sort;
        }
        let root = self.folder();
        let state: WorkspaceState = read(&workspace_file(self.workspace_id().as_deref()));
        self.debug.breakpoints = state.breakpoints.iter().cloned().collect();
        self.debug.active = !state.breakpoints_off;
        self.debug.watches = state.watches.clone();
        self.debug.exception_choices = state.exception_breakpoints.clone();
        self.debug.selected_config = state.debug_config.clone();
        for (i, (open, h)) in state.debug_sections.iter().flatten().enumerate().take(self.debug.view.open.len()) {
            self.debug.view.open[i] = *open;
            self.debug.view.heights[i] = *h;
        }
        self.assistant_restore(&state.assistant_chats, state.assistant_active.as_deref());
        if state.groups.is_empty() && state.view.is_empty() {
            return; // nothing saved yet
        }
        self.sidebar_visible = state.sidebar_visible;
        if state.sidebar_width > 0.0 {
            self.sidebar_w = state.sidebar_width;
        }
        self.view = view_from(&state.view);
        self.panel_visible = state.panel_visible && state.panel_tab != super::PANEL_TERMINAL;
        if state.panel_height > 0.0 {
            self.panel_h = state.panel_height;
        }
        self.panel_maximized = state.panel_maximized;
        self.panel_tab = state.panel_tab;
        self.aux.visible = state.aux_visible;
        if state.aux_width > 0.0 {
            self.aux.width = state.aux_width;
        }
        self.aux.tab = state.aux_tab;
        if let (Some(tree), Some(root)) = (&mut self.tree, &root) {
            let paths: Vec<PathBuf> = state.expanded.iter().map(|p| root.join(p)).collect();
            tree.expand_paths(&paths);
        }

        let mut groups = Vec::new();
        let mut settings_at = None;
        for (gi, gs) in state.groups.iter().enumerate() {
            let mut tabs = Vec::new();
            for t in &gs.tabs {
                if t.settings {
                    settings_at = Some((gi, tabs.len()));
                    continue;
                }
                // An image reopens in its preview.
                if let Some(path) = t.path.as_ref().filter(|p| crate::imageio::is_image(p) && p.is_file()) {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let mut doc = Doc::virtual_named(&name);
                    doc.set_path(path.clone());
                    let mut ed = EditorState::new(self.add_doc(doc));
                    ed.image = Some(Box::new(super::image_view::ImagePreview::load(path)));
                    ed.preview = t.preview;
                    tabs.push(ed);
                    continue;
                }
                // A saved search reopens in a search editor.
                if let Some(path) = t.path.as_ref().filter(|p| crate::search_editor::is_search_file(p) && p.is_file()) {
                    if let Some(ed) = self.saved_search_tab(path) {
                        tabs.push(ed);
                    }
                    continue;
                }
                let doc = match (&t.path, t.untitled) {
                    (Some(path), _) => {
                        // A file deleted since, with no unsaved contents, is dropped.
                        if !path.is_file() && t.backup.is_none() {
                            continue;
                        }
                        let existing = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path.as_path())));
                        match existing {
                            Some(i) => i,
                            None => match Doc::open(path.clone()) {
                                Ok(doc) => self.add_doc(doc),
                                Err(_) => {
                                    let mut doc = Doc::virtual_named(&path.to_string_lossy());
                                    doc.set_path(path.clone());
                                    self.add_doc(doc)
                                }
                            },
                        }
                    }
                    (None, Some(n)) => {
                        self.untitled_count = self.untitled_count.max(n);
                        self.add_doc(Doc::untitled(n))
                    }
                    _ => continue,
                };
                if let (Some(text), Some(d)) = (&t.backup, self.docs[doc].as_mut()) {
                    if *text != d.buffer.text() {
                        let all = Selection { anchor: Pos::new(0, 0), head: d.buffer.end(), goal_col: None };
                        d.buffer.insert(all, text);
                        d.buffer.break_undo_group();
                    }
                }
                let mut ed = EditorState::new(doc);
                if let Some(d) = self.docs[doc].as_ref() {
                    let [al, ac, hl, hc] = t.selection;
                    let anchor = d.buffer.clamp(Pos::new(al, ac));
                    let head = d.buffer.clamp(Pos::new(hl, hc));
                    ed.set_selection(Selection { anchor, head, goal_col: None });
                }
                if let Some(d) = self.docs[doc].as_ref() {
                    ed.folds.restore(&d.buffer, t.folds.clone());
                }
                ed.preview = t.preview;
                ed.scroll_y = t.scroll[0];
                ed.scroll_x = t.scroll[1];
                tabs.push(ed);
            }
            let active = gs.active.min(tabs.len().saturating_sub(1));
            groups.push(Group { tabs, active, find: Default::default() });
        }
        // Drop groups that came back empty, but keep at least one.
        let mut active_group = state.active_group;
        let mut i = 0;
        groups.retain(|g| {
            let keep = !g.tabs.is_empty() || settings_at.is_some_and(|(sg, _)| sg == i);
            if !keep && i < active_group {
                active_group -= 1;
            }
            i += 1;
            keep
        });
        if groups.is_empty() {
            groups.push(Group { tabs: Vec::new(), active: 0, find: Default::default() });
        }
        self.active_group = active_group.min(groups.len() - 1);
        self.groups = groups;
        if let Some((g, i)) = settings_at {
            let (prev_group, prev_active) = (self.active_group, self.groups.get(g).map(|gr| gr.active));
            self.active_group = g.min(self.groups.len() - 1);
            let group = &mut self.groups[self.active_group];
            group.active = i.min(group.tabs.len()).saturating_sub(1);
            self.open_settings_ui();
            // open_settings_ui focuses the settings tab; keep the saved active tab.
            self.active_group = prev_group;
            if let (Some(a), Some(group)) = (prev_active, self.groups.get_mut(g)) {
                if a >= i {
                    group.active = a;
                }
            }
        }
        self.focus = Focus::Editor;
    }

    /// Quits (⌘Q, closing the window): with hot exit, remembers unsaved changes; otherwise
    /// asks to save them. Returns false if the user cancelled.
    pub fn quit(&mut self) -> bool {
        let hot = self.hot_exit();
        if !hot && self.has_unsaved() && !self.close_all() {
            return false;
        }
        self.save_session(hot);
        self.shutdown();
        self.install_update_on_quit();
        true
    }

    /// The app is terminating without asking (logout, Dock "Quit"): save what we can.
    pub fn terminating(&mut self) {
        self.save_session(self.hot_exit());
        self.shutdown();
        self.install_update_on_quit();
    }

    /// Switches folders: remembers the current folder's session, closes its editors (with
    /// hot exit their unsaved changes are kept), then opens `path` with its saved session.
    /// Returns false if the user cancelled.
    pub(super) fn switch_folder(&mut self, path: &Path) -> bool {
        let hot = self.hot_exit();
        if !hot && self.has_unsaved() && !self.close_all() {
            return false;
        }
        self.save_session(hot);
        self.clear_docs();
        self.groups = vec![Group { tabs: Vec::new(), active: 0, find: Default::default() }];
        self.active_group = 0;
        self.open_folder_raw(path);
        self.restore_session();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_files_are_stable_per_folder() {
        let a = workspace_file(Some(Path::new("/Users/me/project")));
        assert_eq!(a, workspace_file(Some(Path::new("/Users/me/project"))));
        assert_ne!(a, workspace_file(Some(Path::new("/Users/me/other"))));
        assert!(workspace_file(None).ends_with("empty.json"));
    }

    #[test]
    fn state_round_trips() {
        let state = WorkspaceState {
            sidebar_visible: true,
            sidebar_width: 300.0,
            view: "scm".into(),
            groups: vec![GroupState {
                tabs: vec![TabState { path: Some("/a/b.rs".into()), backup: Some("fn x() {}".into()), selection: [1, 2, 3, 4], scroll: [18.0, 0.0], ..Default::default() }],
                active: 0,
            }],
            ..Default::default()
        };
        let text = serde_json::to_string(&state).unwrap();
        let back: WorkspaceState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, state);
    }

    /// Opening another folder while a file shows used to crash: features keep state per
    /// document index, and the old indexes outlived the documents.
    #[test]
    fn switching_folders_forgets_the_old_documents() {
        let dir = std::env::temp_dir().join(format!("orbvane-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("main.rs"), "fn main() {}\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(a.clone()), &[a.join("main.rs")], std::sync::Arc::new(|| {}));
        let tick = |wb: &mut Workbench| {
            wb.inlay_tick();
            wb.lens_tick();
            wb.semantic_tick();
        };
        tick(&mut wb);
        assert_eq!(wb.on_screen_docs().len(), 1);
        assert!(wb.switch_folder(&b));
        tick(&mut wb);
        assert!(wb.on_screen_docs().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
