//! Editor features over a parsed stylesheet: completion, hovers, the outline, folding, colors,
//! and references to variables, classes and ids. Positions are byte offsets.

use crate::colors;
use crate::data::{data, Entry, HTML_TAGS, LESS_AT_RULES, SCSS_AT_RULES};
use crate::parse::{Kind, Stylesheet, Syntax, Token, T};

/// LSP completion item kinds.
pub const KIND_FUNCTION: u32 = 3;
pub const KIND_VARIABLE: u32 = 6;
pub const KIND_PROPERTY: u32 = 10;
pub const KIND_UNIT: u32 = 11;
pub const KIND_VALUE: u32 = 12;
pub const KIND_KEYWORD: u32 = 14;
pub const KIND_COLOR: u32 = 16;

#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    pub kind: u32,
    pub documentation: Option<String>,
    pub insert: String,
    /// `insert` is a snippet.
    pub snippet: bool,
    pub sort: String,
    pub range: (usize, usize),
    /// Open the suggestions again after inserting (a property, then its value).
    pub retrigger: bool,
}

impl Item {
    pub fn new(label: impl Into<String>, kind: u32, range: (usize, usize)) -> Item {
        let label = label.into();
        Item { insert: label.clone(), sort: label.clone(), label, kind, documentation: None, snippet: false, range, retrigger: false }
    }

    pub fn docs(mut self, d: Option<String>) -> Item {
        self.documentation = d.filter(|d| !d.is_empty());
        self
    }

    pub fn snippet(mut self, s: impl Into<String>) -> Item {
        self.insert = s.into();
        self.snippet = true;
        self
    }

    pub fn sort(mut self, s: impl Into<String>) -> Item {
        self.sort = s.into();
        self
    }
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80
}

/// The word around `offset` (letters, digits, `-`, `_`, and a leading `@`, `$` or `!`).
pub fn word_at(text: &str, offset: usize) -> (usize, usize) {
    let b = text.as_bytes();
    let mut start = offset.min(b.len());
    while start > 0 && is_word(b[start - 1]) {
        start -= 1;
    }
    if start > 0 && matches!(b[start - 1], b'@' | b'$' | b'!') {
        start -= 1;
    }
    let mut end = offset.min(b.len());
    while end < b.len() && is_word(b[end]) {
        end += 1;
    }
    (start, end)
}

/// Sorts by relevance, vendor-prefixed names last.
fn relevance_sort(e: &Entry) -> String {
    let vendor = e.name.starts_with('-');
    format!("{}{:03}{}", if vendor { "x" } else { "d" }, 1000 - e.relevance().clamp(0, 999), e.name)
}

pub fn complete(s: &Stylesheet, offset: usize) -> Vec<Item> {
    let id = s.node_at(offset);
    let n = s.node(id);
    let word = word_at(&s.text, offset);
    let typed = &s.text[word.0..offset.max(word.0)];
    let in_block = |n: &crate::parse::Node| matches!(n.block, Some((open, _)) if offset > open);
    match n.kind {
        Kind::Declaration | Kind::Variable => match n.colon {
            Some(colon) if offset > colon => {
                let property = if n.kind == Kind::Declaration { s.slice(n.name).to_ascii_lowercase() } else { String::new() };
                values(s, &property, offset, word)
            }
            _ if n.kind == Kind::Declaration && typed.starts_with('@') => at_rules(s, word),
            _ if n.kind == Kind::Declaration => properties(s, if offset <= n.name.1 { n.name } else { word }),
            _ => Vec::new(),
        },
        Kind::RuleSet if in_block(n) => statement(s, offset, word, true),
        Kind::RuleSet => selectors(s, offset, word),
        Kind::AtRule if offset <= n.name.1 => at_rules(s, n.name),
        Kind::AtRule if in_block(n) => statement(s, offset, word, s.holds_declarations(id)),
        Kind::AtRule => Vec::new(),
        Kind::Stylesheet => statement(s, offset, word, n.block.is_some()),
    }
}

