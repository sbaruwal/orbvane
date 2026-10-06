//! The integrated terminals in the panel: several terminals, split side by side in groups,
//! with a terminal list on the right. Drawing the grid,
//! keyboard encoding, selection, scrollback and each shell's lifecycle.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use render::{Canvas, Color, Rect, TextStyle};
use terminal::{flags, CursorShape, Terminal};

use super::{Focus, Hit, Workbench, PANEL_TERMINAL};
use crate::config;
use crate::icons;
use crate::input::{Key, KeyInput};

const PAD_X: f32 = 20.0;
const PAD_Y: f32 = 4.0;
const SCROLLBAR_W: f32 = 10.0;
/// Width of the terminal list shown when there is more than one terminal.
/// How often terminal titles (the foreground process) are refreshed.
const TITLE_POLL: Duration = Duration::from_secs(1);

/// Terminal cell height (`terminal.integrated.lineHeight` x font size).
fn line_h() -> f32 {
    config::get().terminal_line_height
}

/// Where a terminal's grid was last drawn, for mouse hit-testing.
#[derive(Clone, Copy, Default)]
pub(super) struct TermGeom {
    origin: (f32, f32),
    cw: f32,
    cols: usize,
    rows: usize,
}

/// A selection in absolute (line, col) terminal coordinates (scrollback + screen).
pub(super) type TermSelection = ((usize, usize), (usize, usize));

/// One terminal: a shell (started when first drawn, at the pane's size) and its view state.
pub(super) struct TermInstance {
    term: Option<Terminal>,
    error: Option<String>,
    cwd: PathBuf,
    /// Lines scrolled back from the live screen.
    scroll: usize,
    selection: Option<TermSelection>,
    geom: TermGeom,
    /// The foreground process ("zsh", "cargo"), and when it was last checked.
    title: String,
    title_checked: Option<Instant>,
    /// For a task's terminal: the task's label, and whether its end was reported.
    pub(super) task: Option<String>,
    pub(super) task_done: bool,
}

impl TermInstance {
    fn new(cwd: PathBuf) -> Self {
        Self { term: None, error: None, cwd, scroll: 0, selection: None, geom: TermGeom::default(), title: String::new(), title_checked: None, task: None, task_done: false }
    }

    /// A terminal running a task (started right away, at a default size until drawn).
    pub(super) fn for_task(cwd: PathBuf, label: &str, term: Terminal) -> Self {
        Self { term: Some(term), task: Some(label.to_string()), ..Self::new(cwd) }
    }

    fn title(&self) -> &str {
        if let Some(label) = &self.task {
            return label;
        }
        if self.title.is_empty() { self.term.as_ref().map_or("zsh", |t| t.shell_name.as_str()) } else { &self.title }
    }

    /// Converts a window point to an absolute (line, col).
    fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let t = self.term.as_ref()?;
        let g = self.geom;
        let col = ((x - g.origin.0) / g.cw.max(1.0)).floor().clamp(0.0, (g.cols.max(1) - 1) as f32) as usize;
        let row = ((y - g.origin.1) / line_h()).floor().clamp(0.0, (g.rows.max(1) - 1) as f32) as usize;
        let first = t.lock().term.scrollback_len().saturating_sub(self.scroll);
        Some((first + row, col))
    }
}

/// Terminals shown side by side (a split).
pub(super) struct TermGroup {
    panes: Vec<TermInstance>,
    active: usize,
}

#[derive(Default)]
pub(super) struct Terminals {
    groups: Vec<TermGroup>,
    active: usize,
    /// The pane being drag-selected in (active group).
    selecting: Option<usize>,
}

impl Terminals {
    pub(super) fn count(&self) -> usize {
        self.groups.iter().map(|g| g.panes.len()).sum()
    }

    fn active_instance(&self) -> Option<&TermInstance> {
        let g = self.groups.get(self.active)?;
        g.panes.get(g.active)
    }

    fn active_instance_mut(&mut self) -> Option<&mut TermInstance> {
        let g = self.groups.get_mut(self.active)?;
        g.panes.get_mut(g.active)
    }

    /// The active terminal's shell, if running.
    fn active_term(&self) -> Option<&Terminal> {
        self.active_instance()?.term.as_ref()
    }

