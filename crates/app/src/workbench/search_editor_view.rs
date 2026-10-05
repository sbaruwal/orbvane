//! The Search Editor: search results in an editor tab. A header holds the
//! query with its Match Case / Whole Word / Regex toggles, context lines, and the
//! include/exclude fields; below it the results are an ordinary, editable document (language
//! "Search Result", `crate::search_editor` has its format). Double-clicking a result opens
//! it; ⌘S saves the search as a `.code-search` file, which opens as a search editor again.

use std::path::Path;
use std::time::{Duration, Instant};

use render::{Canvas, Rect, TextStyle};
use search::{FileMatches, Search};
use text::{Pos, Selection};

use super::{Focus, Hit, Workbench, SMALL, UI};
use crate::editor::{Doc, EditorState};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::search_editor::{self as se, Config};
use crate::widgets::{FieldEvent, TextField};

const DEBOUNCE: Duration = Duration::from_millis(300);
const INPUT_H: f32 = 26.0;
const TOGGLE: f32 = 20.0;
const LABEL_H: f32 = 20.0;

/// The header's text fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeField {
    Query,
    Context,
    Include,
    Exclude,
}

/// The header's toggles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeToggle {
    MatchCase,
    WholeWord,
    Regex,
    ContextLines,
    Details,
    UseIgnoreFiles,
}

pub(crate) struct SearchEditorState {
    query: TextField,
    context: TextField,
    include: TextField,
    exclude: TextField,
    field: SeField,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    use_ignore_files: bool,
    show_context: bool,
    show_details: bool,
    running: Option<Search>,
    results: Vec<FileMatches>,
    /// A search waits for typing to pause.
    dirty_since: Option<Instant>,
    /// The matches in the document, for highlighting (dropped once the text is edited).
    pub matches: Vec<(Pos, Pos)>,
    matches_version: u64,
    error: Option<String>,
}

impl SearchEditorState {
    pub fn new(config: &Config) -> Self {
        let field = |s: &str| {
            let mut f = TextField::default();
            f.set_text(s);
            f
        };
        SearchEditorState {
            query: field(&config.query),
            context: field(&config.context_lines.max(1).to_string()),
            include: field(&config.include),
            exclude: field(&config.exclude),
            field: SeField::Query,
            case_sensitive: config.case_sensitive,
            whole_word: config.whole_word,
            regex: config.regex,
            use_ignore_files: config.use_ignore_files,
            show_context: config.context_lines > 0,
            show_details: !config.include.is_empty() || !config.exclude.is_empty() || !config.use_ignore_files,
            running: None,
            results: Vec::new(),
            dirty_since: None,
            matches: Vec::new(),
            matches_version: 0,
            error: None,
        }
    }

    pub fn config(&self) -> Config {
        Config {
            query: self.query.text.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
            include: self.include.text.clone(),
            exclude: self.exclude.text.clone(),
            use_ignore_files: self.use_ignore_files,
            context_lines: if self.show_context { self.context.text.trim().parse().unwrap_or(0) } else { 0 },
        }
    }

    fn field_mut(&mut self, f: SeField) -> &mut TextField {
        match f {
            SeField::Query => &mut self.query,
            SeField::Context => &mut self.context,
            SeField::Include => &mut self.include,
            SeField::Exclude => &mut self.exclude,
        }
    }

    /// The header's height.
    pub fn header_height(&self) -> f32 {
        let details = if self.show_details { 2.0 * (LABEL_H + INPUT_H) + 4.0 } else { 0.0 };
        6.0 + INPUT_H + 4.0 + 22.0 + details + 8.0
    }

    /// The matches to highlight in a document at `version`.
    pub fn highlights(&self, version: u64) -> &[(Pos, Pos)] {
        if version == self.matches_version {
            &self.matches
        } else {
            &[]
        }
    }
}

impl Workbench {
    fn active_search_editor(&mut self) -> Option<&mut SearchEditorState> {
        let g = self.active_group;
        let gr = self.groups.get_mut(g)?;
        gr.tabs.get_mut(gr.active)?.search.as_deref_mut()
    }

