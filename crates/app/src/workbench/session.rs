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
    /// The other windows open at the last quit (`last_folder` and `window` are the first's).
    #[serde(default)]
    other_windows: Vec<SavedWindow>,
}

/// A window to reopen: its folder (None: an empty window) and place.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SavedWindow {
    pub folder: Option<PathBuf>,
    pub bounds: Option<WindowBounds>,
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
    /// The file (None: an untitled document).
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

fn reopenable(f: &Path) -> bool {
    f.is_dir() || (crate::workspace::is_workspace_file(f) && f.is_file())
}

/// The windows to reopen after the first one (which reopens `folder_to_restore`) on a launch
/// without arguments (`window.restoreWindows`).
pub fn other_windows_to_restore(store: &settings::Store) -> Vec<SavedWindow> {
    windows_from(read(&state_dir().join("global.json")), &store.string("window.restoreWindows"))
}

fn windows_from(global: Global, mode: &str) -> Vec<SavedWindow> {
    global
        .other_windows
        .into_iter()
        .filter(|w| match &w.folder {
            Some(f) => mode != "none" && reopenable(f),
            None => mode == "all",
        })
        .collect()
}

/// Remembers the windows open when quitting, the one in front first: it reopens with the
/// launch (`folder_to_restore`, `window_bounds`), the others after it.
pub fn save_windows(windows: &[SavedWindow]) {
    let path = state_dir().join("global.json");
    let mut global: Global = read(&path);
    remember_windows(&mut global, windows);
    write(&path, &global);
}

fn remember_windows(global: &mut Global, windows: &[SavedWindow]) {
    let mut list: Vec<SavedWindow> = Vec::new();
    for w in windows {
        if w.folder.is_none() || !list.iter().any(|l| l.folder == w.folder) {
            list.push(w.clone());
        }
    }
    let mut list = list.into_iter();
    if let Some(first) = list.next() {
        global.last_folder = first.folder;
        if first.bounds.is_some() {
            global.window = first.bounds;
        }
    }
    global.other_windows = list.collect();
}

/// The folder to reopen on a launch without arguments (`window.restoreWindows`).
pub fn folder_to_restore(store: &settings::Store) -> Option<PathBuf> {
    if store.string("window.restoreWindows") == "none" {
        return None;
    }
    read::<Global>(&state_dir().join("global.json")).last_folder.filter(|f| reopenable(f))
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
        if let Some(b) = self.window_bounds_now() {
            global.window = Some(b);
        }
        write(&path, &global);
    }

    /// The other windows of the last session to reopen with this, the first one.
    pub fn windows_to_restore(&self) -> Vec<SavedWindow> {
        other_windows_to_restore(&self.settings)
    }

    /// This window as it is now, to reopen it later.
    pub fn saved_window(&self) -> SavedWindow {
        SavedWindow { folder: self.workspace_id(), bounds: self.window_bounds_now() }
    }

    fn window_bounds_now(&self) -> Option<WindowBounds> {
        let w = self.window.as_ref()?;
        let scale = w.scale_factor();
        let size = w.inner_size().to_logical::<f64>(scale);
        let pos = w.outer_position().ok()?.to_logical::<f64>(scale);
        Some(WindowBounds { x: pos.x, y: pos.y, width: size.width, height: size.height, maximized: w.is_maximized() })
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
        for gs in &state.groups {
            let mut tabs = Vec::new();
            for t in &gs.tabs {
                // Settings used to be a tab; it's a sheet now and isn't restored.
                if t.settings {
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
            let keep = !g.tabs.is_empty();
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

    /// Closes this window (one of several): like quitting, for this window only. Returns
    /// false if the user cancelled.
    pub fn close_window(&mut self) -> bool {
        let hot = self.hot_exit();
        if !hot && self.has_unsaved() && !self.close_all() {
            return false;
        }
        self.save_session(hot);
        self.shutdown();
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
    fn windows_are_remembered_in_order() {
        let dir = std::env::temp_dir().join(format!("orbvane-windows-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let bounds = WindowBounds { x: 10.0, y: 20.0, width: 800.0, height: 600.0, maximized: false };
        let win = |f: Option<&PathBuf>| SavedWindow { folder: f.cloned(), bounds: Some(bounds) };
        let mut global = Global::default();
        // The one in front first; the same folder twice is one window.
        remember_windows(&mut global, &[win(Some(&b)), win(None), win(Some(&a)), win(Some(&b)), win(None)]);
        assert_eq!(global.last_folder.as_ref(), Some(&b));
        assert_eq!(global.window, Some(bounds));
        let folders: Vec<Option<PathBuf>> = global.other_windows.iter().map(|w| w.folder.clone()).collect();
        assert_eq!(folders, vec![None, Some(a.clone()), None]);

        let reopened = |mode: &str| -> Vec<Option<PathBuf>> {
            let g: Global = serde_json::from_str(&serde_json::to_string(&global).unwrap()).unwrap();
            windows_from(g, mode).into_iter().map(|w| w.folder).collect()
        };
        assert_eq!(reopened("all"), vec![None, Some(a.clone()), None]);
        assert_eq!(reopened("folders"), vec![Some(a.clone())]);
        assert!(reopened("none").is_empty());
        // Folders that are gone stay closed.
        std::fs::remove_dir_all(&a).unwrap();
        assert_eq!(reopened("folders"), Vec::<Option<PathBuf>>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_open_in_a_new_window_once_one_is_open() {
        let dir = std::env::temp_dir().join(format!("orbvane-open-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let opened = |wb: &mut Workbench| -> Vec<(PathBuf, bool)> {
            wb.take_effects()
                .into_iter()
                .filter_map(|e| match e {
                    super::super::Effect::OpenFolder { path, new_window } => Some((path, new_window)),
                    _ => None,
                })
                .collect()
        };
        // An empty window takes the folder itself.
        let mut empty = Workbench::new_window(None, std::sync::Arc::new(|| {}));
        empty.take_effects();
        empty.open_folder_by_user(&a);
        assert_eq!(opened(&mut empty), vec![(a.clone(), false)]);
        // One with a folder asks for a new window; its own folder does nothing.
        let mut wb = Workbench::new_window(Some(a.clone()), std::sync::Arc::new(|| {}));
        wb.take_effects();
        wb.open_folder_by_user(&b);
        assert_eq!(opened(&mut wb), vec![(b.clone(), true)]);
        wb.open_folder_by_user(&a);
        assert!(opened(&mut wb).is_empty());
        // "off": replaces it.
        wb.settings.set(settings::Scope::Workspace, "window.openFoldersInNewWindow", Some(serde_json::json!("off"))).unwrap();
        wb.open_folder_by_user(&b);
        assert_eq!(opened(&mut wb), vec![(b.clone(), false)]);
        wb.shutdown();
        empty.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

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
