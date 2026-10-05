//! Emmet in the editor: the expansion of the abbreviation
//! before the caret offered in the suggest widget, Emmet: Expand Abbreviation (also Tab with
//! `emmet.triggerExpansionOnTab`) and Emmet: Wrap with Abbreviation. The engine is
//! `crate::emmet`.

use lsp::{CompletionItem, Encoding, Position, Range, TextEdit};
use text::{Pos, Selection};

use super::git_actions::GitInput;
use super::Workbench;
use crate::config::{self, EmmetSuggest};
use crate::emmet::{self, Expansion, Syntax};
use crate::palette::{InputBox, Palette};

/// How many lines before the caret are read for context (open tags, CSS rules).
const CONTEXT_LINES: usize = 300;

/// The completion kind for snippets.
const SNIPPET_KIND: u32 = 15;

impl Workbench {
    /// The Emmet syntax of a document, unless its language is in `emmet.excludeLanguages`.
    fn emmet_syntax(&self, doc: usize) -> Option<Syntax> {
        let path = self.docs[doc].as_ref()?.buffer.path()?;
        let (syntax, lang) = Syntax::of(path)?;
        let excluded = self.settings.get("emmet.excludeLanguages");
        if excluded.as_array().is_some_and(|a| a.iter().any(|v| v.as_str() == Some(lang))) {
            return None;
        }
        Some(syntax)
    }

    /// The abbreviation before the active editor's caret (one cursor, nothing selected):
    /// where it starts, the caret, and its expansion.
    fn emmet_at_caret(&self, suggest: bool) -> Option<(Pos, Pos, Expansion, Syntax)> {
        let ed = self.active_editor().filter(|e| !e.is_special() && e.extra.is_empty() && e.sel.is_empty())?;
        let syntax = self.emmet_syntax(ed.doc)?;
        let b = &self.docs[ed.doc].as_ref()?.buffer;
        let caret = ed.sel.head;
        let first = caret.line.saturating_sub(CONTEXT_LINES);
        let before = b.text_in(&Selection { anchor: Pos::new(first, 0), head: caret, goal_col: None });
        let line: String = b.line(caret.line).chars().take(caret.col).collect();
        let e = emmet::at_caret(syntax, &before, &line, suggest)?;
        Some((Pos::new(caret.line, e.start), caret, e, syntax))
    }

    /// The suggest widget's Emmet item for the caret, and whether it goes first (when the
    /// abbreviation is more than the word being completed, `prefix`).
    pub(super) fn emmet_completion(&self, encoding: Encoding, prefix: &str) -> Option<(CompletionItem, bool)> {
        let cfg = config::get();
        if cfg.emmet_suggest == EmmetSuggest::Never {
            return None;
        }
        let (start, caret, e, syntax) = self.emmet_at_caret(true)?;
        if syntax == Syntax::Jsx && cfg.emmet_suggest == EmmetSuggest::MarkupAndStylesheets {
            return None;
        }
        let line = self.active_doc()?.buffer.line(caret.line);
        let pos = |p: Pos| Position { line: p.line as u32, character: encoding.to_lsp(&line, p.col) };
        let first = e.abbr != prefix;
        let item = CompletionItem {
            label: e.abbr.clone(),
            label_detail: None,
            detail: Some("Emmet Abbreviation".into()),
            kind: SNIPPET_KIND,
            filter_text: e.abbr.clone(),
            sort_text: e.abbr.clone(),
            insert_text: e.snippet.clone(),
            // The details pane previews the expansion.
            documentation: Some(format!("```\n{}\n```", crate::snippet::parse(&e.snippet, &()).text)),
            edit: Some(TextEdit { range: Range { start: pos(start), end: pos(caret) }, new_text: e.snippet }),
            additional_edits: Vec::new(),
            snippet: true,
            command: None,
        };
        Some((item, first))
    }

    /// Emmet: Expand Abbreviation. False if there's no abbreviation before the caret.
    pub(super) fn emmet_expand(&mut self) -> bool {
        let Some((start, caret, e, _)) = self.emmet_at_caret(false) else { return false };
        self.completion = None;
        self.insert_snippet(start, caret, &e.snippet);
        true
    }