    /// Removes a pane (and its group if it was the last pane). The shell hangs up on drop.
    fn remove(&mut self, g: usize, p: usize) {
        let Some(group) = self.groups.get_mut(g) else { return };
        if p >= group.panes.len() {
            return;
        }
        group.panes.remove(p);
        group.active = group.active.min(group.panes.len().saturating_sub(1));
        if group.panes.is_empty() {
            self.groups.remove(g);
            if self.active >= g && self.active > 0 {
                self.active -= 1;
            }
        }
        self.active = self.active.min(self.groups.len().saturating_sub(1));
        self.selecting = None;
    }

    /// Drops all terminals (hanging up their shells).
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Colors for drawing a terminal, looked up once per frame.
struct TermColors {
    fg: Color,
    bg: Color,
    selection: Color,
    cursor: Color,
    dim: Color,
    error: Color,
    scrollbar: Color,
    palette: [Color; 16],
}

impl Workbench {
    fn terminal_cwd(&self) -> PathBuf {
        // The active editor's workspace folder, else the first.
        self.active_doc()
            .and_then(|d| d.buffer.path())
            .and_then(|p| self.folder_of(p))
            .or_else(|| self.folder())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Shows the panel on the Terminal tab and focuses it, creating a terminal if there is none.
    pub(super) fn show_terminal(&mut self) {
        self.panel_visible = true;
        self.panel_tab = PANEL_TERMINAL;
        self.focus = Focus::Terminal;
        if self.terms.groups.is_empty() {
            let cwd = self.terminal_cwd();
            self.terms.groups.push(TermGroup { panes: vec![TermInstance::new(cwd)], active: 0 });
            self.terms.active = 0;
        }
    }

    pub(super) fn toggle_terminal(&mut self) {
        if self.panel_visible && self.panel_tab == PANEL_TERMINAL && self.focus == Focus::Terminal {
            self.panel_visible = false;
            self.focus = Focus::Editor;
        } else {
            self.show_terminal();
        }
    }

    /// "Terminal: Create New Terminal": a new group, in the workspace folder.
    pub(super) fn new_terminal(&mut self) {
        let cwd = self.terminal_cwd();
        self.terms.groups.push(TermGroup { panes: vec![TermInstance::new(cwd)], active: 0 });
        self.terms.active = self.terms.groups.len() - 1;
        self.show_terminal();
    }

    /// A new terminal in `cwd` (Open in Integrated Terminal).
    pub(super) fn new_terminal_in(&mut self, cwd: PathBuf) {
        self.terms.groups.push(TermGroup { panes: vec![TermInstance::new(cwd)], active: 0 });
        self.terms.active = self.terms.groups.len() - 1;
        self.show_terminal();
    }

    /// "Terminal: Split Terminal": a new pane next to the active one, in its directory.
    pub(super) fn split_terminal(&mut self) {
        if self.terms.groups.is_empty() {
            return self.new_terminal();
        }
        let cwd = self.terms.active_term().and_then(Terminal::cwd).unwrap_or_else(|| self.terminal_cwd());
        let group = &mut self.terms.groups[self.terms.active];
        group.active += 1;
        group.panes.insert(group.active, TermInstance::new(cwd));
        self.show_terminal();
    }

    /// "Terminal: Kill the Active Terminal Instance". The panel hides with the last one.
    pub(super) fn kill_terminal(&mut self) {
        let g = self.terms.active;
        let p = self.terms.groups.get(g).map_or(0, |gr| gr.active);
        self.kill_terminal_at(g, p);
    }

    fn kill_terminal_at(&mut self, g: usize, p: usize) {
        self.terms.remove(g, p);
        if self.terms.groups.is_empty() {
            self.panel_visible = false;
            if self.focus == Focus::Terminal {
                self.focus = Focus::Editor;
            }
        }
    }

    /// Next / previous terminal group (`forward`), or pane within the group (`pane`).
    pub(super) fn focus_terminal(&mut self, forward: bool, pane: bool) {
        let t = &mut self.terms;
        if t.groups.is_empty() {
            return;
        }
        let step = |i: usize, n: usize| if forward { (i + 1) % n } else { (i + n - 1) % n };
        if pane {
            let g = &mut t.groups[t.active];
            g.active = step(g.active, g.panes.len());
        } else {
            t.active = step(t.active, t.groups.len());
        }
        self.show_terminal();
    }

    /// Closes terminals whose shell exited cleanly.
    fn close_exited_terminals(&mut self) {
        let mut closed = false;
        for g in (0..self.terms.groups.len()).rev() {
            for p in (0..self.terms.groups[g].panes.len()).rev() {
                // A task's terminal stays open with its output.
                let inst = &self.terms.groups[g].panes[p];
                let done = inst.task.is_none() && inst.term.as_ref().is_some_and(|t| t.has_exited() && t.exit_code() == Some(0));
                if done {
                    self.terms.remove(g, p);
                    closed = true;
                }
            }
        }
        if closed && self.terms.groups.is_empty() {
            self.panel_visible = false;
            if self.focus == Focus::Terminal {
                self.focus = Focus::Editor;
            }
        }
    }

    /// Runs a task's command line in a terminal of its own (replacing the task's previous,
    /// finished terminal) and shows the panel without taking focus.
    pub(super) fn run_task_terminal(&mut self, label: &str, command: &str, cwd: &std::path::Path, env: &[(String, String)]) -> Result<(), String> {
        let running = |i: &TermInstance| i.task.as_deref() == Some(label) && i.term.as_ref().is_some_and(|t| !t.has_exited());
        if self.terms.groups.iter().flat_map(|g| &g.panes).any(running) {
            return Err(format!("The task '{label}' is already active."));
        }
        for g in (0..self.terms.groups.len()).rev() {
            for p in (0..self.terms.groups[g].panes.len()).rev() {
                if self.terms.groups[g].panes[p].task.as_deref() == Some(label) {
                    self.terms.remove(g, p);
                }
            }
        }
        let banner = format!("\x1b[1m*\x1b[0m  Executing task: {command} \n\n");
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let term = Terminal::spawn_shell_command(command, cwd, &env, 80, 24, &banner, self.waker.clone()).map_err(|e| format!("Could not start the task '{label}': {e}"))?;
        self.terms.groups.push(TermGroup { panes: vec![TermInstance::for_task(cwd.to_path_buf(), label, term)], active: 0 });
        self.terms.active = self.terms.groups.len() - 1;
        self.panel_visible = true;
        self.panel_tab = PANEL_TERMINAL;
        Ok(())
    }

    /// Tasks that finished since the last call, with their exit codes. Each gets the
    /// closing message in its terminal.
    pub(super) fn finished_tasks(&mut self) -> Vec<(String, Option<i32>)> {
        let mut out = Vec::new();
        for inst in self.terms.groups.iter_mut().flat_map(|g| g.panes.iter_mut()) {
            let (Some(label), Some(term)) = (&inst.task, &inst.term) else { continue };
            if inst.task_done || !term.has_exited() {
                continue;
            }
            inst.task_done = true;
            let code = term.exit_code();
            let mut msg = String::from("\n");
            if code != Some(0) {
                let code = code.map_or("unknown".to_string(), |c| c.to_string());
                msg.push_str(&format!("\x1b[1m*\x1b[0m  The terminal process terminated with exit code: {code}. \n"));
            }
            msg.push_str("\x1b[1m*\x1b[0m  Terminal will be reused by tasks, press any key to close it. \n");
            term.echo(&msg);
            out.push((label.clone(), code));
        }
        out
    }

    /// Labels of the tasks still running.
    pub(super) fn running_tasks(&self) -> Vec<String> {
        self.terms.groups.iter().flat_map(|g| &g.panes).filter(|i| i.term.as_ref().is_some_and(|t| !t.has_exited())).filter_map(|i| i.task.clone()).collect()
    }

    /// Stops a running task (^C to its process group, then a hang-up if it ignores that).
    pub(super) fn terminate_task_terminal(&mut self, label: &str) {
        for inst in self.terms.groups.iter().flat_map(|g| &g.panes) {
            if inst.task.as_deref() == Some(label) {
                if let Some(t) = inst.term.as_ref().filter(|t| !t.has_exited()) {
                    t.write(b"\x03");
                }
            }
        }
    }

    /// Handles a key while the terminal has focus.
    pub(super) fn terminal_key(&mut self, k: &KeyInput) {
        // ⌘K clears the terminal. (Copy/paste/select all arrive as commands,
        // usually via the Edit menu, and `run` routes them here when the terminal is focused.)
        if k.cmd && !k.ctrl && !k.alt && k.key == Key::Char("k".into()) {
            if let Some(inst) = self.terms.active_instance_mut() {
                if let Some(t) = &inst.term {
                    t.lock().term.clear_scrollback();
                    t.write(b"\x0c");
                }
                inst.scroll = 0;
            }
            return;
        }
        // ⌥⌘← / ⌥⌘→ move between split terminals.
        if k.cmd && k.alt && matches!(k.key, Key::Left | Key::Right) {
            return self.focus_terminal(k.key == Key::Right, true);
        }
        if let Some(cmd) = k.command() {
            self.run(cmd);
            return;
        }
        let Some(inst) = self.terms.active_instance_mut() else { return };
        let Some(term) = &inst.term else { return };
        if term.has_exited() {
            // After the shell fails, any key closes the terminal.
            return self.kill_terminal();
        }
        let app_cursor = term.lock().term.app_cursor_keys;
        if let Some(bytes) = encode_key(k, app_cursor) {
            term.write(&bytes);
            inst.scroll = 0;
            inst.selection = None;
        }
    }

    pub(super) fn terminal_paste(&mut self) {
        let text = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok());
        let Some(inst) = self.terms.active_instance_mut() else { return };
        if let (Some(t), Some(text)) = (&inst.term, text) {
            t.paste(&text);
        }
        inst.scroll = 0;
        inst.selection = None;
    }