/// Completion where a new statement starts: properties in a declaration block, else selectors;
/// `@` starts an at-rule either way.
fn statement(s: &Stylesheet, offset: usize, word: (usize, usize), declarations: bool) -> Vec<Item> {
    let typed = &s.text[word.0..offset.max(word.0)];
    if typed.starts_with('@') {
        return at_rules(s, word);
    }
    if declarations {
        properties(s, word)
    } else {
        selectors(s, offset, word)
    }
}

fn properties(s: &Stylesheet, range: (usize, usize)) -> Vec<Item> {
    let rest = s.text[range.1..].trim_start_matches([' ', '\t']);
    // Editing the name of a property that already has its colon: just the name.
    let has_colon = rest.starts_with(':');
    let semicolon = if rest.starts_with(';') { "" } else { ";" };
    data()
        .properties
        .iter()
        .filter(|e| !e.obsolete())
        .map(|e| {
            let mut item = Item::new(&e.name, KIND_PROPERTY, range).docs(Some(e.markdown())).sort(relevance_sort(e));
            if !has_colon {
                item = item.snippet(format!("{}: $0{semicolon}", e.name));
                item.retrigger = true;
            }
            item
        })
        .collect()
}

fn at_rules(s: &Stylesheet, range: (usize, usize)) -> Vec<Item> {
    let mut items: Vec<Item> = data().at_directives.iter().map(|e| Item::new(&e.name, KIND_KEYWORD, range).docs(Some(e.markdown()))).collect();
    let extra = match s.syntax {
        Syntax::Scss => SCSS_AT_RULES,
        Syntax::Less => LESS_AT_RULES,
        Syntax::Css => &[],
    };
    for (name, d) in extra {
        if !items.iter().any(|i| i.label == *name) {
            items.push(Item::new(*name, KIND_KEYWORD, range).docs(Some(d.to_string())));
        }
    }
    items
}

fn selectors(s: &Stylesheet, offset: usize, word: (usize, usize)) -> Vec<Item> {
    let b = s.text.as_bytes();
    let typed = &s.text[word.0..offset.max(word.0)];
    if typed.starts_with('@') {
        return at_rules(s, word);
    }
    // After `:` or `::`: pseudo-classes and pseudo-elements.
    let colons = b[..word.0].iter().rev().take_while(|&&c| c == b':').count().min(2);
    if colons > 0 {
        let range = (word.0 - colons, word.1);
        let mut items = Vec::new();
        let lists: &[&Vec<Entry>] = if colons == 2 { &[&data().pseudo_elements] } else { &[&data().pseudo_classes, &data().pseudo_elements] };
        for list in lists {
            for e in list.iter().filter(|e| !e.obsolete()) {
                let mut item = Item::new(e.name.trim_end_matches("()"), KIND_FUNCTION, range).docs(Some(e.markdown())).sort(relevance_sort(e));
                if let Some(name) = e.name.strip_suffix("()") {
                    item = item.snippet(format!("{name}($1)"));
                }
                items.push(item);
            }
        }
        return items;
    }
    // After `.` or `#` there's nothing to offer but what's typed.
    if word.0 > 0 && matches!(b[word.0 - 1], b'.' | b'#') {
        return Vec::new();
    }
    HTML_TAGS.iter().map(|t| Item::new(*t, KIND_KEYWORD, word)).collect()
}

