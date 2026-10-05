//! Language support: definitions for the built-in languages and syntax highlighting.
//!
//! Languages with a tree-sitter grammar are parsed incrementally; the rest (and any grammar
//! that fails to load) fall back to a line-based lexer.

mod lang;
mod lexer;
mod treesitter;

use std::collections::HashMap;

use text::Buffer;
use theme::Token;

pub use lang::{init, Lang, LangDef, ServerDef};
pub use treesitter::GRAMMARS;
/// The line lexer, for features that need to know what's code, string or comment (brackets).
pub use lexer::{highlight_line, LineState};

/// A highlighted byte range within one line.
pub type Span = (usize, usize, Token);

/// Per-document highlighter with a cache of spans for recently drawn lines.
pub struct Highlighter {
    lang: Lang,
    syntax: Option<treesitter::Syntax>,
    lexer: lexer::HighlightCache,
    version: Option<u64>,
    cache: HashMap<usize, Vec<Span>>,
}

impl Highlighter {
    pub fn new(lang: Lang) -> Self {
        Self::with_parser(lang, true)
    }

    /// `parse`: false highlights with the line lexer only (large files: a tree-sitter parse of
    /// the whole file would take seconds, while the lexer only looks at the lines on screen).
    pub fn with_parser(lang: Lang, parse: bool) -> Self {
        Self {
            lang,
            syntax: if parse { treesitter::Syntax::new(lang) } else { None },
            lexer: lexer::HighlightCache::default(),
            version: None,
            cache: HashMap::new(),
        }
    }

    /// Linked tag names (JSX): the byte ranges of an element's opening and closing tag names
    /// when `byte` is in one of them. Needs an up-to-date parse (`update`).
    pub fn linked_tags(&self, byte: usize) -> Option<[(usize, usize); 2]> {
        self.syntax.as_ref().filter(|s| s.is_parsed())?.linked_tags(byte)
    }

    /// True if this language is parsed with tree-sitter.
    pub fn is_tree_sitter(&self) -> bool {
        self.syntax.is_some()
    }

    /// Brings the highlighter up to date with the buffer. Call before `spans` each frame.
    pub fn update(&mut self, buffer: &mut Buffer) {
        match &mut self.syntax {
            Some(syntax) => syntax.update(buffer),
            None => self.lexer.invalidate(&buffer.take_changes()),
        }
        if self.version != Some(buffer.version()) {
            self.version = Some(buffer.version());
            self.cache.clear();
        }
    }

