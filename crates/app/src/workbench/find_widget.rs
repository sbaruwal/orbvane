//! The in-editor Find/Replace widget (⌘F / ⌥⌘F), floating at the top right of an editor.

use render::{Canvas, Icon, Rect, TextStyle};
use search::Query;
use text::{Pos, Selection};

use super::{Focus, Hit, Workbench, UI};
use crate::icons;
use crate::input::{Key, KeyInput};
use crate::widgets::{FieldEvent, TextField};

const WIDTH: f32 = 419.0;
const ROW: f32 = 33.0;
const INPUT_H: f32 = 25.0;
const TOGGLE: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FindField {
    Find,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FindAction {
    Previous,
    Next,
    Close,
    ReplaceOne,
    ReplaceAll,
    ToggleReplace,
    MatchCase,
    WholeWord,
    Regex,
}

#[derive(Default)]
pub(super) struct FindWidget {
    pub visible: bool,
    find: TextField,
    replace: TextField,
    show_replace: bool,
    field: Option<FindField>,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    /// Matches in the document, sorted by position.
    pub matches: Vec<(Pos, Pos)>,
    pub current: Option<usize>,
    error: Option<String>,
    /// What `matches` was computed for: (doc, buffer version, query).
    computed_for: Option<(usize, u64, String, bool, bool, bool)>,
    /// Where find-as-you-type starts looking (the caret when the widget opened).
    origin: Pos,
}

impl FindWidget {
    fn query(&self) -> Query {
        Query {
            pattern: self.find.text.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
            ..Default::default()
        }
    }

    fn field_mut(&mut self, f: FindField) -> &mut TextField {
        match f {
            FindField::Find => &mut self.find,
            FindField::Replace => &mut self.replace,
        }
    }
}

impl Workbench {
    fn find_widget(&mut self) -> Option<&mut FindWidget> {
        self.groups.get_mut(self.active_group).map(|g| &mut g.find).filter(|f| f.visible)
    }

    /// Opens the widget, seeding it from the selection or the word at the caret.
    pub(super) fn open_find(&mut self, replace: bool) {
        let Some((ed, doc)) = self.active_mut() else { return };
        let seed = if ed.sel.is_empty() {
            let w = doc.buffer.word_at(ed.sel.head);
            (!w.is_empty()).then(|| doc.buffer.text_in(&w))
        } else {
            Some(doc.buffer.text_in(&ed.sel)).filter(|t| !t.contains('\n'))
        };
        let origin = ed.sel.ordered().0;
        let g = self.active_group;
        let w = &mut self.groups[g].find;
        w.visible = true;
        w.origin = origin;
        if let Some(seed) = seed {
            w.find.set_text(&seed);
        }
        w.find.select_all();
        if replace {
            w.show_replace = true;
        }
        w.field = Some(if replace && !w.find.text.is_empty() { FindField::Replace } else { FindField::Find });
        if replace {
            w.replace.select_all();
        }
        self.focus = Focus::Find;
        self.refresh_find_matches(g);
        self.select_match_from(g, origin, true, true);
    }

    pub(super) fn close_find(&mut self) {
        if let Some(w) = self.find_widget() {
            w.visible = false;
            w.field = None;
        }
        self.focus = Focus::Editor;
    }

    /// Recomputes matches if the document or query changed.
    pub(super) fn refresh_find_matches(&mut self, g: usize) {
        let Some(group) = self.groups.get_mut(g) else { return };
        let Some(ed) = group.tabs.get(group.active) else { return };
        let w = &mut group.find;
        if !w.visible {
            return;
        }
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let key = (ed.doc, doc.buffer.version(), w.find.text.clone(), w.case_sensitive, w.whole_word, w.regex);
        if w.computed_for.as_ref() == Some(&key) {
            return;
        }
        w.computed_for = Some(key);
        w.matches.clear();
        w.current = None;
        w.error = None;
        if w.find.text.is_empty() {
            return;
        }
        let regex = match w.query().compile() {
            Ok(r) => r,
            Err(e) => {
                w.error = Some(e);
                return;
            }
        };
        let text = doc.buffer.text();
        let mut line_cache: Option<(usize, String)> = None;
        for m in search::search_text(&text, &regex) {
            // Byte offsets within the line -> char columns.
            if line_cache.as_ref().is_none_or(|(l, _)| *l != m.line) {
                line_cache = Some((m.line, doc.buffer.line(m.line)));
            }
            let line = &line_cache.as_ref().unwrap().1;
            let col = |byte: usize| line[..byte.min(line.len())].chars().count();
            w.matches.push((Pos::new(m.line, col(m.start)), Pos::new(m.line, col(m.end))));
        }
        // Keep the current match on the editor's selection if it is one.
        let sel = ed.sel.ordered();
        w.current = w.matches.iter().position(|m| *m == sel);
    }

    /// Selects the first match at/after `from` (or before it, going backwards), wrapping
    /// around. `inclusive` allows a match starting exactly at `from`.
    fn select_match_from(&mut self, g: usize, from: Pos, forward: bool, inclusive: bool) {
        let group = &mut self.groups[g];
        let w = &mut group.find;
        if w.matches.is_empty() {
            w.current = None;
            return;
        }
        let n = w.matches.len();
        let i = if forward {
            w.matches.iter().position(|m| if inclusive { m.0 >= from } else { m.0 > from }).unwrap_or(0)
        } else {
            w.matches.iter().rposition(|m| if inclusive { m.0 <= from } else { m.0 < from }).unwrap_or(n - 1)
        };
        w.current = Some(i);
        let (a, z) = w.matches[i];
        if let Some(ed) = group.tabs.get_mut(group.active) {
            ed.set_selection(Selection { anchor: a, head: z, goal_col: None });
            ed.reveal = true;
        }
    }

    /// Next/previous match (Enter, ⌘G / ⇧⌘G, the arrow buttons).
    pub(super) fn find_step(&mut self, forward: bool) {
        let g = self.active_group;
        if !self.groups[g].find.visible || self.groups[g].find.find.text.is_empty() {
            return self.open_find(false);
        }
        self.refresh_find_matches(g);
        let Some(ed) = self.active_editor() else { return };
        // From the selection's start: when it's already a match, step strictly past it in
        // either direction; otherwise the match at the caret counts.
        let (a, z) = ed.sel.ordered();
        let on_match = self.groups[g].find.matches.iter().any(|m| *m == (a, z));
        self.select_match_from(g, a, forward, !on_match);
    }

    fn replacement_for(&self, g: usize, matched: &str) -> Option<String> {
        let w = &self.groups[g].find;
        let regex = w.query().compile().ok()?;
        Some(if w.regex {
            regex.replace(matched, w.replace.text.as_str()).into_owned()
        } else {
            w.replace.text.clone()
        })
    }

    /// Replaces the current match (if it's selected) and moves to the next one.
    fn replace_one(&mut self) {
        let g = self.active_group;
        self.refresh_find_matches(g);
        let Some(ed) = self.active_editor() else { return };
        let sel = ed.sel.ordered();
        let selected_match = self.groups[g].find.matches.iter().any(|m| *m == sel);
        if !selected_match {
            return self.find_step(true);
        }
        let matched = self.active_doc().map(|d| d.buffer.text_in(&ed.sel)).unwrap_or_default();
        let Some(new_text) = self.replacement_for(g, &matched) else { return };
        let Some((ed, doc)) = self.active_mut() else { return };
        ed.set_selection(doc.buffer.insert(ed.sel, &new_text));
        doc.buffer.break_undo_group();
        let after = ed.sel.head;
        self.refresh_find_matches(g);
        self.select_match_from(g, after, true, true);
    }

    /// Replaces every match in the document as one undo step.
    fn replace_all_in_doc(&mut self) {
        let g = self.active_group;
        let w = &self.groups[g].find;
        let (query, replacement) = (w.query(), w.replace.text.clone());
        let Ok(regex) = query.compile() else { return };
        let Some((ed, doc)) = self.active_mut() else { return };
        let (new_text, n) = search::replace_text(&doc.buffer.text(), &regex, &replacement, query.regex);
        if n == 0 {
            return;
        }
        let caret = ed.sel.head;
        let all = Selection { anchor: Pos::new(0, 0), head: doc.buffer.end(), goal_col: None };
        doc.buffer.insert(all, &new_text);
        doc.buffer.break_undo_group();
        ed.set_selection(Selection::caret(doc.buffer.clamp(caret)));
        ed.reveal = true;
        self.refresh_find_matches(g);
    }

    pub(super) fn find_action(&mut self, action: FindAction) {
        let g = self.active_group;
        match action {
            FindAction::Previous => self.find_step(false),
            FindAction::Next => self.find_step(true),
            FindAction::Close => self.close_find(),
            FindAction::ReplaceOne => self.replace_one(),
            FindAction::ReplaceAll => self.replace_all_in_doc(),
            FindAction::ToggleReplace => {
                let w = &mut self.groups[g].find;
                w.show_replace = !w.show_replace;
                if !w.show_replace && w.field == Some(FindField::Replace) {
                    w.field = Some(FindField::Find);
                }
            }
            FindAction::MatchCase | FindAction::WholeWord | FindAction::Regex => {
                let w = &mut self.groups[g].find;
                match action {
                    FindAction::MatchCase => w.case_sensitive = !w.case_sensitive,
                    FindAction::WholeWord => w.whole_word = !w.whole_word,
                    _ => w.regex = !w.regex,
                }
                self.refresh_find_matches(g);
                let origin = self.groups[g].find.origin;
                self.select_match_from(g, origin, true, true);
            }
        }
    }

    /// Keys while the widget has focus.
    pub(super) fn find_key(&mut self, k: &KeyInput) {
        let g = self.active_group;
        let Some(field) = self.groups[g].find.field.filter(|_| self.groups[g].find.visible) else {
            self.focus = Focus::Editor;
            return;
        };
        match k.key {
            Key::Escape => return self.close_find(),
            Key::Enter if field == FindField::Find => return self.find_step(!k.shift),
            Key::Enter if k.cmd => return self.replace_all_in_doc(),
            Key::Enter => return self.replace_one(),
            Key::Tab => {
                let w = &mut self.groups[g].find;
                if w.show_replace {
                    let next = if field == FindField::Find { FindField::Replace } else { FindField::Find };
                    w.field = Some(next);
                    w.field_mut(next).select_all();
                }
                return;
            }
            _ => {}
        }
        if k.cmd && k.alt {
            let action = match &k.key {
                Key::Char(c) if c == "c" => Some(FindAction::MatchCase),
                Key::Char(c) if c == "w" => Some(FindAction::WholeWord),
                Key::Char(c) if c == "r" => Some(FindAction::Regex),
                _ => None,
            };
            if let Some(a) = action {
                return self.find_action(a);
            }
        }
        match self.groups[g].find.field_mut(field).key(k) {
            FieldEvent::Changed if field == FindField::Find => {
                // Find as you type, starting from where the caret was when the widget opened.
                self.refresh_find_matches(g);
                let origin = self.groups[g].find.origin;
                self.select_match_from(g, origin, true, true);
            }
            FieldEvent::Changed | FieldEvent::Moved => {}
            FieldEvent::Ignored => {
                if let Some(cmd) = k.command() {
                    self.run(cmd);
                }
            }
        }
    }

    /// Clipboard commands while a find field has focus.
    pub(super) fn find_clipboard(&mut self, cut: bool, paste: bool, select_all: bool) {
        let g = self.active_group;
        let Some(field) = self.groups[g].find.field else { return };
        if select_all {
            return self.groups[g].find.field_mut(field).select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.groups[g].find.field_mut(field).insert(&text);
                if field == FindField::Find {
                    self.refresh_find_matches(g);
                    let origin = self.groups[g].find.origin;
                    self.select_match_from(g, origin, true, true);
                }
            }
            return;
        }
        let f = self.groups[g].find.field_mut(field);
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }

    pub(super) fn click_find_field(&mut self, g: usize, field: FindField, x: f32, shift: bool) {
        self.active_group = g;
        self.focus = Focus::Find;
        let w = &mut self.groups[g].find;
        w.field = Some(field);
        w.field_mut(field).click(x, shift);
    }

    // ------------------------------------------------------------------ drawing

    fn find_button(&mut self, c: &mut Canvas, r: Rect, icon: &Icon, hit: Hit, enabled: bool) {
        let fg = self.color("icon.foreground");
        if enabled && self.hovered(hit) {
            c.fill_rounded(r, self.color("inputOption.hoverBackground"), 3.0);
        }
        c.icon_in(icon, r, 16.0, if enabled { fg } else { fg.with_alpha(0.4) });
        if enabled {
            self.hits.push((r, hit));
        }
    }

    fn find_toggle(&mut self, c: &mut Canvas, g: usize, r: Rect, label: &str, on: bool, action: FindAction, underline: bool) {
        let hit = Hit::FindAction(g, action);
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

    fn find_input(&mut self, c: &mut Canvas, g: usize, r: Rect, field: FindField, placeholder: &str, reserve: f32, error: bool) {
        let focused = self.focus == Focus::Find && self.active_group == g && self.groups[g].find.field == Some(field);
        let border = if error {
            self.color("inputValidation.errorBorder")
        } else if focused {
            self.color("focusBorder")
        } else {
            self.color("input.border")
        };
        c.bordered(r, self.color("input.background"), border, 1.0, 2.0);
        let style = TextStyle::ui(UI, self.color("input.foreground"));
        let text_r = Rect::new(r.x + 5.0, r.y, r.w - 10.0 - reserve, r.h);
        let (ph, sel) = (self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        let caret_on = self.caret_on();
        self.groups[g].find.field_mut(field).draw(c, text_r, &style, placeholder, ph, focused, caret_on, sel);
        self.hits.push((r, Hit::FindField(g, field)));
    }

    /// Draws the widget over the editor area `editor` of group `g`.
    pub(super) fn draw_find_widget(&mut self, c: &mut Canvas, g: usize, editor: Rect) {
        if !self.groups[g].find.visible {
            return;
        }
        let w = WIDTH.min(editor.w - 40.0);
        if w < 200.0 {
            return;
        }
        let show_replace = self.groups[g].find.show_replace;
        let h = if show_replace { ROW * 2.0 - 4.0 } else { ROW };
        // Right-aligned, clear of the vertical scrollbar.
        let r = Rect::new((editor.right() - 14.0 - w).round(), editor.y, w, h);
        c.push_layer();
        c.shadow(r, 4.0, self.color("widget.shadow"));
        c.bordered(r, self.color("editorWidget.background"), self.color("editorWidget.border"), 1.0, 4.0);
        self.hits.push((r, Hit::FindWidgetBox(g)));

        // Replace toggle chevron, full height.
        let chevron = Rect::new(r.x + 2.0, r.y + 3.0, 16.0, r.h - 6.0);
        let fg = self.color("icon.foreground");
        if self.hovered(Hit::FindAction(g, FindAction::ToggleReplace)) {
            c.fill_rounded(chevron, self.color("inputOption.hoverBackground"), 3.0);
        }
        c.icon_in(if show_replace { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT }, chevron, 16.0, fg);
        self.hits.push((chevron, Hit::FindAction(g, FindAction::ToggleReplace)));

        // Find row: input (with toggles) | count | ↑ ↓ ×
        let buttons_w = 3.0 * 22.0;
        let count_w = 70.0;
        let input_x = r.x + 22.0;
        let input_w = r.w - 22.0 - count_w - buttons_w - 8.0;
        let find_r = Rect::new(input_x, r.y + 4.0, input_w, INPUT_H);
        let (no_results, error, count_label) = {
            let fw = &self.groups[g].find;
            let no_results = !fw.find.text.is_empty() && fw.matches.is_empty();
            let label = if fw.matches.is_empty() {
                "No results".to_string()
            } else {
                match fw.current {
                    Some(i) => format!("{} of {}", i + 1, fw.matches.len()),
                    None => format!("? of {}", fw.matches.len()),
                }
            };
            (no_results, fw.error.is_some(), label)
        };
        self.find_input(c, g, find_r, FindField::Find, "Find", TOGGLE * 3.0 + 4.0, no_results || error);
        let t = |i: f32| Rect::new(find_r.right() - 2.0 - TOGGLE * (3.0 - i), find_r.y + 2.5, TOGGLE, TOGGLE);
        let (cs, ww, rx) = {
            let fw = &self.groups[g].find;
            (fw.case_sensitive, fw.whole_word, fw.regex)
        };
        self.find_toggle(c, g, t(0.0), "Aa", cs, FindAction::MatchCase, false);
        self.find_toggle(c, g, t(1.0), "ab", ww, FindAction::WholeWord, true);
        self.find_toggle(c, g, t(2.0), ".*", rx, FindAction::Regex, false);
        let count_style = TextStyle::ui(12.0, if no_results { self.color("errorForeground") } else { self.color("foreground") });
        c.text_in(Rect::new(find_r.right() + 6.0, find_r.y, count_w, INPUT_H), &count_label, &count_style);
        let has = !self.groups[g].find.matches.is_empty();
        let bx = find_r.right() + 6.0 + count_w;
        let b = |i: f32| Rect::new(bx + i * 22.0, find_r.y + 2.5, 20.0, 20.0);
        self.find_button(c, b(0.0), &icons::ARROW_UP, Hit::FindAction(g, FindAction::Previous), has);
        self.find_button(c, b(1.0), &icons::ARROW_DOWN, Hit::FindAction(g, FindAction::Next), has);
        self.find_button(c, b(2.0), &icons::CLOSE, Hit::FindAction(g, FindAction::Close), true);

        if show_replace {
            let rep_r = Rect::new(input_x, find_r.bottom() + 4.0, input_w, INPUT_H);
            self.find_input(c, g, rep_r, FindField::Replace, "Replace", 0.0, false);
            let rb = |i: f32| Rect::new(rep_r.right() + 6.0 + i * 22.0, rep_r.y + 2.5, 20.0, 20.0);
            self.find_button(c, rb(0.0), &icons::REPLACE, Hit::FindAction(g, FindAction::ReplaceOne), has);
            self.find_button(c, rb(1.0), &icons::REPLACE_ALL, Hit::FindAction(g, FindAction::ReplaceAll), has);
        }
    }
}
