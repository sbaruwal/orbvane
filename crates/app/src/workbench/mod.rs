//! The workbench: title bar, activity bar, side bar, editor groups, panel and status bar,
//! laid out and drawn.
//!
//! Drawing is immediate-mode. Every frame records clickable regions (`hits`), and mouse
//! input is resolved against the regions from the most recent frame.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use render::{Canvas, Color, Icon, Rect, TextStyle};
use theme::Theme;

use crate::commands::Command;
use crate::config;
use crate::editor::{Doc, EditorState};
use crate::explorer::{walk_files, FileTree};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::palette::{Action, Palette, MAX_VISIBLE};

mod assistant;
mod chat_store;
mod welcome;
mod aux_bar;
mod code_lens;
mod color_picker;
mod controls;
mod color_decorators;
mod debug;
mod debug_view;
mod file_ops;
mod a11y;
mod find_widget;
mod ime_view;
mod folding_ranges;
mod git_actions;
mod inlays;
mod intel;
mod keybindings;
mod lightbulb;
mod linked_editing;
mod emmet_actions;
mod ext_host;
mod ext_languages;
#[cfg(test)]
mod chrome_tests;
#[cfg(test)]
mod ext_api_tests;
mod ext_decorations;
mod ext_views;
mod extensions_view;
mod marketplace;
mod mcp;
mod language_mode;
mod language_servers;
mod outline;
mod problems;
mod notifications;
mod output;
pub(crate) mod markdown_view;
pub(crate) mod image_view;
mod peek;
mod signature;
mod snippets;
mod symbols;
mod tasks;
mod preferences;
mod ranges;
mod recent;
mod refactor;
mod scm_graph;
mod scm_view;
mod search_view;
pub(crate) mod merge_view;
pub(crate) mod search_editor_view;
pub(crate) mod semantic;
mod sections;
mod session;
mod settings_view;
mod tabs;
mod terminal_view;
mod testing_view;
mod timeline;
mod updates;
mod watching;
mod workspaces;

pub(crate) use assistant::AgentAction;
pub use debug::DebugPick;
pub use git_actions::{GitInput, GitPick};
pub use a11y::{Role as A11yRole, State as A11yState};
pub use recent::{dock_folders, recent_folders_for_system};
pub use session::{save_windows, window_bounds, SavedWindow, WindowBounds};

const TITLE_H: f32 = 35.0;
const STATUS_H: f32 = 30.0;
/// The view switcher at the top of the sidebar.
const SWITCHER_H: f32 = 34.0;
/// Labels written in capitals ("OUTLINE", "DEBUG CONSOLE") in title case ("Outline",
/// "Debug Console"), the way they're shown; other labels as they are.
/// "rust-analyzer: Indexing 40%" → "rust-analyzer" (when there's no room for the rest).
fn name_only(text: &str) -> String {
    text.split(':').next().unwrap_or(text).to_string()
}

/// Corner radius of a highlighted list row.
const ROW_RADIUS: f32 = 6.0;

/// Where a list row's highlight (selection, hover) is drawn: inset from the list's sides,
/// rounded, like a Mac sidebar.
fn row_pill(r: Rect) -> Rect {
    Rect::new(r.x + 6.0, r.y + 1.0, (r.w - 12.0).max(0.0), (r.h - 2.0).max(0.0))
}

fn calm(label: &str) -> String {
    if label.chars().any(char::is_lowercase) {
        return label.to_string();
    }
    label
        .split(' ')
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |f| f.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Room for the window's traffic lights at the toolbar's left.
const TRAFFIC_LIGHTS_W: f32 = 78.0;
const TAB_H: f32 = 35.0;
const BREADCRUMB_H: f32 = 22.0;
const PANEL_HEADER_H: f32 = 35.0;
/// The palette's input, rows and group headings.
const PALETTE_INPUT: f32 = 52.0;
const PALETTE_ROW: f32 = 30.0;
const PALETTE_HEADER: f32 = 24.0;
const ROW_H: f32 = 22.0;
const UI: f32 = 13.0;
const SMALL: f32 = 11.0;
const CARET_BLINK: Duration = Duration::from_millis(530);
const PANEL_TABS: &[&str] = &["PROBLEMS", "OUTPUT", "DEBUG CONSOLE", "TERMINAL", "PORTS"];
const PANEL_OUTPUT: usize = 1;
const PANEL_DEBUG_CONSOLE: usize = 2;
const PANEL_TERMINAL: usize = 3;
const PANEL_PORTS: usize = 4;
/// Test Results, shown after PORTS while the folder has tests.
const PANEL_TEST_RESULTS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Explorer,
    Search,
    Scm,
    Debug,
    Extensions,
    /// Shown when the folder has a test provider.
    Testing,
    /// An extension's view container (`contributions::container`).
    Ext(u16),
}

impl View {
    const ALL: [View; 6] = [View::Explorer, View::Search, View::Scm, View::Debug, View::Extensions, View::Testing];

    fn icon(self) -> &'static Icon {
        match self {
            View::Ext(i) => crate::contributions::container(i).map_or(&icons::EXTENSIONS, |c| c.icon),
            View::Explorer => &icons::FILES,
            View::Search => &icons::SEARCH,
            View::Scm => &icons::SOURCE_CONTROL,
            View::Debug => &icons::RUN_DEBUG,
            View::Extensions => &icons::EXTENSIONS,
            View::Testing => &icons::BEAKER,
        }
    }

    fn title(self) -> &'static str {
        match self {
            View::Ext(i) => crate::contributions::container(i).map_or("", |c| c.title),
            View::Explorer => "Explorer",
            View::Search => "Search",
            View::Scm => "Source Control",
            View::Debug => "Run and Debug",
            View::Extensions => "Extensions",
            View::Testing => "Testing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    /// Something in an extension's tree view (an index into `ext_views.list`).
    ExtTree(u16, ext_views::TreeHit),
    /// The Output panel's channel dropdown.
    OutputChannels,
    Toast(u64, notifications::ToastHit),
    /// An extension's status bar item (index into `ExtHost::status`).
    ExtStatus(usize),
    Ext(extensions_view::ExtHit),
    /// A button on an extension's page in group `g`.
    ExtPage(usize, extensions_view::PageHit),
    TitleBar,
    CommandCenter,
    ToggleSidebarButton,
    TogglePanelButton,
    /// The toolbar's project and branch pill halves, and its run/stop button.
    ToolbarProject,
    ToolbarBranch,
    ToolbarRun,
    /// A view in the sidebar's switcher.
    Activity(View),
    /// Views that don't fit the switcher.
    SwitcherMore,
    Manage,
    SidebarSash,
    PanelSash,
    SidebarBody,
    ExplorerSection,
    ExplorerRow(usize),
    /// A symbol in group `g`'s breadcrumbs: (group, index in the symbol path).
    BreadcrumbSymbol(usize, usize),
    /// The signature help popup, and its previous/next overload buttons.
    SignatureHelp,
    SignatureCycle(bool),
    OutlineBody,
    OutlineRow(usize),
    OutlineTwistie(usize),
    /// Collapse All (0) or the "..." menu (1) in the Outline header.
    OutlineAction(usize),
    /// The sash above Explorer section `i`'s header (1: Outline, 2: Timeline).
    SectionSash(usize),
    ScmGraphSection,
    ScmGraphBody,
    ScmGraphRow(usize),
    ScmGraphSash,
    TimelineBody,
    TimelineRow(usize),
    OpenFolderButton,
    Tab(usize, usize),
    TabClose(usize, usize),
    TabBar(usize),
    SplitButton(usize),
    /// Open Preview to the Side, on a Markdown file's tab bar.
    PreviewButton(usize),
    Editor(usize),
    Scrollbar(usize),
    Minimap(usize),
    EmptyGroup(usize),
    PanelTab(usize),
    PanelMaximize,
    PanelClose,
    PanelBody,
    StatusProblems,
    /// The active editor's language (Change Language Mode).
    StatusLanguage,
    StatusItem(usize),
    /// The sync / publish item next to the branch.
    StatusSync,
    /// The active file's language server, and the Terminal item.
    StatusServer,
    StatusTerminal,
    /// An "Accept ... Change" action on a merge conflict: (group, index in the editor's list).
    ConflictAction(usize, usize),
    RenameBox,
    /// A fold chevron in the gutter: (group, region start line).
    FoldControl(usize, usize),
    /// The code action lightbulb in group `g`'s gutter.
    Lightbulb(usize),
    PaletteBackdrop,
    PaletteBox,
    PaletteRow(usize),
    HoverPopup,
    CompletionBox,
    CompletionRow(usize),
    ProblemRow(usize),
    /// A terminal pane of the active group.
    TerminalPane(usize),
    /// A row of the terminal list: (group, pane).
    TerminalTab(usize, usize),
    /// Split (true) or kill (false) from a terminal list row.
    TerminalTabAction(usize, usize, bool),
    NewTerminal,
    SplitTerminal,
    SearchField(search_view::Field),
    SearchToggle(search_view::SearchToggle),
    ReplaceAll,
    SearchRow(usize),
    SearchRefresh,
    SearchClear,
    SearchCollapse,
    FindWidgetBox(usize),
    FindField(usize, find_widget::FindField),
    FindAction(usize, find_widget::FindAction),
    Scm(scm_view::ScmAction),
    ScmMessage,
    ScmRow(usize),
    /// A repository in the Source Control view's Repositories list.
    ScmRepo(usize),
    Diff(usize),
    Settings(settings_view::SettingsHit),
    /// Group `g`'s glyph margin (breakpoints).
    GlyphMargin(usize),
    /// A sticky scroll line: (group, buffer line).
    StickyLine(usize, usize),
    /// A code lens: (group, its index in the server's list).
    CodeLens(usize, usize),
    /// A color swatch in group `g`'s editor, by its color's position.
    ColorSwatch(usize, text::Pos),
    ColorPickerBox,
    ColorPickerPicked,
    ColorPickerOriginal,
    ColorPickerPart(color_picker::Part),
    /// A Markdown preview in group `g`.
    Markdown(usize),
    /// The Problems panel's filter box and filter menu button.
    ProblemsFilterField,
    ProblemsFilterMenu,
    /// A search editor's header (group), its fields and toggles.
    SearchEditorHeader(usize),
    /// The Search view's "Open in editor" link.
    SearchOpenInEditor,
    /// A merge editor (group): an input pane (0 incoming, 1 current), an action or checkbox
    /// there, the conflicts remaining link, the Complete Merge button.
    MergeInput(usize, u8),
    MergeAction(usize, usize),
    MergeRemaining(usize),
    MergeComplete(usize),
    /// "Resolve in Merge Editor" on an editor with conflicts.
    OpenMergeEditor(usize),
    SearchEditorField(usize, search_editor_view::SeField),
    SearchEditorToggle(usize, search_editor_view::SeToggle),
    /// The Testing view: its filter box, tree, rows, twisties, row buttons, title buttons.
    TestingFilter,
    TestingBody,
    TestingRow(usize),
    TestingTwistie(usize),
    TestingRowAction(usize, testing_view::TestAction),
    TestingButton(testing_view::TestingButton),
    /// An image preview in group `g`.
    Image(usize),
    /// The peek view: its body, preview, a list row and the close button.
    PeekBody,
    PeekPreview,
    PeekRow(usize),
    PeekClose,
    /// A call tree node's twistie, and the header's callers/callees switch.
    PeekTwistie(usize),
    PeekToggleCalls,
    /// The Explorer's inline name field, and its header actions (New File, New Folder,
    /// Refresh, Collapse Folders).
    ExplorerEditField,
    ExplorerAction(usize),
    /// Run and Debug: a section's header, body, row, row action (section, row, action),
    /// header action (section, action) and the sash above it.
    DebugSection(usize),
    DebugBody(usize),
    DebugRow(usize, usize),
    DebugRowAction(usize, usize, usize),
    DebugAction(usize, usize),
    DebugSash(usize),
    DebugToolbar(debug_view::ToolbarButton),
    DebugToolbarBody,
    DebugConsoleBody,
    DebugConsoleInput,
    /// The view's start button (▷, or "Run and Debug" before there's a launch.json).
    DebugStartButton,
    DebugConfigPicker,
    DebugGear,
    DebugCreateLaunch,
    /// The toolbar's secondary side bar toggle.
    ToggleAuxButton,
    /// The secondary side bar: its body, a tab of its switcher, its close button, its sash.
    AuxBody,
    AuxTab(u8),
    AuxClose,
    AuxSash,
    /// The Assistant: the setup screen's button for agent i and its custom command link, the
    /// transcript, Review on entry (entry, diff), a permission option (entry, option), a
    /// sign-in method (entry, method), a button of an `Entry::Action` (entry, button), the
    /// active file chip, the agent menu, New Chat, the message box, Stop and Send.
    AssistantAgent(u8),
    AssistantCustomAgent,
    AssistantBody,
    AssistantReview(usize, usize),
    AssistantOption(usize, usize),
    AssistantAuth(usize, usize),
    AssistantAction(usize, usize),
    AssistantChip,
    AssistantAgentMenu,
    AssistantNewChat,
    AssistantHistory,
    AssistantModeMenu,
    Welcome(welcome::WelcomeHit),
    AssistantModelMenu,
    /// A chat's tab, and its close button.
    AssistantTab(usize),
    AssistantTabClose(usize),
    /// A History row, and its delete button.
    AssistantHistoryRow(usize),
    AssistantHistoryDelete(usize),
    AssistantInput,
    AssistantStop,
    AssistantSend,
}

enum Drag {
    SidebarSash,
    AuxSash,
    PanelSash,
    SectionSash(usize),
    DebugSash(usize),
    /// An Explorer row being dragged: where the press was, and whether it has moved.
    ExplorerItem { i: usize, from: (f32, f32), moving: bool },
    ScmGraphSash,
    Select(usize),
    /// ⇧⌥-drag box selection in a group, from (line, visual column).
    Column(usize, (usize, f32)),
    Slider(usize, f32),
    Minimap(usize),
    /// A part of the color picker.
    ColorPicker(color_picker::Part),
    /// A tab being dragged: where it is now, where the press was, and whether it has moved.
    Tab { g: usize, i: usize, from: (f32, f32), moving: bool },
}

/// Requests for the platform layer (window/cursor changes).
pub enum Effect {
    DragWindow,
    ToggleMaximize,
    SetTitle(String),
    Cursor(CursorKind),
    /// Match the native window chrome (dialogs, sheets) to a light or dark theme.
    SetAppearance { dark: bool },
    /// Show a native popup menu at a window position; picks come back via `popup_selected`.
    Popup { items: Vec<PopupItem>, x: f32, y: f32 },
    /// The session is saved; end the app (quitting the other windows first).
    Exit,
    /// Quit: every window keeps or asks about its unsaved changes, then the app ends.
    Quit,
    /// Close this window (closing the last one quits).
    CloseWindow,
    /// Open an empty window.
    NewWindow,
    /// Open a folder the user picked: the window that already has it comes forward, else it
    /// opens in a new window or in this one (`window.openFoldersInNewWindow`).
    OpenFolder { path: PathBuf, new_window: bool },
    /// Shortcuts changed (`keybindings.json`): update the menus' key equivalents.
    KeymapChanged,
    /// The recent folders changed: rebuild File > Open Recent (`Workbench::recent_menu`).
    RecentChanged,
    /// Enter or leave full screen (Zen Mode).
    SetFullscreen(bool),
}

/// An entry of a native popup menu (a dropdown or a context menu).
#[derive(Clone, Debug)]
pub enum PopupItem {
    Item { label: String, enabled: bool, checked: Option<bool> },
    Separator,
    Submenu { label: String, items: Vec<PopupItem> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Default,
    Text,
    ColResize,
    RowResize,
    /// A link-like target (code lenses).
    Pointer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Explorer,
    Terminal,
    Search,
    Find,
    Scm,
    Settings,
    /// The Explorer's Outline section.
    Outline,
    /// The Debug Console's input.
    DebugConsole,
    /// The peek view's list.
    Peek,
    /// The Problems panel's filter box.
    ProblemsFilter,
    /// The inline rename box.
    Rename,
    /// A search editor's header fields.
    SearchEditor,
    /// The Testing view's tree, and its filter box.
    Testing,
    TestingFilter,
    /// The Extensions view's search box.
    Extensions,
    /// The Assistant's message box.
    Assistant,
}

struct Group {
    tabs: Vec<EditorState>,
    active: usize,
    find: find_widget::FindWidget,
}

pub struct Workbench {
    theme: Theme,
    tree: Option<FileTree>,
    /// The `.code-workspace` file of a multi-root workspace (None: a single folder).
    workspace_file: Option<PathBuf>,
    workspace_mtime: Option<std::time::SystemTime>,
    workspace_checked: Option<Instant>,
    git_branch: Option<String>,
    /// Repositories of a multi-root workspace other than `repo` (the one shown).
    parked_repos: Vec<scm_view::ParkedRepo>,
    /// Every repository's root, in folder order.
    repo_roots: Vec<PathBuf>,
    /// The editor the shown repository last followed.
    repo_followed: Option<PathBuf>,
    docs: Vec<Option<Doc>>,
    untitled_count: usize,
    groups: Vec<Group>,
    active_group: usize,
    view: View,
    sidebar_visible: bool,
    sidebar_w: f32,
    panel_visible: bool,
    panel_h: f32,
    panel_maximized: bool,
    panel_tab: usize,
    explorer_open: bool,
    focus: Focus,
    palette: Option<Palette>,
    palette_files: Option<Vec<(PathBuf, String)>>,
    hits: Vec<(Rect, Hit)>,
    hover_hit: Option<Hit>,
    /// Text being composed with an input method, and where the last frame showed it.
    preedit: crate::ime::Preedit,
    /// What this frame shows to assistive technology (`a11y.rs`).
    a11y: Vec<a11y::Node>,
    /// Controls named while drawing this frame: (hit, role, label, selected).
    a11y_names: Vec<a11y::Named>,
    /// The controls assistive technology can press: (node, hit).
    a11y_presses: Vec<(u64, Hit)>,
    /// What to say next without moving focus, numbered (`a11y_say`).
    a11y_note: (u64, String),
    ime_area: Option<Rect>,
    mouse: (f32, f32),
    drag: Option<Drag>,
    last_click: Option<(Instant, f32, f32, u32)>,
    caret_epoch: Instant,
    clipboard: Option<arboard::Clipboard>,
    effects: Vec<Effect>,
    cursor: CursorKind,
    title: String,
    main_rect: Rect,
    lsp: crate::servers::Servers,
    hover: Option<intel::HoverState>,
    hover_probe: Option<intel::HoverProbe>,
    completion: Option<intel::Completion>,
    completion_seq: u64,
    problems_scroll: f32,
    problems_filter: problems::ProblemsFilter,
    /// Problems panel rows that jump to a location: (row, file, position).
    problem_targets: Vec<(usize, PathBuf, lsp::Position)>,
    /// Problems rows that are files (row, file), for collapsing.
    problem_files: Vec<(usize, PathBuf)>,
    waker: lsp::Waker,
    terms: terminal_view::Terminals,
    search: search_view::SearchView,
    /// The native window, used as the parent for dialogs so they appear as sheets on it.
    window: Option<std::sync::Arc<winit::window::Window>>,
    repo: Option<scm::Repo>,
    scm: scm_view::ScmView,
    settings: settings::Store,
    /// `editor.fontFamily`, applied to the renderer while drawing.
    font_family: String,
    /// `workbench.interfaceFont` is "editor": the interface uses the editor's font.
    ui_mono: bool,
    /// The first key of a chord (⌘K) while waiting for the second.
    chord: Option<KeyInput>,
    settings_ui: settings_view::SettingsView,
    /// What each entry of the open native popup menu does.
    popup: Vec<preferences::PopupAction>,
    auto_save: preferences::AutoSaveState,
    /// The theme to restore if the color theme picker is cancelled.
    theme_before_picker: Option<String>,
    /// A transient status bar message (chord prompts) and when it was set.
    status_message: Option<(String, Instant)>,
    git: git_actions::GitState,
    rename: Option<refactor::RenameWidget>,
    /// A rename waiting for the server's `prepareRename`: (group, document, position).
    rename_request: Option<(usize, usize, text::Pos)>,
    /// Quick Fix waiting for its answers.
    code_action_request: Option<refactor::ActionRequest>,
    /// The actions in the open code action menu.
    code_actions: Option<Vec<refactor::Action>>,
    /// The edit sequence each file's diagnostics are anchored to (see `shift_diagnostics`).
    diag_seq: std::collections::HashMap<PathBuf, u64>,
    /// Documents waiting for format on save, and since when.
    format_saves: std::collections::HashMap<usize, Instant>,
    lightbulb: lightbulb::Lightbulb,
    linked: linked_editing::LinkedEditing,
    colors: color_decorators::ColorDecorators,
    color_picker: Option<color_picker::ColorPicker>,
    outline: outline::Outline,
    /// A name being typed in the Explorer (New File, New Folder, Rename).
    explorer_edit: Option<file_ops::ExplorerEdit>,
    file_clipboard: Option<file_ops::FileClipboard>,
    /// The file picked with Select for Compare.
    compare_left: Option<PathBuf>,
    debug: debug::Debug,
    lenses: code_lens::LensState,
    testing: testing_view::Testing,
    ext_host: ext_host::ExtHost,
    ext_views: ext_views::ExtViews,
    ext_languages: ext_languages::ExtLanguages,
    ext_decorations: ext_decorations::ExtDecorations,
    extensions: extensions_view::ExtensionsView,
    marketplace: marketplace::Marketplace,
    updates: updates::Updates,
    /// Language servers found not installed this session, by command.
    missing_servers: std::collections::HashMap<&'static str, crate::servers::MissingServer>,
    /// Language servers being installed: the install task's label → the server's command.
    installing: std::collections::HashMap<String, &'static str>,
    toasts: notifications::Toasts,
    output: output::Output,
    /// The peek view (references, definitions), when open.
    peek: Option<peek::Peek>,
    /// The next definitions answer opens in the peek view (Peek Definition).
    peek_definition: bool,
    /// Zen Mode: what was shown before (side bar, panel), and when Escape was last pressed.
    zen: Option<(bool, bool)>,
    zen_escape: Option<Instant>,
    symbol_search: symbols::SymbolSearch,
    signature: signature::SignatureState,
    snippet: Option<snippets::SnippetSession>,
    inlays: inlays::InlayState,
    semantic: semantic::SemanticState,
    folding: folding_ranges::FoldingState,
    watching: Vec<watching::Watching>,
    /// When to refresh git status because files changed.
    git_nudge: Option<Instant>,
    timeline: timeline::Timeline,
    scm_graph: scm_graph::ScmGraph,
    /// The Explorer's sections as laid out last frame.
    sections: sections::SectionRects,
    /// Where the Outline's "..." menu opens.
    outline_menu_at: (f32, f32),
    /// The secondary side bar, and the Assistant drawn in it.
    aux: aux_bar::AuxBar,
    assistant: assistant::Assistant,
    welcome: welcome::Welcome,
    /// The editor's tools for the agent (MCP).
    tools: mcp::EditorTools,
    /// This window's settings for hot paths (`config::get()` once `activate`d).
    config: config::Config,
}

impl Workbench {
    /// The first window: `folder` and `files` from the command line, else the last folder
    /// (`window.restoreWindows`).
    pub fn new(folder: Option<PathBuf>, files: &[PathBuf], waker: lsp::Waker) -> Self {
        Self::create(folder, files, waker, true)
    }