    pub(super) fn terminal_select_all(&mut self) {
        let Some(inst) = self.terms.active_instance_mut() else { return };
        if let Some(t) = &inst.term {
            let e = t.lock();
            let last = e.term.scrollback_len() + e.term.rows() - 1;
            let cols = e.term.cols();
            drop(e);
            inst.selection = Some(((0, 0), (last, cols - 1)));
        }
    }

    pub(super) fn copy_terminal_selection(&mut self) {
        let Some(inst) = self.terms.active_instance() else { return };
        let (Some(t), Some((a, b))) = (&inst.term, inst.selection) else { return };
        let text = t.lock().term.text_between(a, b);
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(text);
        }
    }

    pub(super) fn terminal_mouse_down(&mut self, p: usize, x: f32, y: f32) {
        self.focus = Focus::Terminal;
        let t = &mut self.terms;
        let Some(group) = t.groups.get_mut(t.active) else { return };
        group.active = p.min(group.panes.len().saturating_sub(1));
        if let Some(inst) = group.panes.get_mut(p) {
            inst.selection = inst.cell_at(x, y).map(|c| (c, c));
            t.selecting = Some(p);
        }
    }

    /// Whether a drag-selection is in progress (mouse moves go to it).
    pub(super) fn terminal_selecting(&self) -> bool {
        self.terms.selecting.is_some()
    }

