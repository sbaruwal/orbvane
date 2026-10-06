//! Accessibility, the half that knows nothing about macOS: what the window shows to assistive
//! technology (VoiceOver) and the answers about it. Each frame `draw` collects the window's areas,
//! the editors' text areas and the lists as `Node`s (like `hits`); at its end `a11y_finish` adds
//! the text fields drawn (`widgets::take_drawn_fields`) and a node for every control in `hits`
//! that has a name (`a11y_name` while drawing, else `control`). `crate::a11y` turns them into the
//! platform's accessibility elements and asks the questions below when VoiceOver does.
//! Offsets and lengths are UTF-16 code units, lines are buffer lines.

use render::Rect;
use text::{Pos, Selection};

use super::{find_widget, scm_view, search_view, Focus, Hit, Workbench};
use crate::editor::{line_height, Doc, EditorState};

/// The window's areas.
pub const TOOLBAR: u64 = 1;
pub const SIDEBAR: u64 = 2;
pub const EDITORS: u64 = 3;
pub const PANEL: u64 = 4;
pub const STATUS_BAR: u64 = 5;
pub const SECONDARY_SIDEBAR: u64 = 6;
/// The Settings sheet (modal: what's inside it belongs to it, not to the areas below).
pub const SETTINGS: u64 = 7;
/// Lists: the Explorer's files, the palette's results, Problems, Source Control's changes and
/// Search's results. Their rows are `item(list, index)`.
pub const EXPLORER_LIST: u64 = 10;
pub const PALETTE_LIST: u64 = 11;
pub const PROBLEMS_LIST: u64 = 12;
pub const SCM_LIST: u64 = 13;
pub const SEARCH_LIST: u64 = 14;
/// Run and Debug's sections: `DEBUG_LIST + section`.
pub const DEBUG_LIST: u64 = 20;
/// The Assistant's conversation (its entries).
pub const TRANSCRIPT_LIST: u64 = 30;
/// An editor group's text area: `TEXT_AREA + group`.
pub const TEXT_AREA: u64 = 100;
/// The text fields drawn, in drawing order: `FIELD + index`.
pub const FIELD: u64 = 1 << 41;
/// Controls: `CONTROL |` a hash of their `Hit`, so they keep their identity between frames.
const CONTROL: u64 = 1 << 62;

/// Row `index` of `list`.
pub fn item(list: u64, index: usize) -> u64 {
    (list << 32) | index as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Group,
    TextArea,
    List,
    /// A row of a list (read as its label).
    Item,
    Button,
    /// A tab of a tab strip or switcher (`selected` = the one shown).
    Tab,
    /// An on/off control (`selected` = on).
    CheckBox,
    /// A single-line text field (`value`, `selection`).
    TextField,
    /// A button that opens a menu of values (`value` = the current one).
    PopUp,
}

/// A control named while it's drawn.
pub(super) struct Named {
    pub hit: Hit,
    pub role: Role,
    pub label: String,
    pub selected: bool,
    pub value: Option<String>,
    /// Said after a pause (a setting's description).
    pub help: String,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: u64,
    pub parent: Option<u64>,
    pub role: Role,
    pub label: String,
    /// In window coordinates.
    pub frame: Rect,
    /// A row or tab that's selected, a check box that's on.
    pub selected: bool,
    /// A text field's text.
    pub value: Option<String>,
    /// A text field's selection: (start, length).
    pub selection: (usize, usize),
    /// A text field that has the keyboard.
    pub focused: bool,
    /// Said after a pause.
    pub help: String,
}

impl Node {
    fn new(id: u64, parent: Option<u64>, role: Role, label: String, frame: Rect) -> Node {
        Node { id, parent, role, label, frame, selected: false, value: None, selection: (0, 0), focused: false, help: String::new() }
    }
}

/// What notifications compare between frames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub focused: Option<u64>,
    pub version: u64,
    pub selection: (usize, usize),
    /// The selected suggestion while typing (said without moving focus).
    pub suggestion: Option<String>,
    /// Counts what `a11y_say` asked to say (`a11y_note` is the last).
    pub note: u64,
}

/// How a file's git status is said.
pub fn status_word(s: scm::FileStatus) -> &'static str {
    use scm::FileStatus::*;
    match s {
        Modified => "modified",
        Added => "added",
        Deleted => "deleted",
        Renamed => "renamed",
        Copied => "copied",
        TypeChanged => "type changed",
        Untracked => "untracked",
        Conflicted => "conflict",
    }
}