    /// A window opened while the app runs (New Window, a folder opened in a new window): the
    /// folder with its saved session, or an empty window.
    pub fn new_window(folder: Option<PathBuf>, waker: lsp::Waker) -> Self {
        Self::create(folder, &[], waker, false)
    }

    fn create(folder: Option<PathBuf>, files: &[PathBuf], waker: lsp::Waker, launch: bool) -> Self {
        // Before anything asks which language a file is (extensions add languages). Once per
        // launch: languages are indexes every window shares.
        let languages_problem = if launch {
            crate::contributions::load();
            crate::languages::load()
        } else {
            Ok(())
        };
        let mut wb = Self {
            theme: Theme::default_theme(),
            tree: None,
            workspace_file: None,
            workspace_mtime: None,
            workspace_checked: None,
            git_branch: None,
            parked_repos: Vec::new(),
            repo_roots: Vec::new(),
            repo_followed: None,
            docs: Vec::new(),
            untitled_count: 0,
            groups: vec![Group { tabs: Vec::new(), active: 0, find: Default::default() }],
            active_group: 0,
            view: View::Explorer,
            sidebar_visible: true,
            sidebar_w: 260.0,
            panel_visible: false,
            panel_h: 260.0,
            panel_maximized: false,
            panel_tab: 0,
            explorer_open: true,
            focus: Focus::Editor,
            palette: None,
            palette_files: None,
            hits: Vec::new(),
            hover_hit: None,
            preedit: Default::default(),
            a11y: Vec::new(),
            a11y_names: Vec::new(),
            a11y_presses: Vec::new(),
            a11y_note: (0, String::new()),
            ime_area: None,
            mouse: (0.0, 0.0),
            drag: None,
            last_click: None,
            caret_epoch: Instant::now(),
            clipboard: arboard::Clipboard::new().ok(),
            effects: Vec::new(),
            cursor: CursorKind::Default,
            title: String::new(),
            main_rect: Rect::default(),
            lsp: crate::servers::Servers::new(waker.clone()),
            hover: None,
            hover_probe: None,
            completion: None,
            completion_seq: 0,
            problems_scroll: 0.0,
            problems_filter: Default::default(),
            problem_targets: Vec::new(),
            problem_files: Vec::new(),
            waker,
            terms: Default::default(),
            search: Default::default(),
            window: None,
            repo: None,
            scm: Default::default(),
            settings: settings::Store::new(settings::Store::default_user_path()),
            font_family: String::new(),
            ui_mono: true,
            chord: None,
            settings_ui: Default::default(),
            popup: Vec::new(),
            auto_save: Default::default(),
            theme_before_picker: None,
            status_message: None,
            git: Default::default(),
            rename: None,
            rename_request: None,
            code_action_request: None,
            code_actions: None,
            diag_seq: Default::default(),
            format_saves: Default::default(),
            lightbulb: Default::default(),
            linked: Default::default(),
            colors: Default::default(),
            color_picker: None,
            outline: Default::default(),
            explorer_edit: None,
            file_clipboard: None,
            compare_left: None,
            debug: Default::default(),
            lenses: Default::default(),
            testing: Default::default(),
            ext_host: Default::default(),
            ext_views: Default::default(),
            ext_languages: Default::default(),
            ext_decorations: Default::default(),
            extensions: Default::default(),
            marketplace: Default::default(),
            updates: Default::default(),
            missing_servers: Default::default(),
            installing: Default::default(),
            toasts: Default::default(),
            output: Default::default(),
            peek: None,
            peek_definition: false,
            zen: None,
            zen_escape: None,
            symbol_search: Default::default(),
            signature: Default::default(),
            snippet: None,
            inlays: Default::default(),
            semantic: Default::default(),
            folding: Default::default(),
            watching: Vec::new(),
            git_nudge: None,
            timeline: Default::default(),
            scm_graph: Default::default(),
            sections: Default::default(),
            outline_menu_at: (0.0, 0.0),
            aux: Default::default(),
            assistant: Default::default(),
            welcome: Default::default(),
            tools: Default::default(),
            config: config::Config::default(),
        };
        wb.apply_settings();
        if let Err(e) = crate::keymap::load() {
            wb.set_status_message(&e);
        }
        if let Err(e) = languages_problem {
            wb.set_status_message(&e);
        }
        wb.start_askpass();
        // Without arguments, reopen the last folder (`window.restoreWindows`).
        let folder = if launch { folder.or_else(|| if files.is_empty() { session::folder_to_restore(&wb.settings) } else { None }) } else { folder };
        match folder {
            Some(path) if path.is_dir() || crate::workspace::is_workspace_file(&path) => wb.open_folder(&path),
            Some(path) if path.is_file() => {
                if let Some(parent) = path.parent() {
                    wb.open_folder(parent);
                }
                wb.open_file(&path);
            }
            _ if launch && wb.settings.string("window.restoreWindows") != "none" => wb.restore_session(),
            _ => {}
        }
        for file in files {
            wb.open_file(file);
        }
        // (Tests start without it.)
        if !cfg!(test) {
            wb.welcome_at_startup();
        }
        wb
    }

    pub fn background(&self) -> Color {
        self.theme.color("editor.background")
    }

    pub fn set_window(&mut self, window: std::sync::Arc<winit::window::Window>) {
        self.window = Some(window);
    }

    /// A native alert attached to our window as a sheet. (Without a parent, rfd falls back
    /// to a system notification alert that can end up hidden behind the window.)
    fn message_dialog(&self) -> rfd::MessageDialog {
        let dialog = rfd::MessageDialog::new();
        match &self.window {
            Some(w) => dialog.set_parent(w.as_ref()),
            None => dialog,
        }
    }

    fn file_dialog(&self) -> rfd::FileDialog {
        let dialog = rfd::FileDialog::new();
        match &self.window {
            Some(w) => dialog.set_parent(w.as_ref()),
            None => dialog,
        }
    }

    pub fn mouse_position(&self) -> (f32, f32) {
        self.mouse
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// Whether the pointer is over the title bar (where a click drags the window).
    pub fn title_bar_at_mouse(&self) -> bool {
        self.palette.is_none() && self.hit_at(self.mouse.0, self.mouse.1) == Some(Hit::TitleBar)
    }

    /// When the next redraw is needed without input (caret blink, hover delay).
    pub fn next_wakeup(&self) -> Option<Instant> {
        let focused_editor = self.palette.is_none() && self.focus == Focus::Editor && self.active_editor().is_some();
        let focused_editor = focused_editor && config::get().cursor_blink;
        let focused_field = self.palette.is_some() || self.focus == Focus::Search || self.settings_caret_visible() || (self.focus == Focus::Assistant && self.aux.visible);
        let blink = (focused_editor || focused_field).then(|| {
            let n = self.caret_epoch.elapsed().as_millis() / CARET_BLINK.as_millis() + 1;
            self.caret_epoch + CARET_BLINK * n as u32
        });
        let status = self.status_message.as_ref().filter(|_| self.chord.is_none()).map(|(_, t)| *t + Duration::from_secs(3));
        [blink, self.hover_deadline(), self.search_deadline(), self.scm_deadline(), self.auto_save_deadline(), status, self.git_deadline(), self.terminal_deadline(), self.format_save_deadline(), self.lightbulb_deadline(), self.outline_deadline(), self.symbol_search_deadline(), self.inlay_deadline(), self.semantic_deadline(), self.folding_deadline(), self.watch_deadline(), self.debug_deadline(), self.lens_deadline(), self.testing_deadline(), self.search_editor_deadline(), self.linked_deadline(), self.color_deadline(), self.toasts_deadline(), self.marketplace_deadline(), self.updates_deadline(), self.idle_servers_deadline(), self.assistant_deadline(), self.mcp_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Closes every document at once (switching or closing folders). What features keep per
    /// document is keyed by its index in `docs`, and the indexes get reused, so it all goes too.
    fn clear_docs(&mut self) {
        self.docs.clear();
        self.inlays.forget_docs();
        self.semantic.forget_docs();
        self.folding.forget_docs();
        self.lenses.forget_docs();
        self.colors.forget_docs();
        self.ext_decorations.forget_docs();
        self.git.forget_docs();
        self.auto_save.forget_docs();
        self.format_saves.clear();
        // Popups and requests in flight that point at a document.
        self.hover = None;
        self.hover_probe = None;
        self.completion = None;
        self.rename = None;
        self.color_picker = None;
        self.peek = None;
        self.snippet = None;
        self.lightbulb.forget_docs();
        self.linked = Default::default();
        self.signature = Default::default();
    }

    /// The modifier keys changed (inlay hints can show only while ⌃⌥ is held).
    pub fn set_modifiers(&mut self, ctrl: bool, alt: bool) {
        self.inlays.ctrl_alt = ctrl && alt;
    }

    /// Stops language servers. Call before the app exits.
    pub fn shutdown(&mut self) {
        self.assistant_shutdown();
        self.ext_shutdown();
        self.debug_shutdown();
        self.lsp.shutdown();
        self.terms.clear();
        self.git.askpass = None;
    }

    fn caret_on(&self) -> bool {
        (self.caret_epoch.elapsed().as_millis() / CARET_BLINK.as_millis()) % 2 == 0
    }

    /// The editor caret's blink phase (`editor.cursorBlinking: solid` keeps it on).
    fn editor_caret_on(&self) -> bool {
        !config::get().cursor_blink || self.caret_on()
    }

    fn reset_caret(&mut self) {
        self.caret_epoch = Instant::now();
    }

    pub fn has_unsaved(&self) -> bool {
        self.docs.iter().flatten().any(|d| d.buffer.is_dirty())
    }

    // ------------------------------------------------------------------ documents & editors

    /// Opens a folder in this window with the editors and layout it had last time. Editors
    /// open for another folder (or none) are put away first.
    pub fn open_folder(&mut self, path: &Path) {
        if self.workspace_id().as_deref() == Some(path) {
            return;
        }
        if self.tree.is_some() || self.groups.iter().any(|g| !g.tabs.is_empty()) {
            self.switch_folder(path);
        } else {
            self.open_folder_raw(path);
            self.restore_session();
        }
    }

    /// Opens a folder the user picked (Open Folder, Open Recent, the Welcome page): in a new
    /// window when this one has a folder (`window.openFoldersInNewWindow`), and a window that
    /// has it already comes forward instead (`main.rs` decides, knowing the windows).
    pub(super) fn open_folder_by_user(&mut self, path: &Path) {
        if self.workspace_id().as_deref() == Some(path) {
            return;
        }
        let new_window = match self.settings.string("window.openFoldersInNewWindow").as_str() {
            "on" => true,
            "off" => false,
            _ => self.workspace_id().is_some(),
        };
        self.effects.push(Effect::OpenFolder { path: path.to_path_buf(), new_window });
    }

    /// Makes this window's settings the ones `config::get()` reads (several windows share
    /// the thread, each with its own workspace settings): before handling its input or
    /// drawing it.
    pub fn activate(&self) {
        config::set(self.config);
    }

    /// Opens a folder, or a `.code-workspace` file's folders.
    fn open_folder_raw(&mut self, path: &Path) {
        if crate::workspace::is_workspace_file(path) {
            if !self.load_workspace_file(path) {
                return;
            }
        } else {
            self.workspace_file = None;
            self.settings.set_workspace_folder(Some(path));
            self.tree = Some(FileTree::new(path));
        }
        self.folders_changed();
        self.remember_folder(path);
        self.view = View::Explorer;
        self.sidebar_visible = true;
    }

    fn active_editor(&self) -> Option<&EditorState> {
        let g = self.groups.get(self.active_group)?;
        g.tabs.get(g.active)
    }

    fn active_doc(&self) -> Option<&Doc> {
        self.docs.get(self.active_editor()?.doc)?.as_ref()
    }

    /// The active editor and its document, borrowed together.
    fn active_mut(&mut self) -> Option<(&mut EditorState, &mut Doc)> {
        let g = self.groups.get_mut(self.active_group)?;
        let ed = g.tabs.get_mut(g.active)?;
        let doc = self.docs.get_mut(ed.doc)?.as_mut()?;
        Some((ed, doc))
    }

    /// The open document for `path`, opening it (without a tab) if needed.
    fn doc_for_path(&mut self, path: &Path) -> Option<usize> {
        if let Some(i) = self.docs.iter().position(|d| d.as_ref().is_some_and(|d| d.buffer.path() == Some(path))) {
            return Some(i);
        }
        match Doc::open(path.to_path_buf()) {
            Ok(doc) => {
                if doc.large {
                    self.set_status_message("This is a large file: folding, word wrap, the git gutter and language features are turned off for it.");
                }
                Some(self.add_doc(doc))
            }
            Err(e) => {
                self.message_dialog()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("Unable to open file")
                    .set_description(format!("{}: {e}", path.display()))
                    .show();
                None
            }
        }
    }

    pub fn open_file(&mut self, path: &Path) {
        if crate::imageio::is_image(path) {
            return self.open_image(path, false);
        }
        if crate::search_editor::is_search_file(path) {
            return self.open_saved_search(path);
        }
        let Some(doc) = self.doc_for_path(path) else { return };
        self.show_doc(doc);
        if let Some(tree) = &mut self.tree {
            tree.reveal(path);
        }
    }

    pub(super) fn add_doc(&mut self, doc: Doc) -> usize {
        if let Some(i) = self.docs.iter().position(Option::is_none) {
            self.docs[i] = Some(doc);
            i
        } else {
            self.docs.push(Some(doc));
            self.docs.len() - 1
        }
    }

    fn show_doc(&mut self, doc: usize) {
        let g = &mut self.groups[self.active_group];
        match g.tabs.iter().position(|t| t.doc == doc && !t.is_special()) {
            Some(i) => g.active = i,
            None => {
                let at = if g.tabs.is_empty() { 0 } else { g.active + 1 };
                g.tabs.insert(at, EditorState::new(doc));
                g.active = at;
            }
        }
        self.reset_caret();
    }

    fn new_file(&mut self) {
        self.untitled_count += 1;
        let doc = self.add_doc(Doc::untitled(self.untitled_count));
        self.show_doc(doc);
        self.focus = Focus::Editor;
    }

    /// ⌘S: with `editor.formatOnSave`, formats first and saves when the edits arrive.
    fn save_with_format(&mut self) {
        let Some(doc_id) = self.active_editor().map(|e| e.doc) else { return };
        if !self.format_then_save(doc_id) {
            self.save();
        }
    }

    fn save(&mut self) {
        // An image tab's document is a placeholder: never write it over the image.
        if self.active_editor().is_some_and(|e| e.image.is_some()) {
            return;
        }
        let root = self.tree.as_ref().map(|t| t.root_path().to_path_buf());
        let (file_dialog, error_dialog) = (self.file_dialog(), self.message_dialog());
        let Some(doc_id) = self.active_editor().map(|e| e.doc) else { return };
        if self.save_search_editor(doc_id, false).is_some() {
            return;
        }
        let Some(doc) = self.docs[doc_id].as_mut() else { return };
        if doc.buffer.path().is_none() {
            let mut dialog = file_dialog.set_file_name(format!("{}.txt", doc.title()));
            if let Some(root) = &root {
                dialog = dialog.set_directory(root);
            }
            let Some(path) = dialog.save_file() else { return };
            doc.set_path(path);
        }
        Self::before_save(doc);
        match doc.buffer.save() {
            Ok(()) => {
                if let Some(path) = doc.buffer.path().map(Path::to_path_buf) {
                    self.lsp.saved(&path);
                    self.after_save(&path);
                }
            }
            Err(e) => {
                error_dialog
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("Failed to save")
                    .set_description(e.to_string())
                    .show();
            }
        }
        if let Some(tree) = &mut self.tree {
            tree.refresh();
        }
    }

    /// Closes a tab, asking to save if it's the last view of a modified document.
    /// Returns false if the user cancelled.
    fn close_tab(&mut self, g: usize, i: usize) -> bool {
        let Some(doc_id) = self.groups.get(g).and_then(|gr| gr.tabs.get(i)).map(|t| t.doc) else { return true };
        let others = self.groups.iter().enumerate().any(|(gi, gr)| {
            gr.tabs.iter().enumerate().any(|(ti, t)| t.doc == doc_id && (gi, ti) != (g, i))
        });
        if !others {
            if let Some(doc) = self.docs[doc_id].as_ref().filter(|d| d.buffer.is_dirty()) {
                let answer = self.message_dialog()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title(format!("Do you want to save the changes you made to {}?", doc.title()))
                    .set_description("Your changes will be lost if you don't save them.")
                    .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
                        "Save".into(),
                        "Don't Save".into(),
                        "Cancel".into(),
                    ))
                    .show();
                match answer {
                    rfd::MessageDialogResult::Custom(s) if s == "Save" => {
                        let (ag, at) = (self.active_group, self.groups[g].active);
                        self.active_group = g;
                        self.groups[g].active = i;
                        self.save();
                        self.active_group = ag;
                        self.groups[g].active = at;
                        if self.docs[doc_id].as_ref().is_some_and(|d| d.buffer.is_dirty()) {
                            return false;
                        }
                    }
                    rfd::MessageDialogResult::Custom(s) if s == "Don't Save" => {}
                    rfd::MessageDialogResult::Yes => self.save(),
                    rfd::MessageDialogResult::No => {}
                    _ => return false,
                }
            }
            if let Some(path) = self.docs[doc_id].as_ref().and_then(|d| d.buffer.path()).map(Path::to_path_buf) {
                self.close_lsp_doc(&path);
            }
            self.docs[doc_id] = None;
        }
        let group = &mut self.groups[g];
        group.tabs.remove(i);
        if group.active >= i && group.active > 0 {
            group.active -= 1;
        }
        if group.tabs.is_empty() && self.groups.len() > 1 {
            self.groups.remove(g);
            self.active_group = self.active_group.min(self.groups.len() - 1);
        }
        true
    }

    /// Closes every editor, prompting for unsaved changes. Returns false if cancelled.
    pub fn close_all(&mut self) -> bool {
        while let Some(g) = self.groups.iter().position(|g| !g.tabs.is_empty()) {
            let last = self.groups[g].tabs.len() - 1;
            if !self.close_tab(g, last) {
                return false;
            }
        }
        true
    }

    fn split(&mut self) {
        let Some(ed) = self.active_editor() else { return };
        if self.groups.len() >= 4 {
            return;
        }
        let mut copy = EditorState::new(ed.doc);
        copy.sel = ed.sel;
        copy.scroll_y = ed.scroll_y;
        let at = self.active_group + 1;
        self.groups.insert(at, Group { tabs: vec![copy], active: 0, find: Default::default() });
        self.active_group = at;
    }

    fn copy_selection(&mut self, cut: bool) {
        let Some((ed, doc)) = self.active_mut() else { return };
        let text = if cut { ed.cut(doc) } else { ed.copy_ranges(doc).0 };
        ed.reveal = true;
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(text);
        }
    }

    fn paste(&mut self) {
        let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) else { return };
        let text = text.replace("\r\n", "\n");
        if let Some((ed, doc)) = self.active_mut() {
            ed.paste(doc, &text);
        }
    }

    // ------------------------------------------------------------------ commands

