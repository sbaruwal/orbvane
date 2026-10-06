//! The Settings editor (⌘,): search, User/Workspace tabs, a table of contents and the list of
//! settings with their controls, laid out.

use render::{Canvas, Rect, TextStyle};
use serde_json::Value;
use settings::schema::{self, Kind, Section, Setting};
use settings::Scope;

use super::preferences::PopupAction;
use super::{Focus, Hit, PopupItem, Workbench, UI};
use crate::editor::{Doc, EditorState};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::{wrap, FieldEvent, TextField};

const MAX_WIDTH: f32 = 1200.0;
const TOC_W: f32 = 180.0;
const ROW_PAD_TOP: f32 = 12.0;
const ROW_PAD_BOTTOM: f32 = 16.0;
const TITLE_H: f32 = 20.0;
const DESC_LINE_H: f32 = 18.0;
const CONTROL_H: f32 = 26.0;
const NUMBER_W: f32 = 200.0;
const TEXT_W: f32 = 500.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingsHit {
    Search,
    ScopeTab(bool),
    OpenJson,
    Toc(usize),
    Body,
    /// A setting row (index into `schema::all()`).
    Row(usize),
    Gear(usize),
    Checkbox(usize),
    Dropdown(usize),
    Input(usize),
}

/// Table of contents entries: (label, indent level, section it scrolls to).
const BUILTIN_TOC: &[(&str, u8, Option<Section>)] = &[
    ("Commonly Used", 0, None),
    ("Text Editor", 0, Some(Section::TextEditor)),
    ("Cursor", 1, Some(Section::Cursor)),
    ("Font", 1, Some(Section::Font)),
    ("Minimap", 1, Some(Section::Minimap)),
    ("Files", 1, Some(Section::Files)),
    ("Workbench", 0, Some(Section::Workbench)),
    ("Appearance", 1, Some(Section::Appearance)),
    ("Features", 0, Some(Section::Search)),
    ("Search", 1, Some(Section::Search)),
    ("Terminal", 1, Some(Section::Terminal)),
    ("Source Control", 1, Some(Section::SourceControl)),
    ("Testing", 1, Some(Section::Testing)),
    ("Language Servers", 1, Some(Section::LanguageServers)),
    ("Assistant", 1, Some(Section::Assistant)),
    ("Extensions", 1, Some(Section::Extensions)),
    ("Application", 0, Some(Section::Update)),
    ("Update", 1, Some(Section::Update)),
    ("Extensions", 0, Some(Section::Emmet)),
    ("Emmet", 1, Some(Section::Emmet)),
    ("Git", 1, Some(Section::Git)),
    ("HTML", 1, Some(Section::Html)),
    ("JSON", 1, Some(Section::Json)),
    ("LLDB DAP", 1, Some(Section::LldbDap)),
];

/// The built-in entries, then one per extension's settings under "Extensions".
fn toc() -> Vec<(&'static str, u8, Option<Section>)> {
    let mut toc = BUILTIN_TOC.to_vec();
    toc.extend(schema::extension_sections().into_iter().map(|s| (s.label(), 1, Some(s))));
    toc
}

fn toc_index(section: Section) -> usize {
    toc().iter().rposition(|(_, _, s)| *s == Some(section)).unwrap_or(0)
}

#[derive(Clone, Copy, Debug)]
enum Row {
    Header { label: &'static str, level: u8, toc: usize },
    Setting { index: usize, toc: usize },
}

#[derive(Default)]
pub struct SettingsView {
    pub search: TextField,
    workspace: bool,
    scroll: f32,
    /// The value field being edited: (setting index, field).
    editing: Option<(usize, TextField)>,
    /// Content y of each TOC entry's first row, from the last layout.
    anchors: Vec<(usize, f32)>,
    content_h: f32,
    view_h: f32,
    /// Which TOC entry the list is scrolled to.
    current_toc: usize,
}

impl SettingsView {
    fn scope(&self) -> Scope {
        if self.workspace { Scope::Workspace } else { Scope::User }
    }

