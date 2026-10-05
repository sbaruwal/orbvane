//! Drawing for debugging (`debug.rs` has the state): the Run and Debug view (Variables,
//! Watch, Call Stack and Breakpoints sections, or the welcome text before there's a
//! `launch.json`), the floating debug toolbar, and the Debug Console panel.

use std::path::PathBuf;

use render::{Canvas, Color, Rect, TextStyle};

use super::debug::{DebugPick, State};
use super::sections::{split, MIN_BODY};
use super::{Drag, Focus, Hit, Workbench, ROW_H, SMALL, UI};
use crate::icons;
use crate::input::{Key, KeyInput};

pub(super) const SECTIONS: usize = 4;
const TITLES: [&str; SECTIONS] = ["VARIABLES", "WATCH", "CALL STACK", "BREAKPOINTS"];

/// A row of a section, for clicks.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum DRow {
    /// A scope, variable, watch expression or its child: its tree path and children reference.
    Node { path: String, reference: i64, watch: Option<usize> },
    Thread(i64),
    Frame(usize),
    Breakpoint(PathBuf, usize),
    /// An exception breakpoint (index into `Debug::exception_filters`).
    Exception(usize),
    /// A line of text without actions ("Running").
    Note,
}

/// The toolbar's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolbarButton {
    ContinueOrPause,
    StepOver,
    StepInto,
    StepOut,
    Restart,
    Stop,
}

pub(super) struct DebugViewState {
    pub open: [bool; SECTIONS],
    pub heights: [Option<f32>; SECTIONS],
    pub scroll: [f32; SECTIONS],
    pub rows: [Vec<DRow>; SECTIONS],
    pub heads: [Rect; SECTIONS],
    pub bodies: [Rect; SECTIONS],
    /// The selected row per section (by value, so it survives rebuilding).
    pub selected: Option<(usize, DRow)>,
}

impl Default for DebugViewState {
    fn default() -> Self {
        DebugViewState {
            open: [true; SECTIONS],
            heights: [None; SECTIONS],
            scroll: [0.0; SECTIONS],
            rows: Default::default(),
            heads: [Rect::default(); SECTIONS],
            bodies: [Rect::default(); SECTIONS],
            selected: None,
        }
    }
}

/// A row as drawn: indent, twistie, and colored text runs.
struct Line {
    depth: usize,
    /// Some(open) draws a chevron.
    twistie: Option<bool>,
    runs: Vec<(String, Color)>,
    /// Right-aligned text (a frame's file and line, a breakpoint's line).
    right: String,
    dim: bool,
    row: DRow,
}

/// The color we give a value in the variables tree (`debugTokenExpression.*`).
fn value_color(wb: &Workbench, value: &str) -> Color {
    let v = value.trim();
    let key = if v.starts_with('"') || v.starts_with('\'') {
        "debugTokenExpression.string"
    } else if v == "true" || v == "false" {
        "debugTokenExpression.boolean"
    } else if v.parse::<f64>().is_ok() || v.starts_with("0x") {
        "debugTokenExpression.number"
    } else {
        "debugTokenExpression.value"
    };
    wb.color(key)
}

impl Workbench {
    /// The Run and Debug view's title actions: the configuration to start (▷ name) and the
    /// gear that opens `launch.json`.
    pub(super) fn draw_debug_header_actions(&mut self, c: &mut Canvas, header: Rect) {
        let Ok(configs) = self.launch_configs() else { return };
        if configs.is_empty() || self.debug.session.is_some() {
            return;
        }
        let fg = self.color("icon.foreground");
        let gear = Rect::new(header.right() - 32.0, header.y + 6.0, 24.0, 22.0);
        self.icon_button(c, gear, &icons::GEAR, Hit::DebugGear, fg);
        let name = self.debug.selected_config.clone().filter(|n| configs.iter().any(|c| c["name"].as_str() == Some(n))).or_else(|| configs[0]["name"].as_str().map(String::from)).unwrap_or_default();
        let style = TextStyle::ui(12.0, self.color("foreground"));
        // As wide as the name needs, in the room the view's title leaves.
        let title_right = header.x + 20.0 + c.measure(super::View::Debug.title(), &TextStyle::ui(SMALL, self.color("sideBarTitle.foreground"))) + 10.0;
        let w = (c.measure(&name, &style) + 44.0).min(gear.x - 4.0 - title_right).max(60.0);
        let pick = Rect::new(gear.x - 4.0 - w, header.y + 6.0, w, 22.0);
        c.bordered(pick, self.color("dropdown.background"), self.color("dropdown.border"), 1.0, 2.0);
        let play = Rect::new(pick.x, pick.y, 22.0, pick.h);
        if self.hovered(Hit::DebugStartButton) {
            c.fill(play, self.color("toolbar.hoverBackground"));
        }
        c.icon_in(&icons::DEBUG_START, play, 14.0, self.color("debugIcon.startForeground"));
        let label = Rect::new(play.right() + 2.0, pick.y, pick.w - 42.0, pick.h);
        c.push_clip(label);
        c.text_fit(label, &name, &style);
        c.pop_clip();
        c.icon_in(&icons::CHEVRON_DOWN, Rect::new(pick.right() - 18.0, pick.y, 16.0, pick.h), 14.0, fg);
        self.hits.push((Rect::new(play.right(), pick.y, pick.w - play.w, pick.h), Hit::DebugConfigPicker));
        self.hits.push((play, Hit::DebugStartButton));
    }