/// Custom properties (`--x`) declared in the stylesheet.
fn custom_properties(s: &Stylesheet) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for n in &s.nodes {
        if n.kind == Kind::Declaration {
            let name = s.slice(n.name);
            if name.starts_with("--") && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

fn values(s: &Stylesheet, property: &str, offset: usize, word: (usize, usize)) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let typed = &s.text[word.0..offset.max(word.0)];
    // Inside `var(`: the custom properties.
    if s.text[..word.0].trim_end().ends_with("var(") {
        return custom_properties(s).into_iter().map(|v| Item::new(v, KIND_VARIABLE, word)).collect();
    }
    let entry = data().property(property);
    if let Some(e) = entry {
        for v in &e.values {
            let mut item = Item::new(&v.name, KIND_VALUE, word).docs(v.description.clone()).sort(format!("{}{}", if v.name.starts_with('-') { "x" } else { "d" }, v.name));
            if let Some(name) = v.name.strip_suffix("()") {
                item = item.snippet(format!("{name}($1)"));
                item.label = v.name.clone();
            }
            items.push(item);
        }
        if e.restricted_to("color") {
            for (name, rgb) in colors::NAMED {
                items.push(Item::new(*name, KIND_COLOR, word).docs(Some(format!("#{rgb:06x}"))));
            }
            for name in ["currentColor", "transparent"] {
                items.push(Item::new(name, KIND_VALUE, word));
            }
            for (name, snippet, d) in colors::FUNCTIONS {
                items.push(Item::new(*name, KIND_FUNCTION, word).snippet(*snippet).docs(Some(d.to_string())));
            }
            // Colors already used in this file.
            for c in used_colors(s) {
                if !items.iter().any(|i| i.label.eq_ignore_ascii_case(c)) {
                    items.push(Item::new(c, KIND_COLOR, word).sort(format!("a{c}")));
                }
            }
        }
        if e.restricted_to("timing-function") {
            items.push(Item::new("cubic-bezier()", KIND_FUNCTION, word).snippet("cubic-bezier(${1:0.1}, ${2:0.7}, ${3:1.0}, ${4:0.1})").docs(Some("Specifies a cubic-bezier curve.".into())));
            items.push(Item::new("steps()", KIND_FUNCTION, word).snippet("steps(${1:2}, ${2:start})").docs(Some("Specifies a stepping function.".into())));
        }
        if e.restricted_to("image") || e.restricted_to("url") {
            items.push(Item::new("url()", KIND_FUNCTION, word).snippet("url($1)").docs(Some("Reference an image file by URL".into())));
        }
        if e.restricted_to("image") {
            for (name, d) in [
                ("linear-gradient", "A linear gradient image."),
                ("radial-gradient", "A radial gradient image."),
                ("repeating-linear-gradient", "Same as linear-gradient, except the color-stops are repeated infinitely."),
                ("repeating-radial-gradient", "Same as radial-gradient, except the color-stops are repeated infinitely."),
            ] {
                items.push(Item::new(format!("{name}()"), KIND_FUNCTION, word).snippet(format!("{name}($1)")).docs(Some(d.into())));
            }
        }
        // `10` → `10px`...
        if !typed.is_empty() && typed.bytes().all(|c| c.is_ascii_digit() || c == b'.' || c == b'-') && typed.bytes().any(|c| c.is_ascii_digit()) {
            let mut units: Vec<&str> = Vec::new();
            if e.restricted_to("length") {
                units.extend(["px", "em", "rem", "%", "vh", "vw", "vmin", "vmax", "ch", "ex", "cm", "mm", "in", "pt", "pc"]);
            } else if e.restricted_to("percentage") {
                units.push("%");
            }
            if e.restricted_to("time") {
                units.extend(["ms", "s"]);
            }
            if e.restricted_to("angle") {
                units.extend(["deg", "rad", "grad", "turn"]);
            }
            for (i, u) in units.iter().enumerate() {
                items.push(Item::new(format!("{typed}{u}"), KIND_UNIT, (word.0, word.1)).sort(format!("a{i:02}")));
            }
        }
    }
    for (name, d) in [
        ("inherit", "Represents the value specified as the property's computed value in the parent element."),
        ("initial", "Represents the value specified as the property's initial value."),
        ("unset", "Acts as either `inherit` or `initial`, depending on whether the property is inherited or not."),
        ("revert", "Resets the property to the value it would have had if no changes had been made by the current style origin."),
        ("revert-layer", "Rolls back the value to the value in the previous cascade layer."),
    ] {
        items.push(Item::new(name, KIND_KEYWORD, word).docs(Some(d.into())).sort(format!("z{name}")));
    }
    for v in custom_properties(s) {
        items.push(Item::new(format!("var({v})"), KIND_VARIABLE, word).sort(format!("y{v}")));
    }
    let (prefix, token) = match s.syntax {
        Syntax::Scss => ('$', T::Variable),
        Syntax::Less => ('@', T::AtKeyword),
        Syntax::Css => (' ', T::Eof),
    };
    if token != T::Eof {
        for n in s.nodes.iter().filter(|n| n.kind == Kind::Variable) {
            let name = s.slice(n.name);
            if name.starts_with(prefix) && !items.iter().any(|i| i.label == name) {
                items.push(Item::new(name, KIND_VARIABLE, word).sort(format!("a{name}")));
            }
        }
    }
    if typed.starts_with('!') {
        items.push(Item::new("!important", KIND_KEYWORD, word));
    }
    items
}

/// Color texts used in values (`#fff`, `rgb(...)`), for completion.
fn used_colors(s: &Stylesheet) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for (start, end, _) in document_colors(s) {
        let c = &s.text[start..end];
        if !out.contains(&c) && colors::named(c).is_none() {
            out.push(c);
        }
    }
    out
}