    /// The rows to show; `modified` says whether a setting is set in the selected scope.
    fn rows(&self, modified: &dyn Fn(&str) -> bool) -> Vec<Row> {
        let all = schema::all();
        let query = self.search.text.trim().to_lowercase();
        if !query.is_empty() {
            let modified_only = query.split_whitespace().any(|w| w == "@modified");
            let words: Vec<&str> = query.split_whitespace().filter(|w| *w != "@modified").collect();
            return all
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    let (cat, name) = s.title();
                    let hay = format!("{} {cat}{name} {}", s.key, s.description).to_lowercase();
                    words.iter().all(|w| hay.contains(w)) && (!modified_only || modified(s.key))
                })
                .map(|(index, s)| Row::Setting { index, toc: toc_index(s.section) })
                .collect();
        }
        let mut rows = vec![Row::Header { label: "Commonly Used", level: 1, toc: 0 }];
        for key in schema::COMMONLY_USED {
            if let Some(index) = all.iter().position(|s| s.key == *key) {
                rows.push(Row::Setting { index, toc: 0 });
            }
        }
        for (toc, (label, level, section)) in toc().into_iter().enumerate().skip(1) {
            let (label, level) = (&label, &level);
            let Some(section) = section else { continue };
            rows.push(Row::Header { label, level: level + 1, toc });
            // Groups list their own section's settings ("Features" has none of its own).
            if *level == 0 && section.label() != *label {
                continue;
            }
            for (index, _) in all.iter().enumerate().filter(|(_, s)| s.section == section) {
                rows.push(Row::Setting { index, toc });
            }
        }
        rows
    }
}

