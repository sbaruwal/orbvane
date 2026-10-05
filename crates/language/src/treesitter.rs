//! Tree-sitter parsing and highlighting.
//!
//! Each document keeps a syntax tree that is edited incrementally as the buffer changes. Only
//! the lines being drawn are highlighted, by running the grammar's highlight query over their
//! byte range.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use text::{Buffer, Change};
use theme::Token;
use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, Tree};

use crate::lang::{Lang, LangDef};
use crate::Span;

pub(crate) struct Grammar {
    pub name: &'static str,
    pub language: tree_sitter::Language,
    pub query: Query,
    /// Per pattern: whether it tests the node's text (`#eq?`, `#any-of?`, `#match?`). Those
    /// are more specific than the patterns without, wherever they are in the query.
    predicated: Vec<bool>,
    /// Where other languages are embedded (`<script>`, Markdown code fences).
    pub injections: Option<Query>,
}

/// The built-in grammars, by the name `languages.json` uses. `markdown_inline` is only used
/// inside Markdown.
pub const GRAMMARS: &[&str] = &[
    "rust", "python", "go", "c", "cpp", "javascript", "typescript", "tsx", "json", "toml", "bash", "html", "css", "scss",
    "yaml", "xml", "java", "c_sharp", "ruby", "swift", "lua", "sql", "make", "php", "kotlin", "zig", "scala", "haskell",
    "elixir", "markdown", "markdown_inline", "dockerfile", "dart", "diff", "ini", "objc", "r",
];

/// Loads (once) the grammar and highlight query for `lang`.
pub(crate) fn grammar(lang: Lang) -> Option<&'static Grammar> {
    by_name(lang.def().grammar?)
}

/// Loads (once) a grammar by name. Each loads on first use: compiling a query takes a while.
fn by_name(name: &str) -> Option<&'static Grammar> {
    static LOADED: OnceLock<HashMap<&'static str, OnceLock<Option<Grammar>>>> = OnceLock::new();
    let all = LOADED.get_or_init(|| GRAMMARS.iter().map(|&g| (g, OnceLock::new())).collect());
    let (&name, cell) = all.get_key_value(name)?;
    cell.get_or_init(|| load(name)).as_ref()
}

/// The grammar an injection names: a grammar name, or a language's id, alias or extension
/// (Markdown fences say "rs", "py", "sh"...).
fn injected_grammar(name: &str) -> Option<&'static Grammar> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let lang = || Lang::from_name(&name).or_else(|| Some(Lang::detect(Some(std::path::Path::new(&format!("x.{name}"))))));
    by_name(&name).or_else(|| grammar(lang()?))
}

#[cfg(test)]
pub(crate) fn load_by_name(name: &'static str) -> bool {
    load(name).is_some()
}