/// The colors in values: `(start, end, rgba)`.
pub fn document_colors(s: &Stylesheet) -> Vec<(usize, usize, colors::Rgba)> {
    let mut out = Vec::new();
    for n in s.nodes.iter().filter(|n| matches!(n.kind, Kind::Declaration | Kind::Variable)) {
        let toks: Vec<&Token> = s.tokens_in(n.value).collect();
        for (i, t) in toks.iter().enumerate() {
            let text = &s.text[t.start..t.end];
            match t.kind {
                T::Hash => {
                    if let Some(c) = colors::hex(text) {
                        out.push((t.start, t.end, c));
                    }
                }
                T::Ident => {
                    if let Some(c) = colors::named(text) {
                        out.push((t.start, t.end, c));
                    }
                }
                T::Function => {
                    let Some(close) = toks[i + 1..].iter().find(|c| c.kind == T::ParenR) else { continue };
                    if let Some(c) = colors::function(&text[..text.len() - 1], &s.text[t.end..close.start]) {
                        out.push((t.start, close.end, c));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Specificity of one selector (ids, classes/attributes/pseudo-classes, types).
pub fn specificity(selector: &str) -> (u32, u32, u32) {
    let s = Stylesheet::parse(&format!("{selector}{{}}"), Syntax::Css);
    let toks: Vec<Token> = s.tokens.iter().copied().filter(|t| t.end <= selector.len()).collect();
    let mut spec = (0, 0, 0);
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        let text = &selector[t.start..t.end];
        match t.kind {
            T::Hash => spec.0 += 1,
            T::Delim(b'.') => {
                spec.1 += 1;
                i += 1;
            }
            T::BracketL => {
                spec.1 += 1;
                while i < toks.len() && toks[i].kind != T::BracketR {
                    i += 1;
                }
            }
            T::Colon => {
                let elem = toks.get(i + 1).is_some_and(|n| n.kind == T::Colon);
                if elem {
                    spec.2 += 1;
                    i += 2;
                } else if let Some(next) = toks.get(i + 1) {
                    let name = selector[next.start..next.end].trim_end_matches('(').to_ascii_lowercase();
                    if next.kind == T::Function {
                        // `:not()`, `:is()`, `:has()` count their most specific argument;
                        // `:where()` counts nothing.
                        let mut depth = 1;
                        let mut j = i + 2;
                        let start = toks.get(j).map_or(selector.len(), |t| t.start);
                        while j < toks.len() && depth > 0 {
                            match toks[j].kind {
                                T::ParenL | T::Function => depth += 1,
                                T::ParenR => depth -= 1,
                                _ => {}
                            }
                            j += 1;
                        }
                        let end = toks.get(j - 1).map_or(selector.len(), |t| t.start);
                        if matches!(name.as_str(), "not" | "is" | "has" | "matches") {
                            let inner = selector[start..end.max(start)].split(',').map(specificity).max().unwrap_or_default();
                            spec = (spec.0 + inner.0, spec.1 + inner.1, spec.2 + inner.2);
                        } else if name != "where" {
                            spec.1 += 1;
                        }
                        i = j;
                        continue;
                    }
                    // Legacy pseudo-elements with one colon.
                    if matches!(name.as_str(), "before" | "after" | "first-line" | "first-letter") {
                        spec.2 += 1;
                    } else {
                        spec.1 += 1;
                    }
                    i += 1;
                }
            }
            T::Ident if text != "*" => {
                let prev = i.checked_sub(1).map(|p| toks[p].kind);
                if !matches!(prev, Some(T::Delim(b'.')) | Some(T::Colon)) {
                    spec.2 += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    spec
}

/// The selector hover: an element preview and the specificity.
fn selector_hover(selector: &str) -> String {
    let s = selector.trim();
    // Compounds and their combinators.
    let mut lines: Vec<String> = Vec::new();
    let mut indent = 0;
    for (i, part) in s.split([' ', '>', '+', '~']).filter(|p| !p.is_empty()).enumerate() {
        let mut tag = String::new();
        let mut id = None;
        let mut classes = Vec::new();
        let mut rest = part;
        let take = |r: &str| r.find(['.', '#', '[', ':']).unwrap_or(r.len());
        let n = take(rest);
        tag.push_str(&rest[..n]);
        rest = &rest[n..];
        while !rest.is_empty() {
            let kind = rest.as_bytes()[0];
            let n = take(&rest[1..]) + 1;
            let value = &rest[1..n];
            match kind {
                b'#' => id = Some(value.to_string()),
                b'.' => classes.push(value.to_string()),
                _ => {}
            }
            rest = &rest[n..];
        }
        let tag = if tag.is_empty() || tag == "*" || tag == "&" { "element".to_string() } else { tag };
        let mut line = format!("<{tag}");
        if let Some(id) = id {
            line.push_str(&format!(" id=\"{id}\""));
        }
        if !classes.is_empty() {
            line.push_str(&format!(" class=\"{}\"", classes.join(" ")));
        }
        line.push('>');
        if i > 0 {
            lines.push(format!("{}…", "  ".repeat(indent)));
            indent += 1;
        }
        lines.push(format!("{}{line}", "  ".repeat(indent)));
        indent += 1;
    }
    let (a, b, c) = specificity(s);
    format!("```html\n{}\n```\n[Selector Specificity](https://developer.mozilla.org/docs/Web/CSS/Specificity): ({a}, {b}, {c})", lines.join("\n"))
}

pub fn hover(s: &Stylesheet, offset: usize) -> Option<(String, (usize, usize))> {
    let id = s.node_at(offset);
    let n = s.node(id);
    let within = |(a, b): (usize, usize)| a <= offset && offset <= b && a < b;
    match n.kind {
        Kind::Declaration if within(n.name) => {
            let e = data().property(s.slice(n.name))?;
            Some((e.markdown(), n.name))
        }
        Kind::AtRule if within(n.name) => {
            let e = data().at_directive(s.slice(n.name))?;
            Some((e.markdown(), n.name))
        }
        Kind::RuleSet if within(n.name) => {
            // The comma-separated selector under the cursor.
            let mut start = n.name.0;
            for part in s.slice(n.name).split(',') {
                let end = start + part.len();
                if offset <= end {
                    let lead = part.len() - part.trim_start().len();
                    let range = (start + lead, start + part.trim_end().len());
                    return Some((selector_hover(part), range));
                }
                start = end + 1;
            }
            None
        }
        _ => None,
    }
}

/// An outline entry.
#[derive(Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: u32,
    pub range: (usize, usize),
    pub selection: (usize, usize),
    pub children: Vec<Symbol>,
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn symbols(s: &Stylesheet) -> Vec<Symbol> {
    fn walk(s: &Stylesheet, id: usize) -> Vec<Symbol> {
        let mut out = Vec::new();
        for &c in &s.node(id).children {
            let n = s.node(c);
            let (name, kind, selection) = match n.kind {
                Kind::RuleSet => (collapse(s.slice(n.name)), 5, n.name),
                Kind::AtRule if n.block.is_some() => {
                    let end = n.value.1.max(n.name.1);
                    (collapse(&s.text[n.name.0..end]), 2, (n.name.0, end))
                }
                Kind::Variable => (s.slice(n.name).to_string(), 13, n.name),
                Kind::Declaration if s.slice(n.name).starts_with("--") => (s.slice(n.name).to_string(), 13, n.name),
                _ => continue,
            };
            if name.is_empty() {
                continue;
            }
            out.push(Symbol { name, kind, range: (n.start, n.end), selection, children: walk(s, c) });
        }
        out
    }
    walk(s, 0)
}

/// Foldable regions: multi-line blocks and comments, and `/* #region */` ... `/* #endregion */`.
/// Blocks are `(open brace, close brace)`.
pub fn folding(s: &Stylesheet) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = s.nodes.iter().filter_map(|n| match n.block {
        Some((open, Some(close))) => Some((open, close + 1)),
        _ => None,
    }).collect();
    let mut regions = Vec::new();
    for t in s.tokens.iter().filter(|t| t.kind == T::Comment) {
        let text = &s.text[t.start..t.end];
        let body = text.trim_start_matches(['/', '*']).trim_start();
        if body.starts_with("#region") {
            regions.push(t.start);
        } else if body.starts_with("#endregion") {
            if let Some(start) = regions.pop() {
                out.push((start, t.end));
            }
        } else if text.starts_with("/*") {
            out.push((t.start, t.end));
        }
    }
    out
}

/// What can be found and renamed: a variable, custom property, class or id.
#[derive(Debug, PartialEq, Eq)]
enum Symbolic {
    Name(String),
    Class(String),
    Id(String),
}

/// The symbol at `offset` and the byte range of its name.
fn symbol_at(s: &Stylesheet, offset: usize) -> Option<(Symbolic, (usize, usize))> {
    let i = s.tokens.iter().position(|t| t.start <= offset && offset <= t.end && !matches!(t.kind, T::Whitespace | T::Eof))?;
    // At a token boundary, prefer the token ending here only if the next one isn't a name.
    let i = match s.tokens.get(i + 1) {
        Some(n) if n.start == offset && matches!(n.kind, T::Ident | T::Variable | T::AtKeyword | T::Hash) => i + 1,
        _ => i,
    };
    let t = s.tokens[i];
    let text = &s.text[t.start..t.end];
    let in_selector = s.nodes.iter().any(|n| n.kind == Kind::RuleSet && n.name.0 <= t.start && t.end <= n.name.1);
    match t.kind {
        T::Ident if text.starts_with("--") => Some((Symbolic::Name(text.into()), (t.start, t.end))),
        T::Variable => Some((Symbolic::Name(text.into()), (t.start, t.end))),
        T::AtKeyword if s.syntax == Syntax::Less && s.nodes.iter().any(|n| n.kind == Kind::Variable && s.slice(n.name) == text) => {
            Some((Symbolic::Name(text.into()), (t.start, t.end)))
        }
        T::Ident if in_selector && i > 0 && s.tokens[i - 1].kind == T::Delim(b'.') => Some((Symbolic::Class(text.into()), (t.start, t.end))),
        T::Hash if in_selector => Some((Symbolic::Id(text[1..].into()), (t.start + 1, t.end))),
        _ => None,
    }
}

/// Every occurrence of the symbol at `offset` (name ranges), and the definitions among them.
pub fn references(s: &Stylesheet, offset: usize) -> Option<(Vec<(usize, usize)>, Vec<(usize, usize)>)> {
    let (sym, _) = symbol_at(s, offset)?;
    let mut refs = Vec::new();
    let mut defs = Vec::new();
    for (i, t) in s.tokens.iter().enumerate() {
        let text = &s.text[t.start..t.end];
        let in_selector = || s.nodes.iter().any(|n| n.kind == Kind::RuleSet && n.name.0 <= t.start && t.end <= n.name.1);
        let found = match &sym {
            Symbolic::Name(name) => text == name && matches!(t.kind, T::Ident | T::Variable | T::AtKeyword),
            Symbolic::Class(name) => t.kind == T::Ident && text == name && i > 0 && s.tokens[i - 1].kind == T::Delim(b'.') && in_selector(),
            Symbolic::Id(name) => t.kind == T::Hash && &text[1..] == name && in_selector(),
        };
        if !found {
            continue;
        }
        let range = if matches!(sym, Symbolic::Id(_)) { (t.start + 1, t.end) } else { (t.start, t.end) };
        refs.push(range);
        if s.nodes.iter().any(|n| matches!(n.kind, Kind::Declaration | Kind::Variable) && n.name == (t.start, t.end)) {
            defs.push(range);
        }
    }
    Some((refs, defs))
}

/// The range of the renamable name at `offset`.
pub fn rename_range(s: &Stylesheet, offset: usize) -> Option<(usize, usize)> {
    symbol_at(s, offset).map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_at(text_with_caret: &str, syntax: Syntax) -> Vec<Item> {
        let offset = text_with_caret.find('|').unwrap();
        let text = text_with_caret.replacen('|', "", 1);
        complete(&Stylesheet::parse(&text, syntax), offset)
    }

    fn has(items: &[Item], label: &str) -> bool {
        items.iter().any(|i| i.label == label)
    }

    #[test]
    fn completes_properties_values_and_selectors() {
        let items = complete_at("a { col| }", Syntax::Css);
        let color = items.iter().find(|i| i.label == "color").unwrap();
        assert_eq!(color.insert, "color: $0;");
        assert!(color.retrigger);
        assert_eq!(color.range, (4, 7));
        // An existing colon: only the name.
        let items = complete_at("a { col|: red; }", Syntax::Css);
        assert_eq!(items.iter().find(|i| i.label == "color").unwrap().insert, "color");
        let items = complete_at("a { display: f| }", Syntax::Css);
        assert!(has(&items, "flex") && has(&items, "inherit"));
        let items = complete_at("a { color: | }", Syntax::Css);
        assert!(has(&items, "rebeccapurple") && has(&items, "rgb") && has(&items, "currentColor"));
        let items = complete_at("a { width: 10| }", Syntax::Css);
        assert!(has(&items, "10px") && has(&items, "10%"));
        let items = complete_at("a:ho| {}", Syntax::Css);
        let hover = items.iter().find(|i| i.label == ":hover").unwrap();
        assert_eq!(hover.range, (1, 4));
        assert!(has(&complete_at("|", Syntax::Css), "div"));
        assert!(has(&complete_at("@m|", Syntax::Css), "@media"));
        let items = complete_at(":root { --main: red; } a { color: var(|) }", Syntax::Css);
        assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["--main"]);
        assert!(has(&complete_at("$c: red; a { color: | }", Syntax::Scss), "$c"));
        // Nested rules in SCSS blocks still get properties.
        assert!(has(&complete_at(".a { .b { mar| } }", Syntax::Scss), "margin"));
        // At-rules that hold rules offer selectors.
        assert!(has(&complete_at("@media screen { d| }", Syntax::Css), "div"));
    }

    #[test]
    fn hovers_and_specificity() {
        let text = "#main .card > a:hover, p { color: red; }";
        let s = Stylesheet::parse(text, Syntax::Css);
        let (h, range) = hover(&s, 2).unwrap();
        assert_eq!(&text[range.0..range.1], "#main .card > a:hover");
        assert!(h.contains("(1, 2, 1)"), "{h}");
        assert!(h.contains("<element id=\"main\">"), "{h}");
        assert!(hover(&s, text.find("color").unwrap()).unwrap().0.contains("MDN Reference"));
        assert_eq!(specificity(":not(#a) b::before"), (1, 0, 2));
        assert_eq!(specificity(":where(.a) li"), (0, 0, 1));
    }

    #[test]
    fn finds_colors_symbols_and_references() {
        let text = ":root { --c: #ff0000; }\n.a { color: var(--c); background: rgb(0, 0, 255); border-color: red; }\n.a:hover {}";
        let s = Stylesheet::parse(text, Syntax::Css);
        let colors: Vec<&str> = document_colors(&s).iter().map(|(a, b, _)| &text[*a..*b]).collect();
        assert_eq!(colors, ["#ff0000", "rgb(0, 0, 255)", "red"]);
        let names: Vec<String> = symbols(&s).into_iter().map(|s| s.name).collect();
        assert_eq!(names, [":root", ".a", ".a:hover"]);
        let (refs, defs) = references(&s, text.find("var(--c").unwrap() + 5).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(defs.len(), 1);
        let (refs, _) = references(&s, text.find(".a").unwrap() + 1).unwrap();
        assert_eq!(refs.len(), 2);
    }
}