    pub(super) fn draw_debug_view(&mut self, c: &mut Canvas, r: Rect) {
        let has_configs = self.launch_configs().is_ok_and(|c| !c.is_empty());
        if !has_configs && self.debug.session.is_none() {
            return self.draw_debug_welcome(c, r);
        }
        let v = &self.debug.view;
        let (heads, bodies) = split(r, &v.open, &v.heights);
        self.debug.view.heads = heads.clone().try_into().unwrap();
        self.debug.view.bodies = bodies.clone().try_into().unwrap();
        for i in 0..SECTIONS {
            self.section_header(c, heads[i], TITLES[i], self.debug.view.open[i], Hit::DebugSection(i));
            if self.debug.view.open[i] {
                let lines = self.debug_lines(i);
                self.draw_debug_rows(c, i, bodies[i], lines);
            }
            let above_open = (0..i).any(|j| self.debug.view.open[j]);
            if i > 0 && self.debug.view.open[i] && above_open {
                let sash = Rect::new(r.x, heads[i].y - 2.0, r.w, 4.0);
                self.hits.push((sash, Hit::DebugSash(i)));
                if matches!(self.drag, Some(Drag::DebugSash(j)) if j == i) || (self.drag.is_none() && self.hover_hit == Some(Hit::DebugSash(i))) {
                    c.fill(sash, self.color("sash.hoverBorder"));
                }
            }
        }
        // Section actions, shown while the pointer is over the section.
        let (mx, my) = self.mouse;
        for i in 0..SECTIONS {
            let (head, body) = (heads[i], bodies[i]);
            if !(head.contains(mx, my) || body.contains(mx, my)) {
                continue;
            }
            let actions: &[&render::Icon] = match i {
                0 => &[&icons::COLLAPSE_ALL],
                1 => &[&icons::ADD, &icons::COLLAPSE_ALL, &icons::CLOSE_ALL],
                2 => &[],
                _ => &[&icons::BREAKPOINTS_ACTIVATE, &icons::CLOSE_ALL],
            };
            let fg = self.color_or("sideBarSectionHeader.foreground", "sideBar.foreground");
            for (k, icon) in actions.iter().enumerate() {
                let x = head.right() - 28.0 - (actions.len() - 1 - k) as f32 * 24.0;
                let b = Rect::new(x, head.y, 22.0, head.h);
                if self.hovered(Hit::DebugAction(i, k)) {
                    c.fill_rounded(b.inset(0.0, 2.0), self.color("toolbar.hoverBackground"), 3.0);
                }
                c.icon_in(icon, b, 16.0, fg);
                self.hits.push((b, Hit::DebugAction(i, k)));
            }
        }
    }