    pub(super) fn terminal_mouse_drag(&mut self, x: f32, y: f32) {
        let t = &mut self.terms;
        let Some(p) = t.selecting else { return };
        let Some(inst) = t.groups.get_mut(t.active).and_then(|g| g.panes.get_mut(p)) else { return };
        if let (Some(cell), Some((a, _))) = (inst.cell_at(x, y), inst.selection) {
            inst.selection = Some((a, cell));
        }
    }

    pub(super) fn terminal_mouse_up(&mut self) {
        let t = &mut self.terms;
        let Some(p) = t.selecting.take() else { return };
        // A click without dragging just focuses; don't leave a one-cell selection.
        if let Some(inst) = t.groups.get_mut(t.active).and_then(|g| g.panes.get_mut(p)) {
            if inst.selection.is_some_and(|(a, b)| a == b) {
                inst.selection = None;
            }
        }
    }

    pub(super) fn terminal_scroll(&mut self, p: usize, dy: f32) {
        let t = &mut self.terms;
        let Some(inst) = t.groups.get_mut(t.active).and_then(|g| g.panes.get_mut(p)) else { return };
        let Some(term) = &inst.term else { return };
        let e = term.lock();
        if e.term.is_alt_screen() {
            return; // full-screen programs manage their own scrolling
        }
        let max = e.term.scrollback_len();
        drop(e);
        let lines = (dy / line_h()).round() as isize;
        inst.scroll = (inst.scroll as isize + lines).clamp(0, max as isize) as usize;
    }

    pub(super) fn click_terminal_tab(&mut self, g: usize, p: usize) {
        if g < self.terms.groups.len() {
            self.terms.active = g;
            let group = &mut self.terms.groups[g];
            group.active = p.min(group.panes.len() - 1);
            self.focus = Focus::Terminal;
        }
    }

    pub(super) fn terminal_tab_action(&mut self, g: usize, p: usize, split: bool) {
        if split {
            self.click_terminal_tab(g, p);
            self.split_terminal();
        } else {
            self.kill_terminal_at(g, p);
        }
    }