fn load(name: &'static str) -> Option<Grammar> {
    // Some grammars build on another's query (C++ on C, TypeScript on JavaScript). The trailing
    // `@variable` patterns come last so they only apply where nothing more specific matched.
    let ts = || format!("{}\n{}", tree_sitter_typescript::HIGHLIGHTS_QUERY, tree_sitter_javascript::HIGHLIGHT_QUERY);
    let (language, query, injections): (tree_sitter::Language, String, &str) = match name {
        "rust" => (tree_sitter_rust::LANGUAGE.into(), format!("{}\n(identifier) @variable", tree_sitter_rust::HIGHLIGHTS_QUERY), ""),
        "python" => (tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY.into(), ""),
        "go" => (tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY.into(), ""),
        "c" => (tree_sitter_c::LANGUAGE.into(), format!("{}\n(identifier) @variable", tree_sitter_c::HIGHLIGHT_QUERY), ""),
        "cpp" => (
            tree_sitter_cpp::LANGUAGE.into(),
            format!("{}\n{}\n(identifier) @variable", tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY),
            "",
        ),
        "javascript" => (tree_sitter_javascript::LANGUAGE.into(), tree_sitter_javascript::HIGHLIGHT_QUERY.into(), ""),
        "typescript" => (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), ts(), ""),
        "tsx" => (tree_sitter_typescript::LANGUAGE_TSX.into(), ts(), ""),
        "json" => (tree_sitter_json::LANGUAGE.into(), tree_sitter_json::HIGHLIGHTS_QUERY.into(), ""),
        "toml" => (tree_sitter_toml_ng::LANGUAGE.into(), tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(), ""),
        "bash" => (tree_sitter_bash::LANGUAGE.into(), tree_sitter_bash::HIGHLIGHT_QUERY.into(), ""),
        "html" => (tree_sitter_html::LANGUAGE.into(), tree_sitter_html::HIGHLIGHTS_QUERY.into(), tree_sitter_html::INJECTIONS_QUERY),
        "css" => (tree_sitter_css::LANGUAGE.into(), tree_sitter_css::HIGHLIGHTS_QUERY.into(), ""),
        // SCSS's query only adds to CSS's.
        "scss" => (tree_sitter_scss::language(), format!("{}\n{}", tree_sitter_scss::HIGHLIGHTS_QUERY, tree_sitter_css::HIGHLIGHTS_QUERY), ""),
        "yaml" => (tree_sitter_yaml::LANGUAGE.into(), tree_sitter_yaml::HIGHLIGHTS_QUERY.into(), ""),
        "xml" => (tree_sitter_xml::LANGUAGE_XML.into(), tree_sitter_xml::XML_HIGHLIGHT_QUERY.into(), ""),
        "java" => (tree_sitter_java::LANGUAGE.into(), tree_sitter_java::HIGHLIGHTS_QUERY.into(), ""),
        "c_sharp" => (tree_sitter_c_sharp::LANGUAGE.into(), tree_sitter_c_sharp::HIGHLIGHTS_QUERY.into(), ""),
        "ruby" => (tree_sitter_ruby::LANGUAGE.into(), tree_sitter_ruby::HIGHLIGHTS_QUERY.into(), ""),
        "swift" => (tree_sitter_swift::LANGUAGE.into(), tree_sitter_swift::HIGHLIGHTS_QUERY.into(), ""),
        "lua" => (tree_sitter_lua::LANGUAGE.into(), tree_sitter_lua::HIGHLIGHTS_QUERY.into(), ""),
        "sql" => (tree_sitter_sequel::LANGUAGE.into(), tree_sitter_sequel::HIGHLIGHTS_QUERY.into(), ""),
        "make" => (tree_sitter_make::LANGUAGE.into(), tree_sitter_make::HIGHLIGHTS_QUERY.into(), ""),
        // PHP files are HTML outside `<?php ?>`.
        "php" => (
            tree_sitter_php::LANGUAGE_PHP.into(),
            tree_sitter_php::HIGHLIGHTS_QUERY.into(),
            "((text) @injection.content (#set! injection.language \"html\"))",
        ),
        "kotlin" => (tree_sitter_kotlin_ng::LANGUAGE.into(), include_str!("../queries/kotlin.scm").into(), ""),
        "zig" => (tree_sitter_zig::LANGUAGE.into(), tree_sitter_zig::HIGHLIGHTS_QUERY.into(), ""),
        "scala" => (tree_sitter_scala::LANGUAGE.into(), tree_sitter_scala::HIGHLIGHTS_QUERY.into(), ""),
        "haskell" => (tree_sitter_haskell::LANGUAGE.into(), tree_sitter_haskell::HIGHLIGHTS_QUERY.into(), ""),
        "elixir" => (tree_sitter_elixir::LANGUAGE.into(), tree_sitter_elixir::HIGHLIGHTS_QUERY.into(), ""),
        "markdown" => (tree_sitter_md::LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(), tree_sitter_md::INJECTION_QUERY_BLOCK),
        "markdown_inline" => (tree_sitter_md::INLINE_LANGUAGE.into(), tree_sitter_md::HIGHLIGHT_QUERY_INLINE.into(), ""),
        "dockerfile" => (
            tree_sitter_containerfile::LANGUAGE.into(),
            tree_sitter_containerfile::HIGHLIGHTS_QUERY.into(),
            tree_sitter_containerfile::INJECTIONS_QUERY,
        ),
        "dart" => (tree_sitter_dart::LANGUAGE.into(), tree_sitter_dart::HIGHLIGHTS_QUERY.into(), ""),
        "diff" => (tree_sitter_diff::LANGUAGE.into(), tree_sitter_diff::HIGHLIGHTS_QUERY.into(), ""),
        "ini" => (tree_sitter_ini::LANGUAGE.into(), tree_sitter_ini::HIGHLIGHTS_QUERY.into(), ""),
        "objc" => (
            tree_sitter_objc::LANGUAGE.into(),
            format!("{}\n{}\n(identifier) @variable", tree_sitter_objc::HIGHLIGHTS_QUERY, tree_sitter_c::HIGHLIGHT_QUERY),
            "",
        ),
        "r" => (tree_sitter_r::LANGUAGE.into(), tree_sitter_r::HIGHLIGHTS_QUERY.into(), ""),
        _ => return None,
    };
    // `lua-match?` patterns are plain regexes in the queries we ship.
    let query = query.replace("#lua-match?", "#match?");
    let compile = |kind: &str, source: &str| match Query::new(&language, source) {
        Ok(query) => Some(query),
        Err(e) => {
            eprintln!("orbvane: {kind} query for {name} failed to load: {e}");
            None
        }
    };
    let mut highlights = compile("highlight", &query)?;
    // Predicates we can't evaluate (`is-not? local`, `has-ancestor?`) would make their patterns
    // match everywhere.
    for i in 0..highlights.pattern_count() {
        if !highlights.general_predicates(i).is_empty() || !highlights.property_predicates(i).is_empty() {
            highlights.disable_pattern(i);
        }
    }
    let predicated = (0..highlights.pattern_count())
        .map(|i| {
            let source = &query[highlights.start_byte_for_pattern(i)..highlights.end_byte_for_pattern(i)];
            ["#eq?", "#any-of?", "#match?"].iter().any(|p| source.contains(p))
        })
        .collect();
    let injections = if injections.is_empty() { None } else { compile("injection", injections) };
    Some(Grammar { name, language, query: highlights, predicated, injections })
}