impl Workbench {
    /// One of the window's areas, drawn at `frame`.
    pub(super) fn a11y_area(&mut self, id: u64, label: &str, frame: Rect) {
        self.a11y.push(Node::new(id, None, Role::Group, label.into(), frame));
    }

    /// Group `g`'s editor text (`frame` is where the text is drawn).
    pub(super) fn a11y_text_area(&mut self, g: usize, frame: Rect) {
        let Some((_, doc)) = self.a11y_editor_of(g) else { return };
        let label = doc.title();
        self.a11y.push(Node::new(TEXT_AREA + g as u64, Some(EDITORS), Role::TextArea, label, frame));
    }

    /// A list inside `parent` (None: on its own, like the palette).
    pub(super) fn a11y_list(&mut self, id: u64, parent: Option<u64>, label: &str, frame: Rect) {
        self.a11y.push(Node::new(id, parent, Role::List, label.into(), frame));
    }

    /// Row `index` of `list`, as drawn.
    pub(super) fn a11y_item(&mut self, list: u64, index: usize, label: String, frame: Rect, selected: bool) {
        self.a11y.push(Node { selected, ..Node::new(item(list, index), Some(list), Role::Item, label, frame) });
    }

    /// Names the control drawn for `hit` (its text, or what it does when it's an icon).
    pub(super) fn a11y_name(&mut self, hit: Hit, role: Role, label: impl Into<String>, selected: bool) {
        self.a11y_names.push(Named { hit, role, label: label.into(), selected, value: None, help: String::new() });
    }

    /// Names a control that has a value or a description too.
    pub(super) fn a11y_name_with(&mut self, named: Named) {
        self.a11y_names.push(named);
    }

    /// The end of a frame: the text fields drawn and the named controls become nodes, inside the
    /// innermost area or list around them.
    pub(super) fn a11y_finish(&mut self) {
        let fields = crate::widgets::take_drawn_fields();
        for (i, f) in fields.into_iter().enumerate() {
            let utf16 = |chars: usize| f.text.chars().take(chars).map(char::len_utf16).sum::<usize>();
            let (a, b) = (utf16(f.selection.0), utf16(f.selection.1));
            let parent = self.a11y_container(f.frame);
            let node = Node::new(FIELD + i as u64, parent, Role::TextField, f.placeholder, f.frame);
            self.a11y.push(Node { value: Some(f.text), selection: (a, b - a), focused: f.focused, ..node });
        }
        self.a11y_presses.clear();
        let names = std::mem::take(&mut self.a11y_names);
        let hits = std::mem::take(&mut self.hits);
        // Under the Settings sheet's backdrop nothing can be reached.
        let backdrop = hits.iter().position(|h| h.1 == Hit::Settings(super::settings_view::SettingsHit::Backdrop)).unwrap_or(0);
        for &(rect, hit) in &hits[backdrop..] {
            let named = names.iter().rev().find(|n| n.hit == hit).map(|n| (n.role, n.label.clone(), n.selected, n.value.clone(), n.help.clone()));
            let Some((role, label, selected, value, help)) = named.or_else(|| self.control(hit).map(|(r, l, s)| (r, l, s, None, String::new()))) else {
                continue;
            };
            let id = control_id(hit);
            // A text field drawn inside it while it's edited stands for it (with its name).
            if role == Role::TextField {
                let inside = |n: &Node| n.role == Role::TextField && n.id & FIELD != 0 && rect.contains(n.frame.x + n.frame.w / 2.0, n.frame.y + n.frame.h / 2.0);
                if let Some(field) = self.a11y.iter_mut().find(|n| inside(n)) {
                    if field.label.is_empty() {
                        field.label = label;
                    }
                    field.help = help;
                    continue;
                }
            }
            let parent = self.a11y_container(rect);
            // Later pushes win, as for clicks.
            self.a11y.retain(|n| n.id != id);
            self.a11y.push(Node { selected, value, help, ..Node::new(id, parent, role, label, rect) });
            self.a11y_presses.retain(|p| p.0 != id);
            self.a11y_presses.push((id, hit));
        }
        self.hits = hits;
    }

