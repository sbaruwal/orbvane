//! The Problems panel's filter: a filter box in the panel's title bar
//! (text in the message, source, code or file; `!text` excludes; a path glob such as
//! `**/*.rs` picks files) and the filter menu (Show Errors / Warnings / Infos, Show Active
//! File Only). Clicking a file row collapses it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lsp::Severity;
use render::{Canvas, Rect, TextStyle};

use super::preferences::PopupAction;
use super::{Focus, Hit, PopupItem, Workbench, UI};
use crate::icons;
use crate::widgets::TextField;

pub(super) struct ProblemsFilter {
    pub field: TextField,
    pub errors: bool,
    pub warnings: bool,
    pub infos: bool,
    pub active_only: bool,
    pub collapsed: HashSet<PathBuf>,
}

impl Default for ProblemsFilter {
    fn default() -> Self {
        ProblemsFilter { field: TextField::default(), errors: true, warnings: true, infos: true, active_only: false, collapsed: HashSet::new() }
    }
}

/// Which item of the filter menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProblemsToggle {
    Errors,
    Warnings,
    Infos,
    ActiveFile,
}

/// Whether a diagnostic passes the filter text: comma-separated terms; `!term` excludes;
/// a term with `*` or `/` is a glob over the file's path, others match text.
pub(super) fn text_matches(filter: &str, rel_path: &str, d: &lsp::Diagnostic) -> bool {
    let haystack = format!("{} {} {} {}", d.message, d.source.as_deref().unwrap_or_default(), d.code.as_deref().unwrap_or_default(), rel_path).to_lowercase();
    let term_matches = |t: &str| {
        if t.contains('*') || t.contains('/') {
            search::GlobSet::parse(t).is_ok_and(|g| g.matches(rel_path))
        } else {
            haystack.contains(&t.to_lowercase())
        }
    };
    let (mut includes, mut any_include) = (false, false);
    for term in filter.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match term.strip_prefix('!') {
            Some(ex) if !ex.is_empty() => {
                if term_matches(ex) {
                    return false;
                }
            }
            Some(_) => {}
            None => {
                any_include = true;
                includes |= term_matches(term);
            }
        }
    }
    includes || !any_include
}