/// How deep injections nest (Markdown → HTML → JavaScript).
const MAX_DEPTH: usize = 3;

/// One parsed language in a document: the document's own, or one embedded in it.
struct Layer {
    grammar: &'static Grammar,
    /// For control keywords (`refine`); `None` for grammars no language uses directly.
    def: Option<&'static LangDef>,
    depth: usize,
    parser: Parser,
    tree: Option<Tree>,
    /// The document byte ranges this layer covers (empty for the document's own layer).
    ranges: Vec<tree_sitter::Range>,
    /// Theme token per query capture index.
    capture_tokens: Vec<Option<Token>>,
}

impl Layer {
    fn new(grammar: &'static Grammar, def: Option<&'static LangDef>, depth: usize) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&grammar.language).ok()?;
        let capture_tokens = grammar.query.capture_names().iter().map(|n| capture_token(n)).collect();
        Some(Self { grammar, def, depth, parser, tree: None, ranges: Vec::new(), capture_tokens })
    }

    fn parse(&mut self, buffer: &Buffer) {
        if self.depth > 0 && self.parser.set_included_ranges(&self.ranges).is_err() {
            self.tree = None;
            return;
        }
        let len = buffer.len_bytes();
        let mut read = |byte: usize, _: Point| -> &[u8] {
            if byte >= len {
                return &[];
            }
            let (chunk, start) = buffer.chunk_at_byte(byte);
            &chunk.as_bytes()[byte - start..]
        };
        self.tree = self.parser.parse_with_options(&mut read, self.tree.as_ref(), None);
    }

    /// The ranges this layer embeds, by grammar.
    fn injections(&self, buffer: &Buffer, out: &mut Vec<(&'static Grammar, Vec<tree_sitter::Range>)>) {
        let (Some(query), Some(tree)) = (&self.grammar.injections, &self.tree) else { return };
        let content = query.capture_index_for_name("injection.content");
        let language = query.capture_index_for_name("injection.language");
        let mut cursor = QueryCursor::new();
        let text = |node: tree_sitter::Node| buffer.byte_chunks(node.byte_range()).map(str::as_bytes);
        let mut matches = cursor.matches(query, tree.root_node(), text);
        while let Some(m) = matches.next() {
            let named = || {
                let prop = query.property_settings(m.pattern_index).iter().find(|p| &*p.key == "injection.language");
                prop.and_then(|p| p.value.as_deref()).map(str::to_string)
            };
            let name = m
                .captures()
                .iter()
                .find(|c| Some(c.index) == language)
                .map(|c| buffer.byte_chunks(c.node.byte_range()).collect::<String>())
                .or_else(named);
            let Some(grammar) = name.as_deref().and_then(injected_grammar) else { continue };
            let slot = match out.iter().position(|(g, _)| g.name == grammar.name) {
                Some(i) => i,
                None => {
                    out.push((grammar, Vec::new()));
                    out.len() - 1
                }
            };
            for cap in m.captures().iter().filter(|c| Some(c.index) == content) {
                // The node minus its named children (`> ` prefixes in Markdown quotes).
                let node = cap.node;
                let mut start = node.start_byte();
                let mut start_point = node.start_position();
                let mut walk = node.walk();
                let ranges = &mut out[slot].1;
                for child in node.named_children(&mut walk) {
                    if child.start_byte() > start {
                        ranges.push(tree_sitter::Range { start_byte: start, end_byte: child.start_byte(), start_point, end_point: child.start_position() });
                    }
                    start = start.max(child.end_byte());
                    start_point = child.end_position();
                }
                if node.end_byte() > start {
                    ranges.push(tree_sitter::Range { start_byte: start, end_byte: node.end_byte(), start_point, end_point: node.end_position() });
                }
            }
        }
    }
}