/// Setting descriptions reference settings as `#files.autoSaveDelay#` and code as `code`.
fn plain_description(s: &Setting) -> String {
    let mut out = String::new();
    let mut rest = s.description;
    while let Some(i) = rest.find("`#") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(end) = after.find("#`") else { break };
        let key = &after[..end];
        match schema::find(key) {
            Some(target) => {
                let (cat, name) = target.title();
                out.push_str(&format!("{cat}{name}"));
            }
            None => out.push_str(key),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out.replace('`', "")
}

fn value_label(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        // 16.0 in the file shows as 16.
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => (f as i64).to_string(),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// The validation message for a number field, or the parsed value.
fn validate_number(text: &str, kind: Kind) -> Result<Value, String> {
    let Kind::Number { min, max, integer } = kind else { return Err(String::new()) };
    let n: f64 = text.trim().parse().map_err(|_| "Value must be a number.".to_string())?;
    if integer && n.fract() != 0.0 {
        return Err("Value must be an integer.".into());
    }
    if n < min {
        return Err(format!("Value must be greater than or equal to {min}."));
    }
    if n > max {
        return Err(format!("Value must be less than or equal to {max}."));
    }
    // Whole numbers are written without a fraction ("16", not "16.0").
    Ok(if integer || (n.fract() == 0.0 && n.abs() < 1e15) {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    })
}

impl Workbench {
    /// Opens (or focuses) the Settings editor, with the search box focused.
    pub(super) fn open_settings_ui(&mut self) {
        let existing = self
            .groups
            .iter()
            .enumerate()
            .find_map(|(g, gr)| gr.tabs.iter().position(|t| t.settings).map(|i| (g, i)));
        match existing {
            Some((g, i)) => {
                self.active_group = g;
                self.groups[g].active = i;
            }
            None => {
                let doc = self.add_doc(Doc::virtual_named("Settings"));
                let g = &mut self.groups[self.active_group];
                let at = if g.tabs.is_empty() { 0 } else { g.active + 1 };
                let mut ed = EditorState::new(doc);
                ed.settings = true;
                g.tabs.insert(at, ed);
                g.active = at;
            }
        }
        self.settings_ui.editing = None;
        self.settings_ui.search.select_all();
        self.focus = Focus::Settings;
    }

    /// The value shown for a setting in the selected scope: its own, else inherited.
    fn shown_value(&self, s: &Setting) -> Value {
        let scope = self.settings_ui.scope();
        self.settings
            .get_in(scope, s.key)
            .or_else(|| if scope == Scope::Workspace { self.settings.get_in(Scope::User, s.key) } else { None })
            .filter(|v| s.accepts(v))
            .cloned()
            .unwrap_or_else(|| s.default_value())
    }

    fn set_from_ui(&mut self, key: &'static str, value: Value) {
        let scope = self.settings_ui.scope();
        if key == "workbench.colorTheme" {
            if let Some(name) = value.as_str() {
                self.set_theme(name);
            }
        }
        self.update_setting(scope, key, Some(value));
    }

    fn commit_editing(&mut self) {
        let Some((index, field)) = &self.settings_ui.editing else { return };
        let s = &schema::all()[*index];
        let value = match s.kind {
            Kind::Number { .. } => validate_number(&field.text, s.kind).ok(),
            _ => Some(Value::String(field.text.clone())),
        };
        if let Some(v) = value.filter(|v| *v != self.shown_value(s)) {
            self.set_from_ui(s.key, v);
        }
    }

    /// ⌘F in the Settings editor focuses its search box.
    pub(super) fn focus_settings_search(&mut self) {
        self.commit_editing();
        self.settings_ui.editing = None;
        self.settings_ui.search.select_all();
    }

    pub(super) fn settings_key(&mut self, k: &KeyInput) {
        if k.cmd && !k.shift && !k.alt && k.key == Key::Char("f".into()) {
            return self.focus_settings_search();
        }
        if let Some((_, field)) = &mut self.settings_ui.editing {
            match k.key {
                Key::Escape | Key::Enter | Key::Tab => {
                    self.commit_editing();
                    self.settings_ui.editing = None;
                    return;
                }
                _ => {}
            }
            if field.key(k) == FieldEvent::Changed {
                self.commit_editing();
            } else if let Some(cmd) = k.command() {
                self.run(cmd);
            }
            return;
        }
        match k.key {
            Key::Escape if !self.settings_ui.search.text.is_empty() => {
                self.settings_ui.search.set_text("");
                self.settings_ui.scroll = 0.0;
            }
            Key::PageDown => self.settings_ui.scroll += self.settings_ui.view_h * 0.9,
            Key::PageUp => self.settings_ui.scroll -= self.settings_ui.view_h * 0.9,
            Key::Down if !k.cmd => self.settings_ui.scroll += 40.0,
            Key::Up if !k.cmd => self.settings_ui.scroll -= 40.0,
            _ => match self.settings_ui.search.key(k) {
                FieldEvent::Changed => self.settings_ui.scroll = 0.0,
                FieldEvent::Moved => {}
                FieldEvent::Ignored => {
                    if let Some(cmd) = k.command() {
                        self.run(cmd);
                    }
                }
            },
        }
    }

    pub(super) fn settings_clipboard(&mut self, cut: bool, paste: bool, select_all: bool) {
        let clipboard_text = if paste { self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) } else { None };
        let ui = &mut self.settings_ui;
        let field = match &mut ui.editing {
            Some((_, f)) => f,
            None => &mut ui.search,
        };
        if select_all {
            return field.select_all();
        }
        if let Some(text) = clipboard_text {
            field.insert(&text.replace('\n', " "));
            if ui.editing.is_some() {
                self.commit_editing();
            }
            return;
        }
        let text = if cut { field.cut() } else { field.copy() };
        if cut && ui.editing.is_some() {
            self.commit_editing();
        }
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    pub(super) fn scroll_settings(&mut self, dy: f32) {
        self.settings_ui.scroll -= dy;
    }

    pub(super) fn click_settings(&mut self, hit: SettingsHit, x: f32, shift: bool) {
        self.focus = Focus::Settings;
        let all = schema::all();
        // Clicking anywhere else ends an edit.
        if !matches!(hit, SettingsHit::Input(i) if self.settings_ui.editing.as_ref().is_some_and(|(e, _)| *e == i)) {
            self.commit_editing();
            self.settings_ui.editing = None;
        }
        match hit {
            SettingsHit::Search => self.settings_ui.search.click(x, shift),
            SettingsHit::ScopeTab(ws) => self.settings_ui.workspace = ws,
            SettingsHit::OpenJson => {
                let scope = self.settings_ui.scope();
                self.open_settings_json(scope);
            }
            SettingsHit::Toc(i) => {
                if let Some((_, y)) = self.settings_ui.anchors.iter().find(|(t, _)| *t == i) {
                    self.settings_ui.scroll = *y;
                }
            }
            SettingsHit::Body | SettingsHit::Row(_) => {}
            SettingsHit::Checkbox(i) => {
                let s = &all[i];
                let on = self.shown_value(s).as_bool().unwrap_or(false);
                self.set_from_ui(s.key, Value::Bool(!on));
            }
            SettingsHit::Input(i) => {
                if !self.settings_ui.editing.as_ref().is_some_and(|(e, _)| *e == i) {
                    let mut field = TextField::default();
                    field.set_text(&value_label(&self.shown_value(&all[i])));
                    self.settings_ui.editing = Some((i, field));
                }
                if let Some((_, f)) = &mut self.settings_ui.editing {
                    f.click(x, shift);
                }
            }
            SettingsHit::Dropdown(i) => self.open_dropdown(i),
            SettingsHit::Gear(i) => self.open_setting_menu(i),
        }
    }

    fn control_rect(&self, index: usize) -> Option<Rect> {
        self.hits.iter().find(|(_, h)| *h == Hit::Settings(SettingsHit::Dropdown(index))).map(|(r, _)| *r)
    }

    fn open_dropdown(&mut self, index: usize) {
        let s = &schema::all()[index];
        let current = self.shown_value(s);
        let scope = self.settings_ui.scope();
        let options: Vec<String> = match s.kind {
            Kind::Enum(options) => options.iter().map(|(v, _)| v.to_string()).collect(),
            Kind::Theme => self.themes().into_iter().map(|t| t.name).collect(),
            _ => return,
        };
        let entries = options
            .into_iter()
            .map(|o| {
                let checked = current.as_str() == Some(o.as_str()) || current.as_bool().is_some_and(|b| b.to_string() == o);
                let value = match o.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::String(o.clone()),
                };
                (PopupItem::Item { label: o.clone(), enabled: true, checked: Some(checked) }, PopupAction::SetValue(scope, s.key, value))
            })
            .collect();
        let r = self.control_rect(index).unwrap_or_default();
        self.show_popup(entries, r.x, r.bottom() + 2.0);
    }

    /// The gear menu: reset, copy id, copy as JSON.
    fn open_setting_menu(&mut self, index: usize) {
        let s = &schema::all()[index];
        let scope = self.settings_ui.scope();
        let modified = self.settings.get_in(scope, s.key).is_some();
        let entries = vec![
            (PopupItem::Item { label: "Reset Setting".into(), enabled: modified, checked: None }, PopupAction::Reset(scope, s.key)),
            (PopupItem::Separator, PopupAction::None),
            (PopupItem::Item { label: "Copy Setting ID".into(), enabled: true, checked: None }, PopupAction::CopySettingId(s.key)),
            (PopupItem::Item { label: "Copy Setting as JSON".into(), enabled: true, checked: None }, PopupAction::CopySettingJson(s.key)),
        ];
        let r = self
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::Settings(SettingsHit::Gear(index)))
            .map(|(r, _)| *r)
            .unwrap_or_default();
        self.show_popup(entries, r.x, r.bottom() + 2.0);
    }

    // ------------------------------------------------------------------ drawing

    pub(super) fn draw_settings_editor(&mut self, c: &mut Canvas, r: Rect, focused: bool) {
        c.fill(r, self.color("editor.background"));
        self.hits.push((r, Hit::Settings(SettingsHit::Body)));
        let has_workspace = self.settings.path(Scope::Workspace).is_some();
        if !has_workspace {
            self.settings_ui.workspace = false;
        }
        let w = (r.w - 48.0).min(MAX_WIDTH);
        let content = Rect::new(r.x + ((r.w - w) / 2.0).max(24.0).round(), r.y, w, r.h);
        let caret_on = self.caret_on();
        let fg = self.color("foreground");
        let desc_fg = self.color("descriptionForeground");

        // Search box.
        let search_r = Rect::new(content.x, content.y + 12.0, content.w, 28.0);
        let search_focused = focused && self.settings_ui.editing.is_none();
        let border = if search_focused { self.color("focusBorder") } else { self.color_or("input.border", "input.background") };
        c.bordered(search_r, self.color("input.background"), border, 1.0, 2.0);
        let scope = self.settings_ui.scope();
        let rows = self.settings_ui.rows(&|key| self.settings.get_in(scope, key).is_some());
        let query_active = !self.settings_ui.search.text.trim().is_empty();
        let count_w = if query_active {
            let n = rows.iter().filter(|r| matches!(r, Row::Setting { .. })).count();
            let label = match n {
                0 => "No Settings Found".to_string(),
                1 => "1 Setting Found".to_string(),
                n => format!("{n} Settings Found"),
            };
            let st = TextStyle::ui(12.0, desc_fg);
            let lw = c.measure(&label, &st);
            c.text_in(Rect::new(search_r.right() - lw - 10.0, search_r.y, lw + 2.0, search_r.h), &label, &st);
            lw + 20.0
        } else {
            0.0
        };
        let style = TextStyle::ui(UI, self.color("input.foreground"));
        let (ph, sel) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        let field_r = Rect::new(search_r.x + 8.0, search_r.y, search_r.w - 16.0 - count_w, search_r.h);
        self.settings_ui.search.draw(c, field_r, &style, "Search settings", ph, search_focused, caret_on, sel);
        self.hits.push((search_r, Hit::Settings(SettingsHit::Search)));

        // User / Workspace tabs.
        let tabs_y = search_r.bottom() + 10.0;
        let tabs_h = 30.0;
        let mut x = content.x;
        let tabs: &[(&str, bool)] = if has_workspace { &[("User", false), ("Workspace", true)] } else { &[("User", false)] };
        for (label, ws) in tabs {
            let active = self.settings_ui.workspace == *ws;
            let hit = Hit::Settings(SettingsHit::ScopeTab(*ws));
            let color = if active || self.hovered(hit) { self.color("panelTitle.activeForeground") } else { self.color("panelTitle.inactiveForeground") };
            let st = TextStyle::ui(UI, color);
            let lw = c.measure(label, &st);
            let tab = Rect::new(x, tabs_y, lw + 16.0, tabs_h);
            c.text_in(Rect::new(x + 8.0, tabs_y, lw + 2.0, tabs_h), label, &st);
            if active {
                c.fill(Rect::new(x + 8.0, tab.bottom() - 2.0, lw, 1.0), self.color("panelTitle.activeBorder"));
            }
            self.hits.push((tab, hit));
            x += tab.w + 4.0;
        }
        let json_btn = Rect::new(content.right() - 26.0, tabs_y + 4.0, 22.0, 22.0);
        let icon_fg = self.color("icon.foreground");
        self.icon_button(c, json_btn, &icons::GO_TO_FILE, Hit::Settings(SettingsHit::OpenJson), icon_fg);
        let header_bottom = tabs_y + tabs_h;
        c.fill(Rect::new(content.x, header_bottom, content.w, 1.0), self.color("settings.headerBorder"));

        // Body: table of contents and the settings list.
        let body = Rect::new(r.x, header_bottom + 1.0, r.w, r.bottom() - header_bottom - 1.0);
        let show_toc = content.w >= 700.0;
        let list_x = if show_toc { content.x + TOC_W + 24.0 } else { content.x };
        let list_w = content.right() - list_x;
        self.settings_ui.view_h = body.h;

        // Lay out rows (heights depend on wrapped descriptions).
        let all = schema::all();
        let desc_style = TextStyle::ui(UI, fg.with_alpha(0.9));
        let text_w = (list_w - 28.0).max(100.0);
        let mut layout: Vec<(Row, f32, f32, Vec<String>)> = Vec::with_capacity(rows.len());
        let mut y = 0.0;
        let mut anchors = Vec::new();
        for row in rows {
            let (h, lines) = match row {
                Row::Header { level, toc, .. } => {
                    if !anchors.iter().any(|(t, _)| *t == toc) {
                        anchors.push((toc, y));
                    }
                    (if level == 1 { 48.0 } else { 38.0 }, Vec::new())
                }
                Row::Setting { index, toc } => {
                    if !anchors.iter().any(|(t, _)| *t == toc) {
                        anchors.push((toc, y));
                    }
                    let s = &all[index];
                    let desc = plain_description(s);
                    let is_bool = matches!(s.kind, Kind::Bool);
                    let lines = wrap(c, &desc, &desc_style, if is_bool { text_w - 26.0 } else { text_w });
                    let error = self.settings_ui.editing.as_ref().filter(|(i, _)| *i == index).and_then(|(_, f)| match s.kind {
                        Kind::Number { .. } => validate_number(&f.text, s.kind).err(),
                        _ => None,
                    });
                    let h = if is_bool {
                        ROW_PAD_TOP + TITLE_H + 6.0 + (lines.len() as f32 * DESC_LINE_H).max(18.0) + ROW_PAD_BOTTOM
                    } else {
                        ROW_PAD_TOP + TITLE_H + 4.0 + lines.len() as f32 * DESC_LINE_H + 8.0 + CONTROL_H + if error.is_some() { 24.0 } else { 0.0 } + ROW_PAD_BOTTOM
                    };
                    (h, lines)
                }
            };
            layout.push((row, y, h, lines));
            y += h;
        }
        self.settings_ui.content_h = y;
        let max_scroll = (y - body.h * 0.5).max(0.0);
        self.settings_ui.scroll = self.settings_ui.scroll.clamp(0.0, max_scroll);
        let scroll = self.settings_ui.scroll;
        self.settings_ui.current_toc = anchors.iter().rev().find(|(_, ay)| *ay <= scroll + 1.0).map_or(0, |(t, _)| *t);
        self.settings_ui.anchors = anchors;

        if show_toc {
            self.draw_settings_toc(c, Rect::new(content.x, body.y + 12.0, TOC_W, body.h - 12.0), &layout);
        }

        let list = Rect::new(list_x - 26.0, body.y, list_w + 26.0, body.h);
        c.push_clip(list);
        let hover_hit = self.hover_hit;
        for (row, ry, h, lines) in &layout {
            let top = body.y + ry - scroll;
            if top > body.bottom() || top + h < body.y {
                continue;
            }
            match *row {
                Row::Header { label, level, .. } => {
                    let size = if level == 1 { 20.0 } else { 16.0 };
                    let st = TextStyle::ui(size, self.color("settings.headerForeground")).weight(600);
                    c.text(list_x, top + if level == 1 { 16.0 } else { 12.0 }, label, &st);
                }
                Row::Setting { index, .. } => {
                    let row_r = Rect::new(list_x - 26.0, top, list_w + 26.0, *h);
                    self.draw_setting_row(c, index, row_r, list_x, text_w, lines, focused, caret_on, hover_hit);
                }
            }
        }
        if layout.iter().all(|(r, ..)| !matches!(r, Row::Setting { .. })) {
            let st = TextStyle::ui(UI, desc_fg);
            c.text(list_x, body.y + 16.0, "No Settings Found", &st);
        }
        c.pop_clip();

        // Shadow under the header once scrolled.
        if scroll > 0.0 {
            for i in 0..3 {
                let a = 0.3 * (1.0 - i as f32 / 3.0);
                c.fill(Rect::new(r.x, body.y + i as f32, r.w, 1.0), self.color("scrollbar.shadow").with_alpha(a));
            }
        }
    }

    fn draw_settings_toc(&mut self, c: &mut Canvas, r: Rect, layout: &[(Row, f32, f32, Vec<String>)]) {
        let searching = !self.settings_ui.search.text.trim().is_empty();
        let mut y = r.y;
        for (i, (label, level, _)) in toc().into_iter().enumerate() {
            let (label, level) = (&label, &level);
            let count = layout.iter().filter(|(row, ..)| matches!(row, Row::Setting { toc, .. } if *toc == i)).count();
            if searching && count == 0 {
                continue;
            }
            let active = self.settings_ui.current_toc == i;
            let hit = Hit::Settings(SettingsHit::Toc(i));
            let rr = Rect::new(r.x, y, r.w, 22.0);
            if self.hovered(hit) {
                c.fill(rr, self.color("list.hoverBackground"));
            }
            let color = if active { self.color("settings.headerForeground") } else { self.color("foreground").with_alpha(0.8) };
            let st = TextStyle::ui(UI, color).weight(if active { 600 } else { 400 });
            let text = if searching { format!("{label} ({count})") } else { label.to_string() };
            c.text_in(Rect::new(rr.x + 8.0 + *level as f32 * 12.0, rr.y, rr.w - 8.0, rr.h), &text, &st);
            self.hits.push((rr, hit));
            y += 22.0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_setting_row(
        &mut self,
        c: &mut Canvas,
        index: usize,
        row_r: Rect,
        x: f32,
        text_w: f32,
        lines: &[String],
        focused: bool,
        caret_on: bool,
        hover_hit: Option<Hit>,
    ) {
        let s = &schema::all()[index];
        let scope = self.settings_ui.scope();
        let other = if scope == Scope::User { Scope::Workspace } else { Scope::User };
        let editing = self.settings_ui.editing.as_ref().is_some_and(|(i, _)| *i == index);
        let row_hovered = matches!(hover_hit, Some(Hit::Settings(h)) if matches!(h,
            SettingsHit::Row(i) | SettingsHit::Gear(i) | SettingsHit::Checkbox(i) | SettingsHit::Dropdown(i) | SettingsHit::Input(i) if i == index));
        if editing && focused {
            c.fill(row_r, self.color("settings.focusedRowBackground"));
        } else if row_hovered {
            c.fill(row_r, self.color("settings.rowHoverBackground"));
        }
        self.hits.push((row_r, Hit::Settings(SettingsHit::Row(index))));

        let modified_here = self.settings.get_in(scope, s.key).is_some();
        let modified_other = self.settings.path(other).is_some() && self.settings.get_in(other, s.key).is_some();
        let content_bottom = row_r.bottom() - ROW_PAD_BOTTOM + 4.0;
        if modified_here {
            c.fill(Rect::new(x - 8.0, row_r.y + ROW_PAD_TOP, 2.0, content_bottom - row_r.y - ROW_PAD_TOP), self.color("settings.modifiedItemIndicator"));
        }

        // Title: "Editor: " + "Font Size", both bold, and where else it's modified.
        let fg = self.color("foreground");
        let (cat, name) = s.title();
        let title_style = TextStyle::ui(UI, self.color("settings.headerForeground")).weight(600);
        let ty = row_r.y + ROW_PAD_TOP;
        let cw = c.text(x, ty, &cat, &title_style.color(fg.with_alpha(0.9)));
        let nw = c.text(x + cw, ty, &name, &title_style);
        if modified_other {
            let label = if modified_here { "Also modified in: " } else { "Modified in: " };
            let where_ = if other == Scope::Workspace { "Workspace" } else { "User" };
            let st = TextStyle::ui(12.0, self.color("descriptionForeground"));
            let lw = c.text(x + cw + nw + 12.0, ty + 1.0, label, &st);
            c.text(x + cw + nw + 12.0 + lw, ty + 1.0, where_, &st.color(self.color("textLink.foreground")));
        }
        // The gear appears on hover, left of the title.
        let gear = Rect::new(x - 26.0 + 2.0, ty, 20.0, 20.0);
        if row_hovered || modified_here {
            let hit = Hit::Settings(SettingsHit::Gear(index));
            let color = if self.hovered(hit) { self.color("icon.foreground") } else { self.color("icon.foreground").with_alpha(0.7) };
            if row_hovered {
                self.icon_button(c, gear, &icons::GEAR, hit, color);
            }
        }

        let desc_style = TextStyle::ui(UI, fg.with_alpha(0.9));
        let value = self.shown_value(s);
        let mut y = ty + TITLE_H + 4.0;
        if let Kind::Bool = s.kind {
            // Checkbox with the description beside it.
            let bx = Rect::new(x, y + 1.0, 18.0, 18.0);
            let hit = Hit::Settings(SettingsHit::Checkbox(index));
            c.bordered(bx, self.color("settings.checkboxBackground"), self.color("settings.checkboxBorder"), 1.0, 3.0);
            if value.as_bool() == Some(true) {
                c.icon_in(&icons::CHECK, bx, 14.0, self.color("settings.checkboxForeground"));
            }
            self.hits.push((bx, hit));
            for (i, line) in lines.iter().enumerate() {
                c.text(x + 26.0, y + i as f32 * DESC_LINE_H + 1.0, line, &desc_style);
            }
            // The description is clickable too, like a label.
            let label_w = lines.iter().map(|l| c.measure(l, &desc_style)).fold(0.0, f32::max);
            self.hits.push((Rect::new(x + 26.0, y, label_w, lines.len() as f32 * DESC_LINE_H), hit));
            return;
        }
        for line in lines {
            c.text(x, y, line, &desc_style);
            y += DESC_LINE_H;
        }
        y += 8.0;
        match s.kind {
            Kind::Enum(_) | Kind::Theme => {
                let label = value_label(&value);
                let options: Vec<String> = match s.kind {
                    Kind::Enum(o) => o.iter().map(|(v, _)| v.to_string()).collect(),
                    _ => vec![label.clone()],
                };
                let st = TextStyle::ui(UI, self.color("settings.dropdownForeground"));
                let widest = options.iter().map(|o| c.measure(o, &st)).fold(0.0, f32::max).max(c.measure(&label, &st));
                let dd = Rect::new(x, y, (widest + 36.0).max(120.0).min(text_w), CONTROL_H);
                let hit = Hit::Settings(SettingsHit::Dropdown(index));
                c.bordered(dd, self.color("settings.dropdownBackground"), self.color("settings.dropdownBorder"), 1.0, 2.0);
                c.text_in(Rect::new(dd.x + 8.0, dd.y, dd.w - 30.0, dd.h), &label, &st);
                c.icon(&icons::CHEVRON_DOWN, dd.right() - 22.0, dd.y + 5.0, 16.0, self.color("settings.dropdownForeground"));
                self.hits.push((dd, hit));
            }
            Kind::Number { .. } | Kind::String => {
                let number = matches!(s.kind, Kind::Number { .. });
                let w = if number { NUMBER_W } else { TEXT_W.min(text_w) };
                let input = Rect::new(x, y, w, CONTROL_H);
                let (bg, border) = if number {
                    ("settings.numberInputBackground", "settings.numberInputBorder")
                } else {
                    ("settings.textInputBackground", "settings.textInputBorder")
                };
                let error = self.settings_ui.editing.as_ref().filter(|(i, _)| *i == index).and_then(|(_, f)| {
                    if number { validate_number(&f.text, s.kind).err() } else { None }
                });
                let border_color = if error.is_some() {
                    self.color("inputValidation.errorBorder")
                } else if editing && focused {
                    self.color("focusBorder")
                } else {
                    self.color_or(border, bg)
                };
                c.bordered(input, self.color(bg), border_color, 1.0, 2.0);
                let st = TextStyle::ui(UI, self.color("input.foreground"));
                let field_r = Rect::new(input.x + 6.0, input.y, input.w - 12.0, input.h);
                let (ph, sel) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
                match &mut self.settings_ui.editing {
                    Some((i, field)) if *i == index => field.draw(c, field_r, &st, "", ph, focused, caret_on, sel),
                    _ => {
                        c.push_clip(field_r);
                        c.text_in(field_r, &value_label(&value), &st);
                        c.pop_clip();
                    }
                }
                self.hits.push((input, Hit::Settings(SettingsHit::Input(index))));
                if let Some(msg) = error {
                    let st = TextStyle::ui(12.0, self.color("errorForeground"));
                    c.text(x, input.bottom() + 4.0, &msg, &st);
                }
            }
            Kind::Json(_) => {
                // Lists and objects are edited in the file.
                let st = TextStyle::ui(UI, self.color("textLink.foreground"));
                let label = "Edit in settings.json";
                let w = c.measure(label, &st);
                let r = Rect::new(x, y, w, CONTROL_H);
                c.text_in(r, label, &st);
                self.hits.push((r, Hit::Settings(SettingsHit::OpenJson)));
            }
            Kind::Bool => {}
        }
    }

    /// Whether the Settings editor is the active tab of the active group.
    pub(super) fn settings_active(&self) -> bool {
        self.active_editor().is_some_and(|e| e.settings)
    }

    pub(super) fn settings_caret_visible(&self) -> bool {
        self.focus == Focus::Settings && self.settings_active()
    }
}
