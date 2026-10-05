//! Testing in the workbench: the Testing view (the Test Explorer tree with
//! each test's state, a filter and a results summary), run/state icons in the editor's glyph
//! margin, failure messages after the failing lines, the Test Results panel (the runs'
//! output) and the "Test:" commands.
//!
//! The tests come from `crate::testing`'s providers; everything here goes through the
//! `TestProvider` trait, so a new provider needs nothing in this file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use render::{Canvas, Color, Rect, TextStyle};

use super::{Focus, Hit, View, Workbench, PANEL_TEST_RESULTS, ROW_H, SMALL, UI};
use crate::commands::Command;
use crate::icons;
use crate::input::{Key as K, KeyInput};
use crate::servers::ServerKey;
use crate::testing::{self, Key, ServerEvent, TestCx, TestEvent, TestProvider, TestState};
use crate::widgets::TextField;

/// Lines of output kept for Test Results.
const MAX_OUTPUT: usize = 20_000;
const OUTPUT_LINE_H: f32 = 18.0;
const FILTER_H: f32 = 34.0;

/// What can be done to a test (row buttons, context menus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TestAction {
    Run,
    Debug,
    GoTo,
    Reveal,
}

/// The view's title buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TestingButton {
    Refresh,
    RunAll,
    Cancel,
    CollapseAll,
}

#[derive(Default)]
pub(super) struct Testing {
    pub providers: Vec<Box<dyn TestProvider>>,
    /// The workspace folder each provider works in.
    pub roots: Vec<PathBuf>,
    pub tree: testing::TestTree,
    /// Providers with a run in progress.
    running: Vec<usize>,
    /// When the latest run started, and ended.
    run_time: Option<(Instant, Option<Instant>)>,
    /// What the last run included (empty: everything).
    last_run: Option<Vec<Key>>,
    pub output: Vec<String>,
    /// Test Results' scroll, in lines up from the end (0 follows new output).
    pub output_scroll: f32,
    expanded: HashSet<Key>,
    /// Top-level items seen so far (each starts expanded once).
    seen: HashSet<Key>,
    pub selected: Option<Key>,
    pub scroll: f32,
    /// The rows drawn last: (item, depth).
    rows: Vec<(Key, usize)>,
    body: Rect,
    pub filter: TextField,
    /// The user has asked for tests (opened the view, ran a command): providers may start
    /// what finding them needs.
    wanted: bool,
}

impl Testing {
    pub fn new(providers: Vec<(PathBuf, Box<dyn TestProvider>)>) -> Self {
        let (roots, providers) = providers.into_iter().unzip();
        Testing { providers, roots, ..Default::default() }
    }

    /// Whether this folder has tests to show (a provider applies to it).
    pub fn active(&self) -> bool {
        !self.providers.is_empty()
    }

    pub fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// The rows to draw: expanded items, depth first; with a filter, the matching items and
    /// their ancestors (all expanded).
    fn build_rows(&mut self) {
        let query = self.filter.text.trim().to_lowercase();
        let mut rows = Vec::new();
        let roots = self.tree.roots().to_vec();
        for k in roots {
            self.push_rows(&k, 0, &query, &mut rows);
        }
        self.rows = rows;
    }

    fn push_rows(&self, key: &Key, depth: usize, query: &str, rows: &mut Vec<(Key, usize)>) -> bool {
        let Some(item) = self.tree.item(key) else { return false };
        let at = rows.len();
        rows.push((key.clone(), depth));
        let own_match = query.is_empty() || item.label.to_lowercase().contains(query);
        let open = self.expanded.contains(key) || !query.is_empty();
        let mut child_match = false;
        if open {
            for c in self.tree.children(key) {
                // A matching group shows all its children.
                let q = if own_match && !query.is_empty() { "" } else { query };
                child_match |= self.push_rows(c, depth + 1, q, rows);
            }
        }
        if !own_match && !child_match {
            rows.truncate(at);
            return false;
        }
        true
    }

    fn has_children(&self, key: &Key) -> bool {
        !self.tree.children(key).is_empty() || self.tree.item(key).is_some_and(|it| it.has_children)
    }

    fn max_scroll(&self) -> f32 {
        (self.rows.len() as f32 * ROW_H - self.body.h).max(0.0)
    }

    /// "3/4 tests passed (75.0%)", the summary of a run.
    fn summary(&self) -> Option<String> {
        let (started, ended) = self.run_time?;
        let (passed, failed, skipped, all) = self.tree.counts();
        let done = passed + failed + skipped;
        let mut percent = if done == 0 { 0.0 } else { passed as f64 / done as f64 * 100.0 };
        if failed > 0 {
            percent = percent.min(99.9);
        }
        let mut text = if skipped == 0 {
            format!("{passed}/{all} tests passed ({percent:.1}%)")
        } else {
            format!("{passed}/{all} tests passed ({percent:.1}%, {skipped} skipped)")
        };
        match ended {
            None => text = format!("Running tests, {text}"),
            Some(end) => text.push_str(&format!(" in {}", duration_text(end - started))),
        }
        Some(text)
    }
}