impl Workbench {
    /// Whether diagnostic `d` of `path` is shown.
    pub(super) fn problem_shown(&self, path: &Path, d: &lsp::Diagnostic) -> bool {
        let f = &self.problems_filter;
        let severity_ok = match d.severity {
            Severity::Error => f.errors,
            Severity::Warning => f.warnings,
            _ => f.infos,
        };
        if !severity_ok {
            return false;
        }
        if f.active_only && self.active_doc().and_then(|doc| doc.buffer.path()) != Some(path) {
            return false;
        }
        let root = self.folder();
        let rel = root.as_deref().and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path).to_string_lossy().into_owned();
        text_matches(&f.field.text, &rel, d)
    }

    /// Whether any filter is set (the message and badge say "Showing N of M").
    pub(super) fn problems_filtered(&self) -> bool {
        let f = &self.problems_filter;
        !f.field.text.trim().is_empty() || !f.errors || !f.warnings || !f.infos || f.active_only
    }

    /// The filter box and menu button in the panel's title bar, left of `right`.
    pub(super) fn draw_problems_filter(&mut self, c: &mut Canvas, header: Rect, right: f32) {
        let focused = self.focus == Focus::ProblemsFilter && self.palette.is_none();
        let w = 240.0f32.min(right - header.x - 420.0).max(0.0);
        if w < 60.0 {
            return;
        }
        let funnel = Rect::new(right - 26.0, header.y + 7.0, 24.0, 22.0);
        let fg = self.color("icon.foreground");
        let active = self.problems_filtered();
        if active {
            c.fill_rounded(funnel, self.color("inputOption.activeBackground"), 3.0);
        }
        self.icon_button(c, funnel, &icons::FILTER, Hit::ProblemsFilterMenu, fg);
        let field = Rect::new(funnel.x - 4.0 - w, header.y + 6.0, w, 24.0);
        let border = if focused { self.color("focusBorder") } else { self.color_or("input.border", "input.background") };
        c.bordered(field, self.color("input.background"), border, 1.0, 2.0);
        let caret_on = self.editor_caret_on();
        let (fg, ph, sel) = (self.color("input.foreground"), self.color("input.placeholderForeground"), self.color("editor.selectionBackground"));
        self.problems_filter.field.draw(c, Rect::new(field.x + 6.0, field.y, field.w - 12.0, field.h), &TextStyle::ui(UI, fg), "Filter (e.g. text, **/*.ts, !**/node_modules/**)", ph, focused, caret_on, sel);
        self.hits.push((field, Hit::ProblemsFilterField));
    }

    pub(super) fn problems_filter_menu(&mut self, x: f32, y: f32) {
        let f = &self.problems_filter;
        let item = |label: &str, checked: bool| PopupItem::Item { label: label.into(), enabled: true, checked: Some(checked) };
        let entries = vec![
            (item("Show Errors", f.errors), PopupAction::ProblemsToggle(ProblemsToggle::Errors)),
            (item("Show Warnings", f.warnings), PopupAction::ProblemsToggle(ProblemsToggle::Warnings)),
            (item("Show Infos", f.infos), PopupAction::ProblemsToggle(ProblemsToggle::Infos)),
            (PopupItem::Separator, PopupAction::None),
            (item("Show Active File Only", f.active_only), PopupAction::ProblemsToggle(ProblemsToggle::ActiveFile)),
        ];
        self.show_popup(entries, x, y);
    }

    pub(super) fn problems_toggle(&mut self, t: ProblemsToggle) {
        let f = &mut self.problems_filter;
        match t {
            ProblemsToggle::Errors => f.errors = !f.errors,
            ProblemsToggle::Warnings => f.warnings = !f.warnings,
            ProblemsToggle::Infos => f.infos = !f.infos,
            ProblemsToggle::ActiveFile => f.active_only = !f.active_only,
        }
        self.problems_scroll = 0.0;
    }

    pub(super) fn problems_filter_key(&mut self, k: &crate::input::KeyInput) {
        use crate::input::Key;
        match k.key {
            Key::Escape if !self.problems_filter.field.text.is_empty() => self.problems_filter.field.set_text(""),
            Key::Escape => self.focus = Focus::Editor,
            _ => {
                self.problems_filter.field.key(k);
                self.problems_scroll = 0.0;
            }
        }
    }

    pub(super) fn problems_filter_clipboard(&mut self, cut: bool, paste: bool, all: bool) {
        let f = &mut self.problems_filter.field;
        if all {
            return f.select_all();
        }
        if paste {
            if let Some(text) = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()) {
                self.problems_filter.field.insert(&text);
            }
            return;
        }
        let text = if cut { f.cut() } else { f.copy() };
        if let (Some(text), Some(cb)) = (text, &mut self.clipboard) {
            let _ = cb.set_text(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(msg: &str, source: &str) -> lsp::Diagnostic {
        lsp::Diagnostic {
            range: lsp::Range { start: lsp::Position { line: 0, character: 0 }, end: lsp::Position { line: 0, character: 1 } },
            severity: Severity::Error,
            message: msg.into(),
            source: Some(source.into()),
            code: Some("E0308".into()),
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn filter_terms() {
        let d = diag("mismatched types", "rustc");
        assert!(text_matches("", "src/main.rs", &d));
        assert!(text_matches("MISMATCHED", "src/main.rs", &d));
        assert!(text_matches("e0308", "src/main.rs", &d));
        assert!(text_matches("nope, rustc", "src/main.rs", &d));
        assert!(!text_matches("nope", "src/main.rs", &d));
        assert!(!text_matches("!rustc", "src/main.rs", &d));
        assert!(text_matches("**/*.rs", "src/main.rs", &d));
        assert!(!text_matches("!src/**", "src/main.rs", &d));
        assert!(!text_matches("**/*.py", "src/main.rs", &d));
    }
}