    pub fn run(&mut self, cmd: Command) {
        // Diff tabs are read-only: ignore editing commands while one is active in the editor.
        let editing = matches!(
            cmd,
            Command::Undo
                | Command::Redo
                | Command::Cut
                | Command::Paste
                | Command::ToggleComment
                | Command::Find
                | Command::FindReplace
                | Command::InsertCursorAbove
                | Command::InsertCursorBelow
                | Command::InsertCursorAtLineEnds
                | Command::AddNextOccurrence
                | Command::AddPreviousOccurrence
                | Command::MoveToNextOccurrence
                | Command::SelectAllOccurrences
                | Command::JumpToBracket
                | Command::SelectToBracket
                | Command::Fold
                | Command::Unfold
                | Command::ToggleFold
                | Command::FoldRecursively
                | Command::UnfoldRecursively
                | Command::FoldAll
                | Command::UnfoldAll
                | Command::ConflictAcceptAllCurrent
                | Command::ConflictAcceptAllIncoming
                | Command::ConflictAcceptAllBoth
        );
        if editing && self.focus == Focus::Editor && self.active_editor().is_some_and(|e| e.is_special()) {
            return;
        }
        match cmd {
            Command::Ext(_) => self.ext_execute(cmd.id(), Vec::new(), None),
            Command::NewFile => self.new_file(),
            Command::OpenFolder => {
                if let Some(path) = self.file_dialog().pick_folder() {
                    self.open_folder_by_user(&path);
                }
            }
            Command::Quit => self.effects.push(Effect::Quit),
            Command::NewWindow => self.effects.push(Effect::NewWindow),
            Command::CloseWindow => self.effects.push(Effect::CloseWindow),
            Command::ToggleProblems => self.toggle_problems(),
            Command::CloseFolder => {
                // With hot exit, the folder's unsaved changes are kept for next time.
                let hot = self.hot_exit();
                if hot && self.tree.is_some() {
                    self.save_session(true);
                    self.clear_docs();
                    self.groups = vec![Group { tabs: Vec::new(), active: 0, find: Default::default() }];
                    self.active_group = 0;
                }
                if self.close_all() {
                    self.settings.set_workspace_folder(None);
                    self.tree = None;
                    self.workspace_file = None;
                    self.folders_changed();
                }
            }
            Command::Save => self.save_with_format(),
            Command::CloseEditor => {
                let g = self.active_group;
                if let Some(active) = self.groups.get(g).filter(|gr| !gr.tabs.is_empty()).map(|gr| gr.active) {
                    self.close_tab(g, active);
                }
            }
            Command::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            Command::TogglePanel => {
                self.panel_visible = !self.panel_visible;
                self.panel_maximized = false;
                if !self.panel_visible && self.focus == Focus::Terminal {
                    self.focus = Focus::Editor;
                }
            }
            Command::ToggleAuxiliaryBar => {
                self.toggle_aux_bar();
                if !self.aux.visible && self.focus == Focus::Assistant {
                    self.focus = Focus::Editor;
                }
            }
            Command::AssistantNewChat => {
                self.show_aux(aux_bar::AuxTab::Assistant);
                self.assistant_new_chat(String::new());
                self.focus = Focus::Assistant;
            }
            Command::OutlineFocus => {
                self.show_aux(aux_bar::AuxTab::Outline);
                self.focus = Focus::Outline;
            }
            Command::TimelineFocus => self.show_aux(aux_bar::AuxTab::Timeline),
            Command::AssistantFocus => {
                self.show_aux(aux_bar::AuxTab::Assistant);
                self.focus = Focus::Assistant;
            }
            Command::AssistantSelectAgent => self.assistant_select_agent(),
            Command::AssistantHistory => {
                self.show_aux(aux_bar::AuxTab::Assistant);
                if !self.assistant.history {
                    self.assistant_toggle_history();
                }
            }
            Command::ToggleTerminal => self.toggle_terminal(),
            Command::NewTerminal => self.new_terminal(),
            Command::KillTerminal => self.kill_terminal(),
            Command::SplitTerminal => self.split_terminal(),
            Command::FocusNextTerminal => self.focus_terminal(true, false),
            Command::FocusPreviousTerminal => self.focus_terminal(false, false),
            Command::FocusNextTerminalPane => self.focus_terminal(true, true),
            Command::FocusPreviousTerminalPane => self.focus_terminal(false, true),
            // ⌘\ splits the terminal when it has focus.
            Command::SplitEditor if self.focus == Focus::Terminal => self.split_terminal(),
            Command::ToggleWordWrap => {
                let on = config::get().word_wrap != config::WordWrap::Off;
                let value = if on { "off" } else { "on" };
                self.update_setting(settings::Scope::User, "editor.wordWrap", Some(serde_json::Value::String(value.into())));
            }
            Command::ToggleMinimap => {
                let on = config::get().minimap;
                self.update_setting(settings::Scope::User, "editor.minimap.enabled", Some(serde_json::Value::Bool(!on)));
            }
            Command::OpenSettings => self.open_settings_ui(),
            Command::OpenKeybindings => self.open_keyboard_shortcuts(),
            Command::Welcome => self.open_welcome(),
            Command::OpenRecent => self.open_recent_picker(),
            Command::InsertSnippet => self.open_insert_snippet(),
            Command::GitStageSelectedRanges => self.stage_selected_ranges(),
            Command::GitUnstageSelectedRanges => self.unstage_selected_ranges(),
            Command::GitRevertSelectedRanges => self.revert_selected_ranges(),
            Command::ClearRecent => self.clear_recent(),
            Command::OpenKeybindingsFile => self.open_keybindings_json(),
            Command::KeepEditor => self.pin_active(),
            Command::OpenSettingsJson => self.open_settings_json(settings::Scope::User),
            Command::OpenWorkspaceSettingsJson => self.open_settings_json(settings::Scope::Workspace),
            Command::SelectTheme => self.open_theme_picker(),
            Command::SplitEditor => self.split(),
            Command::ShowExplorer => self.show_view(View::Explorer),
            Command::ShowSearch => self.focus_search(false),
            Command::ReplaceInFiles => self.focus_search(true),
            Command::Find if self.focus == Focus::Settings => self.focus_settings_search(),
            Command::Find => self.open_find(false),
            Command::FindReplace => self.open_find(true),
            Command::FindNext => self.find_step(true),
            Command::FindPrevious => self.find_step(false),
            Command::ShowScm => self.focus_scm(),
            Command::GitCommit => self.scm_action(scm_view::ScmAction::Commit),
            Command::GitStageAll => self.scm_action(scm_view::ScmAction::StageAll),
            Command::GitRefresh => self.scm_action(scm_view::ScmAction::Refresh),
            Command::GitInit => self.scm_action(scm_view::ScmAction::InitRepo),
            Command::GitOpenChanges => self.open_changes_for_active(),
            Command::GitCheckout
            | Command::GitCheckoutDetached
            | Command::GitCreateBranch
            | Command::GitCreateBranchFrom
            | Command::GitRenameBranch
            | Command::GitDeleteBranch
            | Command::GitMerge
            | Command::GitAbortMerge
            | Command::GitRebase
            | Command::GitAbortRebase
            | Command::GitContinueRebase
            | Command::GitFetch
            | Command::GitFetchPrune
            | Command::GitFetchAll
            | Command::GitPull
            | Command::GitPullRebase
            | Command::GitPush
            | Command::GitPushForce
            | Command::GitSync
            | Command::GitPublish
            | Command::GitCommitAmend
            | Command::GitCommitPush
            | Command::GitCommitSync
            | Command::GitUndoCommit
            | Command::GitUnstageAll
            | Command::GitDiscardAll
            | Command::GitStash
            | Command::GitStashUntracked
            | Command::GitStashStaged
            | Command::GitStashPopLatest
            | Command::GitStashPop
            | Command::GitStashApplyLatest
            | Command::GitStashApply
            | Command::GitStashDrop
            | Command::GitStashDropAll
            | Command::GitAddRemote
            | Command::GitRemoveRemote
            | Command::GitCreateTag
            | Command::GitDeleteTag
            | Command::GitClone
            | Command::ConflictAcceptAllCurrent
            | Command::ConflictAcceptAllIncoming
            | Command::ConflictAcceptAllBoth
            | Command::ConflictNext
            | Command::ConflictPrevious => self.git_command(cmd),
            Command::NextChange => self.diff_step(true),
            Command::PreviousChange => self.diff_step(false),
            Command::ShowDebug => self.show_view(View::Debug),
            Command::SaveAs => self.save_as(),
            Command::SaveAll => self.save_all(),
            Command::RevertFile => self.revert_file(),
            Command::CloseOtherEditors => self.close_others(),
            Command::CloseEditorsToTheRight => self.close_to_the_right(),
            Command::CloseSavedEditors => self.close_saved(),
            Command::CloseGroupEditors => self.close_group_editors(),
            Command::CloseAllEditors => {
                self.close_all();
            }
            Command::RevealInExplorer => self.reveal_in_explorer(),
            Command::SelectForCompare => self.select_for_compare(),
            Command::CompareWithSelected => self.compare_with_selected(),
            Command::CompareWithSaved => self.compare_with_saved(),
            Command::CompareFileWith => self.compare_file_with(),
            Command::ExplorerNewFile => self.explorer_new(false),
            Command::ExplorerNewFolder => self.explorer_new(true),
            Command::ExplorerRename => self.explorer_rename(),
            Command::ExplorerDelete => self.explorer_delete(),
            Command::CopyFilePath => self.copy_file_path(false),
            Command::CopyRelativeFilePath => self.copy_file_path(true),
            Command::RevealInFinder => self.reveal_in_finder(),
            Command::OpenInTerminal => self.open_in_terminal(),
            Command::CollapseExplorerFolders => {
                if let Some(tree) = &mut self.tree {
                    tree.collapse_all();
                }
            }
            Command::RunTask => self.run_task_prompt(),
            Command::RunBuildTask => self.run_build_task(),
            Command::TerminateTask => self.terminate_task(),
            Command::ConfigureTasks => self.configure_tasks(),
            Command::DebugStart => self.debug_start(false),
            Command::DebugRun => self.debug_start(true),
            Command::DebugStop => self.debug_stop(),
            Command::DebugRestart => self.debug_restart(),
            Command::DebugContinue => self.debug_continue(),
            Command::DebugPause => self.debug_pause(),
            Command::DebugStepOver => self.debug_step("next"),
            Command::DebugStepInto => self.debug_step("stepIn"),
            Command::DebugStepOut => self.debug_step("stepOut"),
            Command::ToggleBreakpoint => self.toggle_breakpoint(),
            Command::ConditionalBreakpoint => self.edit_breakpoint(debug::BpField::Condition),
            Command::AddLogpoint => self.edit_breakpoint(debug::BpField::LogMessage),
            Command::EnableAllBreakpoints => self.set_all_breakpoints_enabled(true),
            Command::DisableAllBreakpoints => self.set_all_breakpoints_enabled(false),
            Command::RemoveAllBreakpoints => self.remove_all_breakpoints(),
            Command::ToggleBreakpointsActivated => self.toggle_breakpoints_active(),
            Command::DebugConfigure => self.open_launch_json(),
            Command::DebugSelectAndStart => self.select_and_start(),
            Command::DebugAddWatch => self.add_watch_selection(),
            Command::ToggleOutput => {
                if self.panel_visible && self.panel_tab == PANEL_OUTPUT {
                    self.panel_visible = false;
                } else {
                    self.panel_visible = true;
                    self.panel_tab = PANEL_OUTPUT;
                }
            }
            Command::ToggleDebugConsole => {
                if self.panel_visible && self.panel_tab == PANEL_DEBUG_CONSOLE {
                    self.panel_visible = false;
                    self.focus = Focus::Editor;
                } else {
                    self.debug_focus_console();
                }
            }
            Command::ShowExtensions => self.show_view(View::Extensions),
            Command::ExtensionsInstallVsix => self.install_vsix(),
            Command::ExtensionsInstallFromLocation => self.install_extension_folder(),
            Command::ExtensionsOpenFolder => self.reveal_extensions_folder(),
            Command::ExtensionsCheckForUpdates => self.check_extension_updates(true),
            Command::ExtensionsUpdateAll => self.update_all_extensions(),
            Command::RestartExtensionHost => self.restart_extensions(),
            Command::CheckForUpdates => self.check_for_updates_command(),
            Command::RestartToUpdate => self.restart_to_update(),
            Command::RestartLanguageServer => self.restart_language_server(),
            Command::StopLanguageServers => self.stop_language_servers(),
            Command::FocusGroup1 | Command::FocusGroup2 | Command::FocusGroup3 => {
                let n = match cmd {
                    Command::FocusGroup1 => 0,
                    Command::FocusGroup2 => 1,
                    _ => 2,
                };
                if n < self.groups.len() {
                    self.active_group = n;
                } else if n == self.groups.len() {
                    self.split();
                }
                self.focus = Focus::Editor;
            }
            Command::QuickOpen => self.open_palette(""),
            Command::CommandPalette => self.open_palette(">"),
            Command::Undo | Command::Redo if matches!(self.focus, Focus::Terminal | Focus::Search | Focus::Find | Focus::Scm | Focus::Settings | Focus::Rename | Focus::Outline | Focus::DebugConsole | Focus::Explorer | Focus::Peek | Focus::ProblemsFilter | Focus::Testing | Focus::TestingFilter | Focus::SearchEditor | Focus::Extensions | Focus::Assistant) => {}
            Command::Cut | Command::Copy if self.focus == Focus::SearchEditor => self.search_editor_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::SearchEditor => self.search_editor_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::SearchEditor => self.search_editor_clipboard(false, false, true),
            Command::SelectAllOccurrences if self.focus == Focus::Editor && self.active_editor().is_some_and(|e| e.search.is_some()) => self.select_all_search_editor_matches(),
            Command::Cut | Command::Copy if self.focus == Focus::Assistant => self.assistant_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::Assistant => self.assistant_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::Assistant => self.assistant_clipboard(false, false, true),
            Command::Cut | Command::Copy if self.focus == Focus::Extensions => self.extensions_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::Extensions => self.extensions_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::Extensions => self.extensions_clipboard(false, false, true),
            Command::Cut | Command::Copy if self.focus == Focus::TestingFilter => self.testing_filter_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::TestingFilter => self.testing_filter_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::TestingFilter => self.testing_filter_clipboard(false, false, true),
            Command::Cut | Command::Copy | Command::Paste | Command::SelectAll if self.focus == Focus::Testing => {}
            Command::Cut | Command::Copy if self.focus == Focus::ProblemsFilter => self.problems_filter_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::ProblemsFilter => self.problems_filter_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::ProblemsFilter => self.problems_filter_clipboard(false, false, true),
            Command::Cut | Command::Copy | Command::Paste | Command::SelectAll if self.focus == Focus::Peek => {}
            Command::Cut | Command::Copy | Command::Paste | Command::SelectAll if self.focus == Focus::Explorer => self.explorer_clipboard_command(cmd),
            Command::Rename if self.focus == Focus::Explorer => self.explorer_rename(),
            Command::Cut | Command::Copy if self.focus == Focus::DebugConsole => self.debug_console_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::DebugConsole => self.debug_console_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::DebugConsole => self.debug_console_clipboard(false, false, true),
            Command::Cut | Command::Copy | Command::Paste | Command::SelectAll if self.focus == Focus::Outline => {}
            Command::Cut | Command::Copy if self.focus == Focus::Rename => self.rename_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::Rename => self.rename_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::Rename => self.rename_clipboard(false, false, true),
            Command::Undo | Command::Redo => {
                if let Some((ed, doc)) = self.active_mut() {
                    ed.undo(doc, cmd == Command::Redo);
                }
            }
            Command::Cut | Command::Copy if self.focus == Focus::Settings => self.settings_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::Settings => self.settings_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::Settings => self.settings_clipboard(false, false, true),
            Command::Cut | Command::Copy if self.focus == Focus::Scm => self.scm_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::Scm => self.scm_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::Scm => self.scm_clipboard(false, false, true),
            Command::Cut | Command::Copy if self.focus == Focus::Find => self.find_clipboard(cmd == Command::Cut, false, false),
            Command::Paste if self.focus == Focus::Find => self.find_clipboard(false, true, false),
            Command::SelectAll if self.focus == Focus::Find => self.find_clipboard(false, false, true),
            Command::Cut | Command::Copy if self.focus == Focus::Search => self.search_clipboard(cmd == Command::Cut, false),
            Command::Paste if self.focus == Focus::Search => self.search_clipboard(false, true),
            Command::SelectAll if self.focus == Focus::Search => self.search_select_all(),
            Command::Cut | Command::Copy if self.focus == Focus::Terminal => self.copy_terminal_selection(),
            Command::Paste if self.focus == Focus::Terminal => self.terminal_paste(),
            Command::SelectAll if self.focus == Focus::Terminal => self.terminal_select_all(),
            Command::Cut => self.copy_selection(true),
            Command::Copy => self.copy_selection(false),
            Command::Paste => self.paste(),
            Command::SelectAll => {
                if let Some((ed, doc)) = self.active_mut() {
                    ed.select_all(doc);
                }
            }
            Command::ToggleComment => {
                if let Some((ed, doc)) = self.active_mut() {
                    ed.toggle_comment(doc);
                }
            }
            Command::InsertCursorAbove
            | Command::InsertCursorBelow
            | Command::InsertCursorAtLineEnds
            | Command::AddNextOccurrence
            | Command::AddPreviousOccurrence
            | Command::MoveToNextOccurrence
            | Command::SelectAllOccurrences => {
                self.focus = Focus::Editor;
                if let Some((ed, doc)) = self.active_mut() {
                    match cmd {
                        Command::InsertCursorAbove => ed.insert_cursor_vertical(doc, false),
                        Command::InsertCursorBelow => ed.insert_cursor_vertical(doc, true),
                        Command::InsertCursorAtLineEnds => ed.cursors_at_line_ends(doc),
                        Command::AddNextOccurrence => ed.add_next_occurrence(doc, false),
                        Command::AddPreviousOccurrence => ed.add_next_occurrence(doc, true),
                        Command::MoveToNextOccurrence => ed.move_to_next_occurrence(doc),
                        _ => ed.select_all_occurrences(doc),
                    }
                    ed.reveal = true;
                }
            }
            Command::JumpToBracket | Command::SelectToBracket => {
                self.focus = Focus::Editor;
                if let Some((ed, doc)) = self.active_mut() {
                    doc.brackets.update(&doc.buffer, doc.lang);
                    if cmd == Command::JumpToBracket {
                        ed.jump_to_bracket(doc);
                    } else {
                        ed.select_to_bracket(doc);
                    }
                }
            }
            Command::Fold
            | Command::Unfold
            | Command::ToggleFold
            | Command::FoldRecursively
            | Command::UnfoldRecursively
            | Command::FoldAll
            | Command::UnfoldAll => {
                self.focus = Focus::Editor;
                if let Some((ed, doc)) = self.active_mut() {
                    let (b, line) = (&doc.buffer, ed.sel.head.line);
                    match cmd {
                        Command::Fold => ed.folds.fold(b, line),
                        Command::Unfold => ed.folds.unfold(b, line),
                        Command::ToggleFold => ed.folds.toggle(b, line),
                        Command::FoldRecursively => ed.folds.set_recursive(b, line, true),
                        Command::UnfoldRecursively => ed.folds.set_recursive(b, line, false),
                        Command::FoldAll => ed.folds.fold_all(b),
                        _ => ed.folds.unfold_all(),
                    }
                    // A cursor inside a region that just folded moves to its header.
                    let hidden = ed.folds.hidden(b);
                    let head = ed.sel.head;
                    if let Some(r) = hidden.iter().find(|r| r.contains(&head.line)) {
                        let header = r.start() - 1;
                        ed.set_selection(text::Selection::caret(text::Pos::new(header, b.line_len(header))));
                    }
                    ed.reveal = true;
                }
            }
            Command::GoToDefinition => self.go_to_definition(None),
            Command::ToggleZenMode => self.toggle_zen(),
            // ⇧⌥H in an open hierarchy peek switches its direction.
            Command::ShowCallHierarchy => {
                if !self.peek_toggle_calls() {
                    self.show_call_hierarchy();
                }
            }
            Command::ShowTypeHierarchy => self.show_type_hierarchy(),
            Command::EmmetExpandAbbreviation => {
                self.emmet_expand();
            }
            Command::EmmetWrapWithAbbreviation => self.emmet_wrap_prompt(),
            Command::ChangeLanguageMode => self.change_language_mode(),
            Command::ShowTesting
            | Command::TestingRunAll
            | Command::TestingRunAtCursor
            | Command::TestingDebugAtCursor
            | Command::TestingRunCurrentFile
            | Command::TestingReRunLastRun
            | Command::TestingReRunFailed
            | Command::TestingCancel
            | Command::TestingRefresh
            | Command::TestingShowOutput
            | Command::TestingClearResults
            | Command::TestingCollapseAll => self.test_command(cmd),
            Command::NewSearchEditor => self.new_search_editor(false),
            Command::NewSearchEditorToSide => self.new_search_editor(true),
            Command::OpenSearchInEditor => self.open_search_results_in_editor(),
            Command::SearchEditorRerun => self.search_editor_rerun(),
            Command::SearchEditorToggleContext => self.search_editor_toggle(search_editor_view::SeToggle::ContextLines),
            Command::SearchEditorIncreaseContext => self.search_editor_context_step(1),
            Command::SearchEditorDecreaseContext => self.search_editor_context_step(-1),
            Command::SearchEditorFocusInput => self.focus_search_editor_input(),
            Command::SearchEditorDeleteFileResults => self.delete_search_editor_file_results(),
            Command::SearchEditorSelectAllMatches => self.select_all_search_editor_matches(),
            Command::GitOpenMergeEditor => self.open_merge_editor_for_active(),
            Command::MergeNextConflict => self.merge_go_to_conflict(true),
            Command::MergePreviousConflict => self.merge_go_to_conflict(false),
            Command::MergeAcceptAllInput1 => self.merge_accept_all(1),
            Command::MergeAcceptAllInput2 => self.merge_accept_all(2),
            Command::MergeAcceptAllCombination => self.merge_accept_all_combination(),
            Command::MergeResetResult => self.merge_reset(),
            Command::MergeComplete => self.complete_merge(),
            Command::OpenWorkspaceFromFile => self.open_workspace_from_file(),
            Command::AddFolderToWorkspace => self.add_folder_to_workspace(),
            Command::RemoveFolderFromWorkspace => self.remove_folder_command(),
            Command::SaveWorkspaceAs => self.save_workspace_as(),
            Command::MarkdownPreview => self.open_markdown_preview(false),
            Command::MarkdownPreviewSide => self.open_markdown_preview(true),
            Command::PeekDefinition => {
                self.peek_definition = true;
                self.go_to_definition(None);
            }
            Command::Rename => self.start_rename(),
            Command::StartLinkedEditing => self.start_linked_editing(),
            Command::GoToReferences => self.find_references(),
            Command::TriggerParameterHints => {
                self.hide_signature();
                self.trigger_signature_help(None);
            }
            Command::GotoSymbol => self.open_palette("@"),
            Command::ShowAllSymbols => self.open_palette("#"),
            Command::GotoLine => self.open_palette(":"),
            Command::QuickFix => self.quick_fix(),
            Command::FormatDocument => self.format_active(false),
            Command::FormatSelection => self.format_active(true),
            Command::TriggerSuggest => self.trigger_completion(None, false),
            Command::RefreshExplorer => {
                if let Some(tree) = &mut self.tree {
                    tree.refresh();
                }
                self.palette_files = None;
            }
        }
        self.reset_caret();
    }

    /// View: Toggle Zen Mode (⌘K Z): full screen, with only the editors (side bar and panel
    /// can still be opened); leaving restores what was shown.
    fn toggle_zen(&mut self) {
        match self.zen.take() {
            Some((sidebar, panel)) => {
                self.sidebar_visible = sidebar;
                self.panel_visible = panel;
                self.effects.push(Effect::SetFullscreen(false));
            }
            None => {
                self.zen = Some((self.sidebar_visible, self.panel_visible));
                self.sidebar_visible = false;
                self.panel_visible = false;
                self.focus = Focus::Editor;
                self.effects.push(Effect::SetFullscreen(true));
            }
        }
    }

    fn show_view(&mut self, view: View) {
        if view == View::Search {
            return self.focus_search(false);
        }
        self.view = view;
        self.sidebar_visible = true;
        if view == View::Explorer {
            self.focus = Focus::Explorer;
        }
    }

    fn open_palette(&mut self, prefix: &str) {
        self.cancel_quick_access();
        let mut p = Palette::new(prefix);
        p.has_repo = self.repo.is_some();
        self.refresh_palette(&mut p);
        self.palette = Some(p);
    }

    fn refresh_palette(&mut self, p: &mut Palette) {
        if p.is_files() && self.palette_files.is_none() {
            let files: Vec<PathBuf> = self.folders().iter().flat_map(|f| walk_files(f, 50_000)).collect();
            self.palette_files = Some(files.into_iter().map(|p| (self.display_path(&p), p)).map(|(l, p)| (p, l)).collect());
        }
        p.update(self.palette_files.as_deref().unwrap_or(&[]));
        self.refresh_quick_access(p);
    }

    fn palette_accept(&mut self) {
        if let Some(mut p) = self.palette.take() {
            if let Some(b) = p.input_box.take() {
                if b.error.is_some() || !self.git_input(b.purpose.clone(), p.input.clone()) {
                    p.input_box = Some(b);
                    self.palette = Some(p);
                }
                return;
            }
            self.palette = Some(p);
        }
        let Some(action) = self.palette.as_ref().and_then(|p| p.selected_action()) else { return self.cancel_palette() };
        self.palette = None;
        if !matches!(action, Action::GotoHere(_)) {
            self.cancel_quick_access(); // leaving a previewed symbol or line
        }
        match action {
            Action::GotoHere(pos) => self.goto_here(pos),
            Action::Git(pick) => self.git_pick(pick),
            Action::Goto(path, pos) => self.goto_location(&path, pos),
            Action::Theme(name) => self.pick_theme(&name),
            Action::DefineKeybinding(cmd) => self.define_keybinding(cmd),
            Action::Debug(pick) => self.debug_pick(pick),
            Action::CompareWith(path) => self.compare_paths(path, None),
            Action::Language(lang) => self.set_language(lang),
            Action::Agent(action) => self.agent_action(action),
            Action::ExtPick(i) => self.ext_answer(serde_json::json!(i)),
            Action::Task(label) => {
                self.run_task(&label);
            }
            Action::TerminateTask(label) => self.terminate_task_terminal(&label),
            Action::InsertSnippet(body) => {
                self.focus = Focus::Editor;
                if let Some(ed) = self.active_editor() {
                    let (a, z) = ed.sel.ordered();
                    self.insert_snippet(a, z, &body);
                }
            }
            Action::OpenFolder(path) => self.open_folder_by_user(&path),
            Action::RemoveRootFolder(path) => self.remove_folder_from_workspace(&path),
            Action::Run(cmd) => {
                self.theme_before_picker = None;
                self.run(cmd)
            }
            Action::Open(path) => {
                if self.settings.bool("workbench.editor.enablePreviewFromQuickOpen") {
                    self.open_file_preview(&path);
                } else {
                    self.open_file(&path);
                }
                self.focus = Focus::Editor;
            }
        }
    }