    /// The search configuration of document `doc` if a search editor shows it.
    pub(super) fn search_config_of(&self, doc: usize) -> Option<Config> {
        self.groups.iter().flat_map(|g| &g.tabs).find(|t| t.doc == doc).and_then(|t| t.search.as_ref()).map(|s| s.config())
    }

    /// A search editor tab for `config`, with `body` as its text (a saved search) or searching
    /// right away.
    fn search_tab(&mut self, config: &Config, path: Option<&Path>, body: Option<&str>) -> EditorState {
        let mut doc = Doc::virtual_named(&se::title(&config.query));
        doc.lang = language::Lang::SearchResult;
        doc.highlight = language::Highlighter::new(doc.lang);
        if let Some(path) = path {
            doc.set_path(path.to_path_buf());
        }
        if let Some(body) = body {
            doc.buffer.insert(Selection::default(), body);
            doc.buffer.break_undo_group();
            doc.buffer.mark_saved();
        }
        let doc = self.add_doc(doc);
        let mut ed = EditorState::new(doc);
        let mut state = SearchEditorState::new(config);
        if body.is_none() && !config.query.is_empty() {
            state.dirty_since = Some(Instant::now() - DEBOUNCE);
        }
        if let (Some(body), Some(d)) = (body, self.docs[doc].as_ref()) {
            state.matches = se::find_matches(body, &config.search_query());
            state.matches_version = d.buffer.version();
        }
        ed.search = Some(Box::new(state));
        ed
    }

    /// A saved search's tab (restoring a session).
    pub(super) fn saved_search_tab(&mut self, path: &Path) -> Option<EditorState> {
        let text = std::fs::read_to_string(path).ok()?.replace("\r\n", "\n");
        let (config, body) = se::parse(&text);
        Some(self.search_tab(&config, Some(path), Some(&body)))
    }

    /// Opens a search editor for `config` (in the next group when `side`).
    fn open_search_editor(&mut self, config: Config, side: bool, path: Option<&Path>, body: Option<&str>) {
        let ed = self.search_tab(&config, path, body);
        let g = if side && self.groups.len() < 4 {
            self.groups.insert(self.active_group + 1, super::Group { tabs: Vec::new(), active: 0, find: Default::default() });
            self.active_group + 1
        } else if side {
            (self.active_group + 1).min(self.groups.len() - 1)
        } else {
            self.active_group
        };
        let group = &mut self.groups[g];
        let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
        group.tabs.insert(at, ed);
        group.active = at;
        self.active_group = g;
        self.focus = if body.is_some() { Focus::Editor } else { Focus::SearchEditor };
        if let Some(s) = self.active_search_editor() {
            s.query.select_all();
        }
    }

    /// The search a new search editor starts with: the selection as the query, the context
    /// lines setting, and (`search.searchEditor.reusePriorSearchConfiguration`) the last
    /// search editor's other options.
    fn new_search_config(&self) -> Config {
        let mut config = Config::new();
        if self.settings.bool("search.searchEditor.reusePriorSearchConfiguration") {
            if let Some(prev) = self.groups.iter().flat_map(|g| &g.tabs).filter_map(|t| t.search.as_ref()).last() {
                config = prev.config();
            }
        }
        config.context_lines = self.settings.get("search.searchEditor.defaultNumberOfContextLines").as_u64().unwrap_or(1) as usize;
        let seed = self.active_editor().zip(self.active_doc()).map(|(ed, doc)| doc.buffer.text_in(&ed.sel)).filter(|t| !t.is_empty() && !t.contains('\n'));
        config.query = seed.unwrap_or_default();
        config
    }

    /// Search Editor: New Search Editor (and to the side).
    pub(super) fn new_search_editor(&mut self, side: bool) {
        let config = self.new_search_config();
        self.open_search_editor(config, side, None, None);
    }

