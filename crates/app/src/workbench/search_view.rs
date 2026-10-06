//! The Search view in the side bar: query and replace fields with option toggles,
//! include/exclude globs, streaming results grouped by file, and Replace All.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use render::{Canvas, Color, Rect, TextStyle};
use search::{FileMatches, Query, Search};
use text::{Pos, Selection};

use super::{Focus, Hit, View, Workbench, ROW_H, SMALL, UI};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::{FieldEvent, TextField};

const DEBOUNCE: Duration = Duration::from_millis(300);
const INPUT_H: f32 = 26.0;
const TOGGLE: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Query,
    Replace,
    Include,
    Exclude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SearchToggle {
    MatchCase,
    WholeWord,
    Regex,
    UseIgnoreFiles,
    ShowReplace,
    ShowDetails,
}

pub(super) struct SearchView {
    query: TextField,
    replace: TextField,
    include: TextField,
    exclude: TextField,
    field: Field,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    use_ignore_files: bool,
    show_replace: bool,
    show_details: bool,
    running: Option<Search>,
    /// Query the current results belong to.
    searched: Option<Query>,
    results: Vec<FileMatches>,
    collapsed: HashSet<PathBuf>,
    error: Option<String>,
    scroll: f32,
    /// A search is pending after typing stops.
    dirty_since: Option<Instant>,
    /// Row geometry from the last draw: (row index, file index, match index).
    rows: Vec<(usize, usize, Option<usize>)>,
}

impl Default for SearchView {
    fn default() -> Self {
        Self {
            query: TextField::default(),
            replace: TextField::default(),
            include: TextField::default(),
            exclude: TextField::default(),
            field: Field::Query,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            use_ignore_files: true,
            show_replace: false,
            show_details: false,
            running: None,
            searched: None,
            results: Vec::new(),
            collapsed: HashSet::new(),
            error: None,
            scroll: 0.0,
            dirty_since: None,
            rows: Vec::new(),
        }
    }
}

impl SearchView {
    fn field_mut(&mut self, f: Field) -> &mut TextField {
        match f {
            Field::Query => &mut self.query,
            Field::Replace => &mut self.replace,
            Field::Include => &mut self.include,
            Field::Exclude => &mut self.exclude,
        }
    }

    fn query(&self) -> Query {
        Query {
            pattern: self.query.text.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
            include: self.include.text.clone(),
            exclude: self.exclude.text.clone(),
            use_ignore_files: self.use_ignore_files,
        }
    }

    /// The search as a search editor's configuration (Open Results in Editor).
    pub(super) fn editor_config(&self) -> crate::search_editor::Config {
        let q = self.query();
        crate::search_editor::Config {
            query: q.pattern,
            case_sensitive: q.case_sensitive,
            whole_word: q.whole_word,
            regex: q.regex,
            include: q.include,
            exclude: q.exclude,
            use_ignore_files: q.use_ignore_files,
            context_lines: 0,
        }
    }

    fn total_matches(&self) -> usize {
        self.results.iter().map(|f| f.matches.len()).sum()
    }

    fn clear(&mut self) {
        self.running = None;
        self.searched = None;
        self.results.clear();
        self.error = None;
        self.scroll = 0.0;
        self.collapsed.clear();
    }
}

impl Workbench {
    /// The folders searched: all of the workspace's.
    fn search_roots(&self) -> Option<Vec<PathBuf>> {
        Some(self.folders()).filter(|f| !f.is_empty())
    }

    /// Shows the Search view with the query field focused, seeded from the editor selection.
    pub(super) fn focus_search(&mut self, replace: bool) {
        self.view = View::Search;
        self.sidebar_visible = true;
        self.focus = Focus::Search;
        let seed = self.active_editor().zip(self.active_doc()).and_then(|(ed, doc)| {
            let t = doc.buffer.text_in(&ed.sel);
            (!t.is_empty() && !t.contains('\n')).then_some(t)
        });
        let sv = &mut self.search;
        if let Some(seed) = seed {
            sv.query.set_text(&seed);
            sv.dirty_since = Some(Instant::now() - DEBOUNCE);
        }
        sv.query.select_all();
        sv.field = Field::Query;
        if replace {
            sv.show_replace = true;
            sv.field = Field::Replace;
            sv.replace.select_all();
        }
    }