    /// The innermost area or list around `r`'s center (inside the Settings sheet when it's
    /// there, whatever is under it).
    fn a11y_container(&self, r: Rect) -> Option<u64> {
        let (x, y) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let modal = self.a11y_node(SETTINGS).filter(|n| n.frame.contains(x, y)).is_some();
        self.a11y
            .iter()
            .filter(|n| matches!(n.role, Role::Group | Role::List) && n.frame.contains(x, y))
            .filter(|n| !modal || n.id == SETTINGS || n.parent == Some(SETTINGS))
            .min_by(|a, b| (a.frame.w * a.frame.h).total_cmp(&(b.frame.w * b.frame.h)))
            .map(|n| n.id)
    }

    /// What a control does, when it's drawn without a name of its own.
    fn control(&self, hit: Hit) -> Option<(Role, String, bool)> {
        let button = |s: &str| Some((Role::Button, s.to_string(), false));
        let check = |s: &str, on: bool| Some((Role::CheckBox, s.to_string(), on));
        match hit {
            Hit::CommandCenter => button("Search files and commands"),
            Hit::ToggleSidebarButton => button("Toggle Primary Side Bar"),
            Hit::TogglePanelButton => button("Toggle Panel"),
            Hit::ToggleAuxButton => button("Toggle Secondary Side Bar"),
            Hit::ToolbarRun => button(if self.debug.session.is_some() { "Stop" } else { "Run" }),
            Hit::Activity(v) => Some((Role::Tab, v.title().to_string(), self.view == v)),
            Hit::SwitcherMore => button("More Views"),
            Hit::Manage => button("Manage"),
            Hit::OpenFolderButton => button("Open Folder"),
            Hit::TabClose(g, i) => {
                let title = self.groups.get(g).and_then(|gr| gr.tabs.get(i)).and_then(|e| self.docs.get(e.doc)?.as_ref()).map(|d| d.title());
                Some((Role::Button, format!("Close {}", title.unwrap_or_default()), false))
            }
            Hit::SplitButton(_) => button("Split Editor Right"),
            Hit::PreviewButton(_) => button("Open Preview to the Side"),
            Hit::PanelMaximize => button("Maximize Panel"),
            Hit::PanelClose => button("Close Panel"),
            Hit::StatusSync => button("Synchronize Changes"),
            Hit::NewTerminal => button("New Terminal"),
            Hit::SplitTerminal => button("Split Terminal"),
            Hit::TerminalTabAction(_, _, split) => button(if split { "Split Terminal" } else { "Kill Terminal" }),
            Hit::Lightbulb(_) => button("Show Code Actions"),
            Hit::SignatureCycle(next) => button(if next { "Next Signature" } else { "Previous Signature" }),
            Hit::SearchToggle(t) => {
                let s = &self.search;
                let (label, on) = match t {
                    search_view::SearchToggle::MatchCase => ("Match Case", s.case_sensitive),
                    search_view::SearchToggle::WholeWord => ("Match Whole Word", s.whole_word),
                    search_view::SearchToggle::Regex => ("Use Regular Expression", s.regex),
                    search_view::SearchToggle::UseIgnoreFiles => ("Use Exclude Settings and Ignore Files", s.use_ignore_files),
                    search_view::SearchToggle::ShowReplace => ("Toggle Replace", s.show_replace),
                    search_view::SearchToggle::ShowDetails => ("Toggle Search Details", s.show_details),
                };
                check(label, on)
            }
            Hit::ReplaceAll => button("Replace All"),
            Hit::SearchRefresh => button("Refresh"),
            Hit::SearchClear => button("Clear Search Results"),
            Hit::SearchCollapse => button("Collapse All"),
            Hit::SearchOpenInEditor => button("Open in Editor"),
            Hit::FindAction(g, a) => {
                let fw = &self.groups.get(g)?.find;
                match a {
                    find_widget::FindAction::MatchCase => check("Match Case", fw.case_sensitive),
                    find_widget::FindAction::WholeWord => check("Match Whole Word", fw.whole_word),
                    find_widget::FindAction::Regex => check("Use Regular Expression", fw.regex),
                    find_widget::FindAction::Previous => button("Previous Match"),
                    find_widget::FindAction::Next => button("Next Match"),
                    find_widget::FindAction::Close => button("Close"),
                    find_widget::FindAction::ReplaceOne => button("Replace"),
                    find_widget::FindAction::ReplaceAll => button("Replace All"),
                    find_widget::FindAction::ToggleReplace => button("Toggle Replace"),
                }
            }
            Hit::Scm(a) => match a {
                scm_view::ScmAction::ToggleSection(_) => None, // a row of the changes list
                a => Some((Role::Button, words(&format!("{a:?}")), false)),
            },
            Hit::ExplorerAction(i) => button(["New File", "New Folder", "Refresh Explorer", "Collapse Folders"].get(i)?),
            Hit::OutlineAction(i) => button(if i == 0 { "Collapse All" } else { "More Actions" }),
            Hit::ProblemsFilterMenu => button("Filter"),
            Hit::OutputChannels => button("Output Channel"),
            Hit::DebugRowAction(s, row, k) => {
                let full = self.a11y_node(item(DEBUG_LIST + s as u64, row)).map(|n| n.label.clone()).unwrap_or_default();
                let line = full.split(", ").next().unwrap_or_default();
                match k {
                    9 => Some((Role::CheckBox, format!("Enable {line}"), full.ends_with(", enabled"))),
                    0 => Some((Role::Button, format!("Edit {line}"), false)),
                    _ => Some((Role::Button, format!("Remove {line}"), false)),
                }
            }
            Hit::DebugToolbar(b) => Some((Role::Button, words(&format!("{b:?}")), false)),
            Hit::DebugStartButton => button("Start Debugging"),
            Hit::DebugConfigPicker => button("Debug Configuration"),
            Hit::DebugGear => button("Open launch.json"),
            Hit::DebugCreateLaunch => button("Create a launch.json File"),
            Hit::TestingButton(b) => Some((Role::Button, words(&format!("{b:?}")), false)),
            Hit::PeekClose => button("Close"),
            Hit::MergeComplete(_) => button("Complete Merge"),
            Hit::OpenMergeEditor(_) => button("Resolve in Merge Editor"),
            Hit::AuxClose => button("Close Secondary Side Bar"),
            Hit::AssistantCustomAgent => button("Use a Custom Agent"),
            Hit::AssistantChip => button("Active File"),
            Hit::AssistantAgentMenu => button("Agent"),
            Hit::AssistantNewChat => button("New Chat"),
            Hit::AssistantHistory => button("History"),
            Hit::AssistantModeMenu => button("Mode"),
            Hit::AssistantModelMenu => button("Model"),
            Hit::AssistantTabClose(_) => button("Close Chat"),
            Hit::AssistantHistoryDelete(_) => button("Delete"),
            Hit::AssistantStop => button("Stop"),
            Hit::AssistantSend => button("Send"),
            Hit::Settings(super::settings_view::SettingsHit::Close) => button("Close Settings"),
            Hit::Settings(super::settings_view::SettingsHit::OpenJson) => button("Edit in settings.json"),
            Hit::Toast(_, super::notifications::ToastHit::Close) => button("Close Notification"),
            Hit::Welcome(super::welcome::WelcomeHit::Run(cmd)) => button(cmd.title()),
            _ => None,
        }
    }