    /// Opens the Search view's search in a search editor (⌘Enter in the Search view).
    pub(super) fn open_search_results_in_editor(&mut self) {
        let mut config = self.search.editor_config();
        config.context_lines = self.settings.get("search.searchEditor.defaultNumberOfContextLines").as_u64().unwrap_or(1) as usize;
        self.open_search_editor(config, false, None, None);
    }

    /// Opens a saved search (`.code-search`).
    pub(super) fn open_saved_search(&mut self, path: &Path) {
        if let Some((g, i)) = self.groups.iter().enumerate().find_map(|(g, gr)| gr.tabs.iter().position(|t| t.search.is_some() && self.docs[t.doc].as_ref().and_then(|d| d.buffer.path()) == Some(path)).map(|i| (g, i))) {
            self.active_group = g;
            self.groups[g].active = i;
            return;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t.replace("\r\n", "\n"),
            Err(e) => return self.set_status_message(&format!("Couldn't open {}: {e}", path.display())),
        };
        let (config, body) = se::parse(&text);
        self.open_search_editor(config, false, Some(path), Some(&body));
        if let Some(tree) = &mut self.tree {
            tree.reveal(path);
        }
    }

    /// Saves a search editor's document with its `# Query:` header (asking where for a new
    /// one, or when `ask`). None if `doc_id` isn't a search editor.
    pub(super) fn save_search_editor(&mut self, doc_id: usize, ask: bool) -> Option<bool> {
        let config = self.search_config_of(doc_id)?;
        let root = self.folder();
        let has_path = self.docs[doc_id].as_ref()?.buffer.path().is_some();
        if ask || !has_path {
            let name: String = config.query.chars().filter(|c| !"/\\:\n".contains(*c)).take(40).collect();
            let name = if name.trim().is_empty() { "Untitled".to_string() } else { name };
            let mut dialog = self.file_dialog().set_file_name(format!("{name}.code-search"));
            if let Some(root) = root {
                dialog = dialog.set_directory(root);
            }
            let Some(path) = dialog.save_file() else { return Some(false) };
            self.docs[doc_id].as_mut()?.set_path(path);
        }
        let doc = self.docs[doc_id].as_mut()?;
        // A file's language comes from its name; this one stays a search editor's.
        doc.lang = language::Lang::SearchResult;
        let path = doc.buffer.path()?.to_path_buf();
        let text = format!("{}\n{}", config.header(), doc.buffer.text());
        match std::fs::write(&path, text) {
            Ok(()) => {
                doc.buffer.mark_saved();
                self.after_save(&path);
                Some(true)
            }
            Err(e) => {
                self.message_dialog().set_level(rfd::MessageLevel::Error).set_title("Failed to save").set_description(e.to_string()).show();
                Some(false)
            }
        }
    }