pub(crate) struct Syntax {
    /// The document's own language first, then embedded ones by depth.
    layers: Vec<Layer>,
}

fn point((row, column): (usize, usize)) -> Point {
    Point { row, column }
}

impl Syntax {
    pub fn new(lang: Lang) -> Option<Self> {
        Some(Self { layers: vec![Layer::new(grammar(lang)?, Some(lang.def()), 0)?] })
    }

    /// Applies pending buffer edits to the trees and reparses.
    pub fn update(&mut self, buffer: &mut Buffer) {
        let changes = buffer.take_changes();
        if changes.is_empty() && self.layers[0].tree.is_some() {
            return;
        }
        for change in changes {
            match change {
                Change::Edit(e) => {
                    let edit = InputEdit {
                        start_byte: e.start_byte,
                        old_end_byte: e.old_end_byte,
                        new_end_byte: e.new_end_byte,
                        start_position: point(e.start),
                        old_end_position: point(e.old_end),
                        new_end_position: point(e.new_end),
                    };
                    for tree in self.layers.iter_mut().filter_map(|l| l.tree.as_mut()) {
                        tree.edit(&edit);
                    }
                }
                Change::Reset => self.layers.iter_mut().for_each(|l| l.tree = None),
            }
        }
        self.layers[0].parse(buffer);

        // Reparse the embedded languages, depth by depth, reusing each one's old tree.
        let mut old: Vec<Layer> = self.layers.drain(1..).collect();
        let mut parent = 0;
        while parent < self.layers.len() {
            let depth = self.layers[parent].depth + 1;
            let mut found = Vec::new();
            for layer in self.layers.iter().filter(|l| l.depth + 1 == depth) {
                layer.injections(buffer, &mut found);
            }
            parent = self.layers.len();
            if depth > MAX_DEPTH {
                break;
            }
            for (grammar, mut ranges) in found {
                ranges.sort_by_key(|r| r.start_byte);
                ranges.dedup_by(|b, a| b.start_byte < a.end_byte);
                if ranges.is_empty() {
                    continue;
                }
                let reuse = old.iter().position(|l| l.depth == depth && l.grammar.name == grammar.name);
                let def = Lang::from_name(grammar.name).map(Lang::def);
                let Some(mut layer) = reuse.map(|i| old.swap_remove(i)).or_else(|| Layer::new(grammar, def, depth)) else { continue };
                layer.ranges = ranges;
                layer.parse(buffer);
                self.layers.push(layer);
            }
        }
    }