    /// Assistive technology pressed a control: a click in its middle.
    pub fn a11y_press(&mut self, id: u64) {
        let Some(&(_, hit)) = self.a11y_presses.iter().find(|p| p.0 == id) else { return };
        let Some(&(r, _)) = self.hits.iter().rev().find(|h| h.1 == hit) else { return };
        let (x, y) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        self.mouse_down(x, y, false, false, false);
        self.mouse_up();
        self.last_click = None;
    }

    pub fn a11y_node(&self, id: u64) -> Option<&Node> {
        self.a11y.iter().find(|n| n.id == id)
    }

    /// The nodes under `parent` (None: the window's areas), in drawing order.
    pub fn a11y_children(&self, parent: Option<u64>) -> Vec<u64> {
        // The Settings sheet is modal: the window shows only it (and the palette over it).
        let modal = parent.is_none() && self.a11y_node(SETTINGS).is_some();
        self.a11y.iter().filter(|n| n.parent == parent && (!modal || n.id == SETTINGS || n.id == PALETTE_LIST)).map(|n| n.id).collect()
    }

    /// The node with the keyboard: the palette's selected row while it's open, the Explorer's
    /// selected file, the active editor's text.
    pub fn a11y_focused(&self) -> Option<u64> {
        let shown = |id: u64| self.a11y_node(id).is_some().then_some(id);
        if let Some(p) = &self.palette {
            return shown(item(PALETTE_LIST, p.selected)).or_else(|| shown(PALETTE_LIST));
        }
        // A text field (find, search, commit message, the Assistant's box, settings' search).
        if let Some(n) = self.a11y.iter().find(|n| n.role == Role::TextField && n.focused) {
            return Some(n.id);
        }
        if self.settings_active() {
            return None;
        }
        match self.focus {
            Focus::Editor => shown(TEXT_AREA + self.active_group as u64),
            Focus::Explorer => self.tree.as_ref().and_then(|t| t.selected).and_then(|i| shown(item(EXPLORER_LIST, i))).or_else(|| shown(EXPLORER_LIST)),
            _ => None,
        }
    }