    /// The next title refresh while terminals are on screen (a program may start or finish
    /// without the panel redrawing otherwise).
    pub(super) fn terminal_deadline(&self) -> Option<Instant> {
        if !self.panel_visible || self.panel_tab != PANEL_TERMINAL {
            return None;
        }
        let g = self.terms.groups.get(self.terms.active)?;
        g.panes.iter().filter_map(|i| i.title_checked).min().map(|t| t + TITLE_POLL)
    }


    // ------------------------------------------------------------------ drawing

    /// Draws the active terminal group (and the terminal list), starting shells as needed.
    pub(super) fn draw_terminal(&mut self, c: &mut Canvas, body: Rect) {
        self.close_exited_terminals();
        if self.terms.groups.is_empty() {
            self.show_terminal();
            if !self.panel_visible {
                return;
            }
        }
        let colors = TermColors {
            fg: self.color("terminal.foreground"),
            bg: self.color_or("terminal.background", "panel.background"),
            selection: self.color("terminal.selectionBackground"),
            cursor: self.color_or("terminalCursor.foreground", "terminal.foreground"),
            dim: self.color("descriptionForeground"),
            error: self.color("editorError.foreground"),
            scrollbar: self.color("scrollbarSlider.background"),
            palette: ansi_palette(&self.theme),
        };
        c.fill(body, colors.bg);
        let panes_rect = body;
        let n = self.terms.groups[self.terms.active].panes.len();
        let border = self.color_or("terminal.border", "panel.border");
        let pane_w = panes_rect.w / n as f32;
        let focused_pane = (self.focus == Focus::Terminal && self.palette.is_none()).then(|| self.terms.groups[self.terms.active].active);
        for p in 0..n {
            let r = Rect::new((panes_rect.x + p as f32 * pane_w).round(), panes_rect.y, pane_w.round(), panes_rect.h);
            if p > 0 {
                c.fill(Rect::new(r.x, r.y, 1.0, r.h), border);
            }
            self.hits.push((r, Hit::TerminalPane(p)));
            self.draw_terminal_pane(c, r, p, focused_pane == Some(p), &colors);
        }
    }