    pub fn is_parsed(&self) -> bool {
        self.layers[0].tree.is_some()
    }

    /// The names of a JSX element's opening and closing tags, as byte ranges, when `byte` is
    /// in (or at either end of) one of them.
    pub fn linked_tags(&self, byte: usize) -> Option<[(usize, usize); 2]> {
        let mut node = self.layers[0].tree.as_ref()?.root_node().descendant_for_byte_range(byte, byte)?;
        while !matches!(node.kind(), "jsx_opening_element" | "jsx_closing_element") {
            node = node.parent()?;
        }
        let name = node.child_by_field_name("name")?;
        if byte < name.start_byte() || byte > name.end_byte() {
            return None;
        }
        let element = node.parent().filter(|p| p.kind() == "jsx_element")?;
        let range = |tag: &str| element.child_by_field_name(tag)?.child_by_field_name("name").map(|n| (n.start_byte(), n.end_byte()));
        Some([range("open_tag")?, range("close_tag")?])
    }

    /// Highlight spans (byte ranges within each line) for lines `first..last`.
    pub fn spans(&self, buffer: &Buffer, first: usize, last: usize) -> Vec<Vec<Span>> {
        if !self.is_parsed() {
            return vec![Vec::new(); last - first];
        }
        let start = buffer.line_to_byte(first);
        let end = buffer.line_to_byte(last);
        let line_starts: Vec<usize> = (first..=last).map(|l| buffer.line_to_byte(l)).collect();

        // Collect captures, then paint them so that embedded languages win over the document's,
        // and smaller (inner) nodes win over the nodes containing them. For the same node, a
        // specific capture (function, type, ...) beats a generic variable/property one, then a
        // pattern that tests the text beats one that doesn't, and otherwise the earlier query
        // pattern wins.
        let mut captures: Vec<(usize, usize, usize, (bool, Reverse<usize>), Token)> = Vec::new();
        for layer in &self.layers {
            let Some(tree) = &layer.tree else { continue };
            let query = &layer.grammar.query;
            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(start..end);
            let text = |node: tree_sitter::Node| buffer.byte_chunks(node.byte_range()).map(str::as_bytes);
            let mut it = cursor.captures(query, tree.root_node(), text);
            while let Some((m, i)) = it.next() {
                let cap = m.captures()[*i];
                let Some(token) = layer.capture_tokens[cap.index as usize] else { continue };
                let node = cap.node;
                let token = refine(layer.def, token, node, buffer);
                let specific = layer.grammar.predicated[m.pattern_index];
                captures.push((layer.depth, node.start_byte(), node.end_byte(), (specific, Reverse(m.pattern_index)), token));
            }
        }
        let generic = |t: Token| t == Token::Variable;
        captures.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then((b.2 - b.1).cmp(&(a.2 - a.1)))
                .then(generic(b.4).cmp(&generic(a.4)))
                .then(a.3.cmp(&b.3))
        });

        // One slot per byte of line content (line breaks excluded).
        let mut paint: Vec<Vec<Option<Token>>> = (first..last).map(|l| vec![None; buffer.line(l).len()]).collect();
        for (_, s, e, _, token) in captures {
            let (s, e) = (s.max(start), e.min(end));
            if s >= e {
                continue;
            }
            let mut line = line_starts.partition_point(|&b| b <= s) - 1;
            while line < paint.len() && line_starts[line] < e {
                let ls = line_starts[line];
                let from = s.saturating_sub(ls);
                let to = (e - ls).min(paint[line].len());
                // `Plain` (`@none`, `@embedded`) clears what a containing node painted.
                for slot in paint[line].iter_mut().take(to).skip(from) {
                    *slot = Some(token).filter(|&t| t != Token::Plain);
                }
                line += 1;
            }
        }

        paint
            .into_iter()
            .map(|bytes| {
                let mut spans: Vec<Span> = Vec::new();
                for (i, t) in bytes.into_iter().enumerate() {
                    let Some(t) = t else { continue };
                    match spans.last_mut() {
                        Some(last) if last.1 == i && last.2 == t => last.1 = i + 1,
                        _ => spans.push((i, i + 1, t)),
                    }
                }
                spans
            })
            .collect()
    }
}