    /// Runs searches that are due and shows finished ones. Called every frame.
    pub(super) fn search_editor_tick(&mut self) {
        let roots = self.folders();
        if roots.is_empty() {
            return;
        }
        let context = |s: &SearchEditorState| s.config().context_lines;
        for g in 0..self.groups.len() {
            for t in 0..self.groups[g].tabs.len() {
                let Some(s) = self.groups[g].tabs[t].search.as_deref_mut() else { continue };
                if s.dirty_since.is_some_and(|d| d.elapsed() >= DEBOUNCE) {
                    s.dirty_since = None;
                    s.error = None;
                    s.results.clear();
                    let query = s.config().search_query();
                    if query.pattern.is_empty() {
                        s.running = None;
                        continue;
                    }
                    match Search::start_in(&roots, &query, self.waker.clone()) {
                        Ok(search) => s.running = Some(search),
                        Err(e) => {
                            s.running = None;
                            s.error = Some(e);
                        }
                    }
                }
                let Some(running) = &s.running else { continue };
                s.results.extend(running.poll());
                if !running.is_done() {
                    continue;
                }
                s.results.extend(running.poll());
                let limit_hit = running.limit_hit.load(std::sync::atomic::Ordering::Relaxed);
                s.running = None;
                let mut results = std::mem::take(&mut s.results);
                results.sort_by(|a, b| a.path.cmp(&b.path));
                let context = context(s);
                let doc_id = self.groups[g].tabs[t].doc;
                // The file as the search read it (from disk: an open document's unsaved text would put
                // the matches on the wrong lines).
                let contents: Vec<String> = results.iter().map(|f| std::fs::read_to_string(&f.path).unwrap_or_default()).collect();
                let files: Vec<se::FileResults> = results
                    .iter()
                    .zip(&contents)
                    .map(|(f, text)| se::FileResults { label: self.display_path(&f.path), lines: text.split('\n').collect(), matches: &f.matches })
                    .collect();
                let (text, matches) = se::serialize(&files, context, limit_hit);
                let focus_results = self.settings.bool("search.searchEditor.focusResultsOnSearch");
                let Some(doc) = self.docs[doc_id].as_mut() else { continue };
                let ed = &mut self.groups[g].tabs[t];
                let before = ed.selections();
                let all = (Pos::new(0, 0), doc.buffer.end(), text.as_str());
                doc.buffer.edit(&before, &[all], text::EditKind::Other);
                doc.buffer.break_undo_group();
                if doc.buffer.path().is_none() {
                    doc.buffer.mark_saved();
                }
                ed.set_selection(Selection::caret(Pos::new(0, 0)));
                ed.scroll_y = 0.0;
                ed.refresh_layout(&doc.buffer);
                let s = ed.search.as_deref_mut().unwrap();
                s.matches = matches;
                s.matches_version = doc.buffer.version();
                // The tab's title follows the query (a saved search keeps its file name).
                if doc.buffer.path().is_none() {
                    doc.label = Some(se::title(&s.query.text));
                }
                if focus_results && g == self.active_group && self.focus == Focus::SearchEditor {
                    self.focus = Focus::Editor;
                }
            }
        }
    }

    pub(super) fn search_editor_deadline(&self) -> Option<Instant> {
        self.groups.iter().flat_map(|g| &g.tabs).filter_map(|t| t.search.as_ref()?.dirty_since).map(|t| t + DEBOUNCE).min()
    }

    // ------------------------------------------------------------------ input

    /// Keys while a header field has focus.
    pub(super) fn search_editor_key(&mut self, k: &KeyInput) {
        let Some(s) = self.active_search_editor() else {
            self.focus = Focus::Editor;
            return;
        };
        match k.key {
            Key::Enter => {
                s.dirty_since = Some(Instant::now() - DEBOUNCE);
                return;
            }
            Key::Escape | Key::Down if s.field == SeField::Query => {
                self.focus = Focus::Editor;
                return;
            }
            Key::Tab => {
                let mut order = vec![SeField::Query];
                if s.show_context {
                    order.push(SeField::Context);
                }
                if s.show_details {
                    order.extend([SeField::Include, SeField::Exclude]);
                }
                let i = order.iter().position(|f| *f == s.field).unwrap_or(0);
                let next = (i as isize + if k.shift { -1 } else { 1 }).rem_euclid(order.len() as isize) as usize;
                s.field = order[next];
                s.field_mut(order[next]).select_all();
                return;
            }
            _ => {}
        }
        // ⌥⌘C / ⌥⌘W / ⌥⌘R toggle the options.
        if k.cmd && k.alt {
            let toggle = match &k.key {
                Key::Char(c) if c == "c" => Some(SeToggle::MatchCase),
                Key::Char(c) if c == "w" => Some(SeToggle::WholeWord),
                Key::Char(c) if c == "r" => Some(SeToggle::Regex),
                _ => None,
            };
            if let Some(t) = toggle {
                return self.search_editor_toggle(t);
            }
        }
        let field = s.field;
        if field == SeField::Context && matches!(&k.key, Key::Char(c) if !c.chars().all(|c| c.is_ascii_digit())) && !k.cmd {
            return;
        }
        match s.field_mut(field).key(k) {
            FieldEvent::Changed => s.dirty_since = Some(Instant::now()),
            FieldEvent::Moved => {}
            FieldEvent::Ignored => {
                if let Some(cmd) = k.command() {
                    self.run(cmd);
                }
            }
        }
    }