    /// Spans for lines `first..last`, as byte ranges within each line.
    pub fn spans(&mut self, buffer: &Buffer, first: usize, last: usize) -> Vec<Vec<Span>> {
        let last = last.min(buffer.len_lines());
        if first >= last {
            return Vec::new();
        }
        let missing: Vec<usize> = (first..last).filter(|l| !self.cache.contains_key(l)).collect();
        if let (Some(&lo), Some(&hi)) = (missing.first(), missing.last()) {
            let computed = match &self.syntax {
                Some(syntax) if syntax.is_parsed() => syntax.spans(buffer, lo, hi + 1),
                _ => (lo..=hi)
                    .map(|l| {
                        let mut out = Vec::new();
                        let state = self.lexer.state(self.lang, buffer, l);
                        lexer::highlight_line(self.lang, &buffer.line(l), state, &mut out);
                        out
                    })
                    .collect(),
            };
            for (i, spans) in computed.into_iter().enumerate() {
                self.cache.insert(lo + i, spans);
            }
        }
        if self.cache.len() > 4000 {
            self.cache.retain(|l, _| (first..last).contains(l));
        }
        (first..last).map(|l| self.cache.get(&l).cloned().unwrap_or_default()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Selection;

    fn spans_for(lang: Lang, src: &str) -> Vec<(String, Token)> {
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), src);
        let mut h = Highlighter::new(lang);
        h.update(&mut buffer);
        let lines = h.spans(&buffer, 0, buffer.len_lines());
        lines
            .iter()
            .enumerate()
            .flat_map(|(i, spans)| {
                let line = buffer.line(i);
                spans.iter().map(move |(a, b, t)| (line[*a..*b].to_string(), *t)).collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn lexer_states_follow_edits() {
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), "/* start\nstill comment\n*/ let x = 1;\nlet y = 2;");
        let mut h = Highlighter::with_parser(Lang::Rust, false);
        h.update(&mut buffer);
        let comment = |h: &mut Highlighter, b: &Buffer, line: usize| h.spans(b, line, line + 1)[0].iter().any(|s| s.2 == Token::Comment && s.0 == 0);
        assert!(comment(&mut h, &buffer, 1));
        assert!(!comment(&mut h, &buffer, 3));
        // Closing the comment on the first line un-comments the second.
        buffer.insert(Selection::caret(text::Pos::new(0, 8)), " */");
        h.update(&mut buffer);
        assert!(!comment(&mut h, &buffer, 1));
    }

    #[test]
    fn links_jsx_tag_names() {
        let src = "const a = <Foo.Bar x={1}>\n  <div>hi</div>\n  <br />\n</Foo.Bar>;";
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), src);
        let mut h = Highlighter::new(Lang::JavaScript);
        h.update(&mut buffer);
        let names = |r: Option<[(usize, usize); 2]>| r.map(|r| r.map(|(a, b)| &src[a..b]));
        let at = |s: &str| src.find(s).unwrap();
        // In the name, at its start and at its end; the closing tag links back.
        assert_eq!(names(h.linked_tags(at("Foo") + 1)), Some(["Foo.Bar", "Foo.Bar"]));
        assert_eq!(names(h.linked_tags(at("div"))), Some(["div", "div"]));
        assert_eq!(names(h.linked_tags(at("div>") + 3)), Some(["div", "div"]));
        assert_eq!(h.linked_tags(at("</Foo") + 2), h.linked_tags(at("Foo")));
        // Not in a name: attributes, text, self-closing tags, plain code.
        assert_eq!(h.linked_tags(at("x=")), None);
        assert_eq!(h.linked_tags(at("hi")), None);
        assert_eq!(h.linked_tags(at("br")), None);
        assert_eq!(h.linked_tags(at("const")), None);
    }

    #[test]
    fn all_grammars_load() {
        for lang in Lang::all().filter(|l| l.def().grammar.is_some()) {
            assert!(Highlighter::new(lang).is_tree_sitter(), "{} grammar failed to load", lang.name());
        }
        for name in GRAMMARS {
            assert!(treesitter::load_by_name(name), "{name}");
        }
    }

    #[test]
    fn rust_highlights() {
        let s = spans_for(Lang::Rust, "fn main() {\n    let v: Vec<u8> = vec![1];\n    if v.is_empty() { return; }\n}\n");
        assert!(s.contains(&("fn".into(), Token::Keyword)));
        assert!(s.contains(&("main".into(), Token::Function)));
        assert!(s.contains(&("Vec".into(), Token::Type)));
        assert!(s.contains(&("1".into(), Token::Number)));
        assert!(s.contains(&("if".into(), Token::ControlKeyword)));
        assert!(s.contains(&("return".into(), Token::ControlKeyword)));
        assert!(s.iter().any(|(t, k)| t.starts_with("vec") && *k == Token::Macro));
        assert!(s.contains(&("is_empty".into(), Token::Function)));
        assert!(s.contains(&("v".into(), Token::Variable)));
    }

    #[test]
    fn all_caps_are_constants() {
        let s = spans_for(Lang::Python, "MAX_ITEMS = 100\nclass Item: pass\n");
        assert!(s.contains(&("MAX_ITEMS".into(), Token::Constant)));
        assert!(s.contains(&("Item".into(), Token::Type)));
    }

    #[test]
    fn multiline_comment_spans_lines() {
        let s = spans_for(Lang::Rust, "/* a\nb */ let x = 1;");
        assert!(s.contains(&("/* a".into(), Token::Comment)));
        assert!(s.contains(&("b */".into(), Token::Comment)));
    }

    #[test]
    fn incremental_edit_rehighlights() {
        let mut buffer = Buffer::new();
        let sel = buffer.insert(Selection::default(), "let x = 1;");
        let mut h = Highlighter::new(Lang::Rust);
        h.update(&mut buffer);
        assert!(!h.spans(&buffer, 0, 1)[0].iter().any(|s| s.2 == Token::Comment));
        buffer.insert(Selection::caret(text::Pos::new(0, 0)), "// ");
        let _ = sel;
        h.update(&mut buffer);
        assert_eq!(h.spans(&buffer, 0, 1)[0], vec![(0, 13, Token::Comment)]);
    }

    #[test]
    fn markdown_highlights_inline_and_fenced_code() {
        let src = "# Title\n\ntext `code` and [link](http://x)\n\n```rust\nfn main() {}\n```\n\n```\nplain\n```\n";
        let s = spans_for(Lang::Markdown, src);
        eprintln!("{s:?}");
        assert!(s.contains(&("Title".into(), Token::Keyword)));
        assert!(s.iter().any(|(t, k)| t.contains("code") && *k == Token::String));
        assert!(s.iter().any(|(t, k)| t.contains("http://x") && *k == Token::String));
        // The fence's contents are Rust, not a string.
        assert!(s.contains(&("fn".into(), Token::Keyword)));
        assert!(s.contains(&("main".into(), Token::Function)));
        assert!(!s.iter().any(|(t, k)| t.contains("main") && *k == Token::String));
        assert!(!s.iter().any(|(t, _)| t.contains("plain")));
    }

    #[test]
    fn html_highlights_script_and_style() {
        let src = "<div class=\"a\">hi</div>\n<script>\nconst x = 1;\n</script>\n<style>\np { color: red; }\n</style>\n";
        let s = spans_for(Lang::from_name("html").unwrap(), src);
        assert!(s.contains(&("div".into(), Token::Keyword)));
        assert!(s.contains(&("class".into(), Token::Attribute)));
        assert!(s.contains(&("const".into(), Token::Keyword)));
        assert!(s.contains(&("1".into(), Token::Number)));
        assert!(s.contains(&("color".into(), Token::Variable)));
    }

    #[test]
    fn injections_follow_edits() {
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), "<script>\nlet a = 1;\n</script>\n");
        let mut h = Highlighter::new(Lang::from_name("html").unwrap());
        h.update(&mut buffer);
        let number = |h: &mut Highlighter, b: &Buffer| h.spans(b, 1, 2)[0].iter().any(|s| s.2 == Token::Number);
        assert!(number(&mut h, &buffer));
        // Typing a second script block reuses the JavaScript layer and highlights both.
        buffer.insert(Selection::caret(text::Pos::new(3, 0)), "<script>\nlet b = 2;\n</script>\n");
        h.update(&mut buffer);
        assert!(number(&mut h, &buffer));
        assert!(h.spans(&buffer, 4, 5)[0].iter().any(|s| s.2 == Token::Number));
    }

    #[test]
    fn new_languages_highlight() {
        let cases: &[(&str, &str, &str, Token)] = &[
            ("css", "a { color: red; }", "color", Token::Variable),
            ("scss", "$x: 1px;\na { b { margin: $x; } }", "margin", Token::Variable),
            ("yaml", "key: \"value\"\n", "\"value\"", Token::String),
            ("xml", "<?xml version=\"1.0\"?>\n<a b=\"c\"/>", "c", Token::String),
            ("java", "class A { void f() { return; } }", "return", Token::ControlKeyword),
            ("csharp", "class A { void F() { return; } }", "class", Token::Keyword),
            ("ruby", "def f\n  \"s\"\nend", "def", Token::Keyword),
            ("swift", "func f() -> Int { return 1 }", "func", Token::Keyword),
            ("lua", "local x = \"s\"", "local", Token::Keyword),
            ("sql", "SELECT a FROM t;", "SELECT", Token::Keyword),
            ("makefile", "all: x\n\techo hi\n", "all", Token::Constant),
            ("php", "<p>hi</p>\n<?php echo \"s\"; ?>", "\"s\"", Token::String),
            ("kotlin", "fun main() { val x = \"s\"; if (true) return }", "fun", Token::Keyword),
            ("zig", "const x: u8 = 1;", "const", Token::Keyword),
            ("scala", "object A { def f = 1 }", "def", Token::Keyword),
            ("haskell", "main = putStrLn \"s\"", "\"s\"", Token::String),
            ("elixir", "defmodule A do\nend", "defmodule", Token::Keyword),
            ("dockerfile", "FROM alpine\nRUN echo hi", "FROM", Token::Keyword),
            ("dart", "void main() { var x = 'a'; }", "'a'", Token::String),
            ("diff", "--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n", "x", Token::Plain),
            ("ini", "[s]\nk = v\n", "k", Token::Variable),
            ("objective-c", "@interface A : NSObject\n@end", "@interface", Token::Keyword),
            ("r", "f <- function(x) x + 1", "function", Token::Keyword),
        ];
        let mut failures = Vec::new();
        for &(id, src, text, token) in cases {
            let lang = Lang::from_name(id).unwrap();
            assert!(Highlighter::new(lang).is_tree_sitter(), "{id}");
            let s = spans_for(lang, src);
            if token != Token::Plain && !s.iter().any(|(t, k)| t == text && *k == token) {
                failures.push(format!("{id}: {text:?} not {token:?} in {s:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