    // ------------------------------------------------------------------ input

    pub fn key(&mut self, k: KeyInput) {
        self.reset_caret();
        self.dismiss_hover();
        if self.palette.is_some() {
            self.palette_key(&k);
            return;
        }
        if self.chord_key(&k) {
            return;
        }
        // The Settings sheet is modal: it takes the keys while it's open.
        if self.settings_active() {
            self.focus = Focus::Settings;
        }
        if self.focus == Focus::Settings {
            self.settings_key(&k);
            return;
        }
        if self.focus == Focus::Editor && self.completion_key(&k) {
            return;
        }
        if self.focus == Focus::Editor && self.signature_key(&k) {
            return;
        }
        if self.focus == Focus::Editor && self.snippet_active() && !k.cmd && !k.ctrl && !k.alt {
            match k.key {
                Key::Tab if self.snippet_tab(k.shift) => return,
                Key::Escape => return self.leave_snippet(),
                _ => {}
            }
        }
        if self.focus == Focus::Editor && k.key == Key::Tab && !(k.shift || k.cmd || k.ctrl || k.alt) && config::get().emmet_tab && self.emmet_expand() {
            return;
        }
        if k.key == Key::Escape && self.close_color_picker() {
            return;
        }
        if self.focus == Focus::Editor && k.key == Key::Escape && !k.cmd && !k.ctrl && !k.alt && !k.shift && self.linked_escape() {
            return;
        }
        if self.focus == Focus::Terminal {
            self.terminal_key(&k);
            return;
        }
        if self.focus == Focus::Search {
            self.search_key(&k);
            return;
        }
        if self.focus == Focus::Find {
            self.find_key(&k);
            return;
        }
        if self.focus == Focus::Scm {
            self.scm_key(&k);
            return;
        }
        if self.focus == Focus::Rename {
            self.rename_key(&k);
            return;
        }
        if self.focus == Focus::Editor && self.active_editor().is_some_and(|e| e.diff.is_some()) {
            self.diff_key(&k);
            return;
        }
        // Esc in the editor closes the peek view, then the find widget.
        if self.focus == Focus::Editor && k.key == Key::Escape && self.peek.is_some() {
            return self.close_peek();
        }
        // Escape twice leaves Zen Mode.
        if self.zen.is_some() && k.key == Key::Escape && !k.cmd && !k.alt && !k.ctrl && !k.shift {
            if self.zen_escape.is_some_and(|t| t.elapsed() < Duration::from_millis(800)) {
                self.zen_escape = None;
                return self.toggle_zen();
            }
            self.zen_escape = Some(Instant::now());
        }
        if self.focus == Focus::Editor
            && k.key == Key::Escape
            && self.groups.get(self.active_group).is_some_and(|g| g.find.visible)
        {
            self.close_find();
            return;
        }
        if let Some(cmd) = k.command() {
            self.run(cmd);
            return;
        }
        // Escape hides notification toasts first.
        if k.key == Key::Escape && !(k.cmd || k.ctrl || k.alt || k.shift) && self.focus == Focus::Editor && self.active_editor().is_some_and(|e| e.extra.is_empty()) && self.close_newest_toast() {
            return;
        }
        match self.focus {
            Focus::Explorer => self.explorer_key(&k),
            Focus::Outline => self.outline_key(&k),
            Focus::DebugConsole => self.debug_console_key(&k),
            Focus::Peek => self.peek_key(&k),
            Focus::ProblemsFilter => self.problems_filter_key(&k),
            Focus::Testing => self.testing_key(&k),
            Focus::SearchEditor => self.search_editor_key(&k),
            Focus::TestingFilter => self.testing_filter_key(&k),
            Focus::Extensions => self.extensions_key(&k),
            Focus::Assistant => self.assistant_key(&k),
            Focus::Editor => {
                // Esc in a search editor's results (one cursor, nothing selected) goes back to
                // its query, like the standard Focus Search Editor Input.
                let to_query = k.key == Key::Escape && self.active_editor().is_some_and(|e| e.search.is_some() && e.extra.is_empty() && e.sel.is_empty());
                if to_query {
                    return self.focus_search_editor_input();
                }
                // The Welcome page takes no typing.
                if self.active_editor().is_some_and(|e| e.welcome) {
                    return;
                }
                if let Some((ed, doc)) = self.active_mut() {
                    ed.key(doc, &k);
                }
                self.after_editor_key(&k);
            }
            Focus::Terminal | Focus::Search | Focus::Find | Focus::Scm | Focus::Settings | Focus::Rename => {}
        }
    }

    fn palette_key(&mut self, k: &KeyInput) {
        if self.record_key(k) {
            return;
        }
        if (k.cmd || k.ctrl) && k.key == Key::Char("p".into()) {
            self.open_palette(if k.shift { ">" } else { "" });
            return;
        }
        let Some(mut p) = self.palette.take() else { return };
        let mut changed = false;
        let before = p.selected;
        match &k.key {
            Key::Escape => {
                self.palette = Some(p);
                return self.cancel_palette();
            }
            Key::Enter => {
                self.palette = Some(p);
                self.palette_accept();
                return;
            }
            Key::Up => p.move_selection(-1),
            Key::Down => p.move_selection(1),
            Key::PageUp => p.move_selection(-(MAX_VISIBLE as isize)),
            Key::PageDown => p.move_selection(MAX_VISIBLE as isize),
            Key::Backspace => {
                if k.cmd || k.alt {
                    p.input.clear();
                } else {
                    p.input.pop();
                }
                changed = true;
            }
            _ if !k.cmd && !k.ctrl => {
                if let Some(t) = &k.text {
                    if !t.chars().any(char::is_control) {
                        p.input.push_str(t);
                        changed = true;
                    }
                }
            }
            _ => {}
        }
        if changed {
            self.refresh_palette(&mut p);
            self.validate_input(&mut p);
        }
        let moved = p.selected != before || changed;
        self.palette = Some(p);
        if moved {
            self.preview_picked_theme();
            self.preview_quick_access();
        }
    }

    fn explorer_key(&mut self, k: &KeyInput) {
        if self.explorer_edit_key(k) {
            return;
        }
        // ⌘⌫ moves to the Trash; Enter renames; ⌘↓ opens.
        if k.cmd && k.key == Key::Backspace {
            return self.explorer_delete();
        }
        if !k.cmd && !k.shift && !k.alt && k.key == Key::Enter {
            return self.explorer_rename();
        }
        let Some(tree) = &mut self.tree else { return };
        let n = tree.rows.len();
        if n == 0 {
            return;
        }
        let sel = tree.selected.unwrap_or(0);
        match k.key {
            Key::Up => tree.selected = Some(sel.saturating_sub(1)),
            Key::Down if !k.cmd => tree.selected = Some((sel + 1).min(n - 1)),
            Key::Right => {
                if tree.rows[sel].is_dir {
                    if tree.rows[sel].expanded {
                        tree.selected = Some((sel + 1).min(n - 1));
                    } else {
                        tree.set_expanded(sel, true);
                    }
                }
            }
            Key::Left => {
                if tree.rows[sel].is_dir && tree.rows[sel].expanded {
                    tree.set_expanded(sel, false);
                } else if let Some(p) = tree.parent(sel) {
                    tree.selected = Some(p);
                }
            }
            Key::Down if k.cmd => {
                if tree.rows[sel].is_dir {
                    tree.toggle(sel);
                } else {
                    let path = tree.rows[sel].path.clone();
                    self.open_file(&path);
                    self.pin_active();
                    self.focus = Focus::Editor;
                }
            }
            Key::Space => {
                if tree.rows[sel].is_dir {
                    tree.toggle(sel);
                } else {
                    let path = tree.rows[sel].path.clone();
                    self.open_file_preview(&path);
                }
            }
            Key::Escape => self.focus = Focus::Editor,
            _ => {}
        }
        self.scroll_explorer_to_selection();
    }

    fn scroll_explorer_to_selection(&mut self) {
        let body_h = self.explorer_body_height();
        let Some(tree) = &mut self.tree else { return };
        let Some(sel) = tree.selected else { return };
        let top = sel as f32 * ROW_H;
        if top < tree.scroll {
            tree.scroll = top;
        } else if top + ROW_H > tree.scroll + body_h {
            tree.scroll = top + ROW_H - body_h;
        }
    }