    fn draw_terminal_pane(&mut self, c: &mut Canvas, body: Rect, p: usize, focused: bool, colors: &TermColors) {
        let style = TextStyle::mono(config::get().terminal_font_size, line_h(), colors.fg);
        let cw = c.measure("0000000000", &style) / 10.0;
        let cols = (((body.w - PAD_X - SCROLLBAR_W) / cw).floor() as usize).max(2);
        let rows = (((body.h - PAD_Y) / line_h()).floor() as usize).max(1);
        let origin = (body.x + PAD_X, body.y + PAD_Y);
        let waker = self.waker.clone();
        let g = self.terms.active;
        let inst = &mut self.terms.groups[g].panes[p];
        inst.geom = TermGeom { origin, cw, cols, rows };

        if let Some(err) = &inst.error {
            c.text(origin.0, origin.1, err, &style.color(colors.error));
            return;
        }
        if inst.term.is_none() {
            match Terminal::spawn(&inst.cwd, cols as u16, rows as u16, waker) {
                Ok(t) => inst.term = Some(t),
                Err(e) => inst.error = Some(format!("Could not start the shell: {e}")),
            }
        }
        let Some(term) = &inst.term else { return };
        if inst.title_checked.is_none_or(|t| t.elapsed() >= TITLE_POLL) {
            inst.title_checked = Some(Instant::now());
            // Until the child execs the shell, it still has our name.
            let ours = std::env::current_exe().ok().and_then(|e| e.file_name().map(|n| n.to_string_lossy().into_owned()));
            if let Some(name) = term.process_name().filter(|n| Some(n) != ours.as_ref()) {
                inst.title = name;
            }
        }
        term.resize(cols as u16, rows as u16);
        let exited = term.has_exited();
        let exit_code = term.exit_code();
        let emu = term.lock();
        let t = &emu.term;
        inst.scroll = inst.scroll.min(t.scrollback_len());
        let first_line = t.scrollback_len().saturating_sub(inst.scroll);
        let selection = inst.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });

        c.push_clip(body);
        let mut spans: Vec<(usize, usize, Color)> = Vec::new();
        for r in 0..rows {
            let Some(row) = t.visible_row(r, inst.scroll) else { break };
            let y = origin.1 + r as f32 * line_h();
            let line = first_line + r;
            let mut text = String::with_capacity(row.cells.len());
            spans.clear();
            // Backgrounds are filled per run of same-colored cells (one rect each, so no seams
            // between cells); the text is drawn as one shaped line on top.
            let mut run: Option<(usize, Color)> = None;
            let fill_run = |c: &mut Canvas, run: Option<(usize, Color)>, end: usize| {
                if let Some((start, color)) = run {
                    let x0 = (origin.0 + start as f32 * cw).round();
                    let x1 = (origin.0 + end as f32 * cw).round();
                    c.fill(Rect::new(x0, y, x1 - x0, line_h()), color);
                }
            };
            let mut col = 0;
            while col < row.cells.len() {
                let cell = row.cells[col];
                let (fg, bg) = cell_colors(&cell, &colors.palette, colors.fg);
                let selected = selection.is_some_and(|((la, ca), (lb, cb))| {
                    (line, col) >= (la, ca) && (line, col) <= (lb, cb)
                });
                let x = origin.0 + col as f32 * cw;
                let width = if cell.flags & flags::WIDE != 0 { 2.0 } else { 1.0 };
                let cell_bg = if selected { Some(colors.selection) } else { bg };
                if run.map(|r| r.1) != cell_bg {
                    fill_run(c, run, col);
                    run = cell_bg.map(|color| (col, color));
                }
                if cell.flags & flags::SPACER == 0 {
                    let start = text.len();
                    let ch = if cell.flags & flags::HIDDEN != 0 { ' ' } else { cell.ch };
                    text.push(ch);
                    let fg = if cell.flags & flags::DIM != 0 { fg.with_alpha(0.5) } else { fg };
                    match spans.last_mut() {
                        Some(last) if last.1 == start && last.2 == fg => last.1 = text.len(),
                        _ => spans.push((start, text.len(), fg)),
                    }
                    if cell.flags & flags::UNDERLINE != 0 {
                        c.fill(Rect::new(x, y + line_h() - 2.0, cw * width, 1.0), fg);
                    }
                    if cell.flags & flags::STRIKE != 0 {
                        c.fill(Rect::new(x, y + line_h() / 2.0, cw * width, 1.0), fg);
                    }
                }
                col += 1;
            }
            fill_run(c, run, row.cells.len());
            let trimmed = text.trim_end().len();
            spans.retain(|s| s.0 < trimmed);
            if let Some(last) = spans.last_mut() {
                last.1 = last.1.min(trimmed);
            }
            c.rich_text(origin.0, y, &text[..trimmed], &spans, &style);
        }

        // Cursor (only when viewing the live screen).
        let (cx, cy) = t.cursor();
        if inst.scroll == 0 && t.cursor_visible && !exited && cy < rows {
            let x = origin.0 + cx as f32 * cw;
            let y = origin.1 + cy as f32 * line_h();
            match (focused, t.cursor_shape) {
                (false, _) => c.bordered(Rect::new(x, y, cw, line_h()), Color::TRANSPARENT, colors.cursor, 1.0, 0.0),
                (true, CursorShape::Block) => {
                    c.fill(Rect::new(x, y, cw, line_h()), colors.cursor);
                    let under = t.visible_row(cy, 0).and_then(|r| r.cells.get(cx)).map_or(' ', |c| c.ch);
                    if under != ' ' {
                        c.text(x, y, &under.to_string(), &style.color(colors.bg));
                    }
                }
                (true, CursorShape::Bar) => c.fill(Rect::new(x, y, 2.0, line_h()), colors.cursor),
                (true, CursorShape::Underline) => c.fill(Rect::new(x, y + line_h() - 2.0, cw, 2.0), colors.cursor),
            }
        }

        if exited && inst.task.is_none() {
            // Clean exits close the terminal; this shows for failures (tasks print their own).
            let msg = match exit_code {
                Some(code) => format!("The terminal process terminated with exit code: {code}. Press any key to close the terminal."),
                None => "The terminal process terminated. Press any key to close the terminal.".into(),
            };
            let y = origin.1 + (t.cursor().1 + 1).min(rows.saturating_sub(1)) as f32 * line_h();
            c.text(origin.0, y, &msg, &style.color(colors.dim));
        }

        // Scrollbar when there's scrollback.
        let total = t.scrollback_len() + rows;
        if t.scrollback_len() > 0 && !t.is_alt_screen() {
            let track = Rect::new(body.right() - SCROLLBAR_W, body.y, SCROLLBAR_W, body.h);
            let h = (track.h * rows as f32 / total as f32).max(20.0);
            let top = track.y + (track.h - h) * (first_line as f32 / t.scrollback_len() as f32);
            c.fill(Rect::new(track.x, top, track.w, h), colors.scrollbar);
        }
        c.pop_clip();
    }

    /// The terminal list: one row per terminal, split groups drawn as a tree.
    /// The terminals as chips in the panel's header (in `area`): one per group (split panes
    /// share a chip, their titles joined), the active one filled; × closes its active pane.
    pub(super) fn draw_terminal_chips(&mut self, c: &mut Canvas, area: Rect) {
        let fg = self.color("panelTitle.activeForeground");
        let dim = self.color("panelTitle.inactiveForeground");
        let chips: Vec<(usize, usize, String)> = self
            .terms
            .groups
            .iter()
            .enumerate()
            .map(|(g, group)| (g, group.active, group.panes.iter().map(|p| p.title().to_string()).collect::<Vec<_>>().join(" | ")))
            .collect();
        if chips.is_empty() || area.w < 60.0 {
            return;
        }
        let style = TextStyle::ui(12.0, fg);
        // Chrome around a title: icon, gaps and the close button.
        const CHROME: f32 = 8.0 + 14.0 + 6.0 + 6.0 + 18.0 + 4.0;
        let wanted: Vec<f32> = chips.iter().map(|(_, _, t)| c.measure(t, &style).min(200.0)).collect();
        let room = area.w - 4.0 * chips.len() as f32;
        let total: f32 = wanted.iter().map(|w| w + CHROME).sum();
        // Too many to fit: every title gets the same share.
        let share = if total > room { ((room / chips.len() as f32) - CHROME).max(24.0) } else { f32::MAX };
        c.push_clip(area);
        let mut x = area.x;
        for ((g, p, title), want) in chips.into_iter().zip(wanted) {
            let tw = want.min(share);
            let chip = Rect::new(x, area.y + 7.0, tw + CHROME, area.h - 14.0);
            let active = g == self.terms.active;
            let close = Rect::new(chip.right() - 22.0, chip.y + (chip.h - 18.0) / 2.0, 18.0, 18.0);
            let hovered = self.hovered(Hit::TerminalTab(g, p)) || self.hovered(Hit::TerminalTabAction(g, p, false));
            if active {
                c.fill_rounded(chip, self.color_or("list.inactiveSelectionBackground", "toolbar.hoverBackground"), 6.0);
            } else if hovered {
                c.fill_rounded(chip, self.color("toolbar.hoverBackground"), 6.0);
            }
            let color = if active || hovered { fg } else { dim };
            c.icon(&icons::TERMINAL, chip.x + 8.0, chip.y + (chip.h - 14.0) / 2.0, 14.0, color);
            c.text_fit(Rect::new(chip.x + 28.0, chip.y, tw + 2.0, chip.h), &title, &style.color(color));
            self.hits.push((chip, Hit::TerminalTab(g, p)));
            if active || hovered {
                if self.hovered(Hit::TerminalTabAction(g, p, false)) {
                    c.fill_rounded(close, self.color("toolbar.hoverBackground"), 5.0);
                }
                c.icon_in(&icons::CLOSE, close, 14.0, color);
                self.hits.push((close, Hit::TerminalTabAction(g, p, false)));
            }
            x = chip.right() + 4.0;
        }
        c.pop_clip();
    }
}

