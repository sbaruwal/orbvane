//! The Settings sheet (⌘,): a card in front of the window with the categories on the left and
//! the settings on the right, grouped in cards: name and description on the left, the control
//! on the right. Search and the User/Workspace switch sit at the top.

use render::{Canvas, Color, Rect, TextStyle};
use serde_json::Value;
use settings::schema::{self, Kind, Section, Setting};
use settings::Scope;

use super::controls::FIELD_RADIUS;
use super::preferences::PopupAction;
use super::{Focus, Hit, PopupItem, Workbench, UI};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::{wrap, FieldEvent, TextField};

const SHEET_MAX_W: f32 = 980.0;
const SHEET_MAX_H: f32 = 780.0;
const SHEET_RADIUS: f32 = 14.0;
const TOC_W: f32 = 200.0;
const TOP_H: f32 = 56.0;
const ROW_PAD: f32 = 12.0;
const TITLE_H: f32 = 18.0;
const DESC_LINE_H: f32 = 18.0;
const CONTROL_H: f32 = 26.0;
const NUMBER_W: f32 = 120.0;
const TEXT_W: f32 = 260.0;
const DROPDOWN_MAX_W: f32 = 240.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingsHit {
    /// Outside the sheet: closes it.
    Backdrop,
    Close,
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
    /// `level` 1 = a top-level group, 2 = a category in it.
    Header { label: &'static str, level: u8, toc: usize },
    Setting { index: usize, toc: usize },
}

#[derive(Default)]
pub struct SettingsView {
    /// Whether the sheet is showing.
    pub open: bool,
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
            let toc = toc();
            let mut rows = Vec::new();
            let mut last = None;
            let found = all.iter().enumerate().filter(|(_, s)| {
                let (cat, name) = s.title();
                let hay = format!("{} {cat}{name} {}", s.key, s.description).to_lowercase();
                words.iter().all(|w| hay.contains(w)) && (!modified_only || modified(s.key))
            });
            // Results keep their categories as headings.
            for (index, s) in found {
                let t = toc_index(s.section);
                if last != Some(t) {
                    rows.push(Row::Header { label: toc[t].0, level: 2, toc: t });
                    last = Some(t);
                }
                rows.push(Row::Setting { index, toc: t });
            }
            return rows;
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
    /// Shows the Settings sheet, with the search box focused.
    pub(super) fn open_settings_ui(&mut self) {
        self.settings_ui.open = true;
        self.settings_ui.editing = None;
        self.settings_ui.search.select_all();
        self.focus = Focus::Settings;
    }

    /// Closes the sheet, keeping what was typed into a field.
    pub(super) fn close_settings_ui(&mut self) {
        self.commit_editing();
        self.settings_ui.editing = None;
        self.settings_ui.open = false;
        self.focus = Focus::Editor;
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
            Key::Escape => self.close_settings_ui(),
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
            SettingsHit::Backdrop | SettingsHit::Close => self.close_settings_ui(),
            SettingsHit::Search => self.settings_ui.search.click(x, shift),
            SettingsHit::ScopeTab(ws) => self.settings_ui.workspace = ws,
            SettingsHit::OpenJson => {
                let scope = self.settings_ui.scope();
                self.close_settings_ui();
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

    /// The sheet over a dimmed window; a click outside it closes it.
    pub(super) fn draw_settings_sheet(&mut self, c: &mut Canvas, full: Rect) {
        c.push_layer();
        c.fill(full, self.color("widget.shadow").with_alpha(0.45));
        self.hits.push((full, Hit::Settings(SettingsHit::Backdrop)));
        let w = (full.w - 80.0).clamp(320.0, SHEET_MAX_W).min(full.w - 16.0);
        let h = (full.h - 96.0).clamp(240.0, SHEET_MAX_H).min(full.h - 16.0);
        let sheet = Rect::new((full.x + (full.w - w) / 2.0).round(), (full.y + (full.h - h) / 2.0).round(), w, h);
        c.shadow(sheet, SHEET_RADIUS, self.color("widget.shadow"));
        c.fill_rounded(sheet, self.color("editor.background"), SHEET_RADIUS);
        self.hits.push((sheet, Hit::Settings(SettingsHit::Body)));
        let focused = self.focus == Focus::Settings && self.palette.is_none();
        c.push_clip(sheet);
        self.draw_settings_editor(c, sheet, focused);
        c.pop_clip();
        let border = self.color_or("editorWidget.border", "widget.border");
        c.bordered(sheet, Color::TRANSPARENT, border, 1.0, SHEET_RADIUS);
    }

    fn draw_settings_editor(&mut self, c: &mut Canvas, r: Rect, focused: bool) {
        let has_workspace = self.settings.path(Scope::Workspace).is_some();
        if !has_workspace {
            self.settings_ui.workspace = false;
        }
        let caret_on = self.caret_on();
        let fg = self.color("foreground");
        let desc_fg = self.color("descriptionForeground");
        let border = self.color("widget.border");
        let scope = self.settings_ui.scope();
        let rows = self.settings_ui.rows(&|key| self.settings.get_in(scope, key).is_some());
        let query_active = !self.settings_ui.search.text.trim().is_empty();

        // The categories down the left, on the side bar's color.
        let show_toc = r.w >= 640.0;
        let main = if show_toc {
            c.fill_rounded(Rect::new(r.x, r.y, TOC_W + SHEET_RADIUS * 2.0, r.h), self.color("sideBar.background"), SHEET_RADIUS);
            c.fill(Rect::new(r.x + TOC_W, r.y, SHEET_RADIUS * 2.0, r.h), self.color("editor.background"));
            c.fill(Rect::new(r.x + TOC_W, r.y, 1.0, r.h), border);
            let st = TextStyle::ui(15.0, fg).weight(600);
            c.text_in(Rect::new(r.x + 20.0, r.y, TOC_W - 40.0, TOP_H), "Settings", &st);
            Rect::new(r.x + TOC_W + 1.0, r.y, r.w - TOC_W - 1.0, r.h)
        } else {
            r
        };

        // The top bar: search, User/Workspace, settings.json and close.
        let top = Rect::new(main.x, main.y, main.w, TOP_H);
        let close = Rect::new(top.right() - 16.0 - 26.0, top.y + 15.0, 26.0, 26.0);
        let icon_fg = self.color("icon.foreground");
        self.icon_button(c, close, &icons::CLOSE, Hit::Settings(SettingsHit::Close), icon_fg);
        let json = Rect::new(close.x - 30.0, close.y, 26.0, 26.0);
        self.icon_button(c, json, &icons::GO_TO_FILE, Hit::Settings(SettingsHit::OpenJson), icon_fg);
        let mut right = json.x - 10.0;
        if has_workspace {
            let st = TextStyle::ui(UI, fg);
            let seg_w = c.measure("User", &st) + c.measure("Workspace", &st) + 48.0 + 4.0;
            right -= seg_w;
            let active = self.settings_ui.workspace as usize;
            self.segmented(c, right, top.y + 14.0, 28.0, &["User", "Workspace"], active, |i| Hit::Settings(SettingsHit::ScopeTab(i == 1)));
            right -= 12.0;
        }
        let search_r = Rect::new(top.x + 24.0, top.y + 14.0, (right - top.x - 24.0).max(80.0), 28.0);
        let search_focused = focused && self.settings_ui.editing.is_none();
        self.field_frame(c, search_r, search_focused);
        c.icon(&icons::SEARCH, search_r.x + 9.0, search_r.y + 7.0, 14.0, desc_fg);
        let count_w = if query_active {
            let n = rows.iter().filter(|r| matches!(r, Row::Setting { .. })).count();
            let label = match n {
                0 => "None found".to_string(),
                1 => "1 setting".to_string(),
                n => format!("{n} settings"),
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
        let field_r = Rect::new(search_r.x + 28.0, search_r.y, (search_r.w - 36.0 - count_w).max(0.0), search_r.h);
        self.settings_ui.search.draw(c, field_r, &style, "Search settings", ph, search_focused, caret_on, sel);
        self.hits.push((search_r, Hit::Settings(SettingsHit::Search)));

        // Body: the settings, grouped in cards under their headings.
        let body = Rect::new(main.x, top.bottom(), main.w, main.bottom() - top.bottom());
        let list_x = body.x + 24.0;
        let list_w = (body.w - 48.0).max(100.0);
        let content_x = list_x + 16.0;
        let content_w = list_w - 32.0;
        self.settings_ui.view_h = body.h;

        // Lay out rows (heights depend on wrapped descriptions).
        let all = schema::all();
        let desc_style = TextStyle::ui(UI, desc_fg);
        let mut layout: Vec<Laid> = Vec::with_capacity(rows.len());
        let mut cards: Vec<(f32, f32)> = Vec::new();
        let mut y = 4.0;
        let mut anchors = Vec::new();
        let mut in_card = false;
        for row in rows {
            let toc = match row {
                Row::Header { toc, .. } | Row::Setting { toc, .. } => toc,
            };
            if !anchors.iter().any(|(t, _)| *t == toc) {
                anchors.push((toc, y));
            }
            match row {
                Row::Header { level, .. } => {
                    if in_card {
                        y += 8.0;
                        in_card = false;
                    }
                    let h = if level == 1 { 46.0 } else { 34.0 };
                    layout.push(Laid { row, y, h, lines: Vec::new(), first: false });
                    y += h;
                }
                Row::Setting { index, .. } => {
                    let s = &all[index];
                    let control_w = self.control_width(c, s);
                    let text_w = (content_w - control_w - 24.0).max(120.0);
                    let desc = plain_description(s);
                    let lines = if desc.is_empty() { Vec::new() } else { wrap(c, &desc, &desc_style, text_w) };
                    let error = self.settings_ui.editing.as_ref().filter(|(i, _)| *i == index).and_then(|(_, f)| match s.kind {
                        Kind::Number { .. } => validate_number(&f.text, s.kind).err(),
                        _ => None,
                    });
                    let text_h = TITLE_H + if lines.is_empty() { 0.0 } else { 4.0 + lines.len() as f32 * DESC_LINE_H } + if error.is_some() { 20.0 } else { 0.0 };
                    let h = ROW_PAD * 2.0 + text_h.max(CONTROL_H);
                    if !in_card {
                        cards.push((y, 0.0));
                    }
                    layout.push(Laid { row, y, h, lines, first: !in_card });
                    in_card = true;
                    y += h;
                    if let Some(card) = cards.last_mut() {
                        card.1 = y;
                    }
                }
            }
        }
        y += 24.0;
        self.settings_ui.content_h = y;
        let max_scroll = (y - body.h).max(0.0);
        self.settings_ui.scroll = self.settings_ui.scroll.clamp(0.0, max_scroll);
        let scroll = self.settings_ui.scroll;
        self.settings_ui.current_toc = anchors.iter().rev().find(|(_, ay)| *ay <= scroll + 1.0).map_or(0, |(t, _)| *t);
        self.settings_ui.anchors = anchors;

        if show_toc {
            self.draw_settings_toc(c, Rect::new(r.x, r.y + TOP_H, TOC_W, r.h - TOP_H - 8.0), &layout);
        }

        c.push_clip(body);
        for (y0, y1) in &cards {
            let card = Rect::new(list_x, body.y + y0 - scroll, list_w, y1 - y0);
            if card.y < body.bottom() && card.bottom() > body.y {
                self.card(c, card);
            }
        }
        let hover_hit = self.hover_hit;
        for laid in &layout {
            let top = body.y + laid.y - scroll;
            if top > body.bottom() || top + laid.h < body.y {
                continue;
            }
            match laid.row {
                Row::Header { label, level, .. } => {
                    let st = if level == 1 {
                        TextStyle::ui(17.0, self.color("settings.headerForeground")).weight(600)
                    } else {
                        TextStyle::ui(UI, fg).weight(600)
                    };
                    c.text_in(Rect::new(list_x + 4.0, top + laid.h - 30.0, list_w - 8.0, 26.0), label, &st);
                }
                Row::Setting { index, .. } => {
                    let row_r = Rect::new(list_x, top, list_w, laid.h);
                    if !laid.first {
                        c.fill(Rect::new(content_x, top, content_w, 1.0), border.with_alpha(border.a * 0.7));
                    }
                    self.draw_setting_row(c, index, row_r, content_x, content_w, &laid.lines, focused, caret_on, hover_hit);
                }
            }
        }
        if !layout.iter().any(|l| matches!(l.row, Row::Setting { .. })) {
            let detail = "Try other words, or @modified for the settings you've changed.";
            self.empty_state(c, body, &icons::SEARCH, "No settings found", detail, None);
        }
        c.pop_clip();

        // A line under the top bar, and a shadow once scrolled.
        c.fill(Rect::new(main.x, body.y, main.w, 1.0), self.color("settings.headerBorder"));
        if scroll > 0.0 {
            for i in 1..4 {
                let a = 0.3 * (1.0 - i as f32 / 4.0);
                c.fill(Rect::new(main.x, body.y + i as f32, main.w, 1.0), self.color("scrollbar.shadow").with_alpha(a));
            }
        }
    }

    /// How wide a setting's control is drawn.
    fn control_width(&self, c: &mut Canvas, s: &Setting) -> f32 {
        match s.kind {
            Kind::Bool => 32.0,
            Kind::Enum(_) | Kind::Theme => {
                let st = TextStyle::ui(UI, self.color("settings.dropdownForeground"));
                let label = value_label(&self.shown_value(s));
                let widest = match s.kind {
                    Kind::Enum(o) => o.iter().map(|(v, _)| c.measure(v, &st)).fold(0.0, f32::max),
                    _ => 0.0,
                };
                (widest.max(c.measure(&label, &st)) + 36.0).clamp(120.0, DROPDOWN_MAX_W)
            }
            Kind::Number { .. } => NUMBER_W,
            Kind::String => TEXT_W,
            Kind::Json(_) => c.measure(JSON_LINK, &TextStyle::ui(UI, Color::TRANSPARENT)),
        }
    }

    fn draw_settings_toc(&mut self, c: &mut Canvas, r: Rect, layout: &[Laid]) {
        let searching = !self.settings_ui.search.text.trim().is_empty();
        c.push_clip(r);
        let mut y = r.y;
        for (i, (label, level, _)) in toc().into_iter().enumerate() {
            let count = layout.iter().filter(|l| matches!(l.row, Row::Setting { toc, .. } if toc == i)).count();
            if searching && count == 0 {
                continue;
            }
            let active = self.settings_ui.current_toc == i;
            let hit = Hit::Settings(SettingsHit::Toc(i));
            let rr = Rect::new(r.x + 6.0, y, r.w - 12.0, 26.0);
            if active {
                c.fill_rounded(rr, self.color("list.inactiveSelectionBackground"), super::ROW_RADIUS);
            } else if self.hovered(hit) {
                c.fill_rounded(rr, self.color("list.hoverBackground"), super::ROW_RADIUS);
            }
            let color = if active || level == 0 { self.color("foreground") } else { self.color("foreground").with_alpha(0.8) };
            let st = TextStyle::ui(UI, color).weight(if level == 0 || active { 600 } else { 400 });
            let text = if searching { format!("{label} ({count})") } else { label.to_string() };
            c.text_in(Rect::new(rr.x + 10.0 + level as f32 * 12.0, rr.y, rr.w - 16.0 - level as f32 * 12.0, rr.h), &text, &st);
            self.hits.push((rr.intersect(&r), hit));
            y += 26.0;
        }
        c.pop_clip();
    }

    /// One setting: its name and description on the left, its control on the right.
    #[allow(clippy::too_many_arguments)]
    fn draw_setting_row(
        &mut self,
        c: &mut Canvas,
        index: usize,
        row_r: Rect,
        x: f32,
        content_w: f32,
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
        let pill = Rect::new(row_r.x + 4.0, row_r.y + 3.0, row_r.w - 8.0, row_r.h - 6.0);
        if editing && focused {
            c.fill_rounded(pill, self.color("settings.focusedRowBackground"), 8.0);
        } else if row_hovered {
            c.fill_rounded(pill, self.color("settings.rowHoverBackground"), 8.0);
        }
        self.hits.push((row_r, Hit::Settings(SettingsHit::Row(index))));

        let modified_here = self.settings.get_in(scope, s.key).is_some();
        let modified_other = self.settings.path(other).is_some() && self.settings.get_in(other, s.key).is_some();
        let ty = row_r.y + ROW_PAD;
        if modified_here {
            let h = TITLE_H + if lines.is_empty() { 0.0 } else { 4.0 + lines.len() as f32 * DESC_LINE_H };
            c.fill_rounded(Rect::new(row_r.x + 6.0, ty, 2.0, h), self.color("settings.modifiedItemIndicator"), 1.0);
        }

        // The control, on the right, level with the name.
        let control_w = self.control_width(c, s);
        let cx = x + content_w - control_w;
        let cy = ty + (TITLE_H - CONTROL_H) / 2.0;
        let text_w = (content_w - control_w - 24.0).max(120.0);

        // Name (no category: the heading says it), where else it's set, and the gear on hover.
        let (_, name) = s.title();
        let title_style = TextStyle::ui(UI, self.color("settings.headerForeground")).weight(600);
        let mut tx = x + c.text_in(Rect::new(x, ty, text_w, TITLE_H), &name, &title_style);
        if modified_other {
            let label = if modified_here { "Also set in " } else { "Set in " };
            let where_ = if other == Scope::Workspace { "Workspace" } else { "User" };
            let st = TextStyle::ui(12.0, self.color("descriptionForeground"));
            tx += 10.0;
            let lw = c.text_in(Rect::new(tx, ty, 200.0, TITLE_H), label, &st);
            tx += lw + c.text_in(Rect::new(tx + lw, ty, 100.0, TITLE_H), where_, &st.color(self.color("textLink.foreground")));
        }
        if row_hovered {
            let hit = Hit::Settings(SettingsHit::Gear(index));
            let color = if self.hovered(hit) { self.color("icon.foreground") } else { self.color("icon.foreground").with_alpha(0.7) };
            self.icon_button(c, Rect::new(tx + 6.0, ty - 1.0, 20.0, 20.0), &icons::GEAR, hit, color);
        }

        let desc_style = TextStyle::ui(UI, self.color("descriptionForeground"));
        let mut y = ty + TITLE_H + 4.0;
        for line in lines {
            c.text(x, y, line, &desc_style);
            y += DESC_LINE_H;
        }
        let value = self.shown_value(s);
        match s.kind {
            Kind::Bool => {
                let hit = Hit::Settings(SettingsHit::Checkbox(index));
                self.switch(c, cx, ty, value.as_bool() == Some(true), hit);
                // The name and description toggle it too, like a label.
                let label_h = y - ty;
                self.hits.push((Rect::new(x, ty, text_w, label_h), hit));
            }
            Kind::Enum(_) | Kind::Theme => {
                let label = value_label(&value);
                let st = TextStyle::ui(UI, self.color("settings.dropdownForeground"));
                let dd = Rect::new(cx, cy, control_w, CONTROL_H);
                let hit = Hit::Settings(SettingsHit::Dropdown(index));
                c.bordered(dd, self.color("settings.dropdownBackground"), self.color("settings.dropdownBorder"), 1.0, FIELD_RADIUS);
                c.text_fit(Rect::new(dd.x + 9.0, dd.y, dd.w - 32.0, dd.h), &label, &st);
                c.icon(&icons::CHEVRON_DOWN, dd.right() - 22.0, dd.y + 5.0, 16.0, self.color("settings.dropdownForeground"));
                self.hits.push((dd, hit));
            }
            Kind::Number { .. } | Kind::String => {
                let number = matches!(s.kind, Kind::Number { .. });
                let input = Rect::new(cx, cy, control_w, CONTROL_H);
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
                    self.color_or(border, "widget.border")
                };
                c.bordered(input, self.color(bg), border_color, 1.0, FIELD_RADIUS);
                let st = TextStyle::ui(UI, self.color("input.foreground"));
                let field_r = Self::field_text_rect(input);
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
                    c.text(x, y + 2.0, &msg, &st);
                }
            }
            Kind::Json(_) => {
                // Lists and objects are edited in the file.
                let st = TextStyle::ui(UI, self.color("textLink.foreground"));
                let r = Rect::new(cx, cy, control_w + 2.0, CONTROL_H);
                c.text_in(r, JSON_LINK, &st);
                self.hits.push((r, Hit::Settings(SettingsHit::OpenJson)));
            }
        }
    }

    /// Whether the Settings sheet is showing.
    pub(super) fn settings_active(&self) -> bool {
        self.settings_ui.open
    }

    pub(super) fn settings_caret_visible(&self) -> bool {
        self.focus == Focus::Settings && self.settings_active()
    }
}

/// A laid out row: its content y, height, wrapped description, and whether it starts a card.
struct Laid {
    row: Row,
    y: f32,
    h: f32,
    lines: Vec<String>,
    first: bool,
}

const JSON_LINK: &str = "Edit in settings.json";