    fn draw_debug_welcome(&mut self, c: &mut Canvas, r: Rect) {
        let fg = self.color_or("sideBar.foreground", "foreground");
        let button = Rect::new(r.x + 20.0, r.y + 12.0, r.w - 40.0, 26.0);
        let bg = self.color(if self.hovered(Hit::DebugStartButton) { "button.hoverBackground" } else { "button.background" });
        c.fill_rounded(button, bg, 2.0);
        let bs = TextStyle::ui(UI, self.color("button.foreground"));
        let label = "Run and Debug";
        let tw = c.measure(label, &bs);
        c.text_in(Rect::new(button.x + (button.w - tw) / 2.0, button.y, tw + 2.0, button.h), label, &bs);
        self.hits.push((button, Hit::DebugStartButton));
        if self.folder().is_some() {
            // "To customize Run and Debug create a launch.json file.", the link part clickable,
            // laid out word by word.
            let style = TextStyle::ui(UI, fg);
            let link = TextStyle::ui(UI, self.color("textLink.foreground"));
            let words = "To customize Run and Debug".split(' ').map(|w| (w, false)).chain("create a launch.json file.".split(' ').map(|w| (w, true)));
            let (left, right) = (r.x + 20.0, r.right() - 20.0);
            let (mut x, mut y) = (left, button.bottom() + 14.0);
            let space = c.measure(" ", &style);
            for (word, is_link) in words {
                let st = if is_link { &link } else { &style };
                let w = c.measure(word, st);
                if x > left && x + w > right {
                    x = left;
                    y += 18.0;
                }
                c.text(x, y, word, st);
                if is_link {
                    self.hits.push((Rect::new(x, y, w + space, 18.0), Hit::DebugCreateLaunch));
                }
                x += w + space;
            }
        }
    }