    pub(super) fn search_editor_toggle(&mut self, t: SeToggle) {
        let Some(s) = self.active_search_editor() else { return };
        let rerun = match t {
            SeToggle::MatchCase => {
                s.case_sensitive = !s.case_sensitive;
                true
            }
            SeToggle::WholeWord => {
                s.whole_word = !s.whole_word;
                true
            }
            SeToggle::Regex => {
                s.regex = !s.regex;
                true
            }
            SeToggle::UseIgnoreFiles => {
                s.use_ignore_files = !s.use_ignore_files;
                true
            }
            SeToggle::ContextLines => {
                s.show_context = !s.show_context;
                true
            }
            SeToggle::Details => {
                s.show_details = !s.show_details;
                false
            }
        };
        if rerun {
            s.dirty_since = Some(Instant::now() - DEBOUNCE);
        }
    }

    /// Increase / Decrease Context Lines.
    pub(super) fn search_editor_context_step(&mut self, by: isize) {
        let Some(s) = self.active_search_editor() else { return };
        let n = if s.show_context { s.context.text.trim().parse::<isize>().unwrap_or(0) } else { 0 };
        let n = (n + by).max(0);
        s.show_context = n > 0;
        if n > 0 {
            s.context.set_text(&n.to_string());
        }
        s.dirty_since = Some(Instant::now() - DEBOUNCE);
    }

    pub(super) fn search_editor_rerun(&mut self) {
        if let Some(s) = self.active_search_editor() {
            s.dirty_since = Some(Instant::now() - DEBOUNCE);
        }
    }

    /// Focus Search Editor Input (Esc in the results).
    pub(super) fn focus_search_editor_input(&mut self) {
        if let Some(s) = self.active_search_editor() {
            s.field = SeField::Query;
            s.query.select_all();
            self.focus = Focus::SearchEditor;
        }
    }

    pub(super) fn click_search_editor_field(&mut self, g: usize, field: SeField, x: f32, shift: bool) {
        self.active_group = g;
        let Some(s) = self.active_search_editor() else { return };
        s.field = field;
        s.field_mut(field).click(x, shift);
        self.focus = Focus::SearchEditor;
    }