/// "12ms", "1.4s".
fn duration_text(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// A line of output as colored runs (SGR colors and bold; other escapes dropped).
fn ansi_runs(line: &str, palette: &[Color; 16], fg: Color) -> Vec<(String, Color, bool)> {
    let mut runs: Vec<(String, Color, bool)> = Vec::new();
    let (mut color, mut bold) = (fg, false);
    let mut chars = line.chars().peekable();
    let mut text = String::new();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if c != '\r' {
                text.push(c);
            }
            continue;
        }
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut params = String::new();
        let mut end = ' ';
        for c in chars.by_ref() {
            if ('@'..='~').contains(&c) {
                end = c;
                break;
            }
            params.push(c);
        }
        if end != 'm' {
            continue;
        }
        if !text.is_empty() {
            runs.push((std::mem::take(&mut text), color, bold));
        }
        let codes: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => (color, bold) = (fg, false),
                1 => bold = true,
                22 => bold = false,
                39 => color = fg,
                n @ 30..=37 => color = palette[(n - 30) as usize],
                n @ 90..=97 => color = palette[(n - 90 + 8) as usize],
                38 if codes.get(i + 1) == Some(&5) => {
                    if let Some(&n) = codes.get(i + 2) {
                        let n = n.min(255) as u8;
                        color = if n < 16 { palette[n as usize] } else { super::terminal_view::xterm_256(n) };
                    }
                    i += 2;
                }
                _ => {}
            }
            i += 1;
        }
    }
    if !text.is_empty() {
        runs.push((text, color, bold));
    }
    runs
}

impl Workbench {
    /// The workspace's folders changed: their test providers, no tests yet.
    pub(super) fn testing_open_folders(&mut self, roots: &[PathBuf]) {
        let waker = self.waker.clone();
        let providers = roots.iter().flat_map(|r| testing::providers_for(r, &waker).into_iter().map(move |p| (r.clone(), p))).collect();
        self.testing = Testing::new(providers);
        if !self.testing.active() && self.view == View::Testing {
            self.view = View::Explorer;
        }
    }

    /// Lets providers work and applies what they report. Called every frame.
    pub(super) fn testing_tick(&mut self) {
        if !self.testing.active() {
            return;
        }
        self.testing.wanted |= self.sidebar_visible && self.view == View::Testing;
        let wanted = self.testing.wanted;
        for p in 0..self.testing.providers.len() {
            let root = self.testing.roots[p].clone();
            let mut cx = TestCx { lsp: &mut self.lsp, root: &root };
            let events = self.testing.providers[p].tick(&mut cx, wanted);
            self.apply_test_events(p, events);
        }
    }

    /// Files changed on disk: providers that scan files look again.
    pub(super) fn testing_files_changed(&mut self, paths: &[PathBuf]) {
        for p in &mut self.testing.providers {
            p.files_changed(paths);
        }
    }

    /// A language server's answer or notification that the editor core doesn't handle.
    pub(super) fn testing_server_event(&mut self, key: &ServerKey, event: ServerEvent) {
        for p in 0..self.testing.providers.len() {
            let root = self.testing.roots[p].clone();
            let mut cx = TestCx { lsp: &mut self.lsp, root: &root };
            let ev = match &event {
                ServerEvent::Response { id, result } => ServerEvent::Response { id: *id, result },
                ServerEvent::Notification { method, params } => ServerEvent::Notification { method, params },
            };
            let events = self.testing.providers[p].server_event(&mut cx, key, ev);
            self.apply_test_events(p, events);
        }
    }

    fn apply_test_events(&mut self, p: usize, events: Vec<TestEvent>) {
        for ev in events {
            match ev {
                TestEvent::Discovered { replace, items } => {
                    self.testing.tree.apply(p, replace, items);
                    let roots = self.testing.tree.roots().to_vec();
                    for k in roots {
                        if self.testing.seen.insert(k.clone()) {
                            self.testing.expanded.insert(k);
                        }
                    }
                }
                TestEvent::State { id, state, message } => {
                    self.testing.tree.set_state((p, id), state, message);
                    if state.is_failure() && self.settings.string("testing.openTesting") == "openOnTestFailure" {
                        self.show_test_results();
                    }
                }
                TestEvent::Output(line) => {
                    let t = &mut self.testing;
                    t.output.push(line);
                    if t.output.len() > MAX_OUTPUT {
                        t.output.drain(..MAX_OUTPUT / 10);
                    }
                    if t.output_scroll > 0.0 {
                        t.output_scroll += 1.0;
                    }
                }
                TestEvent::RunEnded => {
                    let t = &mut self.testing;
                    if t.running.contains(&p) {
                        t.running.retain(|&q| q != p);
                        if t.running.is_empty() {
                            t.tree.end_run();
                            if let Some((_, end)) = &mut t.run_time {
                                *end = Some(Instant::now());
                            }
                        }
                    }
                }
                TestEvent::Error(e) => {
                    self.testing.output.push(e.clone());
                    self.set_status_message(&e);
                }
            }
        }
    }