    /// The rows of section `i`.
    fn debug_lines(&self, i: usize) -> Vec<Line> {
        let mut out = Vec::new();
        let s = self.debug.session.as_ref();
        let name_color = self.color("debugTokenExpression.name");
        let fg = self.color_or("sideBar.foreground", "foreground");
        let desc = self.color("descriptionForeground");
        let expanded = &self.debug.expanded;
        // A variable and (when expanded) its children, recursively.
        fn node(wb: &Workbench, out: &mut Vec<Line>, v: &dap::Variable, path: String, depth: usize, watch: Option<usize>) {
            let open = wb.debug.expanded.contains(&path);
            let mut runs = Vec::new();
            if !v.name.is_empty() {
                runs.push((v.name.clone(), wb.color("debugTokenExpression.name")));
                runs.push((": ".into(), wb.color_or("sideBar.foreground", "foreground")));
            }
            runs.push((v.value.clone(), value_color(wb, &v.value)));
            out.push(Line { depth, twistie: (v.reference > 0).then_some(open), runs, right: String::new(), dim: false, row: DRow::Node { path: path.clone(), reference: v.reference, watch } });
            if open && v.reference > 0 {
                if let Some(children) = wb.debug.session.as_ref().and_then(|s| s.children.get(&path)) {
                    for c in children {
                        node(wb, out, c, format!("{path}/{}", c.name), depth + 1, None);
                    }
                }
            }
        }
        match i {
            0 => {
                let Some(s) = s else { return out };
                for (k, scope) in s.scopes.iter().enumerate() {
                    let path = format!("scope/{k}");
                    let open = expanded.contains(&path);
                    out.push(Line {
                        depth: 0,
                        twistie: Some(open),
                        runs: vec![(scope.name.clone(), fg)],
                        right: String::new(),
                        dim: false,
                        row: DRow::Node { path: path.clone(), reference: scope.reference, watch: None },
                    });
                    if open {
                        for v in s.children.get(&path).map(Vec::as_slice).unwrap_or_default() {
                            node(self, &mut out, v, format!("{path}/{}", v.name), 1, None);
                        }
                    }
                }
            }
            1 => {
                for (k, expr) in self.debug.watches.iter().enumerate() {
                    let path = format!("watch/{k}");
                    let result = s.filter(|s| s.state == State::Stopped).and_then(|s| s.watch_results.get(k).cloned().flatten());
                    match result {
                        Some(Ok(v)) => node(self, &mut out, &dap::Variable { name: expr.clone(), ..v }, path, 0, Some(k)),
                        Some(Err(e)) => out.push(Line {
                            depth: 0,
                            twistie: None,
                            runs: vec![(expr.clone(), name_color), (": ".into(), fg), (e, self.color("debugTokenExpression.error"))],
                            right: String::new(),
                            dim: false,
                            row: DRow::Node { path, reference: 0, watch: Some(k) },
                        }),
                        None => out.push(Line {
                            depth: 0,
                            twistie: None,
                            runs: vec![(expr.clone(), name_color), (": ".into(), fg), ("not available".into(), desc)],
                            right: String::new(),
                            dim: false,
                            row: DRow::Node { path, reference: 0, watch: Some(k) },
                        }),
                    }
                }
            }
            2 => {
                let Some(s) = s else { return out };
                let label = |st: &State| match st {
                    State::Stopped => {
                        let reason = s.stopped.as_ref().map(|x| if x.description.is_empty() { x.reason.clone() } else { x.description.clone() }).unwrap_or_default();
                        format!("Paused on {reason}").to_uppercase()
                    }
                    State::Ending => "STOPPING".into(),
                    _ => "RUNNING".into(),
                };
                let threads: Vec<(i64, String)> = if s.threads.is_empty() { s.thread.map(|t| (t, format!("Thread #{t}"))).into_iter().collect() } else { s.threads.iter().map(|t| (t.id, t.name.clone())).collect() };
                let show_threads = threads.len() > 1;
                if threads.is_empty() {
                    out.push(Line { depth: 0, twistie: None, runs: vec![(s.name.clone(), fg)], right: label(&s.state), dim: false, row: DRow::Note });
                }
                for (id, name) in threads {
                    let focused = s.thread == Some(id);
                    if show_threads || !focused || s.frames.is_empty() {
                        let right = if focused { label(&s.state) } else { String::new() };
                        out.push(Line { depth: 0, twistie: show_threads.then_some(focused), runs: vec![(name, fg)], right, dim: false, row: DRow::Thread(id) });
                    }
                    if !focused {
                        continue;
                    }
                    for (k, f) in s.frames.iter().enumerate() {
                        let right = match f.source.as_ref() {
                            Some(src) => {
                                let file = src.path.as_deref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| src.name.clone());
                                format!("{file}  {}:{}", f.line, f.column)
                            }
                            None => "Unknown Source".into(),
                        };
                        let dim = f.hint == "subtle" || f.source.as_ref().is_none_or(|src| src.path.is_none());
                        out.push(Line { depth: usize::from(show_threads), twistie: None, runs: vec![(f.name.clone(), fg)], right, dim, row: DRow::Frame(k) });
                    }
                }
            }
            _ => {
                let root = self.folder();
                for (k, f) in self.debug.exception_filters.iter().enumerate() {
                    out.push(Line { depth: 0, twistie: None, runs: vec![(f.label.clone(), fg)], right: String::new(), dim: !self.debug.active, row: DRow::Exception(k) });
                }
                for (path, bps) in &self.debug.breakpoints {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let dir = path.parent().and_then(|p| root.as_deref().and_then(|r| p.strip_prefix(r).ok())).map(|p| p.display().to_string()).unwrap_or_default();
                    for b in bps {
                        let mut runs = vec![(name.clone(), fg)];
                        if !dir.is_empty() {
                            runs.push((format!("  {dir}"), desc));
                        }
                        let dim = !b.enabled || !self.debug.active || b.verified == Some(false);
                        out.push(Line { depth: 0, twistie: None, runs, right: (b.line + 1).to_string(), dim, row: DRow::Breakpoint(path.clone(), b.line) });
                    }
                }
            }
        }
        out
    }

    fn draw_debug_rows(&mut self, c: &mut Canvas, section: usize, r: Rect, lines: Vec<Line>) {
        self.hits.push((r, Hit::DebugBody(section)));
        let max = (lines.len() as f32 * ROW_H - r.h).max(0.0);
        let scroll = self.debug.view.scroll[section].clamp(0.0, max);
        self.debug.view.scroll[section] = scroll;
        let indent = crate::config::get().tree_indent;
        let fg = self.color_or("sideBar.foreground", "foreground");
        let desc = self.color("descriptionForeground");
        let focused_frame = self.debug.session.as_ref().and_then(|s| s.frame);
        let selected = self.debug.view.selected.clone();
        let hover = self.hover_hit;
        let mut hits = Vec::new();
        c.push_clip(r);
        let first = (scroll / ROW_H) as usize;
        let visible = (r.h / ROW_H).ceil() as usize + 1;
        for (i, line) in lines.iter().enumerate().skip(first).take(visible) {
            let y = r.y + i as f32 * ROW_H - scroll;
            let rr = Rect::new(r.x, y, r.w, ROW_H);
            let is_selected = selected.as_ref().is_some_and(|(s, row)| *s == section && *row == line.row);
            let is_focused_frame = matches!(line.row, DRow::Frame(k) if Some(k) == focused_frame);
            if is_selected || is_focused_frame {
                c.fill_rounded(super::row_pill(rr), self.color("list.inactiveSelectionBackground"), super::ROW_RADIUS);
            } else if hover == Some(Hit::DebugRow(section, i)) || matches!(hover, Some(Hit::DebugRowAction(s, j, _)) if s == section && j == i) {
                c.fill_rounded(super::row_pill(rr), self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let mut x = rr.x + 8.0 + line.depth as f32 * indent;
            if let Some(open) = line.twistie {
                c.icon(if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT }, x, y + 3.0, 16.0, fg);
            }
            x += 18.0;
            let checkbox = match &line.row {
                DRow::Breakpoint(path, l) => Some(self.debug.breakpoints.get(path).and_then(|b| b.iter().find(|b| b.line == *l)).is_some_and(|b| b.enabled)),
                DRow::Exception(k) => Some(self.debug.exception_filters.get(*k).is_some_and(|f| f.enabled)),
                _ => None,
            };
            if let Some(enabled) = checkbox {
                // The enable checkbox.
                let cb = Rect::new(x - 16.0, y + 5.0, 13.0, 13.0);
                c.bordered(cb, self.color("checkbox.background"), self.color("checkbox.border"), 1.0, 3.0);
                if enabled {
                    c.icon_in(&icons::CHECK, cb, 11.0, self.color("checkbox.foreground"));
                }
                hits.push((cb.inset(-2.0, -2.0), Hit::DebugRowAction(section, i, 9)));
                x += 4.0;
            }
            // Hover actions: remove a watch; edit or remove a breakpoint.
            let actions: &[&render::Icon] = match &line.row {
                // Top-level watch expressions ("watch/<i>"), not their children.
                DRow::Node { watch: Some(_), path, .. } if path.matches('/').count() == 1 => &[&icons::EDIT, &icons::CLOSE],
                DRow::Breakpoint(..) => &[&icons::EDIT, &icons::CLOSE],
                _ => &[],
            };
            let row_hovered = hover == Some(Hit::DebugRow(section, i)) || matches!(hover, Some(Hit::DebugRowAction(s, j, _)) if s == section && j == i);
            let actions_w = if row_hovered { actions.len() as f32 * 22.0 } else { 0.0 };
            let right_w = if line.right.is_empty() { 0.0 } else { c.measure(&line.right, &TextStyle::ui(SMALL, desc)) + 12.0 };
            let text_right = rr.right() - right_w.max(actions_w) - 6.0;
            let mut tx = x;
            for (text, color) in &line.runs {
                let color = if line.dim { color.with_alpha(0.6) } else { *color };
                let w = c.text_fit(Rect::new(tx, y, (text_right - tx).max(0.0), ROW_H), text, &TextStyle::ui(UI, color));
                tx += w;
                if tx >= text_right {
                    break;
                }
            }
            if row_hovered && !actions.is_empty() {
                for (k, icon) in actions.iter().enumerate() {
                    let b = Rect::new(rr.right() - 6.0 - (actions.len() - k) as f32 * 22.0, y, 22.0, ROW_H);
                    c.icon_in(icon, b, 14.0, self.color("icon.foreground"));
                    hits.push((b, Hit::DebugRowAction(section, i, k)));
                }
            } else if !line.right.is_empty() {
                let st = TextStyle::ui(SMALL, desc);
                let w = c.measure(&line.right, &st);
                c.text_in(Rect::new(rr.right() - w - 10.0, y, w + 2.0, ROW_H), &line.right, &st);
            }
            hits.push((rr.intersect(&r), Hit::DebugRow(section, i)));
        }
        c.pop_clip();
        // Hits: rows first so their actions (pushed after) win.
        hits.sort_by_key(|(_, h)| matches!(h, Hit::DebugRowAction(..)));
        self.hits.extend(hits);
        self.debug.view.rows[section] = lines.into_iter().map(|l| l.row).collect();
    }

    pub(super) fn debug_scroll(&mut self, section: usize, dy: f32) {
        self.debug.view.scroll[section] = (self.debug.view.scroll[section] - dy).max(0.0);
    }

    pub(super) fn drag_debug_sash(&mut self, i: usize, y: f32) {
        let body = self.debug.view.bodies[i];
        self.debug.view.heights[i] = Some((body.bottom() - y - ROW_H).max(MIN_BODY));
    }

    /// A click on row `i` of `section`.
    pub(super) fn click_debug_row(&mut self, section: usize, i: usize, count: u32) {
        let Some(row) = self.debug.view.rows[section].get(i).cloned() else { return };
        self.debug.view.selected = Some((section, row.clone()));
        match row {
            DRow::Node { path, reference, watch } => {
                if count >= 2 && watch.is_some() && !path.trim_start_matches("watch/").contains('/') {
                    return self.edit_watch(watch.unwrap());
                }
                if reference > 0 {
                    self.toggle_variable(path, reference);
                }
            }
            DRow::Thread(id) => {
                if let Some(s) = self.debug.session.as_mut() {
                    if s.thread != Some(id) {
                        s.thread = Some(id);
                        s.frames.clear();
                        s.frame = None;
                    }
                }
                self.debug_refresh_stack();
            }
            DRow::Frame(k) => self.focus_frame(k),
            DRow::Breakpoint(path, line) => self.goto_location(&path, text::Pos::new(line, 0)),
            DRow::Exception(k) => self.toggle_exception_filter(k),
            DRow::Note => {}
        }
    }

    /// A row's hover action: 0 edit, 1 remove, 9 the breakpoint checkbox.
    pub(super) fn debug_row_action(&mut self, section: usize, i: usize, action: usize) {
        let Some(row) = self.debug.view.rows[section].get(i).cloned() else { return };
        match (row, action) {
            (DRow::Node { watch: Some(w), .. }, 0) => self.edit_watch(w),
            (DRow::Node { watch: Some(w), .. }, 1) => self.remove_watch(w),
            (DRow::Breakpoint(path, line), 0) => self.edit_breakpoint_at(path, line, super::debug::BpField::Condition),
            (DRow::Breakpoint(path, line), 1) => self.toggle_breakpoint_at(&path, line),
            (DRow::Exception(k), 9) => self.toggle_exception_filter(k),
            (DRow::Breakpoint(path, line), 9) => {
                let enabled = self.debug.breakpoints.get(&path).and_then(|b| b.iter().find(|b| b.line == line)).is_some_and(|b| b.enabled);
                self.set_breakpoint_enabled(&path, line, !enabled);
            }
            _ => {}
        }
    }

    /// Section header actions (in the order drawn).
    pub(super) fn debug_section_action(&mut self, section: usize, k: usize) {
        match (section, k) {
            (0, 0) => self.debug.expanded.retain(|p| !p.starts_with("scope/")),
            (1, 0) => self.add_watch_prompt(),
            (1, 1) => self.debug.expanded.retain(|p| !p.starts_with("watch/")),
            (1, 2) => {
                self.debug.watches.clear();
                self.evaluate_watches();
            }
            (3, 0) => self.toggle_breakpoints_active(),
            (3, 1) => self.remove_all_breakpoints(),
            _ => {}
        }
    }

    /// Asks for the focused thread's frames again (after picking another thread).
    fn debug_refresh_stack(&mut self) {
        self.debug_request_stack();
    }

    pub(super) fn debug_config_picker(&mut self) {
        self.select_and_start();
    }

    pub(super) fn debug_start_button(&mut self) {
        match self.launch_configs() {
            Ok(c) if !c.is_empty() => {
                let name = self.debug.selected_config.clone().or_else(|| c[0]["name"].as_str().map(String::from));
                if let Some(name) = name {
                    self.debug_pick(DebugPick::Config(name));
                }
            }
            _ => self.debug_start(false),
        }
    }

    // ------------------------------------------------------------ toolbar

    /// The floating debug toolbar, at the top of the editor area while a session runs.
    pub(super) fn draw_debug_toolbar(&mut self, c: &mut Canvas, area: Rect) {
        let Some(s) = self.debug.session.as_ref() else { return };
        let stopped = s.state == State::Stopped;
        let buttons: [(ToolbarButton, &render::Icon, &str, bool); 6] = [
            if stopped {
                (ToolbarButton::ContinueOrPause, &icons::DEBUG_CONTINUE, "debugIcon.continueForeground", true)
            } else {
                (ToolbarButton::ContinueOrPause, &icons::DEBUG_PAUSE, "debugIcon.pauseForeground", s.state == State::Running)
            },
            (ToolbarButton::StepOver, &icons::DEBUG_STEP_OVER, "debugIcon.stepOverForeground", stopped),
            (ToolbarButton::StepInto, &icons::DEBUG_STEP_INTO, "debugIcon.stepIntoForeground", stopped),
            (ToolbarButton::StepOut, &icons::DEBUG_STEP_OUT, "debugIcon.stepOutForeground", stopped),
            (ToolbarButton::Restart, &icons::DEBUG_RESTART, "debugIcon.restartForeground", true),
            (ToolbarButton::Stop, &icons::DEBUG_STOP, "debugIcon.stopForeground", true),
        ];
        let w = 16.0 + buttons.len() as f32 * 26.0 + 6.0;
        let bar = Rect::new(area.x + ((area.w - w) / 2.0).round(), area.y + 2.0, w, 28.0);
        c.push_layer();
        c.fill_rounded(bar.inset(-1.0, -1.0), self.color("widget.shadow"), 5.0);
        c.bordered(bar, self.color("debugToolBar.background"), self.color_or("debugToolBar.border", "widget.border"), 1.0, 4.0);
        c.icon_in(&icons::GRIPPER, Rect::new(bar.x + 2.0, bar.y, 14.0, bar.h), 14.0, self.color("descriptionForeground"));
        for (k, (button, icon, key, enabled)) in buttons.into_iter().enumerate() {
            let b = Rect::new(bar.x + 16.0 + k as f32 * 26.0, bar.y + 3.0, 24.0, 22.0);
            let hit = Hit::DebugToolbar(button);
            if enabled && self.hovered(hit) {
                c.fill_rounded(b, self.color("toolbar.hoverBackground"), 3.0);
            }
            let color = if enabled { self.color(key) } else { self.color(key).with_alpha(0.4) };
            c.icon_in(icon, b, 16.0, color);
            if enabled {
                self.hits.push((b, hit));
            }
        }
        self.hits.push((bar, Hit::DebugToolbarBody));
    }

    pub(super) fn debug_toolbar(&mut self, button: ToolbarButton) {
        match button {
            ToolbarButton::ContinueOrPause => match self.debug.session.as_ref().map(|s| s.state) {
                Some(State::Stopped) => self.debug_continue(),
                _ => self.debug_pause(),
            },
            ToolbarButton::StepOver => self.debug_step("next"),
            ToolbarButton::StepInto => self.debug_step("stepIn"),
            ToolbarButton::StepOut => self.debug_step("stepOut"),
            ToolbarButton::Restart => self.debug_restart(),
            ToolbarButton::Stop => self.debug_stop(),
        }
    }

    // ------------------------------------------------------------ console

    pub(super) fn draw_debug_console(&mut self, c: &mut Canvas, body: Rect) {
        let (lines_rect, input_rect) = body.cut_bottom(26.0);
        self.hits.push((lines_rect, Hit::DebugConsoleBody));
        let lh = 18.0;
        let fg = self.color("foreground");
        let mono = |color: Color| TextStyle::mono(12.0, lh, color);
        // Rows: console lines split at their line breaks.
        let rows: Vec<(&str, &str)> = self.debug.console.iter().flat_map(|l| l.text.trim_end_matches('\n').split('\n').map(move |t| (t, l.category.as_str()))).collect();
        let total = rows.len() as f32 * lh;
        let max = (total - lines_rect.h + 8.0).max(0.0);
        // Scroll is measured up from the bottom, so new output stays in view.
        self.debug.console_scroll = self.debug.console_scroll.clamp(0.0, max);
        let top = max - self.debug.console_scroll;
        c.push_clip(lines_rect);
        let first = (top / lh) as usize;
        let n = (lines_rect.h / lh).ceil() as usize + 1;
        for (i, (text, category)) in rows.iter().enumerate().skip(first).take(n) {
            let y = lines_rect.y + 4.0 + i as f32 * lh - top;
            let color = match *category {
                "stderr" => self.color("debugConsole.errorForeground"),
                "console" | "important" => self.color("debugConsole.infoForeground"),
                "result" => value_color(self, text),
                _ => fg,
            };
            let x = lines_rect.x + 20.0;
            if *category == "input" {
                c.icon_in(&icons::CHEVRON_RIGHT, Rect::new(x - 16.0, y, 14.0, lh), 12.0, self.color("debugConsoleInputIcon.foreground"));
            } else if *category == "result" {
                c.icon_in(&icons::CHEVRON_RIGHT, Rect::new(x - 16.0, y, 14.0, lh), 12.0, self.color("descriptionForeground").with_alpha(0.5));
            }
            c.text(x, y, text, &mono(color));
        }
        c.pop_clip();
        // The input, with the prompt chevron.
        let focused = self.focus == Focus::DebugConsole && self.palette.is_none();
        c.fill(Rect::new(input_rect.x, input_rect.y, input_rect.w, 1.0), self.color("panel.border"));
        c.icon_in(&icons::CHEVRON_RIGHT, Rect::new(input_rect.x + 4.0, input_rect.y, 16.0, input_rect.h), 14.0, self.color("debugConsoleInputIcon.foreground"));
        let field = Rect::new(input_rect.x + 22.0, input_rect.y + 3.0, input_rect.w - 30.0, 20.0);
        let caret_on = self.editor_caret_on();
        let placeholder = self.color("input.placeholderForeground");
        let selection = self.color("editor.selectionBackground");
        self.debug.console_input.draw(c, field, &mono(fg), "", placeholder, focused, caret_on, selection);
        self.hits.push((input_rect, Hit::DebugConsoleInput));
    }

    pub(super) fn debug_console_key(&mut self, k: &KeyInput) {
        match k.key {
            Key::Enter if !k.shift => self.debug_console_submit(),
            Key::Up | Key::Down if !k.cmd && !k.alt => {
                // History, newest last.
                let h = &self.debug.console_history;
                if h.is_empty() {
                    return;
                }
                let cur = h.iter().position(|x| *x == self.debug.console_input.text);
                let next = match (k.key == Key::Up, cur) {
                    (true, None) => Some(h.len() - 1),
                    (true, Some(i)) => Some(i.saturating_sub(1)),
                    (false, Some(i)) if i + 1 < h.len() => Some(i + 1),
                    (false, _) => None,
                };
                let text = next.map(|i| h[i].clone()).unwrap_or_default();
                self.debug.console_input.set_text(&text);
            }
            Key::Escape => {
                if self.debug.console_input.text.is_empty() {
                    self.focus = Focus::Editor;
                } else {
                    self.debug.console_input.set_text("");
                }
            }
            _ => {
                self.debug.console_input.key(k);
            }
        }
    }

    pub(super) fn debug_console_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        let f = &mut self.debug.console_input;
        if all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.debug.console_input.insert(&text.replace('\n', " "));
            }
            return;
        }
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    pub(super) fn debug_console_scroll(&mut self, dy: f32) {
        self.debug.console_scroll = (self.debug.console_scroll + dy).max(0.0);
    }

    /// Whether the status bar should use the debugging colors.
    pub(super) fn debugging(&self) -> bool {
        self.debug.session.as_ref().is_some_and(|s| s.state != State::Ending)
    }

    /// The file and line at window y in group `g`'s glyph margin.
    fn glyph_line(&self, g: usize, y: f32) -> Option<(PathBuf, usize)> {
        let gr = &self.groups[g];
        let ed = gr.tabs.get(gr.active)?;
        let doc = self.docs[ed.doc].as_ref()?;
        let path = doc.buffer.path()?.to_path_buf();
        if y > ed.geom.text.y + (ed.layout.row_count(&doc.buffer) as f32) * crate::editor::line_height() - ed.scroll_y {
            return None; // below the last line
        }
        Some((path, ed.pos_at(doc, ed.geom.text.x, y).line))
    }

    /// A click in the glyph margin: runs the test on that line, else toggles a breakpoint.
    pub(super) fn debug_click_glyph(&mut self, g: usize, x: f32, y: f32) {
        if let Some((path, line)) = self.glyph_line(g, y) {
            if !self.testing_click_glyph(&path, line, x, y) {
                self.toggle_breakpoint_at(&path, line);
            }
        }
    }

    /// A right-click (or ⌃-click): context menus for breakpoints, tests, files, tabs and chats.
    pub fn context_menu(&mut self, x: f32, y: f32) {
        match self.hit_at(x, y) {
            Some(Hit::GlyphMargin(g)) => {
                if let Some((path, line)) = self.glyph_line(g, y) {
                    self.test_glyph_menu(path, line, x, y);
                }
            }
            Some(Hit::TestingRow(i) | Hit::TestingRowAction(i, _) | Hit::TestingTwistie(i)) => self.testing_row_menu(i, x, y),
            Some(Hit::ExplorerRow(i)) => self.explorer_context_menu(Some(i), x, y),
            Some(Hit::Editor(g)) => self.editor_context_menu(g, x, y),
            Some(Hit::Tab(g, i)) => self.tab_context_menu(g, i, x, y),
            Some(Hit::AssistantTab(i) | Hit::AssistantTabClose(i)) => self.chat_menu(i, x, y),
            Some(Hit::ExtTree(v, super::ext_views::TreeHit::Row(i) | super::ext_views::TreeHit::Inline(i, _))) => self.ext_tree_context_menu(v as usize, i, x, y),
            Some(Hit::SidebarBody) if self.view == super::View::Explorer && self.tree.is_some() => self.explorer_context_menu(None, x, y),
            Some(Hit::DebugRow(3, i) | Hit::DebugRowAction(3, i, _)) => {
                if let Some(DRow::Breakpoint(path, line)) = self.debug.view.rows[3].get(i).cloned() {
                    self.breakpoint_menu(path, line, x, y);
                }
            }
            _ => {}
        }
    }
}