    pub(super) fn search_editor_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        let pasted = if paste { self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) } else { None };
        let Some(s) = self.active_search_editor() else { return };
        let field = s.field;
        let f = s.field_mut(field);
        if all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = pasted {
                f.insert(&text.replace('\n', " "));
                s.dirty_since = Some(Instant::now());
            }
            return;
        }
        let text = if cut { f.cut() } else { f.copy() };
        if cut {
            s.dirty_since = Some(Instant::now());
        }
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    /// The result at `pos` in the active search editor opens (double-click,
    /// `search.searchEditor.doubleClickBehaviour`). False when it isn't a result line.
    pub(super) fn open_search_editor_result(&mut self, pos: Pos) -> bool {
        let behaviour = self.settings.string("search.searchEditor.doubleClickBehaviour");
        if behaviour == "selectWord" {
            return false;
        }
        let Some(ed) = self.active_editor().filter(|e| e.search.is_some()) else { return false };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return false };
        let text = doc.buffer.text();
        let lines: Vec<&str> = text.split('\n').collect();
        let Some((label, line, col)) = se::location(&lines, pos.line, pos.col) else { return false };
        let Some(path) = self.resolve_display_path(&label) else { return false };
        if behaviour == "openLocationToSide" {
            if self.active_group + 1 >= self.groups.len() && self.groups.len() < 4 {
                self.groups.insert(self.active_group + 1, super::Group { tabs: Vec::new(), active: 0, find: Default::default() });
            }
            self.active_group = (self.active_group + 1).min(self.groups.len() - 1);
        }
        self.goto_location(&path, Pos::new(line, col));
        true
    }

    /// Search Editor: Select All Matches.
    pub(super) fn select_all_search_editor_matches(&mut self) {
        let g = self.active_group;
        let gr = &mut self.groups[g];
        let Some(ed) = gr.tabs.get_mut(gr.active) else { return };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let Some(s) = ed.search.as_ref() else { return };
        let sels: Vec<Selection> = s.highlights(doc.buffer.version()).iter().map(|&(a, z)| Selection { anchor: a, head: z, goal_col: None }).collect();
        if !sels.is_empty() {
            ed.set_selections(sels);
            ed.reveal = true;
        }
    }

    /// Search Editor: Delete File Results: removes the results of the file at the cursor.
    pub(super) fn delete_search_editor_file_results(&mut self) {
        let Some((ed, doc)) = self.active_mut() else { return };
        if ed.search.is_none() {
            return;
        }
        let line = ed.sel.head.line;
        let n = doc.buffer.len_lines();
        let is_path = |l: &str| !l.is_empty() && !l.starts_with(char::is_whitespace) && l.ends_with(':');
        let Some(start) = (0..=line).rev().find(|&l| is_path(&doc.buffer.line(l))) else { return };
        // The block ends at the next file (or the end), with the blank line after it.
        let end = (start + 1..n).find(|&l| is_path(&doc.buffer.line(l))).unwrap_or(n);
        let before = ed.selections();
        let to = if end < n { Pos::new(end, 0) } else { doc.buffer.end() };
        doc.buffer.edit(&before, &[(Pos::new(start, 0), to, "")], text::EditKind::Other);
        let head = doc.buffer.clamp(Pos::new(start, 0));
        ed.set_selection(Selection::caret(head));
    }

    // ------------------------------------------------------------------ drawing

    /// The header above a search editor's results.
    pub(super) fn draw_search_editor_header(&mut self, c: &mut Canvas, g: usize, r: Rect) {
        c.fill(r, self.color("editor.background"));
        self.hits.push((r, Hit::SearchEditorHeader(g)));
        let active = g == self.active_group && self.focus == Focus::SearchEditor && self.palette.is_none();
        let fg = self.color("foreground");
        let Some(s) = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.search.as_deref()) else { return };
        let (show_context, show_details, focused_field) = (s.show_context, s.show_details, s.field);
        let flags = [s.case_sensitive, s.whole_word, s.regex, s.use_ignore_files];
        let error = s.error.is_some();
        let left = r.x + 19.0;
        let right = r.right() - 24.0;
        let mut y = r.y + 6.0;
        // The query, with the context lines toggle (and count) after it.
        let ctx_w = if show_context { 54.0 } else { 0.0 };
        let query_r = Rect::new(left, y, right - left - 28.0 - ctx_w, INPUT_H);
        self.search_editor_input(c, g, query_r, SeField::Query, "Search", TOGGLE * 3.0 + 6.0, active && focused_field == SeField::Query, error);
        let t = |i: f32| Rect::new(query_r.right() - 3.0 - TOGGLE * (3.0 - i), query_r.y + 3.0, TOGGLE, TOGGLE);
        self.option_toggle(c, t(0.0), "Aa", flags[0], Hit::SearchEditorToggle(g, SeToggle::MatchCase), false);
        self.option_toggle(c, t(1.0), "ab", flags[1], Hit::SearchEditorToggle(g, SeToggle::WholeWord), true);
        self.option_toggle(c, t(2.0), ".*", flags[2], Hit::SearchEditorToggle(g, SeToggle::Regex), false);
        let ctx_toggle = Rect::new(query_r.right() + 4.0, y + 2.0, 22.0, 22.0);
        self.icon_toggle(c, ctx_toggle, &icons::LIST_SELECTION, show_context, Hit::SearchEditorToggle(g, SeToggle::ContextLines));
        if show_context {
            let ctx_r = Rect::new(ctx_toggle.right() + 4.0, y, 50.0, INPUT_H);
            self.search_editor_input(c, g, ctx_r, SeField::Context, "", 0.0, active && focused_field == SeField::Context, false);
        }
        y += INPUT_H + 4.0;
        // "…" shows the include/exclude fields.
        let dots = Rect::new(right - 22.0, y + 2.0, 22.0, 18.0);
        let hit = Hit::SearchEditorToggle(g, SeToggle::Details);
        if self.hovered(hit) || show_details {
            c.fill_rounded(dots, self.color("inputOption.hoverBackground"), 3.0);
        }
        c.icon_in(&icons::ELLIPSIS, dots, 16.0, fg);
        self.hits.push((dots, hit));
        y += 22.0;
        if show_details {
            let label = TextStyle::ui(SMALL, fg).weight(600);
            c.text(left, y + 3.0, "files to include", &label);
            y += LABEL_H;
            self.search_editor_input(c, g, Rect::new(left, y, right - left, INPUT_H), SeField::Include, "", 0.0, active && focused_field == SeField::Include, false);
            y += INPUT_H;
            c.text(left, y + 3.0, "files to exclude", &label);
            y += LABEL_H;
            let ex = Rect::new(left, y, right - left, INPUT_H);
            self.search_editor_input(c, g, ex, SeField::Exclude, "", TOGGLE + 6.0, active && focused_field == SeField::Exclude, false);
            let toggle = Rect::new(ex.right() - 3.0 - TOGGLE, ex.y + 3.0, TOGGLE, TOGGLE);
            self.icon_toggle(c, toggle, &icons::GEAR, flags[3], Hit::SearchEditorToggle(g, SeToggle::UseIgnoreFiles));
        }
        // A bad regex or glob.
        if let Some(e) = self.groups[g].tabs.get(self.groups[g].active).and_then(|t| t.search.as_ref()?.error.clone()) {
            let st = TextStyle::ui(12.0, self.color("errorForeground"));
            c.text_in(Rect::new(left, r.bottom() - 22.0, dots.x - left - 8.0, 20.0), &e, &st);
        }
    }

    fn icon_toggle(&mut self, c: &mut Canvas, r: Rect, icon: &render::Icon, on: bool, hit: Hit) {
        if on {
            c.bordered(r, self.color("inputOption.activeBackground"), self.color("inputOption.activeBorder"), 1.0, 3.0);
        } else if self.hovered(hit) {
            c.fill_rounded(r, self.color("inputOption.hoverBackground"), 3.0);
        }
        let fg = if on { self.color("inputOption.activeForeground") } else { self.color("icon.foreground") };
        c.icon_in(icon, r, 14.0, fg);
        self.hits.push((r, hit));
    }

    #[allow(clippy::too_many_arguments)]
    fn search_editor_input(&mut self, c: &mut Canvas, g: usize, r: Rect, field: SeField, placeholder: &str, reserve: f32, focused: bool, error: bool) {
        let border = if error {
            self.color("inputValidation.errorBorder")
        } else if focused {
            self.color("focusBorder")
        } else {
            self.color_or("searchEditor.textInputBorder", "input.background")
        };
        c.bordered(r, self.color("input.background"), border, 1.0, 2.0);
        let style = TextStyle::ui(UI, self.color("input.foreground"));
        let text_r = Rect::new(r.x + 6.0, r.y, r.w - 12.0 - reserve, r.h);
        let (ph, sel) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        let caret_on = self.caret_on();
        let gr = &mut self.groups[g];
        if let Some(s) = gr.tabs.get_mut(gr.active).and_then(|t| t.search.as_deref_mut()) {
            c.push_clip(text_r);
            s.field_mut(field).draw(c, text_r, &style, placeholder, ph, focused, caret_on, sel);
            c.pop_clip();
        }
        self.hits.push((r, Hit::SearchEditorField(g, field)));
    }
}