/// Foreground and (non-default) background for a cell, applying bold-as-bright and inverse.
fn cell_colors(cell: &terminal::Cell, palette: &[Color; 16], fg_default: Color) -> (Color, Option<Color>) {
    let resolve = |c: terminal::Color, bold: bool| -> Option<Color> {
        match c {
            terminal::Color::Default => None,
            // Bold text in the 8 base colors uses the bright variant, like the standard default.
            terminal::Color::Indexed(i) if i < 8 && bold => Some(palette[i as usize + 8]),
            terminal::Color::Indexed(i) if i < 16 => Some(palette[i as usize]),
            terminal::Color::Indexed(i) => Some(xterm_256(i)),
            terminal::Color::Rgb(r, g, b) => Some(Color::rgba8(r, g, b, 255)),
        }
    };
    let bold = cell.flags & flags::BOLD != 0;
    let fg = resolve(cell.fg, bold);
    let bg = resolve(cell.bg, false);
    if cell.flags & flags::INVERSE != 0 {
        (bg.unwrap_or(Color::hex("#181818").unwrap()), Some(fg.unwrap_or(fg_default)))
    } else {
        (fg.unwrap_or(fg_default), bg)
    }
}

pub(super) fn ansi_palette(theme: &theme::Theme) -> [Color; 16] {
    const NAMES: [&str; 8] = ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "White"];
    std::array::from_fn(|i| {
        let bright = if i >= 8 { "Bright" } else { "" };
        theme.color(&format!("terminal.ansi{bright}{}", NAMES[i % 8]))
    })
}