    fn explorer_body_height(&self) -> f32 {
        self.sections.body.first().map_or(ROW_H, |b| b.h.max(ROW_H))
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) {
        self.mouse = (x, y);
        if self.terminal_selecting() {
            self.terminal_mouse_drag(x, y);
            return;
        }
        match self.drag {
            Some(Drag::SidebarSash) => {
                self.sidebar_w = (x - self.main_rect.x).clamp(170.0, (self.main_rect.w - 300.0).max(170.0));
                return;
            }
            Some(Drag::AuxSash) => {
                self.drag_aux_sash(x);
                return;
            }
            Some(Drag::ScmGraphSash) => {
                self.drag_scm_graph_sash(y);
                return;
            }
            Some(Drag::SectionSash(i)) => {
                self.drag_section_sash(i, y);
                return;
            }
            Some(Drag::DebugSash(i)) => {
                self.drag_debug_sash(i, y);
                return;
            }
            Some(Drag::PanelSash) => {
                let bottom = self.main_rect.bottom();
                self.panel_h = (bottom - y).clamp(80.0, (self.main_rect.h - 120.0).max(80.0));
                return;
            }
            Some(Drag::ColorPicker(part)) => {
                self.drag_color_picker(part, x, y);
                return;
            }
            Some(Drag::Select(g)) => {
                if let Some(ed) = self.groups.get_mut(g).and_then(|gr| gr.tabs.get_mut(gr.active)) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.drag_to(doc, x, y);
                    }
                }
                return;
            }
            Some(Drag::Column(g, from)) => {
                if let Some(ed) = self.groups.get_mut(g).and_then(|gr| gr.tabs.get_mut(gr.active)) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.column_select(doc, from, x, y);
                    }
                }
                return;
            }
            Some(Drag::Slider(g, grab)) => {
                if let Some(ed) = self.groups.get_mut(g).and_then(|gr| gr.tabs.get_mut(gr.active)) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.scroll_to_slider(doc, y - grab);
                    }
                }
                return;
            }
            Some(Drag::Tab { .. }) => {
                self.drag_tab(x, y);
                return;
            }
            Some(Drag::ExplorerItem { i, from, moving }) => {
                if !moving && ((x - from.0).abs() > 4.0 || (y - from.1).abs() > 4.0) {
                    self.drag = Some(Drag::ExplorerItem { i, from, moving: true });
                }
                if moving {
                    self.hover_hit = self.hit_at(x, y);
                    return;
                }
            }
            Some(Drag::Minimap(g)) => {
                if let Some(ed) = self.groups.get_mut(g).and_then(|gr| gr.tabs.get_mut(gr.active)) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.scroll_to_minimap(doc, y);
                    }
                }
                return;
            }
            None => {}
        }
        self.hover_hit = self.hit_at(x, y);
        self.hover_mouse(x, y, self.hover_hit);
        let cursor = match self.hover_hit {
            Some(Hit::Editor(_)) | Some(Hit::Settings(settings_view::SettingsHit::Search | settings_view::SettingsHit::Input(_))) => CursorKind::Text,
            Some(Hit::SidebarSash | Hit::AuxSash) => CursorKind::ColResize,
            Some(Hit::AssistantInput) => CursorKind::Text,
            Some(Hit::CodeLens(..) | Hit::MergeAction(..) | Hit::MergeRemaining(_) | Hit::MergeComplete(_) | Hit::OpenMergeEditor(_)) => CursorKind::Pointer,
            Some(Hit::PanelSash | Hit::SectionSash(_) | Hit::DebugSash(_) | Hit::ScmGraphSash) => CursorKind::RowResize,
            _ => CursorKind::Default,
        };
        self.set_cursor(cursor);
    }

    fn set_cursor(&mut self, cursor: CursorKind) {
        if cursor != self.cursor {
            self.cursor = cursor;
            self.effects.push(Effect::Cursor(cursor));
        }
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, shift: bool, cmd: bool, alt: bool) {
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, lx, ly, n)) if now - t < Duration::from_millis(400) && (lx - x).abs() < 4.0 && (ly - y).abs() < 4.0 => {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, x, y, count));
        self.reset_caret();
        let Some(hit) = self.hit_at(x, y) else { return };
        if hit != Hit::RenameBox {
            self.cancel_rename();
        }
        // Clicking away from the Explorer's name field accepts it.
        if hit != Hit::ExplorerEditField && self.explorer_edit.is_some() {
            if self.explorer_edit.as_ref().is_some_and(|e| e.field.text.is_empty()) {
                self.explorer_edit = None;
            } else {
                self.commit_explorer_edit();
            }
        }
        self.dismiss_hover();
        match hit {
            Hit::ColorPickerBox | Hit::ColorPickerPicked | Hit::ColorPickerOriginal | Hit::ColorPickerPart(_) => {
                return self.click_color_picker(hit, x, y);
            }
            Hit::ColorSwatch(g, pos) => return self.open_color_picker(g, pos),
            _ => {
                self.close_color_picker();
            }
        }
        match hit {
            Hit::CompletionRow(i) => return self.click_completion(i),
            Hit::CompletionBox | Hit::HoverPopup => return,
            _ => self.completion = None,
        }

        if let Hit::Toast(id, t) = hit {
            return self.toast_click(id, t);
        }
        if self.palette.is_some() {
            match hit {
                Hit::PaletteRow(i) => {
                    if let Some(p) = &mut self.palette {
                        p.selected = i;
                    }
                    self.palette_accept();
                }
                Hit::PaletteBox => {}
                _ => self.cancel_palette(),
            }
            return;
        }

        match hit {
            Hit::TitleBar => {
                self.effects.push(if count == 2 { Effect::ToggleMaximize } else { Effect::DragWindow });
            }
            Hit::CommandCenter => self.run(Command::QuickOpen),
            Hit::ToolbarProject => self.run(Command::OpenRecent),
            Hit::ToolbarBranch => {
                if self.repo.is_some() {
                    self.checkout_picker();
                }
            }
            Hit::ToolbarRun => self.run(if self.debugging() { Command::DebugStop } else { Command::DebugStart }),
            Hit::SwitcherMore => self.switcher_more_menu(x, y),
            Hit::Manage => self.manage_menu(x, y),
            Hit::ToggleSidebarButton => self.run(Command::ToggleSidebar),
            Hit::TogglePanelButton => self.run(Command::TogglePanel),
            Hit::Activity(v) => {
                if self.view == v && self.sidebar_visible {
                    self.sidebar_visible = false;
                } else {
                    self.show_view(v);
                }
            }
            Hit::SidebarSash => self.drag = Some(Drag::SidebarSash),
            Hit::SectionSash(i) => self.drag = Some(Drag::SectionSash(i)),
            Hit::ScmGraphSection => self.toggle_scm_graph(),
            Hit::ScmGraphBody => {}
            Hit::ScmGraphRow(i) => self.click_scm_graph_row(i),
            Hit::ScmGraphSash => self.drag = Some(Drag::ScmGraphSash),
            Hit::TimelineBody => {}
            Hit::TimelineRow(i) => self.open_timeline_entry(i),
            Hit::SignatureHelp => {}
            Hit::SignatureCycle(next) => self.cycle_signature(if next { 1 } else { -1 }),
            Hit::BreadcrumbSymbol(g, i) => self.breadcrumb_menu(g, i),
            Hit::Image(g) => self.click_image(g, alt),
            Hit::ProblemsFilterField => {
                self.focus = Focus::ProblemsFilter;
                self.problems_filter.field.click(x, shift);
            }
            Hit::ProblemsFilterMenu => self.problems_filter_menu(x, y + 12.0),
            Hit::SearchEditorHeader(g) => self.active_group = g,
            Hit::MergeInput(g, _) => self.active_group = g,
            Hit::MergeAction(g, i) => self.click_merge_action(g, i),
            Hit::MergeRemaining(g) => {
                self.active_group = g;
                self.focus = Focus::Editor;
                self.merge_go_to_conflict(true);
            }
            // Not the second click of the double-click that opened the merge editor.
            Hit::MergeComplete(g) if count == 1 && !self.merge_just_opened(g) => {
                self.active_group = g;
                self.complete_merge();
            }
            Hit::MergeComplete(g) => self.active_group = g,
            Hit::OpenMergeEditor(g) => {
                self.active_group = g;
                self.open_merge_editor_for_active();
            }
            Hit::SearchOpenInEditor => self.open_search_results_in_editor(),
            Hit::SearchEditorField(g, f) => self.click_search_editor_field(g, f, x, shift),
            Hit::SearchEditorToggle(g, t) => {
                self.active_group = g;
                self.search_editor_toggle(t);
            }
            Hit::TestingFilter => {
                self.focus = Focus::TestingFilter;
                self.testing.filter.click(x, shift);
            }
            Hit::TestingBody => self.focus = Focus::Testing,
            Hit::TestingRow(i) => self.click_testing_row(i, count),
            Hit::TestingTwistie(i) => self.click_testing_twistie(i, count),
            Hit::TestingRowAction(i, a) => self.testing_row_action(i, a),
            Hit::TestingButton(b) => self.testing_button(b),
            Hit::Markdown(g) => {
                self.active_group = g;
                self.focus = Focus::Editor;
                self.click_markdown(g, x, y);
            }
            Hit::PeekBody => self.focus = Focus::Peek,
            Hit::PeekPreview => {
                self.focus = Focus::Peek;
                if count >= 2 {
                    self.peek_key(&crate::input::KeyInput { key: Key::Enter, text: None, cmd: false, shift: false, alt: false, ctrl: false });
                }
            }
            Hit::PeekRow(i) => self.click_peek_row(i, count),
            Hit::PeekClose => self.close_peek(),
            Hit::PeekTwistie(n) => {
                self.focus = Focus::Peek;
                self.toggle_call_node(n);
            }
            Hit::PeekToggleCalls => {
                self.peek_toggle_calls();
            }
            Hit::CodeLens(g, i) if self.groups[g].tabs.get(self.groups[g].active).is_some_and(|t| t.merge.is_some()) => {
                self.active_group = g;
                self.run_merge_lens(g, i);
            }
            Hit::ColorSwatch(..) | Hit::ColorPickerBox | Hit::ColorPickerPicked | Hit::ColorPickerOriginal | Hit::ColorPickerPart(_) => {}
            Hit::CodeLens(g, i) => {
                self.active_group = g;
                if let Some(doc) = self.groups[g].tabs.get(self.groups[g].active).map(|t| t.doc) {
                    self.run_code_lens(doc, i);
                }
            }
            Hit::StickyLine(g, line) => {
                self.active_group = g;
                self.focus = Focus::Editor;
                let gr = &mut self.groups[g];
                if let Some(ed) = gr.tabs.get_mut(gr.active) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.go_to_sticky_line(doc, line);
                    }
                }
            }
            Hit::GlyphMargin(g) => {
                self.active_group = g;
                self.debug_click_glyph(g, x, y);
            }
            Hit::DebugSection(i) => self.debug.view.open[i] = !self.debug.view.open[i],
            Hit::DebugBody(_) | Hit::DebugToolbarBody => {}
            Hit::DebugRow(sec, i) => self.click_debug_row(sec, i, count),
            Hit::DebugRowAction(sec, i, a) => self.debug_row_action(sec, i, a),
            Hit::DebugAction(sec, a) => self.debug_section_action(sec, a),
            Hit::DebugSash(i) => self.drag = Some(Drag::DebugSash(i)),
            Hit::DebugToolbar(b) => self.debug_toolbar(b),
            Hit::DebugConsoleBody => self.focus = Focus::DebugConsole,
            Hit::DebugConsoleInput => {
                self.focus = Focus::DebugConsole;
                self.debug.console_input.click(x, shift);
            }
            Hit::DebugStartButton => self.debug_start_button(),
            Hit::DebugConfigPicker => self.debug_config_picker(),
            Hit::DebugGear => self.open_launch_json(),
            Hit::DebugCreateLaunch => self.open_launch_json(),
            Hit::ToggleAuxButton => self.run(Command::ToggleAuxiliaryBar),
            Hit::AuxBody => {}
            Hit::AuxTab(i) => self.aux.tab = aux_bar::AuxTab::ALL[i as usize],
            Hit::AuxClose => self.run(Command::ToggleAuxiliaryBar),
            Hit::AuxSash => self.drag = Some(Drag::AuxSash),
            Hit::AssistantAgent(i) => {
                if let Some(a) = crate::agents::AGENTS.get(i as usize) {
                    let action = self.agent_setup_action(a);
                    self.agent_action(action);
                }
            }
            Hit::AssistantCustomAgent => self.agent_action(AgentAction::Custom),
            Hit::AssistantAction(e, k) => {
                if let Some(assistant::Entry::Action(_, actions)) = self.assistant.cur().entries.get(e) {
                    if let Some((_, action)) = actions.get(k).cloned() {
                        self.agent_action(action);
                    }
                }
            }
            Hit::AssistantAgentMenu => self.assistant_agent_menu(),
            Hit::AssistantBody => {}
            Hit::AssistantReview(e, d) => self.assistant_review(e, d),
            Hit::AssistantOption(e, o) => self.assistant_answer(e, o),
            Hit::AssistantAuth(e, m) => {
                if let Some(assistant::Entry::Auth(methods)) = self.assistant.cur().entries.get(e) {
                    if let Some((id, _)) = methods.get(m).cloned() {
                        self.assistant_authenticate(&id);
                    }
                }
            }
            Hit::AssistantChip => self.assistant.send_file = !self.assistant.send_file,
            Hit::AssistantNewChat => self.assistant_new_chat_menu(),
            Hit::AssistantHistory => self.assistant_toggle_history(),
            Hit::AssistantModeMenu => self.assistant_mode_menu(),
            Hit::Welcome(w) => self.welcome_click(w),
            Hit::AssistantModelMenu => self.assistant_model_menu(),
            Hit::AssistantTab(i) => self.select_chat(i),
            Hit::AssistantTabClose(i) => self.close_chat(i),
            Hit::AssistantHistoryRow(i) => self.open_saved_chat(i),
            Hit::AssistantHistoryDelete(i) => {
                if let Some(id) = self.assistant.saved.get(i).map(|m| m.id.clone()) {
                    self.agent_action(AgentAction::DeleteChat(id));
                }
            }
            Hit::AssistantInput => {
                self.focus = Focus::Assistant;
                self.assistant.cur_mut().input.click(x, shift);
            }
            Hit::AssistantStop => self.assistant_cancel(),
            Hit::AssistantSend => self.assistant_send(),
            Hit::OutlineBody => self.focus = Focus::Outline,
            Hit::OutlineRow(i) => self.outline_click(i, count),
            Hit::OutlineTwistie(i) => self.outline_toggle_row(i),
            Hit::OutlineAction(a) => self.outline_action(a),
            Hit::PanelSash => self.drag = Some(Drag::PanelSash),
            Hit::SidebarBody if self.view == View::Explorer => self.focus = Focus::Explorer,
            Hit::SidebarBody => {}
            Hit::SearchField(f) => self.click_search_field(f, x, shift),
            Hit::FindWidgetBox(g) => {
                self.active_group = g;
                self.focus = Focus::Find;
            }
            Hit::FindField(g, f) => self.click_find_field(g, f, x, shift),
            Hit::Scm(a) => self.scm_action(a),
            Hit::StatusItem(0) if self.repo.is_some() => self.checkout_picker(),
            Hit::StatusSync => self.status_sync_clicked(),
            Hit::StatusLanguage => self.change_language_mode(),
            Hit::ConflictAction(g, i) => self.click_conflict_action(g, i),
            Hit::RenameBox => self.click_rename(x, shift),
            Hit::Lightbulb(g) => self.open_lightbulb(g),
            Hit::FoldControl(g, line) => {
                self.active_group = g;
                self.focus = Focus::Editor;
                if let Some((ed, _)) = self.active_mut() {
                    ed.folds.toggle_at(line);
                }
            }
            Hit::ScmMessage => self.click_scm_message(x, shift),
            Hit::ScmRow(r) => self.click_scm_row(r),
            Hit::ScmRepo(i) => self.click_scm_repo(i),
            Hit::Diff(g) => {
                self.active_group = g;
                self.focus = Focus::Editor;
            }
            Hit::Settings(h) => self.click_settings(h, x, shift),
            Hit::FindAction(g, a) => {
                self.active_group = g;
                self.find_action(a);
            }
            Hit::SearchToggle(t) => self.search_toggle(t),
            Hit::ReplaceAll => self.replace_all(),
            Hit::SearchRow(r) => self.open_search_row(r),
            Hit::SearchRefresh => self.run_search(),
            Hit::SearchClear => self.clear_search(),
            Hit::SearchCollapse => self.collapse_all_search(),
            Hit::ExplorerSection => self.explorer_open = !self.explorer_open,
            Hit::ExplorerEditField => {
                self.focus = Focus::Explorer;
                if let Some(e) = &mut self.explorer_edit {
                    e.field.click(x, shift);
                }
            }
            Hit::ExplorerAction(a) => match a {
                0 => self.explorer_new(false),
                1 => self.explorer_new(true),
                2 => self.run(Command::RefreshExplorer),
                _ => {
                    if let Some(tree) = &mut self.tree {
                        tree.collapse_all();
                    }
                }
            },
            Hit::ExplorerRow(i) => {
                self.focus = Focus::Explorer;
                self.drag = Some(Drag::ExplorerItem { i, from: (x, y), moving: false });
                let Some(tree) = &mut self.tree else { return };
                tree.selected = Some(i);
                let row = tree.rows[i].clone();
                if row.is_dir {
                    tree.toggle(i);
                } else if count >= 2 {
                    // Double-click keeps the file open (not a preview).
                    self.open_file(&row.path);
                    self.pin_active();
                    self.focus = Focus::Editor;
                } else {
                    self.open_file_preview(&row.path);
                }
            }
            Hit::OpenFolderButton => self.run(Command::OpenFolder),
            Hit::Tab(g, i) => {
                self.active_group = g;
                self.groups[g].active = i;
                self.focus = Focus::Editor;
                if count == 2 {
                    self.pin_active();
                }
                self.start_tab_drag(g, i, x, y);
            }
            Hit::TabClose(g, i) => {
                self.close_tab(g, i);
            }
            Hit::TabBar(g) | Hit::EmptyGroup(g) => {
                self.active_group = g;
                self.focus = Focus::Editor;
                if count == 2 && hit == Hit::TabBar(g) {
                    self.new_file();
                }
            }
            Hit::SplitButton(g) => {
                self.active_group = g;
                self.split();
            }
            Hit::PreviewButton(g) => {
                self.active_group = g;
                self.open_markdown_preview(true);
            }
            Hit::Editor(g) => {
                self.active_group = g;
                self.focus = Focus::Editor;
                let gr = &mut self.groups[g];
                let mut definition_at = None;
                let mut search_result = None;
                if let Some(ed) = gr.tabs.get_mut(gr.active) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        // ⌥-click adds a cursor; ⇧⌥-drag selects a column.
                        if alt && shift {
                            let from = ed.column_at(doc, x, y);
                            ed.column_select(doc, from, x, y);
                            self.drag = Some(Drag::Column(g, from));
                            return;
                        }
                        if alt {
                            if count == 1 {
                                ed.toggle_cursor(doc, x, y);
                            }
                            return;
                        }
                        ed.click(doc, x, y, count, shift);
                        if cmd {
                            definition_at = ed.pos_at_strict(doc, x, y);
                        }
                        // A double-click on a search editor's result opens it.
                        if count == 2 && ed.search.is_some() {
                            search_result = Some(ed.sel.head);
                        }
                    }
                }
                if let Some(pos) = search_result {
                    if self.open_search_editor_result(pos) {
                        self.drag = None;
                        return;
                    }
                }
                // Cmd+click opens a link under the pointer (`editor.links`), else jumps to the
                // definition.
                let link = definition_at.and_then(|pos| {
                    let doc = self.active_doc()?;
                    crate::editor::link_at(&doc.buffer.line(pos.line), pos.col).filter(|_| self.settings.bool("editor.links"))
                });
                if let Some(url) = link {
                    self.drag = None;
                    let _ = std::process::Command::new("open").arg(&url).spawn();
                    return;
                }
                match definition_at {
                    Some(pos) => self.go_to_definition(Some(pos)),
                    None => self.drag = Some(Drag::Select(g)),
                }
            }
            Hit::Scrollbar(g) => {
                self.active_group = g;
                let gr = &mut self.groups[g];
                if let Some(ed) = gr.tabs.get_mut(gr.active) {
                    let slider = ed.geom.slider;
                    let grab = if slider.contains(x, y) { y - slider.y } else { slider.h / 2.0 };
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.scroll_to_slider(doc, y - grab);
                    }
                    self.drag = Some(Drag::Slider(g, grab));
                }
            }
            Hit::Minimap(g) => {
                self.active_group = g;
                let gr = &mut self.groups[g];
                if let Some(ed) = gr.tabs.get_mut(gr.active) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.scroll_to_minimap(doc, y);
                    }
                }
                self.drag = Some(Drag::Minimap(g));
            }
            Hit::PanelTab(i) => {
                self.panel_tab = i;
                if i == PANEL_TERMINAL {
                    self.show_terminal();
                } else if self.focus == Focus::Terminal {
                    self.focus = Focus::Editor;
                }
            }
            Hit::PanelMaximize => self.panel_maximized = !self.panel_maximized,
            Hit::OutputChannels => self.pick_output_channel(x, y + 16.0),
            Hit::ExtStatus(i) => self.ext_status_click(i),
            Hit::Ext(h) => self.extensions_click(h, x, y),
            Hit::ExtTree(v, h) => self.ext_tree_click(v as usize, h, x, y),
            Hit::ExtPage(g, h) => self.extension_page_click(g, h),
            Hit::Toast(..) => {}
            Hit::PanelClose => {
                self.panel_visible = false;
                if self.focus == Focus::Terminal {
                    self.focus = Focus::Editor;
                }
            }
            Hit::StatusProblems => self.toggle_problems(),
            Hit::StatusServer => self.server_menu(x, y),
            Hit::StatusTerminal => self.run(Command::ToggleTerminal),
            Hit::ProblemRow(i) => self.open_problem(i),
            Hit::TerminalPane(p) => self.terminal_mouse_down(p, x, y),
            Hit::TerminalTab(g, p) => self.click_terminal_tab(g, p),
            Hit::TerminalTabAction(g, p, split) => self.terminal_tab_action(g, p, split),
            Hit::NewTerminal => self.new_terminal(),
            Hit::SplitTerminal => self.split_terminal(),
            Hit::PanelBody | Hit::StatusItem(_) => {}
            Hit::PaletteBackdrop | Hit::PaletteBox | Hit::PaletteRow(_) => {}
            Hit::HoverPopup | Hit::CompletionBox | Hit::CompletionRow(_) => {}
        }
    }

    pub fn mouse_up(&mut self) {
        if let Some(Drag::ColorPicker(_)) = self.drag {
            self.color_picker_released();
        }
        if let Some(Drag::ExplorerItem { i, moving: true, .. }) = self.drag {
            self.drag = None;
            let (x, y) = self.mouse;
            match self.hit_at(x, y) {
                Some(Hit::ExplorerRow(j)) if j != i => self.explorer_drop(i, Some(j)),
                Some(Hit::SidebarBody) if self.view == View::Explorer => self.explorer_drop(i, None),
                _ => {}
            }
        }
        self.drag = None;
        if self.terminal_selecting() {
            self.terminal_mouse_up();
        }
        let (x, y) = self.mouse;
        self.mouse_move(x, y);
    }

    pub fn scroll(&mut self, dx: f32, dy: f32) {
        let (x, y) = self.mouse;
        let hit = self.hit_at(x, y);
        if !matches!(hit, Some(Hit::HoverPopup)) {
            self.dismiss_hover();
        }
        match hit {
            Some(Hit::CompletionRow(_) | Hit::CompletionBox) => self.scroll_completion(dy),
            Some(Hit::TerminalPane(p)) => self.terminal_scroll(p, dy),
            Some(Hit::Welcome(_)) => self.welcome_scroll(dy),
            Some(Hit::Diff(g)) => {
                let gr = &mut self.groups[g];
                if let Some(diff) = gr.tabs.get_mut(gr.active).and_then(|t| t.diff.as_mut()) {
                    diff.scroll_by(dx, dy);
                }
            }
            Some(Hit::Settings(_)) => self.scroll_settings(dy),
            Some(Hit::SearchRow(_)) => self.scroll_search(dy),
            Some(Hit::SidebarBody) if self.view == View::Search => self.scroll_search(dy),
            Some(Hit::ScmRow(_) | Hit::Scm(_)) => self.scroll_scm(dy),
            Some(Hit::SidebarBody) if self.view == View::Scm => self.scroll_scm(dy),
            Some(Hit::TestingRow(_) | Hit::TestingRowAction(..) | Hit::TestingTwistie(_) | Hit::TestingBody) => self.scroll_testing(dy),
            Some(Hit::PanelBody) if self.panel_tab == PANEL_TEST_RESULTS => self.scroll_test_results(dy),
            Some(Hit::PanelBody) if self.panel_tab == 1 => self.scroll_output(dy),
            Some(Hit::Ext(_)) => self.scroll_extensions(dy),
            Some(Hit::ExtTree(v, _)) => self.scroll_ext_tree(v as usize, dy),
            Some(Hit::ProblemRow(_)) | Some(Hit::PanelBody) if self.panel_tab == 0 => {
                self.problems_scroll = (self.problems_scroll - dy).max(0.0);
            }
            Some(Hit::Editor(g) | Hit::Minimap(g) | Hit::Scrollbar(g) | Hit::StickyLine(g, _) | Hit::CodeLens(g, _) | Hit::MergeInput(g, _) | Hit::MergeAction(g, _)) => {
                let gr = &mut self.groups[g];
                if let Some(ed) = gr.tabs.get_mut(gr.active) {
                    if let Some(doc) = self.docs[ed.doc].as_ref() {
                        ed.scroll_by(doc, dx, dy);
                    }
                }
            }
            Some(Hit::OutlineRow(_) | Hit::OutlineTwistie(_) | Hit::OutlineBody) => self.outline_scroll(dy),
            Some(Hit::DebugBody(i) | Hit::DebugRow(i, _) | Hit::DebugRowAction(i, _, _)) => self.debug_scroll(i, dy),
            Some(Hit::DebugConsoleBody) => self.debug_console_scroll(dy),
            Some(Hit::AssistantBody | Hit::AssistantReview(..) | Hit::AssistantOption(..) | Hit::AssistantAuth(..) | Hit::AssistantAction(..) | Hit::AssistantHistoryRow(_) | Hit::AssistantHistoryDelete(_)) => self.assistant_scroll(dy),
            Some(Hit::PeekRow(_) | Hit::PeekBody | Hit::PeekTwistie(_)) => self.peek_scroll(dy),
            Some(Hit::Image(g)) => {
                let gr = &mut self.groups[g];
                if let Some(pv) = gr.tabs.get_mut(gr.active).and_then(|t| t.image.as_mut()) {
                    pv.scroll_by(dx, dy);
                }
            }
            Some(Hit::Markdown(g)) => {
                let gr = &mut self.groups[g];
                if let Some(pv) = gr.tabs.get_mut(gr.active).and_then(|t| t.markdown.as_mut()) {
                    pv.scroll_by(dy);
                }
            }
            Some(Hit::TimelineRow(_) | Hit::TimelineBody) => self.timeline_scroll(dy),
            Some(Hit::ScmGraphRow(_) | Hit::ScmGraphBody) => self.scm_graph_scroll(dy),
            Some(Hit::ExplorerRow(_) | Hit::SidebarBody) => {
                let body_h = self.explorer_body_height();
                if let Some(tree) = &mut self.tree {
                    let max = (tree.rows.len() as f32 * ROW_H - body_h + ROW_H).max(0.0);
                    tree.scroll = (tree.scroll - dy).clamp(0.0, max);
                }
            }
            Some(Hit::PaletteRow(_)) => {
                if let Some(p) = &mut self.palette {
                    let steps = (-dy / PALETTE_ROW).round() as isize;
                    let max = p.items.len().saturating_sub(MAX_VISIBLE);
                    p.scroll = (p.scroll as isize + steps).clamp(0, max as isize) as usize;
                }
            }
            _ => {}
        }
    }

    pub fn drop_path(&mut self, path: &Path) {
        if path.is_dir() {
            self.open_folder(path);
        } else {
            self.open_file(path);
            self.focus = Focus::Editor;
        }
    }

    // ------------------------------------------------------------------ drawing

    /// The window gained focus: pick up changes made outside the editor.
    pub fn window_focused(&mut self) {
        self.refresh_scm();
        self.reload_settings();
        self.reload_keymap(false);
    }

    pub fn draw(&mut self, c: &mut Canvas) {
        crate::ime::begin_frame();
        self.a11y.clear();
        self.a11y_names.clear();
        crate::widgets::take_drawn_fields();
        c.set_ui_mono(self.ui_mono);
        let missing = c.set_mono_font(&self.font_family);
        if !missing.is_empty() {
            // Say so instead of quietly using another font.
            let names = missing.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(", ");
            self.set_status_message(&format!("Font not installed: {names} (editor.fontFamily)"));
        }
        if self.status_text().is_none() {
            self.status_message = None;
        }
        self.auto_save_tick();
        // The Timeline fetches history only while its tab shows.
        self.set_timeline_open(self.aux_showing(aux_bar::AuxTab::Timeline));
        self.scm_tick();
        self.watch_tick();
        self.workspace_tick();
        self.lsp_tick();
        self.linked_editing_tick();
        self.color_tick();
        self.search_tick();
        self.search_editor_tick();
        self.debug_tick();
        self.tasks_tick();
        self.ext_tick();
        self.ext_views_sync();
        self.ext_decorations_tick();
        self.marketplace_tick();
        self.updates_tick();
        self.assistant_tick();
        self.mcp_tick();
        self.hits.clear();
        let (w, h) = c.size();
        let full = Rect::new(0.0, 0.0, w, h);
        // Zen Mode: no title bar, activity bar or status bar, and the editors centered.
        let zen = self.zen.is_some();
        let (title, rest) = full.cut_top(if zen { 0.0 } else { TITLE_H });
        let (rest, status) = rest.cut_bottom(if zen { 0.0 } else { STATUS_H });
        self.main_rect = rest;
        let (sidebar, mut main) = if self.sidebar_visible {
            self.sidebar_w = self.sidebar_w.clamp(170.0, (rest.w - 200.0).max(170.0));
            rest.cut_left(self.sidebar_w)
        } else {
            (Rect::default(), rest)
        };
        let aux = if self.aux.visible && !zen {
            self.aux.width = self.aux.width.clamp(aux_bar::AUX_MIN_W, (main.w - 300.0).max(aux_bar::AUX_MIN_W));
            let (m, aux) = main.cut_right(self.aux.width);
            main = m;
            aux
        } else {
            Rect::default()
        };
        let (editors, panel) = if self.panel_visible {
            if self.panel_maximized {
                (Rect::default(), main)
            } else {
                self.panel_h = self.panel_h.clamp(80.0, (main.h - 120.0).max(80.0));
                main.cut_bottom(self.panel_h)
            }
        } else {
            (main, Rect::default())
        };
        let editors = if zen && !self.sidebar_visible && editors.w > 900.0 {
            // Centered layout, like the standard zenMode.centerLayout.
            let w = (editors.w * 0.6).max(900.0);
            c.fill(editors, self.color("editor.background"));
            Rect::new(editors.x + ((editors.w - w) / 2.0).round(), editors.y, w, editors.h)
        } else {
            editors
        };

        if !zen {
            self.draw_title_bar(c, title);
            self.a11y_area(a11y::TOOLBAR, "Toolbar", title);
        }
        if self.sidebar_visible {
            self.draw_sidebar(c, sidebar);
            self.a11y_area(a11y::SIDEBAR, "Primary Side Bar", sidebar);
            self.hits.push((Rect::new(sidebar.right() - 2.0, sidebar.y, 4.0, sidebar.h), Hit::SidebarSash));
        }
        if aux.w > 0.0 {
            self.draw_aux_bar(c, aux);
            self.a11y_area(a11y::SECONDARY_SIDEBAR, "Secondary Side Bar", aux);
            self.hits.push((Rect::new(aux.x - 2.0, aux.y, 4.0, aux.h), Hit::AuxSash));
        }
        if editors.h > 0.0 {
            self.a11y_area(a11y::EDITORS, "Editor", editors);
            self.draw_editor_groups(c, editors);
        }
        if self.panel_visible {
            self.draw_panel(c, panel);
            self.a11y_area(a11y::PANEL, "Panel", panel);
            if !self.panel_maximized {
                self.hits.push((Rect::new(panel.x, panel.y - 2.0, panel.w, 4.0), Hit::PanelSash));
            }
        }
        if !zen {
            self.draw_status_bar(c, status);
            self.a11y_area(a11y::STATUS_BAR, "Status Bar", status);
        }

        // Sash highlight while hovering or dragging.
        let sash_color = self.theme.color("sash.hoverBorder");
        if matches!(self.drag, Some(Drag::SidebarSash)) || (self.drag.is_none() && self.hover_hit == Some(Hit::SidebarSash)) {
            c.fill(Rect::new(sidebar.right() - 2.0, sidebar.y, 4.0, sidebar.h), sash_color);
        }
        if aux.w > 0.0 && (matches!(self.drag, Some(Drag::AuxSash)) || (self.drag.is_none() && self.hover_hit == Some(Hit::AuxSash))) {
            c.fill(Rect::new(aux.x - 2.0, aux.y, 4.0, aux.h), sash_color);
        }
        if matches!(self.drag, Some(Drag::PanelSash)) || (self.drag.is_none() && self.hover_hit == Some(Hit::PanelSash)) {
            c.fill(Rect::new(panel.x, panel.y - 2.0, panel.w, 4.0), sash_color);
        }

        self.draw_intel_overlays(c);
        if self.settings_active() {
            self.draw_settings_sheet(c, full);
        }
        self.draw_toasts(c, Rect::new(main.x, main.y, main.w, main.h));
        if self.palette.is_some() {
            self.draw_palette(c, full);
        }
        self.draw_preedit(c);
        self.a11y_finish();

        let title = self.window_title();
        if title != self.title {
            self.title = title.clone();
            self.effects.push(Effect::SetTitle(title));
        }
        if self.drag.is_none() {
            let (x, y) = self.mouse;
            self.hover_hit = self.hit_at(x, y);
        }
    }

    fn window_title(&self) -> String {
        let folder = self.workspace_label();
        let file = self.active_doc().map(|d| format!("{}{}", if d.buffer.is_dirty() { "● " } else { "" }, d.title()));
        match (file, folder) {
            (Some(f), Some(d)) => format!("{f} — {d}"),
            (Some(f), None) => f,
            (None, Some(d)) => d,
            (None, None) => "Orbvane".into(),
        }
    }

    fn color(&self, key: &str) -> Color {
        self.theme.color(key)
    }

    /// `key`, or `fallback` when the theme leaves `key` unset.
    /// The side bar's colors fall back to the workbench foreground in turn.
    fn color_or(&self, key: &str, fallback: &str) -> Color {
        self.theme.color_opt(key).or_else(|| self.theme.color_opt(fallback)).unwrap_or_else(|| self.theme.color("foreground"))
    }

    fn hovered(&self, hit: Hit) -> bool {
        self.drag.is_none() && self.hover_hit == Some(hit)
    }

    fn icon_button(&mut self, c: &mut Canvas, r: Rect, icon: &Icon, hit: Hit, color: Color) {
        if self.hovered(hit) {
            c.fill_rounded(r, self.color("toolbar.hoverBackground"), 5.0);
        }
        c.icon_in(icon, r, 16.0, color);
        self.hits.push((r, hit));
    }

    /// The current branch for the toolbar, marked with * when the working tree has changes.
    fn branch_label(&self) -> Option<String> {
        match &self.repo {
            Some(repo) => {
                let st = &repo.status;
                let name = st.branch.clone().or_else(|| st.detached_at.clone()).or_else(|| self.git_branch.clone());
                name.map(|n| if st.unstaged.is_empty() && st.conflicts.is_empty() { n } else { format!("{n}*") })
            }
            None => self.git_branch.clone(),
        }
    }

    /// The toolbar: sidebar toggle and the project/branch pill on the left, the search field
    /// in the middle, run, panel toggle and the gear menu on the right.
    fn draw_title_bar(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, self.color("titleBar.activeBackground"));
        c.fill(Rect::new(r.x, r.bottom() - 1.0, r.w, 1.0), self.color("titleBar.border"));
        self.hits.push((r, Hit::TitleBar));
        let fg = self.color("titleBar.activeForeground");
        let dim = self.color("commandCenter.foreground");
        let (y, h) = (r.y + 6.0, 23.0);
        let style = TextStyle::ui(12.0, fg);

        // Left: the sidebar toggle, then the project and its branch.
        let toggle = Rect::new(r.x + TRAFFIC_LIGHTS_W, y, 26.0, h);
        self.icon_button(c, toggle, &icons::LAYOUT_SIDEBAR, Hit::ToggleSidebarButton, fg);
        if self.sidebar_visible {
            c.fill(Rect::new(toggle.x + 6.5, toggle.y + 6.5, 4.0, 10.0), fg);
        }
        // Right: run (stop while debugging), the panel toggle and the gear menu.
        let gear = Rect::new(r.right() - 36.0, y, 26.0, h);
        let aux = Rect::new(gear.x - 30.0, y, 26.0, h);
        let panel = Rect::new(aux.x - 30.0, y, 26.0, h);
        let run = Rect::new(panel.x - 34.0, y, 26.0, h);
        self.icon_button(c, aux, &icons::LAYOUT_SIDEBAR_RIGHT, Hit::ToggleAuxButton, fg);
        if self.aux.visible {
            c.fill(Rect::new(aux.x + 15.5, aux.y + 6.5, 4.0, 10.0), fg);
        }
        self.icon_button(c, gear, &icons::GEAR, Hit::Manage, fg);
        if self.update_ready() {
            let dot = Rect::new(gear.right() - 8.0, gear.y + 2.0, 7.0, 7.0);
            c.fill_rounded(dot, self.color("activityBarBadge.background"), 3.5);
        }
        self.icon_button(c, panel, &icons::LAYOUT_PANEL, Hit::TogglePanelButton, fg);
        if self.panel_visible {
            c.fill(Rect::new(panel.x + 6.5, panel.y + 12.5, 13.0, 4.0), fg);
        }
        let (run_icon, run_color) = if self.debugging() {
            (&icons::DEBUG_STOP, self.color("debugIcon.stopForeground"))
        } else {
            (&icons::DEBUG_START, self.color("debugIcon.startForeground"))
        };
        self.icon_button(c, run, run_icon, Hit::ToolbarRun, run_color);

        let pill_x = toggle.right() + 8.0;
        let right_edge = run.x - 12.0;
        let room = (right_edge - pill_x).max(0.0);
        let project = self.workspace_label().unwrap_or_else(|| "No folder".to_string());
        let branch = self.branch_label();
        let pw = c.measure(&project, &style).min(room * 0.25).max(40.0);
        let bw = branch.as_ref().map_or(0.0, |b| c.measure(b, &style).min(room * 0.18).max(30.0));
        let project_w = 8.0 + 14.0 + 6.0 + pw + 9.0;
        let branch_w = if branch.is_some() { 1.0 + 9.0 + 14.0 + 5.0 + bw + 9.0 } else { 0.0 };
        let pill = Rect::new(pill_x, y, project_w + branch_w, h);
        if room > pill.w + 60.0 {
            c.bordered(pill, self.color("commandCenter.background"), self.color("commandCenter.border"), 1.0, h / 2.0);
            let left = Rect::new(pill.x, y, project_w, h);
            if self.hovered(Hit::ToolbarProject) {
                c.fill_rounded(left, self.color("commandCenter.activeBackground"), h / 2.0);
            }
            c.icon(&icons::FOLDER, left.x + 8.0, y + 4.5, 14.0, dim);
            c.text_fit(Rect::new(left.x + 28.0, y, pw + 1.0, h), &project, &style);
            self.hits.push((left, Hit::ToolbarProject));
            self.a11y_name(Hit::ToolbarProject, a11y::Role::Button, format!("Project: {project}"), false);
            if let Some(branch) = &branch {
                let right = Rect::new(left.right(), y, branch_w, h);
                if self.hovered(Hit::ToolbarBranch) {
                    c.fill_rounded(right, self.color("commandCenter.activeBackground"), h / 2.0);
                }
                c.fill(Rect::new(right.x, y + 5.0, 1.0, h - 10.0), self.color("commandCenter.border"));
                c.icon(&icons::BRANCH, right.x + 10.0, y + 4.5, 14.0, dim);
                c.text_fit(Rect::new(right.x + 29.0, y, bw + 1.0, h), branch, &style);
                self.hits.push((right, Hit::ToolbarBranch));
                self.a11y_name(Hit::ToolbarBranch, a11y::Role::Button, format!("Branch: {branch}"), false);
            }
        }

        // Middle: the search field (Go to File; > for commands), centered on the window
        // when there's room.
        let after_pill = if room > pill.w + 60.0 { pill.right() + 12.0 } else { pill_x };
        let space = (right_edge - after_pill).max(0.0);
        let cw = (r.w * 0.36).clamp(160.0, 520.0).min(space);
        if cw >= 120.0 {
            let cx = ((r.w - cw) / 2.0).round().clamp(after_pill, right_edge - cw);
            let cc = Rect::new(cx, y, cw, h);
            let hovered = self.hovered(Hit::CommandCenter);
            let bg = if hovered { self.color("commandCenter.activeBackground") } else { self.color("commandCenter.background") };
            c.bordered(cc, bg, self.color("commandCenter.border"), 1.0, 6.0);
            let muted = TextStyle::ui(12.0, dim);
            c.icon(&icons::SEARCH, cc.x + 9.0, y + 4.5, 14.0, dim);
            let keys = crate::keymap::keycaps(Command::QuickOpen).map(|k| k.concat());
            let kw = keys.as_ref().map_or(0.0, |k| c.measure(k, &muted) + 10.0);
            c.text_fit(Rect::new(cc.x + 29.0, y, (cc.w - 29.0 - kw - 4.0).max(0.0), h), "Search files, or > for commands", &muted);
            if let Some(k) = keys.filter(|_| cc.w > 260.0) {
                c.text_in(Rect::new(cc.right() - kw, y, kw, h), &k, &muted);
            }
            self.hits.push((cc, Hit::CommandCenter));
        }
    }

    /// The views in the sidebar's switcher, in order.
    fn switcher_views(&self) -> Vec<View> {
        let mut views: Vec<View> = View::ALL.into_iter().filter(|v| *v != View::Testing || self.testing.active()).collect();
        views.extend(self.ext_view_containers());
        views
    }

    /// A view's count badge: pending changes, test results, extension updates.
    fn view_badge(&self, view: View) -> usize {
        match view {
            View::Scm => self.repo.iter().chain(self.parked_repos.iter().map(|p| &p.repo)).map(|r| r.status.change_count()).sum(),
            View::Testing => self.testing_badge(),
            View::Extensions => self.marketplace.updates.len(),
            _ => 0,
        }
    }

    /// The row of view buttons at the top of the sidebar; views that don't fit go in a menu.
    fn draw_switcher(&mut self, c: &mut Canvas, r: Rect) {
        let views = self.switcher_views();
        let (bw, gap) = (30.0, 4.0);
        let fits = (((r.w - 16.0 + gap) / (bw + gap)).floor() as usize).max(1);
        let (shown, more) = if views.len() > fits { (fits - 1, true) } else { (views.len(), false) };
        let mut x = r.x + 8.0;
        let y = r.y + 5.0;
        // A well behind the buttons (themes that set activityBarTop.background).
        let n = shown + usize::from(more);
        let well = Rect::new(x - 3.0, y - 3.0, n as f32 * (bw + gap) - gap + 6.0, 30.0);
        c.fill_rounded(well, self.color("activityBarTop.background"), 8.0);
        for view in views.iter().take(shown) {
            let item = Rect::new(x, y, bw, 24.0);
            let active = self.view == *view;
            let hover = self.hovered(Hit::Activity(*view));
            if active {
                c.fill_rounded(item, self.color_or("activityBarTop.activeBackground", "list.inactiveSelectionBackground"), 6.0);
            } else if hover {
                c.fill_rounded(item, self.color("toolbar.hoverBackground"), 6.0);
            }
            let color = if active { self.color("activityBarTop.activeBorder") } else if hover { self.color("activityBarTop.foreground") } else { self.color("activityBarTop.inactiveForeground") };
            c.icon_in(view.icon(), item, 16.0, color);
            let count = self.view_badge(*view);
            if count > 0 {
                let label = if count > 99 { "99+".to_string() } else { count.to_string() };
                let st = TextStyle::ui(8.5, self.color("activityBarBadge.foreground")).weight(600);
                let w = (c.measure(&label, &st) + 6.0).max(12.0);
                let badge = Rect::new(item.right() - w + 3.0, item.y - 2.0, w, 12.0);
                c.fill_rounded(badge, self.color("activityBarBadge.background"), 6.0);
                let tw = c.measure(&label, &st);
                c.text_in(Rect::new(badge.x + (w - tw) / 2.0, badge.y, tw + 1.0, badge.h), &label, &st);
            }
            self.hits.push((item, Hit::Activity(*view)));
            if count > 0 {
                self.a11y_name(Hit::Activity(*view), a11y::Role::Tab, format!("{}, {count}", view.title()), active);
            }
            x += bw + gap;
        }
        if more {
            let item = Rect::new(x, y, bw, 24.0);
            let hidden_active = views.iter().skip(shown).any(|v| *v == self.view);
            let color = if hidden_active { self.color("activityBarTop.activeBorder") } else { self.color("activityBarTop.inactiveForeground") };
            if self.hovered(Hit::SwitcherMore) {
                c.fill_rounded(item, self.color("toolbar.hoverBackground"), 6.0);
            }
            c.icon_in(&icons::ELLIPSIS, item, 16.0, color);
            self.hits.push((item, Hit::SwitcherMore));
        }
    }

    /// The switcher's overflow menu: the views that didn't fit.
    fn switcher_more_menu(&mut self, x: f32, y: f32) {
        use preferences::PopupAction;
        let views = self.switcher_views();
        let sidebar_w = if self.sidebar_visible { self.sidebar_w } else { 0.0 };
        let fits = (((sidebar_w - 16.0 + 4.0) / 34.0).floor() as usize).max(1);
        let entries = views
            .into_iter()
            .skip(fits.saturating_sub(1))
            .map(|v| {
                let label = v.title().to_string();
                (PopupItem::Item { label, enabled: true, checked: Some(self.view == v) }, PopupAction::ShowView(v))
            })
            .collect();
        self.show_popup(entries, x, y);
    }

    fn draw_sidebar(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, self.color("sideBar.background"));
        c.fill(Rect::new(r.right() - 1.0, r.y, 1.0, r.h), self.color("sideBar.border"));
        self.hits.push((r, Hit::SidebarBody));
        c.push_clip(r);
        let (switcher, r) = r.cut_top(SWITCHER_H);
        self.draw_switcher(c, switcher);
        let (header, body) = if self.view == View::Explorer { (Rect::default(), r) } else { r.cut_top(TITLE_H - 5.0) };
        let title_style = TextStyle::ui(SMALL, self.color("sideBarTitle.foreground")).weight(600);
        if header.h > 0.0 {
            c.text_in(Rect::new(header.x + 20.0, header.y, header.w - 40.0, header.h), self.view.title(), &title_style);
        }
        let icon_fg = self.color("icon.foreground");
        if self.view == View::Search {
            // Search view actions: refresh, clear results, collapse all.
            let actions = [(&icons::REFRESH, Hit::SearchRefresh), (&icons::CLEAR_ALL, Hit::SearchClear), (&icons::COLLAPSE_ALL, Hit::SearchCollapse)];
            for (i, (icon, hit)) in actions.into_iter().enumerate() {
                let r = Rect::new(header.right() - 32.0 - (2 - i) as f32 * 26.0, header.y + 6.0, 24.0, 22.0);
                self.icon_button(c, r, icon, hit, icon_fg);
            }
        } else if self.view == View::Scm {
            self.draw_scm_header_actions(c, header);
        } else if self.view == View::Debug {
            self.draw_debug_header_actions(c, header);
        } else if self.view == View::Testing {
            self.draw_testing_header_actions(c, header);
        } else if self.view == View::Extensions {
            self.draw_extensions_header_actions(c, header);
        }
        match self.view {
            View::Explorer => self.draw_explorer(c, body),
            View::Search => self.draw_search_view(c, body),
            View::Scm => self.draw_scm_view(c, body),
            View::Debug => self.draw_debug_view(c, body),
            View::Testing => self.draw_testing_view(c, body),
            View::Extensions => self.draw_extensions_view(c, body),
            View::Ext(i) => self.draw_ext_container(c, body, i),
        }
        c.pop_clip();
    }

    /// The text color of a selected row, if the theme sets one (`list.activeSelectionForeground`
    /// while the list has focus, else `list.inactiveSelectionForeground`).
    fn selection_fg(&self, focused: bool) -> Option<Color> {
        let key = if focused { "list.activeSelectionForeground" } else { "list.inactiveSelectionForeground" };
        self.theme.color_opt(key)
    }

    fn section_header(&mut self, c: &mut Canvas, r: Rect, label: &str, open: bool, hit: Hit) {
        c.fill(r, self.color("sideBarSectionHeader.background"));
        // A border above every header but the view's first.
        let top = self.main_rect.y + SWITCHER_H + if self.view == View::Explorer { 0.0 } else { TITLE_H - 5.0 };
        if r.y > top + 1.0 {
            c.fill(Rect::new(r.x, r.y, r.w, 1.0), self.color("sideBarSectionHeader.border"));
        }
        let fg = self.color_or("sideBarSectionHeader.foreground", "sideBar.foreground");
        let icon = if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT };
        c.icon(icon, r.x + 2.0, r.y + 3.0, 16.0, fg);
        let style = TextStyle::ui(SMALL, fg).weight(600);
        c.text_fit(Rect::new(r.x + 20.0, r.y, r.w - 24.0, r.h), &calm(label), &style);
        self.hits.push((r, hit));
    }

    fn draw_explorer(&mut self, c: &mut Canvas, r: Rect) {
        // Sections: the folder tree (headed by the project's name), then extensions' views.
        let s = self.layout_sections(r);
        self.sections = s.clone();
        let (head, rows_rect) = (s.head[0], s.body[0]);
        let root_label = self.workspace_label().unwrap_or_else(|| "No Folder Opened".to_string());
        self.section_header(c, head, &root_label, self.explorer_open, Hit::ExplorerSection);
        if self.tree.is_some() {
            self.draw_explorer_actions(c, head, rows_rect);
        }
        for (k, vi) in self.explorer_ext_views().into_iter().enumerate() {
            let i = sections::BUILTIN + k;
            if let (Some(&head), Some(&body)) = (s.head.get(i), s.body.get(i)) {
                self.draw_ext_view_section(c, head, body, vi);
            }
        }
        for i in 1..s.head.len() {
            if self.has_sash(i) {
                let sash = Rect::new(r.x, s.head[i].y - 2.0, r.w, 4.0);
                self.hits.push((sash, Hit::SectionSash(i)));
                if matches!(self.drag, Some(Drag::SectionSash(j)) if j == i) || (self.drag.is_none() && self.hover_hit == Some(Hit::SectionSash(i))) {
                    c.fill(sash, self.theme.color("sash.hoverBorder"));
                }
            }
        }
        if !self.explorer_open {
            return;
        }

        let fg = self.color_or("sideBar.foreground", "foreground");
        if self.tree.is_none() {
            self.empty_state(c, rows_rect, &icons::FOLDER, "No folder open", "Open a folder to see its files here.", Some(("Open Folder", Hit::OpenFolderButton)));
            return;
        }

        let focused = self.focus == Focus::Explorer && self.palette.is_none();
        let active_path = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf));
        let (git_files, git_dirs) = self.git_decorations();
        let dir_color = self.color("gitDecoration.modifiedResourceForeground");
        let hover = self.hover_hit;
        // While a row is dragged, the folder it would go into is highlighted.
        let drop_dir = match (&self.drag, hover) {
            (Some(Drag::ExplorerItem { moving: true, .. }), Some(Hit::ExplorerRow(j))) => {
                self.tree.as_ref().and_then(|t| t.rows.get(j)).map(|r| if r.is_dir { r.path.clone() } else { r.path.parent().map(Path::to_path_buf).unwrap_or_default() })
            }
            _ => None,
        };
        let caret_on = self.editor_caret_on();
        let (focused_fg, unfocused_fg) = (self.selection_fg(true), self.selection_fg(false));
        let sel_fg = |f: bool| if f { focused_fg } else { unfocused_fg };
        let theme = &self.theme;
        let tree = self.tree.as_mut().unwrap();
        // The rows as shown: an input row for New File / New Folder at the top of its folder.
        #[derive(Clone, Copy, PartialEq)]
        enum Slot {
            Row(usize),
            New(usize),
        }
        let mut slots: Vec<Slot> = (0..tree.rows.len()).map(Slot::Row).collect();
        let mut rename_row = None;
        if let Some(edit) = &self.explorer_edit {
            match &edit.kind {
                file_ops::EditKind::Rename(p) => rename_row = tree.rows.iter().position(|r| &r.path == p),
                _ => {
                    let (at, depth) = match tree.rows.iter().position(|r| r.path == edit.dir) {
                        Some(i) => (i + 1, tree.rows[i].depth + 1),
                        None => (0, 0),
                    };
                    slots.insert(at, Slot::New(depth));
                }
            }
        }
        let max_scroll = (slots.len() as f32 * ROW_H - rows_rect.h + ROW_H).max(0.0);
        tree.scroll = tree.scroll.clamp(0.0, max_scroll);
        c.push_clip(rows_rect);
        let first = (tree.scroll / ROW_H) as usize;
        let visible = (rows_rect.h / ROW_H).ceil() as usize + 1;
        let style = TextStyle::ui(UI, fg);
        let mut hits = Vec::new();
        let mut rows_read = Vec::new();
        let mut field_at = None;
        for (s, slot) in slots.iter().enumerate().skip(first).take(visible) {
            let y = rows_rect.y + s as f32 * ROW_H - tree.scroll;
            let rr = Rect::new(rows_rect.x, y, rows_rect.w, ROW_H);
            let i = match *slot {
                Slot::New(depth) => {
                    let x = rr.x + 8.0 + depth as f32 * config::get().tree_indent;
                    let folder = self.explorer_edit.as_ref().is_some_and(|e| e.kind == file_ops::EditKind::NewFolder);
                    let icon = if folder { &icons::CHEVRON_RIGHT } else { &icons::FILE };
                    c.icon(icon, if folder { x } else { x + 16.0 }, y + 3.0, 16.0, fg);
                    field_at = Some(Rect::new(if folder { x + 18.0 } else { x + 36.0 }, y + 1.0, 0.0, ROW_H - 2.0));
                    continue;
                }
                Slot::Row(i) => i,
            };
            let row = &tree.rows[i];
            let pill = row_pill(rr);
            let mut selected_fg = None;
            if drop_dir.as_ref() == Some(&row.path) {
                c.fill_rounded(pill, theme.color("list.dropBackground"), ROW_RADIUS);
            } else if tree.selected == Some(i) {
                let key = if focused { "list.activeSelectionBackground" } else { "list.inactiveSelectionBackground" };
                c.fill_rounded(pill, theme.color(key), ROW_RADIUS);
                selected_fg = sel_fg(focused);
            } else if hover == Some(Hit::ExplorerRow(i)) {
                c.fill_rounded(pill, theme.color("list.hoverBackground"), ROW_RADIUS);
            } else if active_path.as_deref() == Some(row.path.as_path()) && tree.selected.is_none() {
                c.fill_rounded(pill, theme.color("list.inactiveSelectionBackground"), ROW_RADIUS);
                selected_fg = sel_fg(false);
            }
            let x = rr.x + 8.0 + row.depth as f32 * config::get().tree_indent;
            // Git decorations: colored names, a status letter for files, a dot for folders.
            let git = git_files.get(&row.path).copied();
            let label_style = match (selected_fg, git) {
                (Some(fg), _) => style.color(fg),
                (None, Some(s)) => style.color(scm_view::status_color(theme, s)),
                (None, None) if row.is_dir && git_dirs.contains(&row.path) => style.color(dir_color),
                (None, None) => style,
            };
            let badge_w = if git.is_some() || (row.is_dir && git_dirs.contains(&row.path)) { 22.0 } else { 0.0 };
            let name_x = if row.is_dir { x + 20.0 } else { x + 38.0 };
            if row.is_dir {
                let icon = if row.expanded { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT };
                c.icon(icon, x, y + 3.0, 16.0, fg);
            } else {
                c.icon(&icons::FILE, x + 16.0, y + 3.0, 16.0, file_color(&row.path));
            }
            if rename_row == Some(i) {
                field_at = Some(Rect::new(name_x - 2.0, y + 1.0, 0.0, ROW_H - 2.0));
            } else {
                // A multi-root workspace's folders stand out.
                let label_style = if row.root { label_style.weight(700) } else { label_style };
                c.text_in(Rect::new(name_x, y, rr.right() - name_x - badge_w, ROW_H), &row.name, &label_style);
                if let Some(s) = git {
                    c.text_in(Rect::new(rr.right() - 20.0, y, 14.0, ROW_H), &s.letter().to_string(), &label_style);
                } else if badge_w > 0.0 {
                    c.fill_rounded(Rect::new(rr.right() - 16.0, y + 9.0, 5.0, 5.0), dir_color.with_alpha(0.8), 2.5);
                }
            }
            // Clipped: the last row reaches under the next section's header.
            let hit_rect = rr.intersect(&rows_rect);
            if hit_rect.h > 0.0 {
                hits.push((hit_rect, Hit::ExplorerRow(i)));
                let mut label = row.name.clone();
                if row.is_dir {
                    label += if row.expanded { ", folder, expanded" } else { ", folder, collapsed" };
                }
                if let Some(s) = git {
                    label = format!("{label}, {}", a11y::status_word(s));
                }
                rows_read.push((i, label, hit_rect, tree.selected == Some(i)));
            }
        }
        c.pop_clip();
        self.hits.extend(hits);
        self.a11y_list(a11y::EXPLORER_LIST, Some(a11y::SIDEBAR), "Files", rows_rect);
        for (i, label, rect, selected) in rows_read {
            self.a11y_item(a11y::EXPLORER_LIST, i, label, rect, selected);
        }
        if let Some(at) = field_at {
            self.draw_explorer_field(c, Rect::new(at.x, at.y, rows_rect.right() - at.x - 4.0, at.h), focused, caret_on);
        }
    }

    fn draw_editor_groups(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, self.color("editor.background"));
        let n = self.groups.len();
        let gw = (r.w / n as f32).floor();
        let active_group = self.active_group;
        for g in 0..n {
            let x = r.x + g as f32 * gw;
            let w = if g + 1 == n { r.right() - x } else { gw };
            let gr = Rect::new(x, r.y, w, r.h);
            if self.groups[g].tabs.is_empty() {
                self.draw_watermark(c, gr);
                self.hits.push((gr, Hit::EmptyGroup(g)));
            } else {
                self.draw_group(c, g, gr, g == active_group);
            }
            if g > 0 {
                c.fill(Rect::new(x, r.y, 1.0, r.h), self.color("editorGroup.border"));
            }
        }
        self.draw_debug_toolbar(c, r);
    }

    fn draw_watermark(&mut self, c: &mut Canvas, r: Rect) {
        let entries: &[(&str, Command)] = if self.tree.is_some() {
            &[
                ("Show All Commands", Command::CommandPalette),
                ("Go to File", Command::QuickOpen),
                ("Toggle Primary Side Bar", Command::ToggleSidebar),
                ("Toggle Panel", Command::TogglePanel),
                ("Split Editor", Command::SplitEditor),
            ]
        } else {
            &[
                ("Show All Commands", Command::CommandPalette),
                ("Open Folder", Command::OpenFolder),
                ("Open Recent", Command::OpenRecent),
                ("New Untitled Text File", Command::NewFile),
                ("Toggle Primary Side Bar", Command::ToggleSidebar),
            ]
        };
        let fg = self.color("descriptionForeground");
        let style = TextStyle::ui(UI, fg);
        let row_h = 26.0;
        let top = r.y + (r.h - entries.len() as f32 * row_h) / 2.0;
        let cx = r.x + r.w / 2.0;
        for (i, (label, cmd)) in entries.iter().enumerate() {
            let y = top + i as f32 * row_h;
            let lw = c.measure(label, &style);
            c.text_in(Rect::new(cx - 8.0 - lw, y, lw + 2.0, row_h), label, &style);
            if let Some(caps) = crate::keymap::keycaps(*cmd) {
                self.keycaps(c, cx + 8.0, y + 3.0, &caps, fg);
            }
        }
    }

    /// Draws a shortcut as separate keycaps (⇧ ⌘ P), returning the total width.
    fn keycaps(&self, c: &mut Canvas, x: f32, y: f32, keys: &[String], fg: Color) -> f32 {
        let style = TextStyle::ui(SMALL, fg);
        let mut cx = x;
        for k in keys {
            if k.is_empty() {
                cx += 4.0; // the gap between the strokes of a chord
                continue;
            }
            let w = (c.measure(&k, &style) + 10.0).max(20.0);
            let cap = Rect::new(cx, y, w, 20.0);
            c.bordered(cap, self.color("keybindingLabel.background"), self.color("keybindingLabel.border"), 1.0, 3.0);
            let tw = c.measure(&k, &style);
            c.text_in(Rect::new(cap.x + (w - tw) / 2.0, cap.y, tw + 1.0, cap.h), &k, &style);
            cx += w + 3.0;
        }
        cx - x
    }

    fn draw_group(&mut self, c: &mut Canvas, g: usize, r: Rect, active_group: bool) {
        let (tabs_rect, body) = r.cut_top(TAB_H);
        self.draw_tabs(c, g, tabs_rect, active_group);
        if self.groups[g].tabs.get(self.groups[g].active).is_some_and(|t| t.welcome) {
            c.push_clip(body);
            self.draw_welcome(c, body);
            c.pop_clip();
            return;
        }
        if self.groups[g].tabs.get(self.groups[g].active).is_some_and(|t| t.image.is_some()) {
            c.push_clip(body);
            self.draw_image(c, g, body);
            c.pop_clip();
            return;
        }
        if self.groups[g].tabs.get(self.groups[g].active).is_some_and(|t| t.markdown.is_some()) {
            c.push_clip(body);
            self.draw_markdown(c, g, body);
            c.pop_clip();
            return;
        }
        // A search editor has its query header where the breadcrumbs would be.
        let search_header = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.search.as_ref()).map(|s| s.header_height());
        // A merge editor has its inputs above the result, and the result's title bar.
        let merge = self.groups[g].tabs.get(self.groups[g].active).is_some_and(|t| t.merge.is_some());
        if merge {
            self.merge_refresh(g);
        }
        let editor_rect = match search_header {
            _ if merge => {
                let (inputs, rest) = body.cut_top((body.h * 0.5).floor());
                self.draw_merge_inputs(c, g, inputs);
                let (header, rest) = rest.cut_top(merge_view::HEADER_H);
                self.draw_merge_result_header(c, g, header);
                rest
            }
            Some(h) => {
                let (header, rest) = body.cut_top(h);
                self.draw_search_editor_header(c, g, header);
                rest
            }
            None => {
                let (bread, rest) = body.cut_top(BREADCRUMB_H);
                self.draw_breadcrumbs(c, g, bread);
                rest
            }
        };

        let focused = active_group && self.focus == Focus::Editor && self.palette.is_none();
        let caret_on = self.editor_caret_on();
        let minimap = config::get().minimap;
        let hover = Some(self.mouse);
        let dragging = matches!(self.drag, Some(Drag::Slider(dg, _)) if dg == g);
        self.refresh_find_matches(g);
        let git = match self.groups[g].tabs.get(self.groups[g].active).map(|t| t.doc) {
            Some(doc) => self.git_marks(doc),
            None => Vec::new(),
        };
        let conflicts = match self.groups[g].tabs.get(self.groups[g].active).map(|t| t.doc) {
            Some(doc) => self.doc_conflicts(doc),
            None => Vec::new(),
        };
        let lightbulb = self.groups[g].tabs.get(self.groups[g].active).and_then(|ed| self.lightbulb_for(g, ed.doc));
        let snippet = self.groups[g].tabs.get(self.groups[g].active).map(|ed| self.snippet_highlights(g, ed.doc)).unwrap_or_default();
        let linked = self.linked_highlights(g);
        let doc_path = self.groups[g].tabs.get(self.groups[g].active).and_then(|ed| self.docs[ed.doc].as_ref()).and_then(|d| d.buffer.path().map(Path::to_path_buf));
        let breakpoints = self.breakpoint_marks(doc_path.as_deref());
        let tests = self.test_marks(doc_path.as_deref());
        let search_results: Vec<(text::Pos, text::Pos)> = self.groups[g]
            .tabs
            .get(self.groups[g].active)
            .and_then(|t| Some(t.search.as_ref()?.highlights(self.docs[t.doc].as_ref()?.buffer.version()).to_vec()))
            .unwrap_or_default();
        let test_messages = self.test_messages(doc_path.as_deref());
        let merge_marks = if merge { self.merge_marks(g) } else { Vec::new() };
        let resolve_button = !merge && !conflicts.is_empty() && doc_path.as_deref().is_some_and(|p| self.is_conflicted(p));
        let stack_frame = self.stack_frame_mark(doc_path.as_deref());
        let zen = self.zen.is_some();
        let squiggles = {
            let gr = &self.groups[g];
            gr.tabs.get(gr.active).and_then(|ed| self.docs[ed.doc].as_ref()).map(|d| self.squiggles_for(d)).unwrap_or_default()
        };
        let ext_decos = self.groups[g].tabs.get(self.groups[g].active).map(|ed| self.ext_decorations_for(ed.doc)).unwrap_or_default();
        let theme = &self.theme;
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let Some(doc) = self.docs[ed.doc].as_mut() else { return };
        // Another view of the same document may have edited it; keep our selection valid.
        ed.clamp_selections(doc);
        if let Some(diff) = &mut ed.diff {
            c.push_clip(editor_rect);
            diff.draw(c, theme, doc, editor_rect);
            c.pop_clip();
            self.hits.push((editor_rect, Hit::Diff(g)));
            return;
        }
        c.push_clip(editor_rect);
        let deco = if gr.find.visible {
            crate::editor::Decorations { squiggles: &squiggles, matches: &gr.find.matches, current_match: gr.find.current, git: &git, conflicts: &conflicts, lightbulb, snippet: &snippet, breakpoints: &breakpoints, stack_frame, hide_line_numbers: zen, tests: &tests, test_messages: &test_messages, search_results: &search_results, merge: &merge_marks, linked: &linked, ext: &ext_decos }
        } else {
            crate::editor::Decorations { squiggles: &squiggles, git: &git, conflicts: &conflicts, lightbulb, snippet: &snippet, breakpoints: &breakpoints, stack_frame, hide_line_numbers: zen, tests: &tests, test_messages: &test_messages, search_results: &search_results, merge: &merge_marks, linked: &linked, ext: &ext_decos, ..Default::default() }
        };
        ed.draw(c, theme, doc, editor_rect, focused, caret_on, minimap, hover, dragging, &deco);
        c.pop_clip();
        let geom = ed.geom;
        let actions: Vec<Rect> = ed.conflict_actions.iter().map(|a| a.0).collect();
        let chevrons: Vec<(Rect, usize)> = ed.fold_controls.clone();
        let sticky: Vec<(Rect, usize)> = ed.sticky.clone();
        let lenses: Vec<(Rect, usize)> = ed.lens_hits.clone();
        let swatches: Vec<(Rect, text::Pos)> = ed.swatch_hits.clone();
        self.a11y_text_area(g, geom.text);
        self.hits.push((editor_rect, Hit::Editor(g)));
        self.hits.extend(actions.into_iter().enumerate().map(|(i, r)| (r, Hit::ConflictAction(g, i))));
        self.hits.push((geom.glyph, Hit::GlyphMargin(g)));
        self.hits.extend(chevrons.into_iter().map(|(r, line)| (r, Hit::FoldControl(g, line))));
        self.hits.extend(lenses.into_iter().map(|(r, i)| (r, Hit::CodeLens(g, i))));
        self.hits.extend(swatches.into_iter().map(|(r, pos)| (r, Hit::ColorSwatch(g, pos))));
        self.hits.extend(sticky.into_iter().map(|(r, line)| (r, Hit::StickyLine(g, line))));
        if let Some(r) = geom.lightbulb {
            self.hits.push((r, Hit::Lightbulb(g)));
        }
        if let Some(r) = geom.peek {
            self.draw_peek(c, g, r, editor_rect);
        }
        if minimap {
            self.hits.push((geom.minimap, Hit::Minimap(g)));
        }
        if geom.slider.h > 0.0 {
            self.hits.push((geom.scrollbar, Hit::Scrollbar(g)));
        }
        if merge {
            self.draw_merge_complete(c, g, editor_rect);
        } else if resolve_button {
            self.draw_resolve_in_merge_editor(c, g, editor_rect);
        }
        self.draw_find_widget(c, g, editor_rect);
        self.draw_rename(c, g, editor_rect);
        self.draw_color_picker(c, g, editor_rect);
    }

    fn draw_tabs(&mut self, c: &mut Canvas, g: usize, r: Rect, active_group: bool) {
        c.fill(r, self.color("editorGroupHeader.tabsBackground"));
        self.hits.push((r, Hit::TabBar(g)));
        // A Markdown file also gets Open Preview to the Side.
        let markdown = self.groups[g].tabs.get(self.groups[g].active).filter(|t| !t.is_special()).and_then(|t| self.docs[t.doc].as_ref()).is_some_and(|d| d.lang == language::Lang::Markdown);
        let actions_w = if active_group { if markdown { 90.0 } else { 64.0 } } else { 0.0 };
        let strip = Rect::new(r.x, r.y, r.w - actions_w, r.h);
        let style = TextStyle::ui(UI, Color::TRANSPARENT);

        // Measure tabs, then scroll the strip so the active tab is visible.
        self.pin_edited_previews();
        let tabs: Vec<(String, bool, PathBuf)> = self.groups[g]
            .tabs
            .iter()
            .map(|t| {
                let doc = self.docs[t.doc].as_ref().unwrap();
                let path = doc.buffer.path().map(Path::to_path_buf).unwrap_or_default();
                match &t.diff {
                    Some(d) => (d.label.clone(), false, d.spec.path.clone()),
                    None if t.markdown.as_ref().is_some_and(|m| m.extension.is_some()) => (doc.title(), false, PathBuf::new()),
                    None if t.markdown.is_some() => (format!("Preview {}", doc.title()), false, path),
                    None if t.merge.is_some() => (format!("Merging: {}", doc.title()), doc.buffer.is_dirty(), path),
                    None if t.welcome => ("Welcome".to_string(), false, PathBuf::new()),
                    None => (doc.title(), doc.buffer.is_dirty(), path),
                }
            })
            .collect();
        let previews: Vec<bool> = self.groups[g].tabs.iter().map(|t| t.preview).collect();
        let widths: Vec<f32> = tabs
            .iter()
            .zip(&previews)
            .map(|((t, _, _), p)| (10.0 + 16.0 + 6.0 + c.measure(t, &style.italic(*p)) + 4.0 + 28.0).max(80.0))
            .collect();
        let active = self.groups[g].active;
        let active_right: f32 = widths.iter().take(active + 1).sum();
        let offset = (active_right - strip.w).max(0.0);

        c.push_clip(strip);
        let mut x = strip.x - offset;
        for (i, ((title, dirty, path), w)) in tabs.iter().zip(&widths).enumerate() {
            let tr = Rect::new(x, r.y, *w, r.h);
            let is_active = i == active;
            let hovered = self.hovered(Hit::Tab(g, i)) || self.hovered(Hit::TabClose(g, i));
            // Each tab is a rounded chip; the active one is filled and outlined.
            let chip = Rect::new(tr.x + 3.0, tr.y + 5.0, tr.w - 6.0, tr.h - 10.0);
            if is_active {
                let border = if active_group { self.color("tab.activeBorderTop").with_alpha(0.55) } else { self.color("tab.border") };
                c.bordered(chip, self.color("tab.activeBackground"), border, 1.0, 7.0);
            } else if hovered {
                c.fill_rounded(chip, self.color_or("tab.hoverBackground", "tab.inactiveBackground"), 7.0);
            }
            let fg = self.color(if is_active { "tab.activeForeground" } else { "tab.inactiveForeground" });
            let title_fg = match self.repo.as_ref().and_then(|r| r.file_status(path)) {
                Some(s) => scm_view::status_color(&self.theme, s).with_alpha(if is_active { 1.0 } else { 0.8 }),
                None => fg,
            };
            if self.groups[g].tabs[i].welcome {
                c.icon(&icons::INFO, tr.x + 11.0, tr.y + 10.5, 14.0, self.color("icon.foreground"));
            } else {
                c.icon(&icons::FILE, tr.x + 11.0, tr.y + 10.5, 14.0, file_color(path));
            }
            let preview = self.groups[g].tabs[i].preview;
            c.text_in(Rect::new(tr.x + 32.0, tr.y, tr.w - 60.0, tr.h), title, &style.color(title_fg).italic(preview));
            let close = Rect::new(tr.right() - 27.0, tr.y + 8.0, 19.0, 19.0);
            let close_hovered = self.hovered(Hit::TabClose(g, i));
            if close_hovered {
                c.fill_rounded(close, self.color("toolbar.hoverBackground"), 5.0);
            }
            if *dirty && !close_hovered {
                c.icon_in(&icons::DOT, close, 16.0, fg);
            } else if is_active || hovered {
                c.icon_in(&icons::CLOSE, close, 16.0, fg);
            }
            self.hits.push((tr, Hit::Tab(g, i)));
            let read = if *dirty { format!("{title}, edited") } else { title.to_string() };
            self.a11y_name(Hit::Tab(g, i), a11y::Role::Tab, read, is_active);
            self.hits.push((close, Hit::TabClose(g, i)));
            x += w;
        }
        c.pop_clip();

        if active_group {
            let fg = self.color("icon.foreground");
            let split = Rect::new(r.right() - 58.0, r.y + 7.0, 24.0, 22.0);
            self.icon_button(c, split, &icons::SPLIT, Hit::SplitButton(g), fg);
            if markdown {
                let preview = Rect::new(split.x - 26.0, r.y + 7.0, 24.0, 22.0);
                self.icon_button(c, preview, &icons::OPEN_PREVIEW, Hit::PreviewButton(g), fg);
            }
            c.icon_in(&icons::ELLIPSIS, Rect::new(r.right() - 32.0, r.y + 7.0, 24.0, 22.0), 16.0, fg);
        }
    }

    fn draw_breadcrumbs(&mut self, c: &mut Canvas, g: usize, r: Rect) {
        c.fill(r, self.color("breadcrumb.background"));
        let gr = &self.groups[g];
        let Some(doc) = gr.tabs.get(gr.active).and_then(|t| self.docs[t.doc].as_ref()) else { return };
        let fg = self.color("breadcrumb.foreground");
        let style = TextStyle::ui(12.0, fg);
        let parts: Vec<String> = match doc.buffer.path() {
            Some(p) => self.display_path(p).split('/').filter(|s| !s.is_empty()).map(String::from).collect(),
            None => vec![doc.title()],
        };
        // Then the symbols at the cursor (`breadcrumbs.symbolPath`).
        let crumbs = match (gr.tabs.get(gr.active), self.settings.string("breadcrumbs.symbolPath").as_str()) {
            (_, "off") | (None, _) => Vec::new(),
            (Some(ed), mode) => {
                let mut path = self.symbol_path(ed.doc, ed.sel.head);
                if mode == "last" && path.len() > 1 {
                    path.drain(..path.len() - 1);
                }
                path
            }
        };
        let skipped = self.symbol_path_len(g).saturating_sub(crumbs.len());
        c.push_clip(r);
        let mut x = r.x + 18.0;
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            if last {
                c.icon(&icons::FILE, x, r.y + 3.0, 16.0, file_color(doc.buffer.path().unwrap_or(Path::new(""))));
                x += 20.0;
            }
            x += c.text_in(Rect::new(x, r.y, 400.0, r.h), part, &style);
            if !last || !crumbs.is_empty() {
                c.icon(&icons::CHEVRON_RIGHT, x + 2.0, r.y + 3.0, 16.0, fg);
                x += 20.0;
            }
        }
        let mut hits = Vec::new();
        for (i, crumb) in crumbs.iter().enumerate() {
            let (icon, color) = outline::symbol_icon(crumb.kind);
            let start = x;
            c.icon(icon, x, r.y + 3.0, 16.0, self.theme.color(color));
            x += 20.0;
            let hovered = self.hover_hit == Some(Hit::BreadcrumbSymbol(g, skipped + i));
            let st = if hovered { style.color(self.color("breadcrumb.focusForeground")) } else { style };
            x += c.text_in(Rect::new(x, r.y, 400.0, r.h), &crumb.name, &st);
            hits.push((Rect::new(start, r.y, x - start, r.h), Hit::BreadcrumbSymbol(g, skipped + i)));
            if i + 1 < crumbs.len() {
                c.icon(&icons::CHEVRON_RIGHT, x + 2.0, r.y + 3.0, 16.0, fg);
                x += 20.0;
            }
        }
        c.pop_clip();
        self.hits.extend(hits);
    }

    /// How many symbols contain group `g`'s cursor.
    fn symbol_path_len(&self, g: usize) -> usize {
        let gr = &self.groups[g];
        gr.tabs.get(gr.active).map_or(0, |ed| self.symbol_path(ed.doc, ed.sel.head).len())
    }

    /// A click on a breadcrumbs symbol: a menu of the symbols next to it.
    fn breadcrumb_menu(&mut self, g: usize, i: usize) {
        self.active_group = g;
        self.focus = Focus::Editor;
        let gr = &self.groups[g];
        let Some(ed) = gr.tabs.get(gr.active) else { return };
        let path = self.symbol_path(ed.doc, ed.sel.head);
        let Some(crumb) = path.get(i) else { return };
        let Some(r) = self.hits.iter().find(|(_, h)| *h == Hit::BreadcrumbSymbol(g, i)).map(|(r, _)| *r) else { return };
        let entries = crumb
            .siblings
            .iter()
            .enumerate()
            .map(|(j, (name, _, at))| {
                (PopupItem::Item { label: name.clone(), enabled: true, checked: Some(j == crumb.index) }, preferences::PopupAction::GotoHere(*at))
            })
            .collect();
        self.show_popup(entries, r.x, r.bottom());
    }

    /// The panel's tabs that show: Problems, Output and Terminal always; Debug Console, Ports
    /// and Test Results when they have something (or are open). Indexes into `PANEL_TABS`.
    fn panel_tabs(&self) -> Vec<usize> {
        (0..PANEL_TABS.len() + 1)
            .filter(|&i| match i {
                PANEL_DEBUG_CONSOLE => self.debug.session.is_some() || !self.debug.console.is_empty(),
                PANEL_PORTS => false,
                PANEL_TEST_RESULTS => self.testing.is_running() || !self.testing.output.is_empty(),
                _ => true,
            } || i == self.panel_tab)
            .collect()
    }

    fn draw_panel(&mut self, c: &mut Canvas, r: Rect) {
        // A rounded card inset in the editor area.
        c.fill(r, self.color("editor.background"));
        let card = Rect::new(r.x + 8.0, r.y + 2.0, (r.w - 16.0).max(0.0), (r.h - 10.0).max(0.0));
        c.bordered(card, self.color("panel.background"), self.color("panel.border"), 1.0, controls::CARD_RADIUS);
        self.hits.push((r, Hit::PanelBody));
        let (header, rest) = card.cut_top(PANEL_HEADER_H);
        let body = Rect::new(rest.x + 4.0, rest.y, (rest.w - 8.0).max(0.0), (rest.h - 6.0).max(0.0));
        let mut x = header.x + 6.0;
        for i in self.panel_tabs() {
            let active = i == self.panel_tab;
            let hovered = self.hovered(Hit::PanelTab(i));
            let color = self.color(if active || hovered { "panelTitle.activeForeground" } else { "panelTitle.inactiveForeground" });
            let style = TextStyle::ui(12.0, color).weight(if active { 600 } else { 400 });
            let label = calm(PANEL_TABS.get(i).copied().unwrap_or("TEST RESULTS"));
            let icon = match i {
                0 => &icons::WARNING,
                1 => &icons::LIST_SELECTION,
                PANEL_DEBUG_CONSOLE => &icons::RUN_DEBUG,
                PANEL_TERMINAL => &icons::TERMINAL,
                PANEL_PORTS => &icons::REMOTE,
                _ => &icons::BEAKER,
            };
            let count = match i {
                0 => self.lsp.diagnostics.values().map(|(_, d)| d.len()).sum(),
                PANEL_TERMINAL if self.terms.count() > 1 => self.terms.count(),
                _ => 0,
            };
            let w = c.measure(&label, &style);
            let badge_w = if count > 0 { c.measure(&count.to_string(), &TextStyle::ui(SMALL, color)) + 16.0 } else { 0.0 };
            let chip = Rect::new(x, header.y + 6.0, 10.0 + 14.0 + 6.0 + w + badge_w + 10.0, header.h - 12.0);
            // Chips like the editor's tabs: the active one filled and outlined.
            if active {
                c.bordered(chip, self.color("tab.activeBackground"), self.color("tab.activeBorderTop").with_alpha(0.55), 1.0, 7.0);
            } else if hovered {
                c.fill_rounded(chip, self.color("toolbar.hoverBackground"), 7.0);
            }
            c.icon(icon, chip.x + 10.0, chip.y + (chip.h - 14.0) / 2.0, 14.0, color);
            c.text_in(Rect::new(chip.x + 30.0, chip.y, w + 2.0, chip.h), &label, &style);
            if count > 0 {
                self.badge(c, chip.x + 36.0 + w, chip.y + (chip.h - 16.0) / 2.0, count);
            }
            self.hits.push((chip, Hit::PanelTab(i)));
            let read = if count > 0 { format!("{label}, {count}") } else { label.clone() };
            self.a11y_name(Hit::PanelTab(i), a11y::Role::Tab, read, active);
            x = chip.right() + 4.0;
        }
        let fg = self.color("icon.foreground");
        let max_icon = if self.panel_maximized { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_UP };
        self.icon_button(c, Rect::new(header.right() - 58.0, header.y + 7.0, 24.0, 22.0), max_icon, Hit::PanelMaximize, fg);
        self.icon_button(c, Rect::new(header.right() - 30.0, header.y + 7.0, 24.0, 22.0), &icons::CLOSE, Hit::PanelClose, fg);

        if self.panel_tab == 0 {
            self.draw_problems_filter(c, header, header.right() - 68.0);
        }
        if self.panel_tab == 1 {
            self.draw_output_channel_picker(c, header, header.right() - 68.0);
        }
        if self.panel_tab == PANEL_TERMINAL {
            let right = header.right() - 68.0;
            self.icon_button(c, Rect::new(right - 28.0, header.y + 7.0, 24.0, 22.0), &icons::SPLIT, Hit::SplitTerminal, fg);
            self.icon_button(c, Rect::new(right - 56.0, header.y + 7.0, 24.0, 22.0), &icons::ADD, Hit::NewTerminal, fg);
            // A divider, then the terminals.
            c.fill(Rect::new(x + 4.0, header.y + 10.0, 1.0, header.h - 20.0), self.color("panel.border"));
            self.draw_terminal_chips(c, Rect::new(x + 12.0, header.y, (right - 60.0 - x - 12.0).max(0.0), header.h));
        }
        match self.panel_tab {
            0 => return self.draw_problems(c, body),
            1 => return self.draw_output(c, body),
            PANEL_DEBUG_CONSOLE => return self.draw_debug_console(c, body),
            PANEL_TERMINAL => return self.draw_terminal(c, body),
            PANEL_TEST_RESULTS => return self.draw_test_results(c, body),
            _ => {}
        }
        self.empty_state(c, body, &icons::REMOTE, "No forwarded ports", "Ports a task or debug session forwards show here.", None);
    }

    /// A status bar pill at `x`: an optional icon (spinning by `turn`) and text, outlined and
    /// rounded. Returns its width.
    #[allow(clippy::too_many_arguments)]
    fn status_pill(&mut self, c: &mut Canvas, x: f32, r: Rect, icon: Option<(&Icon, Color, u32)>, text: &str, style: &TextStyle, hit: Hit) -> f32 {
        let tw = if text.is_empty() { 0.0 } else { c.measure(text, style) };
        let iw = if icon.is_some() { 14.0 + if text.is_empty() { 0.0 } else { 5.0 } } else { 0.0 };
        let w = 9.0 + iw + tw + 9.0;
        let pill = Rect::new(x, r.y + 4.0, w, r.h - 8.0);
        let bg = if self.hovered(hit) { self.color("statusBarItem.hoverBackground") } else { Color::TRANSPARENT };
        c.bordered(pill, bg, self.color("widget.border"), 1.0, pill.h / 2.0);
        let mut tx = pill.x + 9.0;
        if let Some((icon, color, turn)) = icon {
            c.icon_turned(icon, tx, pill.y + (pill.h - 14.0) / 2.0, 14.0, color, turn);
            tx += iw;
        }
        if !text.is_empty() {
            c.text_in(Rect::new(tx, pill.y, tw + 2.0, pill.h), text, style);
        }
        self.hits.push((pill, hit));
        if !text.is_empty() {
            self.a11y_name(hit, a11y::Role::Button, text, false);
        }
        w
    }

    /// The name shown for language server `command` ("rust-analyzer", "JSON" for built-in ones).
    fn server_label(command: &str) -> String {
        match command.strip_prefix("builtin:") {
            Some(name) => format!("{} server", name.to_uppercase()),
            None => Path::new(command).file_name().map_or_else(|| command.to_string(), |n| n.to_string_lossy().into_owned()),
        }
    }

    fn draw_status_bar(&mut self, c: &mut Canvas, r: Rect) {
        // Orange while debugging.
        let debugging = self.debugging();
        let bg = if debugging {
            "statusBar.debuggingBackground"
        } else if self.tree.is_some() {
            "statusBar.background"
        } else {
            "statusBar.noFolderBackground"
        };
        c.fill(r, self.color(bg));
        c.fill(Rect::new(r.x, r.y, r.w, 1.0), self.color(if debugging { "statusBar.debuggingBorder" } else { "statusBar.border" }));
        let fg = self.color(if debugging { "statusBar.debuggingForeground" } else { "statusBar.foreground" });
        let style = TextStyle::ui(12.0, fg);
        let hover_bg = self.color("statusBarItem.hoverBackground");
        let gap = 6.0;

        // Left: pills for sync (or publish), the language server, problems and the terminal.
        let mut x = r.x + 8.0;
        let sync = self.repo.as_ref().map(|r| &r.status).filter(|s| s.branch.is_some()).map(|s| (s.upstream.is_some(), s.behind, s.ahead));
        if let Some((tracked, behind, ahead)) = sync {
            let label = if tracked && (behind > 0 || ahead > 0) { format!("{behind}↓ {ahead}↑") } else { String::new() };
            let turn = if self.git_spinning() { self.git_spin_turn() } else { 0 };
            let icon = if tracked { &icons::SYNC } else { &icons::CLOUD_UPLOAD };
            x += self.status_pill(c, x, r, Some((icon, fg, turn)), &label, &style, Hit::StatusSync) + gap;
        }
        // The active file's server: a check when it's ready, a spinner with its progress while busy.
        let path = self.active_doc().and_then(|d| d.buffer.path().map(Path::to_path_buf));
        let server = path.as_deref().and_then(|p| self.lsp.key_for(p));
        // A server stopped by hand keeps its item, to start it from there.
        let held = self.active_server().map(|(k, _)| k).filter(|k| server.is_none() && self.lsp.is_held(k));
        let progress = self.lsp.progress_text();
        if let Some(key) = &held {
            let text = format!("{}: stopped", Self::server_label(key.0));
            x += self.status_pill(c, x, r, Some((&icons::DEBUG_STOP, fg, 0)), &text, &style, Hit::StatusServer) + gap;
        }
        if let Some(key) = &server {
            let name = Self::server_label(key.0);
            let ready = self.lsp.ready(key) == Some(true);
            let (icon, color, turn, text) = match (&progress, ready) {
                // Progress titles often name the server already.
                (Some(p), _) if p.starts_with(name.as_str()) => (&icons::SYNC, fg, self.git_spin_turn(), p.clone()),
                (Some(p), _) => (&icons::SYNC, fg, self.git_spin_turn(), format!("{name}: {p}")),
                (None, true) => (&icons::PASS, self.color("testing.iconPassed"), 0, name),
                (None, false) => (&icons::SYNC, fg, self.git_spin_turn(), format!("{name}: starting")),
            };
            let room = (r.w * 0.4).max(120.0);
            let text = if c.measure(&text, &style) > room { name_only(&text) } else { text };
            x += self.status_pill(c, x, r, Some((icon, color, turn)), &text, &style, Hit::StatusServer) + gap;
        }
        let (errors, warnings) = self.lsp.counts();
        if errors == 0 && warnings == 0 {
            x += self.status_pill(c, x, r, None, "0 problems", &style, Hit::StatusProblems) + gap;
        } else {
            // "⊗ 2  ⚠ 1": icons with counts.
            let (e, w) = (errors.to_string(), warnings.to_string());
            let (ew, ww) = (c.measure(&e, &style), c.measure(&w, &style));
            let width = 9.0 + 14.0 + 4.0 + ew + 10.0 + 14.0 + 4.0 + ww + 9.0;
            let pill = Rect::new(x, r.y + 4.0, width, r.h - 8.0);
            let bg = if self.hovered(Hit::StatusProblems) { hover_bg } else { Color::TRANSPARENT };
            c.bordered(pill, bg, self.color("widget.border"), 1.0, pill.h / 2.0);
            let iy = pill.y + (pill.h - 14.0) / 2.0;
            let mut px = pill.x + 9.0;
            c.icon(&icons::ERROR, px, iy, 14.0, if errors > 0 { self.color("editorError.foreground") } else { fg });
            px += 18.0;
            c.text_in(Rect::new(px, pill.y, ew + 2.0, pill.h), &e, &style);
            px += ew + 10.0;
            c.icon(&icons::WARNING, px, iy, 14.0, if warnings > 0 { self.color("editorWarning.foreground") } else { fg });
            px += 18.0;
            c.text_in(Rect::new(px, pill.y, ww + 2.0, pill.h), &w, &style);
            self.hits.push((pill, Hit::StatusProblems));
            let plural = |n: usize, one: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") };
            self.a11y_name(Hit::StatusProblems, a11y::Role::Button, format!("{}, {}", plural(errors, "error"), plural(warnings, "warning")), false);
            x += width + gap;
        }
        x += self.status_pill(c, x, r, Some((&icons::TERMINAL, fg, 0)), "Terminal", &style, Hit::StatusTerminal) + gap;
        let mut left: Vec<usize> = (0..self.ext_host.status.len()).filter(|&i| !self.ext_host.status[i].right).collect();
        left.sort_by_key(|&i| std::cmp::Reverse(self.ext_host.status[i].priority));
        for i in left {
            x += self.draw_ext_status_item(c, i, x, r, &style, false);
        }
        // The active file's server shows its own progress in its pill.
        let progress_in_pill = server.is_some();
        if let Some(msg) = self.status_text().map(str::to_string).or_else(|| self.git_progress_text()) {
            let pr = Rect::new(x + 4.0, r.y, 600.0, r.h);
            c.text_in(pr, &msg, &style);
        } else if let Some(progress) = progress.filter(|_| !progress_in_pill) {
            // Work by a server other than the active file's.
            let pr = Rect::new(x + 4.0, r.y, 600.0, r.h);
            c.push_clip(Rect::new(pr.x, r.y, (r.w * 0.45).min(600.0), r.h));
            c.text_in(pr, &progress, &style);
            c.pop_clip();
        }

        // Right side: editor info.
        let mut items: Vec<String> = Vec::new();
        let mut language_item = None;
        if let Some(pv) = self.active_editor().and_then(|e| e.image.as_ref()) {
            items.extend(pv.status());
        } else if let (Some(ed), Some(doc)) = (self.active_editor().filter(|e| !e.welcome && !e.markdown.as_ref().is_some_and(|m| m.extension.is_some())), self.active_doc()) {
            let (line, col) = ed.line_col();
            let sels = ed.selections();
            let selected: usize = sels.iter().map(|s| doc.buffer.text_in(s).chars().count()).sum();
            if sels.len() > 1 {
                // "3 selections (12 characters selected)".
                let chars = if selected > 0 { format!(" ({selected} characters selected)") } else { String::new() };
                items.push(format!("{} selections{chars}", sels.len()));
            } else if selected > 0 {
                items.push(format!("Ln {line}, Col {col} ({selected} selected)"));
            } else {
                items.push(format!("Ln {line}, Col {col}"));
            }
            let cfg = config::get();
            items.push(if cfg.insert_spaces { format!("Spaces: {}", cfg.tab_size) } else { format!("Tab Size: {}", cfg.tab_size) });
            items.push("UTF-8".into());
            items.push("LF".into());
            language_item = Some(items.len());
            items.push(doc.lang.name().into());
        }
        // Pills from the right edge: the bell, then the editor's items (the language last).
        let pill_w = |c: &mut Canvas, text: &str| 9.0 + c.measure(text, &style) + 9.0;
        let mut x = r.right() - 8.0 - 34.0;
        let bell = Rect::new(x, r.y + 4.0, 34.0, r.h - 8.0);
        c.bordered(bell, Color::TRANSPARENT, self.color("widget.border"), 1.0, bell.h / 2.0);
        c.icon_in(&icons::BELL, bell, 14.0, fg);
        for (i, item) in items.iter().enumerate().rev() {
            let w = pill_w(c, item);
            x -= w + gap;
            let hit = if language_item == Some(i) { Hit::StatusLanguage } else { Hit::StatusItem(i + 1) };
            self.status_pill(c, x, r, None, item, &style, hit);
        }
        x -= gap;
        // Extensions' right-aligned items, left of the editor's (higher priority further left).
        let mut right: Vec<usize> = (0..self.ext_host.status.len()).filter(|&i| self.ext_host.status[i].right).collect();
        right.sort_by_key(|&i| self.ext_host.status[i].priority);
        for i in right {
            x -= self.draw_ext_status_item(c, i, x, r, &style, true);
        }
    }

    /// An extension's status bar item at `x` (its right edge when `from_right`); returns its width.
    /// `$(name)` in the text is drawn as that icon.
    fn draw_ext_status_item(&mut self, c: &mut Canvas, i: usize, x: f32, r: Rect, style: &TextStyle, from_right: bool) -> f32 {
        let item = self.ext_host.status[i].clone();
        let mut parts: Vec<Result<&'static Icon, String>> = Vec::new();
        let mut rest = item.text.as_str();
        while let Some(start) = rest.find("$(") {
            let Some(end) = rest[start..].find(')') else { break };
            if start > 0 {
                parts.push(Err(rest[..start].to_string()));
            }
            let name = rest[start + 2..start + end].split('~').next().unwrap_or("");
            match icons::named(name) {
                Some(icon) => parts.push(Ok(icon)),
                None => {}
            }
            rest = &rest[start + end + 1..];
        }
        if !rest.is_empty() {
            parts.push(Err(rest.to_string()));
        }
        let w = 10.0 + parts.iter().map(|p| match p {
            Ok(_) => 16.0,
            Err(t) => c.measure(t, style),
        }).sum::<f32>();
        if w <= 10.0 {
            return 0.0;
        }
        let ir = Rect::new(if from_right { x - w } else { x }, r.y, w, r.h);
        let hit = Hit::ExtStatus(i);
        if item.command.is_some() && self.hovered(hit) {
            c.fill_rounded(Rect::new(ir.x, r.y + 4.0, ir.w, r.h - 8.0), self.color("statusBarItem.hoverBackground"), (r.h - 8.0) / 2.0);
        }
        let mut px = ir.x + 5.0;
        for p in &parts {
            match p {
                Ok(icon) => {
                    c.icon(icon, px, r.y + (r.h - 14.0) / 2.0, 14.0, style.color);
                    px += 16.0;
                }
                Err(t) => px += c.text_in(Rect::new(px, r.y, w, r.h), t, style),
            }
        }
        self.hits.push((ir, hit));
        w
    }

    fn draw_palette(&mut self, c: &mut Canvas, full: Rect) {
        c.push_layer();
        self.hits.push((full, Hit::PaletteBackdrop));
        let Some(p) = self.palette.as_ref() else { return };
        // A faint scrim, so the card reads as in front.
        c.fill(full, self.color("widget.shadow").with_alpha(0.18));
        // Centered, in the upper part of the window.
        let w = (full.w * 0.56).clamp(360.0, 680.0);
        let visible: Vec<usize> = (p.scroll..(p.scroll + MAX_VISIBLE).min(p.items.len())).collect();
        let headers = visible.iter().filter(|&&i| p.starts_group(i)).count();
        let list_h = match &p.input_box {
            // An input box shows its prompt (one row per line) or its validation error.
            Some(b) if b.error.is_none() => b.prompt.lines().count().max(1) as f32 * PALETTE_ROW,
            Some(_) => PALETTE_ROW,
            None if p.items.is_empty() => PALETTE_ROW + 8.0,
            None => visible.len() as f32 * PALETTE_ROW + headers as f32 * PALETTE_HEADER,
        };
        let h = PALETTE_INPUT + 1.0 + 6.0 + list_h + 8.0;
        let top = full.y + (full.h * 0.16).clamp(44.0, 160.0);
        let bx = Rect::new((full.x + (full.w - w) / 2.0).round(), top.round(), w, h);
        c.shadow(bx, 14.0, self.color("widget.shadow"));
        c.bordered(bx, self.color("quickInput.background"), self.color("widget.border"), 1.0, 14.0);
        self.hits.push((bx, Hit::PaletteBox));

        // The input: large, with a search icon, and a line under it.
        let error = p.input_box.as_ref().and_then(|b| b.error.clone());
        let input = Rect::new(bx.x, bx.y, bx.w, PALETTE_INPUT);
        let dim = self.color("descriptionForeground");
        c.icon(&icons::SEARCH, input.x + 18.0, input.y + (input.h - 18.0) / 2.0, 18.0, dim);
        let big = TextStyle::ui(17.0, self.color("input.foreground"));
        let text_rect = Rect::new(input.x + 46.0, input.y, input.w - 62.0, input.h);
        c.push_clip(input);
        let tw = if p.input.is_empty() {
            c.text_fit(text_rect, p.placeholder(), &big.color(self.color("input.placeholderForeground")))
                .min(0.0)
        } else if p.input_box.as_ref().is_some_and(|b| b.password) {
            let dots = "•".repeat(p.input.chars().count());
            c.text_in(text_rect, &dots, &big)
        } else {
            c.text_in(text_rect, &p.input, &big)
        };
        if self.caret_on() {
            c.fill(Rect::new(text_rect.x + tw + 1.0, input.y + (input.h - 22.0) / 2.0, 1.5, 22.0), self.color("focusBorder"));
        }
        let text_y = input.y + ((input.h - big.line_height) / 2.0).round();
        crate::ime::caret(Rect::new(text_rect.x + tw + 1.0, text_y, 1.0, big.line_height), &big, self.color("quickInput.background"), "", text_rect);
        c.pop_clip();
        let line = if error.is_some() { self.color("inputValidation.errorBorder") } else { self.color("widget.border") };
        c.fill(Rect::new(bx.x + 1.0, input.bottom(), bx.w - 2.0, 1.0), line);

        // Results.
        let style = TextStyle::ui(UI, self.color("quickInput.foreground"));
        let list_y = input.bottom() + 1.0 + 6.0;
        let highlight = self.color("list.highlightForeground");
        let fg = self.color("quickInput.foreground");
        let inner = Rect::new(bx.x + 8.0, list_y, bx.w - 16.0, list_h);
        if let Some(b) = &p.input_box {
            // An input box: the prompt, or the validation message.
            match &error {
                Some(e) => {
                    let fg = self.color_or("inputValidation.errorForeground", "foreground");
                    let r = Rect::new(inner.x, inner.y, inner.w, PALETTE_ROW);
                    c.bordered(r, self.color("inputValidation.errorBackground"), self.color("inputValidation.errorBorder"), 1.0, 8.0);
                    c.text_in(Rect::new(r.x + 10.0, r.y, r.w - 20.0, r.h), e, &style.color(fg));
                }
                None => {
                    let st = style.color(dim);
                    for (i, line) in b.prompt.lines().enumerate() {
                        c.text_in(Rect::new(inner.x + 10.0, inner.y + i as f32 * PALETTE_ROW, inner.w - 20.0, PALETTE_ROW), line, &st);
                    }
                }
            }
        } else if p.items.is_empty() {
            let msg = match &p.message {
                Some(m) => m.as_str(),
                None if p.is_commands() => "No matching commands",
                None => "No matching results",
            };
            c.text_in(Rect::new(inner.x + 10.0, inner.y, inner.w - 20.0, PALETTE_ROW), msg, &style.color(dim));
        }
        let commands = p.is_commands();
        let mut hits = Vec::new();
        let mut rows_read = Vec::new();
        let list_label = p.placeholder().to_string();
        let mut y = inner.y;
        for idx in visible {
            let item = &p.items[idx];
            if p.starts_group(idx) {
                let group = item.group.as_deref().unwrap_or_default();
                let gs = TextStyle::ui(SMALL, self.color("pickerGroup.foreground")).weight(600);
                c.text_in(Rect::new(inner.x + 10.0, y + 4.0, inner.w - 20.0, PALETTE_HEADER - 4.0), &calm(group), &gs);
                y += PALETTE_HEADER;
            }
            let rr = Rect::new(inner.x, y, inner.w, PALETTE_ROW);
            y += PALETTE_ROW;
            if idx == p.selected {
                c.fill_rounded(rr, self.color("quickInputList.focusBackground"), 8.0);
            } else if self.hover_hit == Some(Hit::PaletteRow(idx)) {
                c.fill_rounded(rr, self.color("list.hoverBackground"), 8.0);
            }
            c.push_clip(rr);
            let mut x = rr.x + 10.0;
            let icon_y = rr.y + (rr.h - 16.0) / 2.0;
            if p.is_files() {
                if let Action::Open(path) = &item.action {
                    c.icon(&icons::FILE, x, icon_y, 16.0, file_color(path));
                }
                x += 24.0;
            } else if let Some(kind) = item.kind {
                let (icon, color) = outline::symbol_icon(kind);
                c.icon(icon, x, icon_y, 16.0, self.theme.color(color));
                x += 24.0;
            }
            // Matched characters highlighted; a command's category ("File: ") dimmed.
            let prefix = if commands { item.label.find(": ").map_or(0, |i| i + 2) } else { 0 };
            let spans: Vec<(usize, usize, Color)> = item
                .label
                .char_indices()
                .enumerate()
                .map(|(ci, (bi, ch))| {
                    let color = if item.matches.contains(&ci) { highlight } else if bi < prefix { dim } else { fg };
                    (bi, bi + ch.len_utf8(), color)
                })
                .collect();
            let label_style = style.color(fg);
            let ty = rr.y + ((PALETTE_ROW - label_style.line_height) / 2.0).round();
            let lw = c.rich_text(x, ty, &item.label, &spans, &label_style);
            if !item.detail.is_empty() {
                let ds = TextStyle::ui(12.0, dim);
                c.text_in(Rect::new(x + lw + 8.0, rr.y, rr.w, rr.h), &item.detail, &ds);
            }
            if let Some(sc) = &item.shortcut {
                let caps_w: f32 = sc
                    .iter()
                    .map(|k| if k.is_empty() { 4.0 } else { (c.measure(k, &TextStyle::ui(SMALL, fg)) + 10.0).max(20.0) + 3.0 })
                    .sum();
                self.keycaps(c, rr.right() - caps_w - 8.0, rr.y + (rr.h - 20.0) / 2.0, sc, fg);
            }
            c.pop_clip();
            hits.push((rr, Hit::PaletteRow(idx)));
            let mut label = item.label.clone();
            if !item.detail.is_empty() {
                label = format!("{label}, {}", item.detail);
            }
            if let Some(sc) = &item.shortcut {
                label = format!("{label}, {}", sc.iter().map(|k| if k.is_empty() { " " } else { k }).collect::<String>());
            }
            rows_read.push((idx, label, rr, idx == p.selected));
        }
        self.hits.extend(hits);
        self.a11y_list(a11y::PALETTE_LIST, None, &list_label, inner);
        for (idx, label, rect, selected) in rows_read {
            self.a11y_item(a11y::PALETTE_LIST, idx, label, rect, selected);
        }
    }
}

/// File icon tint by extension.
fn file_color(path: &Path) -> Color {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let hex = match ext {
        "rs" => "#DEA584",
        "toml" | "lock" => "#9C9C9C",
        "md" => "#519ABA",
        "json" | "jsonc" => "#CBCB41",
        "py" => "#519ABA",
        "go" => "#519ABA",
        "js" | "mjs" | "cjs" => "#CBCB41",
        "ts" | "tsx" => "#519ABA",
        "c" | "h" | "cpp" | "hpp" | "cc" => "#599EFF",
        "sh" | "zsh" | "bash" => "#4D5A5E",
        "html" => "#E37933",
        "css" => "#519ABA",
        "yml" | "yaml" => "#A074C4",
        "wgsl" => "#8DC149",
        _ if name.starts_with('.') => "#6D8086",
        _ => "#9C9C9C",
    };
    Color::hex(hex).unwrap()
}

fn read_git_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git/HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_string(),
        None => head.chars().take(7).collect(),
    })
}