    /// The selected rows of `list`.
    pub fn a11y_selected(&self, list: u64) -> Vec<u64> {
        self.a11y.iter().filter(|n| n.parent == Some(list) && n.selected).map(|n| n.id).collect()
    }

    /// Has `text` said without moving focus (an agent's reply, a question it asks).
    pub(super) fn a11y_say(&mut self, text: impl Into<String>) {
        self.a11y_note = (self.a11y_note.0 + 1, text.into());
    }

    /// What `a11y_say` asked to say last.
    pub fn a11y_note(&self) -> &str {
        &self.a11y_note.1
    }

    /// The selected suggestion while the list is open.
    fn a11y_suggestion(&self) -> Option<String> {
        let comp = self.completion.as_ref()?;
        let item = comp.shown.get(comp.selected).map(|&(i, _)| &comp.items[i])?;
        Some(match &item.detail {
            Some(d) if !d.is_empty() => format!("{}, {d}", item.label),
            _ => item.label.clone(),
        })
    }

    /// A text field's node.
    fn a11y_field(&self, id: u64) -> Option<&Node> {
        self.a11y_node(id).filter(|n| n.role == Role::TextField)
    }

    /// Whether assistive technology can press `id` (or focus it, for a text field not being
    /// edited).
    pub fn a11y_pressable(&self, id: u64) -> bool {
        self.a11y_presses.iter().any(|p| p.0 == id)
    }

    /// Group `g`'s active editor when it's a text editor.
    fn a11y_editor_of(&self, g: usize) -> Option<(&EditorState, &Doc)> {
        let group = self.groups.get(g)?;
        let ed = group.tabs.get(group.active).filter(|e| !e.is_special())?;
        let doc = self.docs.get(ed.doc)?.as_ref()?;
        Some((ed, doc))
    }

    fn a11y_editor(&self, id: u64) -> Option<(&EditorState, &Doc)> {
        let g = id.checked_sub(TEXT_AREA)? as usize;
        self.a11y_editor_of(g)
    }

    /// The whole text.
    pub fn a11y_value(&self, id: u64) -> String {
        if let Some(v) = self.a11y_node(id).and_then(|n| n.value.clone()) {
            return v;
        }
        self.a11y_editor(id).map(|(_, d)| d.buffer.text()).unwrap_or_default()
    }

    pub fn a11y_length(&self, id: u64) -> usize {
        if let Some(f) = self.a11y_field(id) {
            return f.value.as_deref().map_or(0, |v| v.encode_utf16().count());
        }
        self.a11y_editor(id).map_or(0, |(_, d)| d.buffer.len_utf16())
    }

    /// The primary selection: (start, length).
    pub fn a11y_selection(&self, id: u64) -> (usize, usize) {
        if let Some(f) = self.a11y_field(id) {
            return f.selection;
        }
        let Some((ed, d)) = self.a11y_editor(id) else { return (0, 0) };
        let (a, z) = ed.sel.ordered();
        let start = d.buffer.utf16_of(a);
        (start, d.buffer.utf16_of(z) - start)
    }

    /// The line `offset` is on.
    pub fn a11y_line_of(&self, id: u64, offset: usize) -> usize {
        self.a11y_editor(id).map_or(0, |(_, d)| d.buffer.pos_of_utf16(offset).line)
    }