    // ------------------------------------------------------------------ running

    /// Runs these tests (empty: every test of every provider), like the standard Run Test.
    pub(super) fn run_tests(&mut self, keys: Vec<Key>) {
        self.testing.wanted = true;
        if self.testing.is_running() {
            self.set_status_message("A test run is already in progress.");
            return;
        }
        let providers: Vec<usize> = if keys.is_empty() { (0..self.testing.providers.len()).collect() } else { keys.iter().map(|k| k.0).collect::<HashSet<_>>().into_iter().collect() };
        let t = &mut self.testing;
        t.output.clear();
        t.output_scroll = 0.0;
        let mut errors = Vec::new();
        for p in providers {
            let ids: Vec<String> = keys.iter().filter(|k| k.0 == p).map(|k| k.1.clone()).collect();
            // Everything the run includes waits in the queue until the provider reports on it.
            let queued: Vec<Key> = if ids.is_empty() {
                t.tree.keys().filter(|k| k.0 == p && t.tree.item(k).is_some_and(|it| !it.has_children)).cloned().collect()
            } else {
                keys.iter().filter(|k| k.0 == p).flat_map(|k| t.tree.tests_under(k)).collect()
            };
            let root = t.roots[p].clone();
            let mut cx = TestCx { lsp: &mut self.lsp, root: &root };
            match t.providers[p].run(&mut cx, &ids) {
                Ok(()) => {
                    for k in queued {
                        t.tree.set_state(k, TestState::Queued, None);
                    }
                    t.running.push(p);
                }
                Err(e) => errors.push(format!("{}: {e}", t.providers[p].name())),
            }
        }
        if !self.testing.running.is_empty() {
            self.testing.run_time = Some((Instant::now(), None));
            self.testing.last_run = Some(keys);
            match self.settings.string("testing.openTesting").as_str() {
                "openOnTestStart" => self.show_test_results(),
                "openExplorerOnTestStart" => self.show_view(View::Testing),
                _ => {}
            }
        }
        if let Some(e) = errors.first() {
            self.set_status_message(e);
        }
    }

    /// Debugs test `key` (the provider says how: usually build it, then launch it).
    pub(super) fn debug_test(&mut self, key: &Key) {
        self.testing.wanted = true;
        let plan = self.testing.providers.get(key.0).and_then(|p| p.debug(&key.1));
        match plan {
            Some(plan) => self.start_debug_plan(plan),
            None => self.set_status_message("This test can't be debugged."),
        }
    }

    pub(super) fn cancel_tests(&mut self) {
        let running = std::mem::take(&mut self.testing.running);
        for p in running {
            let root = self.testing.roots[p].clone();
            let mut cx = TestCx { lsp: &mut self.lsp, root: &root };
            self.testing.providers[p].cancel(&mut cx);
        }
        self.testing.tree.end_run();
        if let Some((_, end)) = &mut self.testing.run_time {
            *end = Some(Instant::now());
        }
    }

    fn refresh_tests(&mut self) {
        self.testing.wanted = true;
        for p in 0..self.testing.providers.len() {
            let root = self.testing.roots[p].clone();
            let mut cx = TestCx { lsp: &mut self.lsp, root: &root };
            self.testing.providers[p].discover(&mut cx, None);
        }
    }

    /// Opens the Test Results panel.
    pub(super) fn show_test_results(&mut self) {
        self.panel_visible = true;
        self.panel_tab = PANEL_TEST_RESULTS;
    }

    /// The test at the cursor: the closest item defined at or above the cursor's line in the
    /// active file (a test rather than its module when both are on the line).
    fn test_at_cursor(&self) -> Option<Key> {
        let ed = self.active_editor()?;
        let path = self.docs[ed.doc].as_ref()?.buffer.path()?;
        let line = ed.sel.head.line;
        self.testing
            .tree
            .in_file(path)
            .filter(|(_, it)| it.line.is_some_and(|l| l <= line))
            .max_by_key(|(_, it)| (it.line, !it.has_children))
            .map(|(k, _)| k.clone())
    }