/// The xterm 256-color palette beyond the 16 ANSI colors.
pub(super) fn xterm_256(i: u8) -> Color {
    if i >= 232 {
        let v = 8 + (i - 232) * 10;
        return Color::rgba8(v, v, v, 255);
    }
    let i = i - 16;
    let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
    Color::rgba8(level(i / 36), level((i / 6) % 6), level(i % 6), 255)
}

/// Encodes a key press as the bytes a terminal program expects (xterm conventions, with
/// the macOS mappings for ⌥/⌘ + arrows and backspace).
pub(super) fn encode_key(k: &KeyInput, app_cursor: bool) -> Option<Vec<u8>> {
    let modifier = 1 + k.shift as u8 + 2 * k.alt as u8 + 4 * k.ctrl as u8;
    let csi = |final_byte: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{final_byte}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_byte}").into_bytes()
        } else {
            format!("\x1b[{final_byte}").into_bytes()
        }
    };
    let tilde = |n: u8| format!("\x1b[{n}~").into_bytes();
    Some(match &k.key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace if k.cmd => vec![0x15],  // delete to line start
        Key::Backspace if k.alt => vec![0x17],  // delete word
        Key::Backspace if k.ctrl => vec![0x08],
        Key::Backspace => vec![0x7f],
        Key::Tab if k.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Left if k.cmd => vec![0x01],
        Key::Right if k.cmd => vec![0x05],
        Key::Left if k.alt && !k.shift => b"\x1bb".to_vec(),
        Key::Right if k.alt && !k.shift => b"\x1bf".to_vec(),
        Key::Up => csi('A'),
        Key::Down => csi('B'),
        Key::Right => csi('C'),
        Key::Left => csi('D'),
        Key::Home => csi('H'),
        Key::End => csi('F'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Delete => tilde(3),
        Key::F(n @ 1..=4) => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
        Key::F(n) => tilde(match n {
            5 => 15,
            6 => 17,
            7 => 18,
            8 => 19,
            9 => 20,
            10 => 21,
            11 => 23,
            12 => 24,
            _ => return None,
        }),
        Key::Space if k.ctrl => vec![0],
        Key::Space => b" ".to_vec(),
        Key::Char(c) if k.ctrl => {
            let b = c.bytes().next()?;
            vec![match b {
                b'a'..=b'z' => b - b'a' + 1,
                b'@' | b'2' | b' ' => 0,
                b'[' | b'3' => 0x1b,
                b'\\' | b'4' => 0x1c,
                b']' | b'5' => 0x1d,
                b'^' | b'6' => 0x1e,
                b'_' | b'-' | b'7' => 0x1f,
                _ => return None,
            }]
        }
        Key::Char(_) if k.cmd => return None,
        Key::Char(_) => k.text.as_ref()?.as_bytes().to_vec(),
        Key::Other => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, text: Option<&str>) -> KeyInput {
        KeyInput { key, text: text.map(String::from), cmd: false, shift: false, alt: false, ctrl: false }
    }

    #[test]
    fn encodes_keys() {
        assert_eq!(encode_key(&key(Key::Char("a".into()), Some("a")), false).unwrap(), b"a");
        assert_eq!(encode_key(&key(Key::Enter, None), false).unwrap(), b"\r");
        assert_eq!(encode_key(&key(Key::Up, None), false).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&key(Key::Up, None), true).unwrap(), b"\x1bOA");
        let ctrl_c = KeyInput { ctrl: true, ..key(Key::Char("c".into()), None) };
        assert_eq!(encode_key(&ctrl_c, false).unwrap(), vec![3]);
        let shift_right = KeyInput { shift: true, ..key(Key::Right, None) };
        assert_eq!(encode_key(&shift_right, false).unwrap(), b"\x1b[1;2C");
        assert_eq!(encode_key(&key(Key::F(5), None), false).unwrap(), b"\x1b[15~");
    }

    #[test]
    fn palette_cube() {
        assert_eq!(xterm_256(16), Color::rgba8(0, 0, 0, 255));
        assert_eq!(xterm_256(231), Color::rgba8(255, 255, 255, 255));
        assert_eq!(xterm_256(196), Color::rgba8(255, 0, 0, 255));
        assert_eq!(xterm_256(244), Color::rgba8(128, 128, 128, 255));
    }
}