/// Adjusts generic captures to the coloring: control-flow keywords are purple and
/// numeric "constants" are numbers.
fn refine(def: Option<&LangDef>, token: Token, node: tree_sitter::Node, buffer: &Buffer) -> Token {
    // Grammars often tag any capitalized identifier as a type/constructor; ALL_CAPS names
    // are constants in every language we ship.
    if matches!(token, Token::Type | Token::Variable) && node.byte_range().len() > 1 && node.byte_range().len() <= 64 {
        let word: String = buffer.byte_chunks(node.byte_range()).collect();
        if word.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            && word.bytes().any(|c| c.is_ascii_uppercase())
        {
            return Token::Constant;
        }
    }
    if !matches!(token, Token::Keyword | Token::Constant) {
        return token;
    }
    let kind = node.kind();
    if kind.contains("integer") || kind.contains("float") || kind.contains("number") {
        return Token::Number;
    }
    if token == Token::Keyword && node.byte_range().len() <= 16 {
        let word: String = buffer.byte_chunks(node.byte_range()).collect();
        if def.is_some_and(|d| d.control.contains(&word.as_str())) {
            return Token::ControlKeyword;
        }
    }
    token
}

/// Maps a tree-sitter capture name (`function.method`, `keyword.control`, ...) to a theme token.
fn capture_token(name: &str) -> Option<Token> {
    let base = name.split('.').next().unwrap_or(name);
    Some(match base {
        "comment" => Token::Comment,
        "string" if name.starts_with("string.special.key") => Token::Variable,
        "string" if name.starts_with("string.special.symbol") => Token::Constant,
        "string" | "escape" | "character" => Token::String,
        "number" | "float" => Token::Number,
        "boolean" => Token::Keyword,
        "constant" if name == "constant.builtin" => Token::Keyword,
        "constant" => Token::Constant,
        "keyword" | "conditional" | "repeat" | "exception" | "include" | "storageclass" | "preproc" => {
            let control = ["control", "return", "conditional", "repeat", "exception", "import", "include", "coroutine", "directive", "preproc"];
            if control.iter().any(|c| name.contains(c)) {
                Token::ControlKeyword
            } else {
                Token::Keyword
            }
        }
        "type" | "constructor" | "module" | "namespace" => Token::Type,
        "function" | "method" if name.contains("macro") => Token::Macro,
        "function" | "method" => Token::Function,
        "variable" if name == "variable.builtin" => Token::Keyword,
        "variable" | "property" | "field" | "parameter" => Token::Variable,
        "attribute" => Token::Attribute,
        "label" | "lifetime" => Token::Lifetime,
        "punctuation" | "operator" | "delimiter" => Token::Punctuation,
        "tag" if name == "tag.attribute" => Token::Attribute,
        "tag" if name == "tag.delimiter" => Token::Punctuation,
        "tag" if name == "tag" || name == "tag.builtin" => Token::Keyword,
        // Markdown (`markup.*` names and the older `text.*` ones).
        "markup" | "text" => match name.split('.').nth(1) {
            Some("heading" | "title") => Token::Keyword,
            Some("raw" | "literal" | "link" | "uri" | "reference") => Token::String,
            Some("quote") => Token::Comment,
            _ => return None,
        },
        "embedded" | "none" => Token::Plain,
        _ => return None,
    })
}