    /// Starts (or restarts) the search for the current fields.
    pub(super) fn run_search(&mut self) {
        let Some(roots) = self.search_roots() else { return };
        let sv = &mut self.search;
        sv.dirty_since = None;
        let query = sv.query();
        sv.clear();
        if query.pattern.is_empty() {
            return;
        }
        match Search::start_in(&roots, &query, self.waker.clone()) {
            Ok(search) => {
                sv.running = Some(search);
                sv.searched = Some(query);
            }
            Err(e) => sv.error = Some(e),
        }
    }

    /// Collects streamed results and runs a debounced search. Called every frame.
    pub(super) fn search_tick(&mut self) {
        if self.search.dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE) {
            self.run_search();
        }
        let sv = &mut self.search;
        let Some(running) = &sv.running else { return };
        for file in running.poll() {
            // Results arrive from parallel workers; keep files sorted by path.
            let at = sv.results.partition_point(|f| f.path < file.path);
            sv.results.insert(at, file);
        }
        if running.is_done() {
            // Keep the handle until drained so late results aren't lost, then drop it.
            let rest = running.poll();
            for file in rest {
                let at = sv.results.partition_point(|f| f.path < file.path);
                sv.results.insert(at, file);
            }
            if sv.running.as_ref().is_some_and(|r| r.limit_hit.load(std::sync::atomic::Ordering::Relaxed)) {
                sv.error = Some(format!("The result set only contains a subset of all matches ({}).", search::MAX_RESULTS));
            }
            sv.running = None;
        }
    }

    pub(super) fn search_deadline(&self) -> Option<Instant> {
        self.search.dirty_since.map(|t| t + DEBOUNCE)
    }

    /// Keys while a search field has focus.
    pub(super) fn search_key(&mut self, k: &KeyInput) {
        let sv = &mut self.search;
        match k.key {
            // ⌘Enter: Open Results in Editor.
            Key::Enter if k.cmd => {
                self.open_search_results_in_editor();
                return;
            }
            Key::Enter if !k.cmd => {
                if sv.field == Field::Replace && k.alt {
                    self.replace_all();
                } else {
                    self.run_search();
                }
                return;
            }
            Key::Escape => {
                self.focus = Focus::Editor;
                return;
            }
            Key::Tab => {
                // Move between the visible fields.
                let mut order = vec![Field::Query];
                if sv.show_replace {
                    order.push(Field::Replace);
                }
                if sv.show_details {
                    order.extend([Field::Include, Field::Exclude]);
                }
                let i = order.iter().position(|f| *f == sv.field).unwrap_or(0);
                let n = order.len() as isize;
                let next = (i as isize + if k.shift { -1 } else { 1 }).rem_euclid(n) as usize;
                sv.field = order[next];
                sv.field_mut(order[next]).select_all();
                return;
            }
            _ => {}
        }
        // Option toggles: ⌥⌘C / ⌥⌘W / ⌥⌘R.
        if k.cmd && k.alt {
            let toggle = match &k.key {
                Key::Char(c) if c == "c" => Some(SearchToggle::MatchCase),
                Key::Char(c) if c == "w" => Some(SearchToggle::WholeWord),
                Key::Char(c) if c == "r" => Some(SearchToggle::Regex),
                _ => None,
            };
            if let Some(t) = toggle {
                return self.search_toggle(t);
            }
        }
        let field = sv.field;
        match sv.field_mut(field).key(k) {
            FieldEvent::Changed => {
                if field != Field::Replace {
                    sv.dirty_since = Some(Instant::now());
                }
            }
            FieldEvent::Moved => {}
            FieldEvent::Ignored => {
                if let Some(cmd) = k.command() {
                    self.run(cmd);
                }
            }
        }
    }

    pub(super) fn search_toggle(&mut self, t: SearchToggle) {
        let sv = &mut self.search;
        let rerun = match t {
            SearchToggle::MatchCase => {
                sv.case_sensitive = !sv.case_sensitive;
                true
            }
            SearchToggle::WholeWord => {
                sv.whole_word = !sv.whole_word;
                true
            }
            SearchToggle::Regex => {
                sv.regex = !sv.regex;
                true
            }
            SearchToggle::UseIgnoreFiles => {
                sv.use_ignore_files = !sv.use_ignore_files;
                true
            }
            SearchToggle::ShowReplace => {
                sv.show_replace = !sv.show_replace;
                sv.field = if sv.show_replace { Field::Replace } else { Field::Query };
                false
            }
            SearchToggle::ShowDetails => {
                sv.show_details = !sv.show_details;
                false
            }
        };
        if rerun && !sv.query.text.is_empty() {
            self.run_search();
        }
    }

    /// Clipboard commands while a search field has focus.
    pub(super) fn search_clipboard(&mut self, cut: bool, paste: bool) {
        let field = self.search.field;
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.search.field_mut(field).insert(&text);
                if field != Field::Replace {
                    self.search.dirty_since = Some(Instant::now());
                }
            }
            return;
        }
        let f = self.search.field_mut(field);
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
        if cut && field != Field::Replace {
            self.search.dirty_since = Some(Instant::now());
        }
    }

    pub(super) fn search_select_all(&mut self) {
        let field = self.search.field;
        self.search.field_mut(field).select_all();
    }

    pub(super) fn click_search_field(&mut self, field: Field, x: f32, shift: bool) {
        self.focus = Focus::Search;
        self.search.field = field;
        self.search.field_mut(field).click(x, shift);
    }

    pub(super) fn scroll_search(&mut self, dy: f32) {
        self.search.scroll = (self.search.scroll - dy).max(0.0);
    }

    pub(super) fn clear_search(&mut self) {
        self.search.clear();
        self.search.query.set_text("");
        self.search.dirty_since = None;
    }

    pub(super) fn collapse_all_search(&mut self) {
        let sv = &mut self.search;
        let all: HashSet<PathBuf> = sv.results.iter().map(|f| f.path.clone()).collect();
        if sv.collapsed.len() == all.len() {
            sv.collapsed.clear();
        } else {
            sv.collapsed = all;
        }
    }

    /// Opens a clicked result: toggles a file row, or jumps to a match.
    pub(super) fn open_search_row(&mut self, row: usize) {
        let Some(&(_, file, m)) = self.search.rows.iter().find(|(r, ..)| *r == row) else { return };
        let path = self.search.results[file].path.clone();
        let Some(mi) = m else {
            if !self.search.collapsed.remove(&path) {
                self.search.collapsed.insert(path);
            }
            return;
        };
        let m = self.search.results[file].matches[mi].clone();
        self.open_file_preview(&path);
        self.focus = Focus::Editor;
        if let Some((ed, doc)) = self.active_mut() {
            // Match offsets are bytes within the line; the editor works in chars.
            let line = doc.buffer.line(m.line);
            let to_col = |byte: usize| line.char_indices().take_while(|(b, _)| *b < byte).count();
            let (start, end) = (Pos::new(m.line, to_col(m.start)), Pos::new(m.line, to_col(m.end)));
            ed.jump_to(doc, start);
            ed.set_selection(Selection { anchor: start, head: end, goal_col: None });
        }
    }

    /// Replaces every match of the current search, after confirmation.
    pub(super) fn replace_all(&mut self) {
        let Some(query) = self.search.searched.clone() else { return };
        let Ok(regex) = query.compile() else { return };
        let replacement = self.search.replace.text.clone();
        let (files, total) = (self.search.results.len(), self.search.total_matches());
        if total == 0 {
            return;
        }
        let answer = self.message_dialog()
            .set_level(rfd::MessageLevel::Warning)
            .set_title(format!(
                "Replace {total} occurrence{} across {files} file{} with '{replacement}'?",
                if total == 1 { "" } else { "s" },
                if files == 1 { "" } else { "s" }
            ))
            .set_buttons(rfd::MessageButtons::OkCancelCustom("Replace".into(), "Cancel".into()))
            .show();
        if !matches!(answer, rfd::MessageDialogResult::Custom(ref s) if s == "Replace") && answer != rfd::MessageDialogResult::Ok {
            return;
        }
        let paths: Vec<PathBuf> = self.search.results.iter().map(|f| f.path.clone()).collect();
        let mut failed = Vec::new();
        for path in paths {
            // Open documents are edited in place (one undo step, left unsaved);
            // other files are rewritten on disk.
            if let Some(doc) = self.docs.iter_mut().flatten().find(|d| d.buffer.path() == Some(path.as_path())) {
                let (new_text, n) = search::replace_text(&doc.buffer.text(), &regex, &replacement, query.regex);
                if n > 0 {
                    let all = Selection { anchor: Pos::new(0, 0), head: doc.buffer.end(), goal_col: None };
                    doc.buffer.insert(all, &new_text);
                    doc.buffer.break_undo_group();
                }
                continue;
            }
            let result = std::fs::read_to_string(&path).and_then(|text| {
                let (new_text, n) = search::replace_text(&text, &regex, &replacement, query.regex);
                if n > 0 { std::fs::write(&path, new_text) } else { Ok(()) }
            });
            if result.is_err() {
                failed.push(path.display().to_string());
            }
        }
        if !failed.is_empty() {
            self.message_dialog()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Some files could not be changed")
                .set_description(failed.join("\n"))
                .show();
        }
        self.run_search();
    }

    // ------------------------------------------------------------------ drawing

    pub(super) fn option_toggle(&mut self, c: &mut Canvas, r: Rect, label: &str, on: bool, hit: Hit, underline: bool) {
        if on {
            c.bordered(r, self.color("inputOption.activeBackground"), self.color("inputOption.activeBorder"), 1.0, 3.0);
        } else if self.hovered(hit) {
            c.fill_rounded(r, self.color("inputOption.hoverBackground"), 3.0);
        }
        let fg = if on { self.color("inputOption.activeForeground") } else { self.color("icon.foreground") };
        let style = TextStyle::mono(11.0, r.h, fg);
        let w = c.measure(label, &style);
        let x = r.x + ((r.w - w) / 2.0).round();
        c.text(x, r.y, label, &style);
        if underline {
            c.fill(Rect::new(x, r.bottom() - 4.0, w, 1.0), fg);
        }
        self.hits.push((r, hit));
    }

    /// Draws an input box with its text field. `reserve` leaves room on the right for toggles.
    fn search_input(&mut self, c: &mut Canvas, r: Rect, field: Field, placeholder: &str, reserve: f32, error: bool) {
        let focused = self.focus == Focus::Search && self.search.field == field && self.palette.is_none();
        let border = if error {
            self.color("inputValidation.errorBorder")
        } else if focused {
            self.color("focusBorder")
        } else {
            self.color("input.border")
        };
        c.bordered(r, self.color("input.background"), border, 1.0, super::controls::FIELD_RADIUS);
        let style = TextStyle::ui(UI, self.color("input.foreground"));
        let text_r = Rect::new(r.x + 6.0, r.y, r.w - 12.0 - reserve, r.h);
        let (ph, sel) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        let caret_on = self.caret_on();
        self.search.field_mut(field).draw(c, text_r, &style, placeholder, ph, focused, caret_on, sel);
        self.hits.push((r, Hit::SearchField(field)));
    }

    pub(super) fn draw_search_view(&mut self, c: &mut Canvas, body: Rect) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        let dim = self.color("descriptionForeground");
        let right = body.right() - 12.0;
        let mut y = body.y + 4.0;

        // Replace toggle (chevron) spanning the query/replace rows.
        let rows_h = if self.search.show_replace { INPUT_H * 2.0 + 4.0 } else { INPUT_H };
        let chevron = Rect::new(body.x + 2.0, y, 16.0, rows_h);
        if self.hovered(Hit::SearchToggle(SearchToggle::ShowReplace)) {
            c.fill_rounded(chevron, self.color("inputOption.hoverBackground"), 3.0);
        }
        let icon = if self.search.show_replace { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT };
        c.icon_in(icon, chevron, 16.0, fg);
        self.hits.push((chevron, Hit::SearchToggle(SearchToggle::ShowReplace)));
        let x_in = body.x + 20.0;

        // Query row with option toggles inside.
        let query_r = Rect::new(x_in, y, right - x_in, INPUT_H);
        let error = self.search.error.is_some() && self.search.running.is_none() && self.search.results.is_empty();
        self.search_input(c, query_r, Field::Query, "Search", TOGGLE * 3.0 + 6.0, error && self.search.searched.is_none());
        let t = |i: f32| Rect::new(query_r.right() - 3.0 - TOGGLE * (3.0 - i), query_r.y + 3.0, TOGGLE, TOGGLE);
        let (cs, ww, rx) = (self.search.case_sensitive, self.search.whole_word, self.search.regex);
        self.option_toggle(c, t(0.0), "Aa", cs, Hit::SearchToggle(SearchToggle::MatchCase), false);
        self.option_toggle(c, t(1.0), "ab", ww, Hit::SearchToggle(SearchToggle::WholeWord), true);
        self.option_toggle(c, t(2.0), ".*", rx, Hit::SearchToggle(SearchToggle::Regex), false);
        y += INPUT_H + 4.0;

        if self.search.show_replace {
            let button_w = 78.0;
            let replace_r = Rect::new(x_in, y, right - x_in - button_w - 4.0, INPUT_H);
            self.search_input(c, replace_r, Field::Replace, "Replace", 0.0, false);
            let btn = Rect::new(replace_r.right() + 4.0, y, button_w, INPUT_H);
            let enabled = self.search.total_matches() > 0;
            let bg = if !enabled {
                self.color("input.background")
            } else if self.hovered(Hit::ReplaceAll) {
                self.color("button.hoverBackground")
            } else {
                self.color("button.background")
            };
            c.fill_rounded(btn, bg, super::controls::FIELD_RADIUS);
            let bs = TextStyle::ui(12.0, if enabled { self.color("button.foreground") } else { dim });
            let tw = c.measure("Replace All", &bs);
            c.text_in(Rect::new(btn.x + (btn.w - tw) / 2.0, btn.y, tw + 1.0, btn.h), "Replace All", &bs);
            if enabled {
                self.hits.push((btn, Hit::ReplaceAll));
            }
            y += INPUT_H + 4.0;
        }

        // "…" toggles the include/exclude fields.
        let dots = Rect::new(right - 22.0, y, 22.0, 18.0);
        if self.hovered(Hit::SearchToggle(SearchToggle::ShowDetails)) || self.search.show_details {
            c.fill_rounded(dots, self.color("inputOption.hoverBackground"), 3.0);
        }
        c.icon_in(&icons::ELLIPSIS, dots, 16.0, fg);
        self.hits.push((dots, Hit::SearchToggle(SearchToggle::ShowDetails)));
        y += 20.0;
        if self.search.show_details {
            let label = TextStyle::ui(SMALL, fg);
            c.text(x_in, y, "files to include", &label);
            y += 18.0;
            self.search_input(c, Rect::new(x_in, y, right - x_in, INPUT_H), Field::Include, "e.g. *.rs, src/**/include", 0.0, false);
            y += INPUT_H + 6.0;
            c.text(x_in, y, "files to exclude", &label);
            y += 18.0;
            let ex = Rect::new(x_in, y, right - x_in, INPUT_H);
            self.search_input(c, ex, Field::Exclude, "e.g. *.log, **/fixtures", TOGGLE + 6.0, false);
            let toggle = Rect::new(ex.right() - 3.0 - TOGGLE, ex.y + 3.0, TOGGLE, TOGGLE);
            let on = self.search.use_ignore_files;
            let hit = Hit::SearchToggle(SearchToggle::UseIgnoreFiles);
            if on {
                c.bordered(toggle, self.color("inputOption.activeBackground"), self.color("inputOption.activeBorder"), 1.0, 3.0);
            } else if self.hovered(hit) {
                c.fill_rounded(toggle, self.color("inputOption.hoverBackground"), 3.0);
            }
            c.icon_in(&icons::GEAR, toggle, 14.0, if on { self.color("inputOption.activeForeground") } else { fg });
            self.hits.push((toggle, hit));
            y += INPUT_H + 6.0;
        }

        // Status line.
        let status = TextStyle::ui(UI, dim);
        let sv = &self.search;
        let (total, files) = (sv.total_matches(), sv.results.len());
        let msg = if self.tree.is_none() {
            Some(("Open a folder to search its files.".to_string(), dim))
        } else if let (Some(err), None) = (&sv.error, &sv.searched) {
            Some((err.clone(), self.color("errorForeground")))
        } else if sv.running.is_some() && total == 0 {
            Some(("Searching…".into(), dim))
        } else if sv.searched.is_some() && total == 0 && sv.running.is_none() {
            Some(("No results found. Review your settings for configured exclusions and check your gitignore files.".into(), dim))
        } else if total > 0 {
            let s = |n: usize| if n == 1 { "" } else { "s" };
            let mut m = format!("{total} result{} in {files} file{}", s(total), s(files));
            if let Some(note) = &sv.error {
                m = format!("{m}. {note}");
            }
            Some((m, dim))
        } else {
            None
        };
        if let Some((m, color)) = msg {
            let lines = wrap_words(c, &m, &status.color(color), right - x_in);
            let n = lines.len();
            for (i, line) in lines.into_iter().enumerate() {
                let w = c.text(x_in, y, &line, &status.color(color));
                // "3 results in 2 files - Open in editor".
                if i + 1 == n && total > 0 && sv.running.is_none() {
                    let link = TextStyle::ui(UI, self.color("textLink.foreground"));
                    let sep = " - ";
                    let lw = c.measure("Open in editor", &link);
                    let mut lx = x_in + w + c.measure(sep, &status);
                    let mut ly = y;
                    if lx + lw > right {
                        (lx, ly) = (x_in, y + status.line_height);
                        y += status.line_height;
                    } else {
                        c.text(x_in + w, y, sep, &status.color(color));
                    }
                    c.text(lx, ly, "Open in editor", &link);
                    self.hits.push((Rect::new(lx, ly, lw, status.line_height), Hit::SearchOpenInEditor));
                }
                y += status.line_height;
            }
            y += 4.0;
        }

        let list = Rect::new(body.x, y, body.w, (body.bottom() - y).max(0.0));
        self.draw_search_results(c, list);
    }

    fn draw_search_results(&mut self, c: &mut Canvas, list: Rect) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        let dim = self.color("descriptionForeground");
        let style = TextStyle::ui(UI, fg);
        let small = TextStyle::ui(12.0, dim);
        let highlight = self.color("editor.findMatchHighlightBackground");
        let (removed, inserted) = (self.color("diffEditor.removedTextBackground"), self.color("diffEditor.insertedTextBackground"));
        let replacing = self.search.show_replace && self.search.searched.is_some();
        let replacement = self.search.replace.text.clone();

        // Flatten the tree into rows: (file, match).
        let mut flat: Vec<(usize, Option<usize>)> = Vec::new();
        for (fi, f) in self.search.results.iter().enumerate() {
            flat.push((fi, None));
            if !self.search.collapsed.contains(&f.path) {
                flat.extend((0..f.matches.len()).map(|mi| (fi, Some(mi))));
            }
        }
        let max = (flat.len() as f32 * ROW_H - list.h + ROW_H).max(0.0);
        self.search.scroll = self.search.scroll.min(max);
        let first = (self.search.scroll / ROW_H) as usize;
        let visible = (list.h / ROW_H).ceil() as usize + 1;
        c.push_clip(list);
        self.search.rows.clear();
        self.a11y_list(super::a11y::SEARCH_LIST, Some(super::a11y::SIDEBAR), "Search Results", list);
        let mut hits = Vec::new();
        for (row, &(fi, mi)) in flat.iter().enumerate().skip(first).take(visible) {
            let y = list.y + row as f32 * ROW_H - self.search.scroll;
            let rr = Rect::new(list.x, y, list.w, ROW_H);
            if self.hover_hit == Some(Hit::SearchRow(row)) {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let file = &self.search.results[fi];
            let read = match mi {
                None => {
                    let collapsed = self.search.collapsed.contains(&file.path);
                    let chevron = if collapsed { &icons::CHEVRON_RIGHT } else { &icons::CHEVRON_DOWN };
                    c.icon(chevron, rr.x + 8.0, y + 3.0, 16.0, fg);
                    c.icon(&icons::FILE, rr.x + 26.0, y + 3.0, 16.0, super::file_color(&file.path));
                    let name = file.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let badge_w = 30.0;
                    c.push_clip(Rect::new(rr.x, y, rr.w - badge_w - 8.0, ROW_H));
                    let nw = c.text_in(Rect::new(rr.x + 48.0, y, rr.w, ROW_H), &name, &style);
                    let dir = file.path.parent().map(|p| self.display_path(p)).unwrap_or_default();
                    c.text_in(Rect::new(rr.x + 54.0 + nw, y, rr.w, ROW_H), &dir, &small);
                    c.pop_clip();
                    let n = file.matches.len();
                    let bw = self.badge_width(c, n);
                    self.badge(c, rr.right() - bw - 10.0, y + 3.0, n);
                    let state = if collapsed { "collapsed" } else { "expanded" };
                    format!("{name}, {dir}, {n} {}, {state}", if n == 1 { "result" } else { "results" })
                }
                Some(mi) => {
                    let m = &file.matches[mi];
                    let (before, hit_text, after) = (
                        &m.preview[..m.preview_start],
                        &m.preview[m.preview_start..m.preview_end],
                        &m.preview[m.preview_end..],
                    );
                    let mut x = rr.x + 40.0;
                    c.push_clip(Rect::new(rr.x, y, rr.w - 4.0, ROW_H));
                    x += c.text_in(Rect::new(x, y, 2000.0, ROW_H), before, &style);
                    let hw = c.measure(hit_text, &style);
                    if replacing {
                        // Show the change: old text struck through, new text after it.
                        c.fill(Rect::new(x, y + 3.0, hw, ROW_H - 6.0), removed);
                        c.text_in(Rect::new(x, y, hw + 2.0, ROW_H), hit_text, &style);
                        c.fill(Rect::new(x, y + ROW_H / 2.0, hw, 1.0), fg);
                        x += hw;
                        let rw = c.measure(&replacement, &style);
                        c.fill(Rect::new(x, y + 3.0, rw, ROW_H - 6.0), inserted);
                        x += c.text_in(Rect::new(x, y, rw + 2.0, ROW_H), &replacement, &style);
                    } else {
                        c.fill(Rect::new(x, y + 3.0, hw, ROW_H - 6.0), highlight);
                        x += c.text_in(Rect::new(x, y, hw + 2.0, ROW_H), hit_text, &style);
                    }
                    c.text_in(Rect::new(x, y, 2000.0, ROW_H), after, &style);
                    c.pop_clip();
                    format!("{}, line {}", m.preview.trim(), m.line + 1)
                }
            };
            self.search.rows.push((row, fi, mi));
            hits.push((rr, Hit::SearchRow(row)));
            self.a11y_item(super::a11y::SEARCH_LIST, row, read, rr.intersect(&list), false);
        }
        c.pop_clip();
        self.hits.extend(hits);
    }

    fn badge_width(&self, c: &mut Canvas, n: usize) -> f32 {
        let style = TextStyle::ui(SMALL, Color::TRANSPARENT);
        (c.measure(&n.to_string(), &style) + 10.0).max(18.0)
    }
}

/// Greedy word wrap for status messages.
fn wrap_words(c: &mut Canvas, text: &str, style: &TextStyle, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if !line.is_empty() && c.measure(&candidate, style) > width {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