    /// The outermost items defined in the active file.
    fn tests_in_active_file(&self) -> Vec<Key> {
        let Some(path) = self.active_doc().and_then(|d| d.buffer.path()) else { return Vec::new() };
        let tree = &self.testing.tree;
        tree.in_file(path)
            .filter(|(k, it)| !it.parent.as_ref().and_then(|p| tree.item(&(k.0, p.clone()))).is_some_and(|parent| parent.path.as_deref() == Some(path)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// The "Test:" commands.
    pub(super) fn test_command(&mut self, cmd: Command) {
        if !self.testing.active() {
            self.set_status_message("No test provider is available for this folder.");
            return;
        }
        self.testing.wanted = true;
        match cmd {
            Command::ShowTesting => self.show_view(View::Testing),
            Command::TestingRunAll => self.run_tests(Vec::new()),
            Command::TestingRunAtCursor | Command::TestingDebugAtCursor => match self.test_at_cursor() {
                Some(k) if cmd == Command::TestingDebugAtCursor => self.debug_test(&k),
                Some(k) => self.run_tests(vec![k]),
                None => self.set_status_message("No test found at the cursor."),
            },
            Command::TestingRunCurrentFile => {
                let keys = self.tests_in_active_file();
                if keys.is_empty() {
                    self.set_status_message("No tests found in this file.");
                } else {
                    self.run_tests(keys);
                }
            }
            Command::TestingReRunLastRun => match self.testing.last_run.clone() {
                Some(keys) => self.run_tests(keys),
                None => self.set_status_message("No test run to rerun."),
            },
            Command::TestingReRunFailed => {
                let failed = self.testing.tree.failed();
                if failed.is_empty() {
                    self.set_status_message("No failed tests to rerun.");
                } else {
                    self.run_tests(failed);
                }
            }
            Command::TestingCancel => self.cancel_tests(),
            Command::TestingRefresh => self.refresh_tests(),
            Command::TestingShowOutput => self.show_test_results(),
            Command::TestingClearResults => {
                let t = &mut self.testing;
                let keys: Vec<Key> = t.tree.keys().cloned().collect();
                for k in keys {
                    t.tree.set_state(k, TestState::Unset, None);
                }
                t.run_time = None;
                t.output.clear();
            }
            Command::TestingCollapseAll => self.testing.expanded.clear(),
            _ => {}
        }
    }

    pub(super) fn test_popup(&mut self, key: Key, action: TestAction) {
        match action {
            TestAction::Run => self.run_tests(vec![key]),
            TestAction::Debug => self.debug_test(&key),
            TestAction::GoTo => self.go_to_test(&key),
            TestAction::Reveal => {
                self.show_view(View::Testing);
                self.reveal_test(&key);
            }
        }
    }

    /// Opens the test's definition (or where it failed).
    fn go_to_test(&mut self, key: &Key) {
        let failure = self.testing.tree.result(key).filter(|r| r.state.is_failure()).and_then(|r| r.message.as_ref()).and_then(|m| m.location.clone());
        let at = failure.filter(|(p, _)| p.is_file()).or_else(|| {
            let it = self.testing.tree.item(key)?;
            Some((it.path.clone()?, it.line?))
        });
        if let Some((path, line)) = at {
            self.goto_location(&path, text::Pos::new(line, 0));
        }
    }

    /// Expands the test's ancestors, selects it and scrolls to it.
    fn reveal_test(&mut self, key: &Key) {
        let mut k = key.clone();
        while let Some(parent) = self.testing.tree.item(&k).and_then(|it| it.parent.clone()) {
            k = (k.0, parent);
            self.testing.expanded.insert(k.clone());
        }
        self.testing.filter.set_text("");
        self.testing.selected = Some(key.clone());
        self.testing.build_rows();
        if let Some(i) = self.testing.rows.iter().position(|(r, _)| r == key) {
            let t = &mut self.testing;
            let top = i as f32 * ROW_H;
            if top < t.scroll || top + ROW_H > t.scroll + t.body.h {
                t.scroll = (top - (t.body.h - ROW_H) / 2.0).max(0.0);
            }
        }
    }

    // ------------------------------------------------------------------ the editor

    /// The test to run from the gutter at `line` of `path` (a test over its module).
    fn test_on_line(&self, path: &Path, line: usize) -> Option<Key> {
        self.testing.tree.in_file(path).filter(|(_, it)| it.line == Some(line)).max_by_key(|(_, it)| !it.has_children).map(|(k, _)| k.clone())
    }

    /// Glyph margin icons for `path`: (line, state).
    pub(super) fn test_marks(&mut self, path: Option<&Path>) -> Vec<(usize, TestState)> {
        let Some(path) = path else { return Vec::new() };
        if !self.testing.active() || !self.settings.bool("testing.gutterEnabled") {
            return Vec::new();
        }
        let keys: Vec<(Key, usize)> = self.testing.tree.in_file(path).filter_map(|(k, it)| Some((k.clone(), it.line?))).collect();
        let mut marks: Vec<(usize, TestState)> = keys.into_iter().map(|(k, line)| (line, self.testing.tree.shown(&k))).collect();
        // Several tests on a line (subtests): the most important state shows.
        marks.sort_by_key(|m| (m.0, std::cmp::Reverse(m.1.priority())));
        marks.dedup_by_key(|m| m.0);
        marks
    }

    /// Failure messages to show after their lines in `path`.
    pub(super) fn test_messages(&self, path: Option<&Path>) -> Vec<(usize, String)> {
        path.map(|p| self.testing.tree.messages_in(p)).unwrap_or_default()
    }

    /// A click in the glyph margin on a test's line runs it (`testing.defaultGutterClickAction`).
    /// False when there's no test there (the click sets a breakpoint).
    pub(super) fn testing_click_glyph(&mut self, path: &Path, line: usize, x: f32, y: f32) -> bool {
        if !self.settings.bool("testing.gutterEnabled") || self.debug.breakpoints.get(path).is_some_and(|b| b.iter().any(|b| b.line == line)) {
            return false;
        }
        let Some(key) = self.test_on_line(path, line) else { return false };
        match self.settings.string("testing.defaultGutterClickAction").as_str() {
            "debug" => self.debug_test(&key),
            "contextMenu" => self.test_glyph_menu(path.to_path_buf(), line, x, y),
            _ => self.run_tests(vec![key]),
        }
        true
    }

    /// The glyph margin's context menu: the line's test actions, then the breakpoint ones.
    pub(super) fn test_glyph_menu(&mut self, path: PathBuf, line: usize, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        let Some(key) = self.test_on_line(&path, line).filter(|_| self.settings.bool("testing.gutterEnabled")) else {
            return self.breakpoint_menu(path, line, x, y);
        };
        let debuggable = self.testing.tree.item(&key).is_some_and(|it| it.debuggable);
        let item = |label: &str, enabled: bool| PopupItem::Item { label: label.into(), enabled, checked: None };
        let act = |a: TestAction| PopupAction::Test(key.clone(), a);
        let mut entries = vec![
            (item("Run Test", true), act(TestAction::Run)),
            (item("Debug Test", debuggable), act(TestAction::Debug)),
            (PopupItem::Separator, PopupAction::None),
            (item("Reveal in Test Explorer", true), act(TestAction::Reveal)),
            (PopupItem::Separator, PopupAction::None),
        ];
        let bp = self.debug.breakpoints.get(&path).is_some_and(|b| b.iter().any(|b| b.line == line));
        let bp_item = if bp { "Remove Breakpoint" } else { "Add Breakpoint" };
        entries.push((item(bp_item, true), PopupAction::Breakpoint(path, line, super::debug::BpMenu::Add)));
        self.show_popup(entries, x, y);
    }

    // ------------------------------------------------------------------ the view

    /// The failed count on the activity bar icon (`testing.countBadge`).
    pub(super) fn testing_badge(&self) -> usize {
        let (passed, failed, skipped, _) = self.testing.tree.counts();
        match self.settings.string("testing.countBadge").as_str() {
            "off" => 0,
            "passed" => passed,
            "skipped" => skipped,
            _ => failed,
        }
    }

    /// Keeps frames coming while a spinner shows.
    pub(super) fn testing_deadline(&self) -> Option<Instant> {
        self.testing.is_running().then(|| Instant::now() + Duration::from_millis(100))
    }

    pub(super) fn draw_testing_header_actions(&mut self, c: &mut Canvas, header: Rect) {
        let fg = self.color("icon.foreground");
        let run_or_cancel = if self.testing.is_running() { (&icons::DEBUG_STOP, TestingButton::Cancel) } else { (&icons::RUN_ALL, TestingButton::RunAll) };
        let buttons = [(&icons::REFRESH, TestingButton::Refresh), run_or_cancel, (&icons::COLLAPSE_ALL, TestingButton::CollapseAll)];
        for (i, (icon, b)) in buttons.into_iter().enumerate() {
            let r = Rect::new(header.right() - 32.0 - (2 - i) as f32 * 26.0, header.y + 6.0, 24.0, 22.0);
            self.icon_button(c, r, icon, Hit::TestingButton(b), fg);
        }
    }

    pub(super) fn testing_button(&mut self, b: TestingButton) {
        match b {
            TestingButton::Refresh => self.test_command(Command::TestingRefresh),
            TestingButton::RunAll => self.test_command(Command::TestingRunAll),
            TestingButton::Cancel => self.test_command(Command::TestingCancel),
            TestingButton::CollapseAll => self.test_command(Command::TestingCollapseAll),
        }
    }

    pub(super) fn draw_testing_view(&mut self, c: &mut Canvas, r: Rect) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        // The filter box.
        let (filter_r, rest) = r.cut_top(FILTER_H);
        let field = Rect::new(filter_r.x + 12.0, filter_r.y + 5.0, filter_r.w - 24.0, 24.0);
        let focused = self.focus == Focus::TestingFilter && self.palette.is_none();
        let border = if focused { self.color("focusBorder") } else { self.color_or("input.border", "input.background") };
        c.bordered(field, self.color("input.background"), border, 1.0, 2.0);
        let caret_on = self.editor_caret_on();
        let (ifg, ph, sel) = (self.color("input.foreground"), self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        c.push_clip(field);
        self.testing.filter.draw(c, Rect::new(field.x + 6.0, field.y, field.w - 30.0, field.h), &TextStyle::ui(UI, ifg), "Filter (e.g. text, !exclude, @tag)", ph, focused, caret_on, sel);
        c.icon_in(&icons::FILTER, Rect::new(field.right() - 24.0, field.y + 1.0, 22.0, 22.0), 14.0, self.color("icon.foreground"));
        c.pop_clip();
        self.hits.push((field, Hit::TestingFilter));

        // The latest run's summary.
        let mut body = rest;
        if let Some(summary) = self.testing.summary() {
            let (line, below) = body.cut_top(24.0);
            body = below;
            let (_, failed, _, _) = self.testing.tree.counts();
            let (icon_r, text_x) = (Rect::new(line.x + 12.0, line.y + 3.0, 16.0, 18.0), line.x + 34.0);
            if self.testing.is_running() {
                icons::draw_spinner(c, icon_r, 14.0, fg);
            } else {
                let state = if failed > 0 { TestState::Failed } else { TestState::Passed };
                let (icon, key) = icons::test_state(state);
                c.icon_in(icon, icon_r, 14.0, self.color(key));
            }
            let style = TextStyle::ui(12.0, self.color("descriptionForeground"));
            c.push_clip(line);
            c.text_in(Rect::new(text_x, line.y, line.right() - text_x - 8.0, line.h), &summary, &style);
            c.pop_clip();
        }

        self.testing.body = body;
        self.hits.push((body, Hit::TestingBody));
        if self.testing.tree.is_empty() {
            let msg = "No tests have been found in this workspace yet.";
            let style = TextStyle::ui(UI, fg);
            c.push_clip(body);
            let lines = super::intel::wrap(c, msg, &style, body.w - 40.0);
            for (i, l) in lines.iter().enumerate() {
                c.text(body.x + 20.0, body.y + 8.0 + i as f32 * 18.0, l, &style);
            }
            c.pop_clip();
            return;
        }
        self.testing.build_rows();
        self.testing.scroll = self.testing.scroll.clamp(0.0, self.testing.max_scroll());

        let hover = self.hover_hit;
        let focused = self.focus == Focus::Testing && self.palette.is_none();
        let indent = crate::config::get().tree_indent;
        let style = TextStyle::ui(UI, fg);
        let dim = TextStyle::ui(SMALL, self.color("descriptionForeground"));
        let icon_fg = self.color("icon.foreground");
        let first = (self.testing.scroll / ROW_H) as usize;
        let visible = (body.h / ROW_H).ceil() as usize + 1;
        let (mx, my) = self.mouse;
        c.push_clip(body);
        for i in first..(first + visible).min(self.testing.rows.len()) {
            let (key, depth) = self.testing.rows[i].clone();
            let Some(item) = self.testing.tree.item(&key).cloned() else { continue };
            let y = body.y + i as f32 * ROW_H - self.testing.scroll;
            let rr = Rect::new(body.x, y, body.w, ROW_H);
            let hovered = rr.contains(mx, my) && matches!(hover, Some(Hit::TestingRow(_) | Hit::TestingRowAction(..)));
            if self.testing.selected.as_ref() == Some(&key) {
                let key = if focused { "list.activeSelectionBackground" } else { "list.inactiveSelectionBackground" };
                c.fill_rounded(super::row_pill(rr), self.color(key), super::ROW_RADIUS);
            } else if hovered {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let x = rr.x + 8.0 + depth as f32 * indent;
            // The row first: its twistie and buttons are pushed after it, so they win clicks.
            self.hits.push((rr, Hit::TestingRow(i)));
            if self.testing.has_children(&key) {
                let open = self.testing.expanded.contains(&key) || !self.testing.filter.text.trim().is_empty();
                c.icon(if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT }, x, y + 3.0, 16.0, fg);
                self.hits.push((Rect::new(x, y, 18.0, ROW_H), Hit::TestingTwistie(i)));
            }
            let state = self.testing.tree.shown(&key);
            let icon_r = Rect::new(x + 18.0, y + 3.0, 16.0, 16.0);
            if state == TestState::Running {
                icons::draw_spinner(c, icon_r, 14.0, self.color("testing.iconUnset"));
            } else {
                let (icon, ckey) = icons::test_state(state);
                c.icon_in(icon, icon_r, 14.0, self.color(ckey));
            }
            let label_x = x + 40.0;
            let label_w = c.measure(&item.label, &style);
            c.text(label_x, y + ((ROW_H - style.line_height) / 2.0).round(), &item.label, &style);
            // The time the test took, dimmed after the name.
            if let Some(d) = self.testing.tree.result(&key).and_then(|r| r.duration).filter(|_| !item.has_children) {
                let dx = label_x + label_w + 8.0;
                if dx < rr.right() - 60.0 {
                    c.text_in(Rect::new(dx, y, rr.right() - dx - 4.0, ROW_H), &duration_text(d), &dim);
                }
            }
            // Run, Debug and Go to Test on the hovered or selected row.
            if hovered || self.testing.selected.as_ref() == Some(&key) {
                let mut actions = vec![(&icons::RUN, TestAction::Run)];
                if item.debuggable {
                    actions.push((&icons::RUN_DEBUG, TestAction::Debug));
                }
                if item.path.is_some() {
                    actions.push((&icons::GO_TO_FILE, TestAction::GoTo));
                }
                let n = actions.len();
                let bg = if self.testing.selected.as_ref() == Some(&key) {
                    self.color(if focused { "list.activeSelectionBackground" } else { "list.inactiveSelectionBackground" })
                } else {
                    self.color("list.hoverBackground")
                };
                c.fill(Rect::new(rr.right() - 8.0 - n as f32 * 22.0, y, n as f32 * 22.0 + 8.0, ROW_H), self.color("sideBar.background"));
                c.fill(Rect::new(rr.right() - 8.0 - n as f32 * 22.0, y, n as f32 * 22.0 + 8.0, ROW_H), bg);
                for (j, (icon, a)) in actions.into_iter().enumerate() {
                    let b = Rect::new(rr.right() - 6.0 - (n - j) as f32 * 22.0, y + 1.0, 20.0, 20.0);
                    if self.hovered(Hit::TestingRowAction(i, a)) {
                        c.fill_rounded(b, self.color("toolbar.hoverBackground"), 3.0);
                    }
                    c.icon_in(icon, b, 16.0, icon_fg);
                    self.hits.push((b, Hit::TestingRowAction(i, a)));
                }
            }
        }
        c.pop_clip();
    }

    pub(super) fn click_testing_row(&mut self, i: usize, count: u32) {
        let Some((key, _)) = self.testing.rows.get(i).cloned() else { return };
        self.focus = Focus::Testing;
        self.testing.selected = Some(key.clone());
        // Like the trees, a click opens or closes a group; a double-click on a test
        // goes to it.
        if self.testing.has_children(&key) {
            if count == 1 {
                self.toggle_test_expanded(&key);
            }
        } else if count >= 2 {
            self.go_to_test(&key);
        }
    }

    pub(super) fn click_testing_twistie(&mut self, i: usize, count: u32) {
        let Some((key, _)) = self.testing.rows.get(i).cloned() else { return };
        self.focus = Focus::Testing;
        self.testing.selected = Some(key.clone());
        // The second click of a double-click doesn't undo the first.
        if count == 1 {
            self.toggle_test_expanded(&key);
        }
    }

    fn toggle_test_expanded(&mut self, key: &Key) {
        if !self.testing.expanded.remove(key) {
            self.testing.expanded.insert(key.clone());
            // Children not found yet: ask for them.
            if self.testing.tree.children(key).is_empty() && self.testing.tree.item(key).is_some_and(|it| it.has_children) {
                let root = self.testing.roots[key.0].clone();
            let mut cx = TestCx { lsp: &mut self.lsp, root: &root };
                self.testing.providers[key.0].discover(&mut cx, Some(&key.1));
            }
        }
    }

    pub(super) fn testing_row_action(&mut self, i: usize, action: TestAction) {
        if let Some((key, _)) = self.testing.rows.get(i).cloned() {
            self.testing.selected = Some(key.clone());
            self.test_popup(key, action);
        }
    }

    /// The context menu of row `i`.
    pub(super) fn testing_row_menu(&mut self, i: usize, x: f32, y: f32) {
        use super::preferences::PopupAction;
        use super::PopupItem;
        let Some((key, _)) = self.testing.rows.get(i).cloned() else { return };
        self.testing.selected = Some(key.clone());
        let Some(item) = self.testing.tree.item(&key).cloned() else { return };
        let entry = |label: &str, enabled: bool, a: TestAction| (PopupItem::Item { label: label.into(), enabled, checked: None }, PopupAction::Test(key.clone(), a));
        let entries = vec![
            entry("Run Test", true, TestAction::Run),
            entry("Debug Test", item.debuggable, TestAction::Debug),
            (PopupItem::Separator, PopupAction::None),
            entry("Go to Test", item.path.is_some(), TestAction::GoTo),
        ];
        self.show_popup(entries, x, y);
    }

    pub(super) fn scroll_testing(&mut self, dy: f32) {
        self.testing.scroll = (self.testing.scroll - dy).clamp(0.0, self.testing.max_scroll());
    }

    /// Keys for the tree: arrows move and open/close, Enter goes to the test, ⌘Enter runs it.
    pub(super) fn testing_key(&mut self, k: &KeyInput) {
        let t = &mut self.testing;
        let n = t.rows.len();
        let cur = t.selected.as_ref().and_then(|s| t.rows.iter().position(|(r, _)| r == s));
        let select = |t: &mut Testing, i: usize| {
            t.selected = t.rows.get(i).map(|r| r.0.clone());
            let top = i as f32 * ROW_H;
            if top < t.scroll {
                t.scroll = top;
            } else if top + ROW_H > t.scroll + t.body.h {
                t.scroll = top + ROW_H - t.body.h;
            }
        };
        match k.key {
            K::Down if n > 0 => select(t, cur.map_or(0, |i| (i + 1).min(n - 1))),
            K::Up if n > 0 => select(t, cur.map_or(0, |i| i.saturating_sub(1))),
            K::Home if n > 0 => select(t, 0),
            K::End if n > 0 => select(t, n - 1),
            K::Right | K::Left => {
                let Some(i) = cur else { return };
                let (key, depth) = t.rows[i].clone();
                let open = t.expanded.contains(&key);
                if self.testing.has_children(&key) && open == (k.key == K::Left) {
                    self.toggle_test_expanded(&key);
                } else if k.key == K::Left {
                    // To the parent.
                    if let Some(p) = (0..i).rev().find(|&j| self.testing.rows[j].1 < depth) {
                        select(&mut self.testing, p);
                    }
                } else if open && i + 1 < n {
                    select(&mut self.testing, i + 1);
                }
            }
            K::Enter => {
                if let Some(key) = t.selected.clone() {
                    if k.cmd {
                        self.run_tests(vec![key]);
                    } else {
                        self.go_to_test(&key);
                    }
                }
            }
            K::Escape => self.focus = Focus::Editor,
            _ => {}
        }
    }

    pub(super) fn testing_filter_key(&mut self, k: &KeyInput) {
        match k.key {
            K::Escape if !self.testing.filter.text.is_empty() => self.testing.filter.set_text(""),
            K::Escape => self.focus = Focus::Editor,
            K::Down => self.focus = Focus::Testing,
            _ => {
                self.testing.filter.key(k);
                self.testing.scroll = 0.0;
            }
        }
    }

    pub(super) fn testing_filter_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        let f = &mut self.testing.filter;
        if all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.testing.filter.insert(&text);
            }
            return;
        }
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    // ------------------------------------------------------------------ Test Results

    /// The Test Results panel: the latest run's output, with its colors.
    pub(super) fn draw_test_results(&mut self, c: &mut Canvas, body: Rect) {
        let fg = self.color("terminal.foreground");
        c.push_clip(body);
        let t = &self.testing;
        if t.output.is_empty() {
            let style = TextStyle::ui(UI, self.color("foreground"));
            let msg = if t.is_running() { "Running tests..." } else { "No test results yet." };
            c.text(body.x + 20.0, body.y + 4.0, msg, &style);
            c.pop_clip();
            return;
        }
        let palette = super::terminal_view::ansi_palette(&self.theme);
        let n = ((body.h - 8.0) / OUTPUT_LINE_H).floor().max(0.0) as usize;
        let max = t.output.len().saturating_sub(n) as f32;
        let back = self.testing.output_scroll.clamp(0.0, max) as usize;
        self.testing.output_scroll = back as f32;
        let t = &self.testing;
        let end = t.output.len() - back;
        let start = end.saturating_sub(n);
        for (i, line) in t.output[start..end].iter().enumerate() {
            let y = body.y + 4.0 + i as f32 * OUTPUT_LINE_H;
            let mut x = body.x + 20.0;
            for (text, color, bold) in ansi_runs(line, &palette, fg) {
                let mut st = TextStyle::mono(12.0, OUTPUT_LINE_H, color);
                if bold {
                    st = st.weight(700);
                }
                x += c.text(x, y, &text, &st);
            }
        }
        c.pop_clip();
    }

    pub(super) fn scroll_test_results(&mut self, dy: f32) {
        self.testing.output_scroll = (self.testing.output_scroll + dy / OUTPUT_LINE_H).max(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_output() {
        let palette = [Color::rgba8(1, 0, 0, 255); 16];
        let fg = Color::rgba8(9, 9, 9, 255);
        let runs = ansi_runs("\x1b[1m\x1b[92m   Compiling\x1b[0m demo", &palette, fg);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].0, "   Compiling");
        assert!(runs[0].2);
        assert_eq!((runs[1].0.as_str(), runs[1].1, runs[1].2), (" demo", fg, false));
        assert_eq!(duration_text(Duration::from_millis(12)), "12ms");
        assert_eq!(duration_text(Duration::from_millis(1450)), "1.4s");
    }
}