    /// Emmet: Wrap with Abbreviation: asks for the abbreviation.
    pub(super) fn emmet_wrap_prompt(&mut self) {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        if self.emmet_syntax(ed.doc).is_none_or(|s| s == Syntax::Css) {
            return;
        }
        let b = InputBox { prompt: "Enter Abbreviation".into(), placeholder: String::new(), purpose: GitInput::EmmetWrap, error: None, password: false };
        self.palette = Some(Palette::with_input(b, ""));
    }

    /// Wraps the selection (or the caret's line) with `abbr`.
    pub(super) fn emmet_wrap(&mut self, abbr: String) {
        let Some(ed) = self.active_editor().filter(|e| !e.is_special()) else { return };
        let Some(syntax) = self.emmet_syntax(ed.doc) else { return };
        let Some(doc) = self.docs[ed.doc].as_ref() else { return };
        let b = &doc.buffer;
        let (mut a, mut z) = ed.sel.ordered();
        if a == z {
            // The caret's line, without its indentation.
            a = Pos::new(a.line, b.first_non_blank(a.line));
            z = Pos::new(a.line, b.line_len(a.line));
        }
        if a == z {
            return;
        }
        // Indentation before the selection counts for the text's own indentation.
        let line = b.line(a.line);
        let head: String = line.chars().take(a.col).collect();
        let lead = if head.trim().is_empty() { head } else { String::new() };
        let text = lead + &b.text_in(&Selection { anchor: a, head: z, goal_col: None });
        let Some(snippet) = emmet::wrap(&abbr, &text, syntax) else { return };
        self.insert_snippet(a, z, &snippet);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeyInput};

    fn typed(c: char) -> KeyInput {
        KeyInput { key: Key::Char(c.to_lowercase().to_string()), text: Some(c.to_string()), cmd: false, shift: false, alt: false, ctrl: false }
    }

    fn key(k: Key) -> KeyInput {
        KeyInput { key: k, text: None, cmd: false, shift: false, alt: false, ctrl: false }
    }

    #[test]
    fn suggests_expands_and_wraps() {
        let dir = std::env::temp_dir().join(format!("orbvane-emmet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("index.html");
        std::fs::write(&file, "<body>\n\n</body>\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[], std::sync::Arc::new(|| {}));
        wb.open_file(&file);
        let text = |wb: &Workbench| wb.active_doc().unwrap().buffer.text();
        wb.active_mut().unwrap().0.set_selection(Selection::caret(Pos::new(1, 0)));

        // The suggest widget offers the expansion as the abbreviation grows past `>`.
        for c in "ul>li*2".chars() {
            wb.key(typed(c));
        }
        let comp = wb.completion.as_ref().expect("suggest widget");
        let (first, _) = comp.shown[0];
        assert_eq!(comp.items[first].label, "ul>li*2");
        wb.key(key(Key::Enter));
        let unit = crate::editor::indent_unit();
        assert_eq!(text(&wb), format!("<body>\n<ul>\n{unit}<li></li>\n{unit}<li></li>\n</ul>\n</body>\n"));
        assert_eq!(wb.active_editor().unwrap().sel.head, Pos::new(2, unit.len() + 4));

        // Emmet: Expand Abbreviation, and nothing inside a tag.
        wb.leave_snippet();
        wb.active_mut().unwrap().0.set_selection(Selection::caret(Pos::new(4, 5)));
        for c in "p.x".chars() {
            wb.key(typed(c));
        }
        wb.completion = None;
        assert!(wb.emmet_expand());
        assert!(text(&wb).contains("</ul><p class=\"x\"></p>\n"), "{}", text(&wb));
        wb.leave_snippet();
        wb.active_mut().unwrap().0.set_selection(Selection::caret(Pos::new(0, 5)));
        assert!(!wb.emmet_expand(), "inside <body>");

        // Wrap the caret's line.
        wb.active_mut().unwrap().0.set_selection(Selection::caret(Pos::new(4, 0)));
        wb.emmet_wrap("div.w".into());
        assert!(text(&wb).contains("<div class=\"w\"></ul><p class=\"x\"></p></div>"), "{}", text(&wb));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