    /// Line `line`: (start, length), its line break included.
    pub fn a11y_line_range(&self, id: u64, line: usize) -> (usize, usize) {
        if self.a11y_field(id).is_some() {
            return (0, self.a11y_length(id));
        }
        let Some((_, d)) = self.a11y_editor(id) else { return (0, 0) };
        let b = &d.buffer;
        if line >= b.len_lines() {
            return (b.len_utf16(), 0);
        }
        let start = b.utf16_of(Pos::new(line, 0));
        let end = if line + 1 < b.len_lines() { b.utf16_of(Pos::new(line + 1, 0)) } else { b.len_utf16() };
        (start, end - start)
    }

    pub fn a11y_string(&self, id: u64, start: usize, len: usize) -> String {
        if let Some(f) = self.a11y_field(id) {
            let units: Vec<u16> = f.value.as_deref().unwrap_or_default().encode_utf16().skip(start).take(len).collect();
            return String::from_utf16_lossy(&units);
        }
        self.a11y_editor(id).map(|(_, d)| d.buffer.text_utf16(start, start + len)).unwrap_or_default()
    }

    /// Where a range is drawn (window coordinates): its first row, from its start to its end
    /// (or the text's right edge when it continues on later rows).
    pub fn a11y_range_frame(&self, id: u64, start: usize, len: usize) -> Rect {
        if let Some(f) = self.a11y_field(id) {
            return f.frame;
        }
        let Some((ed, d)) = self.a11y_editor(id) else { return Rect::default() };
        let (a, z) = (d.buffer.pos_of_utf16(start), d.buffer.pos_of_utf16(start + len));
        let (x0, y0) = ed.point_of(d, a);
        let (x1, y1) = ed.point_of(d, z);
        let right = if (y1 - y0).abs() < 0.5 { x1.max(x0 + 1.0) } else { ed.geom.text.right() };
        Rect::new(x0, y0, right - x0, line_height())
    }

    /// The lines on screen: (start, length).
    pub fn a11y_visible(&self, id: u64) -> (usize, usize) {
        if self.a11y_field(id).is_some() {
            return (0, self.a11y_length(id));
        }
        let Some((ed, d)) = self.a11y_editor(id) else { return (0, 0) };
        let b = &d.buffer;
        let rows = ed.layout.row_count(b).max(1);
        let first = ((ed.scroll_y / line_height()).floor().max(0.0) as usize).min(rows - 1);
        let last = (((ed.scroll_y + ed.geom.text.h) / line_height()).ceil() as usize).clamp(first, rows - 1);
        let (l0, l1) = (ed.layout.row(first, b).line, ed.layout.row(last, b).line);
        let start = b.utf16_of(Pos::new(l0, 0));
        let end = if l1 + 1 < b.len_lines() { b.utf16_of(Pos::new(l1 + 1, 0)) } else { b.len_utf16() };
        (start, end - start)
    }

    /// Assistive technology moved the caret or selected text.
    pub fn a11y_select(&mut self, id: u64, start: usize, len: usize) {
        let Some(g) = id.checked_sub(TEXT_AREA).map(|g| g as usize) else { return };
        let Some((_, d)) = self.a11y_editor_of(g) else { return };
        let (anchor, head) = (d.buffer.pos_of_utf16(start), d.buffer.pos_of_utf16(start + len));
        let group = &mut self.groups[g];
        let ed = &mut group.tabs[group.active];
        ed.set_selection(Selection { anchor, head, goal_col: None });
        ed.reveal = true;
    }

    /// What notifications compare from frame to frame.
    pub fn a11y_state(&self) -> State {
        let (suggestion, note) = (self.a11y_suggestion(), self.a11y_note.0);
        let Some(id) = self.a11y_focused() else { return State { suggestion, note, ..State::default() } };
        let version = match self.a11y_field(id) {
            Some(f) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                f.value.hash(&mut h);
                h.finish()
            }
            None => self.a11y_editor(id).map_or(0, |(_, d)| d.buffer.version()),
        };
        State { focused: Some(id), version, selection: self.a11y_selection(id), suggestion, note }
    }
}

/// A control's id: stable while the same `Hit` is drawn.
fn control_id(hit: Hit) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{hit:?}").hash(&mut h);
    CONTROL | (h.finish() & (CONTROL - 1))
}

/// "ContinueOrPause" → "Continue or pause"; a payload ("Stage(2)") is dropped.
fn words(debug: &str) -> String {
    let name = debug.split(['(', ' ', '{']).next().unwrap_or_default();
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
