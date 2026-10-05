//! A tokenizer and error-tolerant parser for CSS, SCSS and Less: rules, at-rules and
//! declarations with their byte ranges (enough for completion, hovers, outline and lint;
//! selectors and values stay token lists).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    Css,
    Scss,
    Less,
}

impl Syntax {
    pub fn of(language_id: &str) -> Syntax {
        match language_id {
            "scss" => Syntax::Scss,
            "less" => Syntax::Less,
            _ => Syntax::Css,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum T {
    Ident,
    /// `@media`, or a Less variable.
    AtKeyword,
    /// `#fff` or `#id`.
    Hash,
    String,
    BadString,
    Number,
    Percentage,
    Dimension,
    /// `url(...)` without quotes.
    Url,
    /// `name(` (the token ends after the parenthesis).
    Function,
    Delim(u8),
    Colon,
    Semicolon,
    Comma,
    CurlyL,
    CurlyR,
    ParenL,
    ParenR,
    BracketL,
    BracketR,
    Whitespace,
    Comment,
    /// SCSS `$name`.
    Variable,
    /// SCSS `#{...}` or Less `@{...}`.
    Interpolation,
    Eof,
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: T,
    pub start: usize,
    pub end: usize,
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 || c == b'\\'
}

fn is_name(c: u8) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == b'-'
}

pub fn tokenize(text: &str, syntax: Syntax) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let name_end = |mut j: usize| {
        while j < b.len() && is_name(b[j]) {
            j += if b[j] == b'\\' { 2 } else { 1 };
        }
        j.min(b.len())
    };
    // An identifier starts at `j`: a name start, or `-` followed by one (or `--`).
    let ident_at = |j: usize| match b.get(j) {
        Some(&c) if is_name_start(c) => true,
        Some(b'-') => b.get(j + 1).is_some_and(|&c| is_name_start(c) || c == b'-'),
        _ => false,
    };
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0c) {
                    i += 1;
                }
                T::Whitespace
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = match text[i + 2..].find("*/") {
                    Some(k) => i + 2 + k + 2,
                    None => b.len(),
                };
                T::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'/') && syntax != Syntax::Css => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                T::Comment
            }
            b'"' | b'\'' => {
                i += 1;
                let mut kind = T::BadString;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        b'\n' => break,
                        q if q == c => {
                            i += 1;
                            kind = T::String;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                i = i.min(b.len());
                kind
            }
            b'#' if b.get(i + 1) == Some(&b'{') && syntax == Syntax::Scss => {
                i = interpolation_end(b, i + 1);
                T::Interpolation
            }
            b'@' if b.get(i + 1) == Some(&b'{') && syntax == Syntax::Less => {
                i = interpolation_end(b, i + 1);
                T::Interpolation
            }
            b'#' if b.get(i + 1).is_some_and(|&n| is_name(n)) => {
                i = name_end(i + 1);
                T::Hash
            }
            b'@' if ident_at(i + 1) => {
                i = name_end(i + 1);
                T::AtKeyword
            }
            b'$' if syntax == Syntax::Scss && ident_at(i + 1) => {
                i = name_end(i + 1);
                T::Variable
            }
            b'0'..=b'9' | b'.' | b'+' | b'-'
                if c.is_ascii_digit()
                    || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
                    || ((c == b'+' || c == b'-')
                        && (b.get(i + 1).is_some_and(u8::is_ascii_digit)
                            || (b.get(i + 1) == Some(&b'.') && b.get(i + 2).is_some_and(u8::is_ascii_digit)))) =>
            {
                if c == b'+' || c == b'-' {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i + 1 < b.len() && (b[i] == b'e' || b[i] == b'E') && (b[i + 1].is_ascii_digit() || (matches!(b[i + 1], b'+' | b'-') && b.get(i + 2).is_some_and(u8::is_ascii_digit))) {
                    i += 2;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if b.get(i) == Some(&b'%') {
                    i += 1;
                    T::Percentage
                } else if ident_at(i) {
                    i = name_end(i);
                    T::Dimension
                } else {
                    T::Number
                }
            }
            _ if ident_at(i) => {
                i = name_end(i);
                if b.get(i) == Some(&b'(') {
                    if text[start..i].eq_ignore_ascii_case("url") {
                        // An unquoted URL runs to the parenthesis.
                        let mut j = i + 1;
                        while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n') {
                            j += 1;
                        }
                        if !matches!(b.get(j), Some(b'"' | b'\'')) {
                            i = text[j..].find(')').map_or(b.len(), |k| j + k + 1);
                            out.push(Token { kind: T::Url, start, end: i });
                            continue;
                        }
                    }
                    i += 1;
                    T::Function
                } else {
                    T::Ident
                }
            }
            b':' => {
                i += 1;
                T::Colon
            }
            b';' => {
                i += 1;
                T::Semicolon
            }
            b',' => {
                i += 1;
                T::Comma
            }
            b'{' => {
                i += 1;
                T::CurlyL
            }
            b'}' => {
                i += 1;
                T::CurlyR
            }
            b'(' => {
                i += 1;
                T::ParenL
            }
            b')' => {
                i += 1;
                T::ParenR
            }
            b'[' => {
                i += 1;
                T::BracketL
            }
            b']' => {
                i += 1;
                T::BracketR
            }
            _ => {
                i += 1;
                while i < b.len() && (b[i] & 0xC0) == 0x80 {
                    i += 1;
                }
                T::Delim(c)
            }
        };
        out.push(Token { kind, start, end: i });
    }
    out.push(Token { kind: T::Eof, start: b.len(), end: b.len() });
    out
}

/// The end of a `{...}` that starts at `open`, nesting included.
fn interpolation_end(b: &[u8], open: usize) -> usize {
    let mut depth = 0;
    for (k, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            _ => {}
        }
    }
    b.len()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Stylesheet,
    RuleSet,
    AtRule,
    Declaration,
    /// SCSS `$x: 1;` or Less `@x: 1;`.
    Variable,
}

#[derive(Debug)]
pub struct Node {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// RuleSet: the selector. AtRule: the at-keyword. Declaration: the property. Variable: its name.
    pub name: (usize, usize),
    /// Declaration and Variable: the colon.
    pub colon: Option<usize>,
    /// Declaration and Variable: the value. AtRule: its prelude (between the name and `{`/`;`).
    pub value: (usize, usize),
    /// The `{` of a block, and its `}` if there is one.
    pub block: Option<(usize, Option<usize>)>,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub start: usize,
    pub end: usize,
    pub message: &'static str,
}

pub struct Stylesheet {
    pub text: String,
    pub syntax: Syntax,
    pub tokens: Vec<Token>,
    pub nodes: Vec<Node>,
    pub errors: Vec<Error>,
}

/// At-rules whose blocks hold rules (at the top level); the others hold declarations.
pub fn holds_rules(at_rule: &str) -> bool {
    matches!(
        at_rule.to_ascii_lowercase().as_str(),
        "@media" | "@supports" | "@layer" | "@container" | "@document" | "@-moz-document" | "@scope" | "@starting-style" | "@keyframes" | "@-webkit-keyframes"
    )
}

struct Parser<'a> {
    text: &'a str,
    syntax: Syntax,
    toks: &'a [Token],
    pos: usize,
    nodes: Vec<Node>,
    errors: Vec<Error>,
}

impl Parser<'_> {
    fn tok(&self) -> Token {
        self.toks[self.pos]
    }

    fn kind(&self) -> T {
        self.toks[self.pos].kind
    }

    fn bump(&mut self) {
        if self.kind() != T::Eof {
            self.pos += 1;
        }
    }

    fn skip_trivia(&mut self) {
        while matches!(self.kind(), T::Whitespace | T::Comment) || self.at_cdo_cdc() {
            self.bump();
        }
    }

    /// `<!--` and `-->` are allowed (and ignored) between rules.
    fn at_cdo_cdc(&self) -> bool {
        let t = self.tok();
        matches!(t.kind, T::Delim(b'<') | T::Delim(b'-')) && (self.text[t.start..].starts_with("<!--") || self.text[t.start..].starts_with("-->"))
    }

    fn error(&mut self, start: usize, end: usize, message: &'static str) {
        if self.errors.last().is_some_and(|e| e.start == start) {
            return;
        }
        self.errors.push(Error { start, end, message });
    }

    /// Where the last non-trivia token before the current one ends.
    fn last_end(&self) -> usize {
        self.toks[..self.pos].iter().rev().find(|t| !matches!(t.kind, T::Whitespace | T::Comment)).map_or(0, |t| t.end)
    }

    fn push(&mut self, kind: Kind, start: usize, parent: Option<usize>) -> usize {
        self.nodes.push(Node { kind, start, end: start, parent, children: Vec::new(), name: (start, start), colon: None, value: (start, start), block: None });
        let id = self.nodes.len() - 1;
        if let Some(p) = parent {
            self.nodes[p].children.push(id);
        }
        id
    }

    /// Statements until `}` (inside a block) or the end. `declarations`: the block holds
    /// declarations (a rule's, or a nested at-rule's).
    fn statements(&mut self, parent: usize, in_block: bool, declarations: bool) {
        loop {
            self.skip_trivia();
            if self.at_cdo_cdc() {
                self.bump();
                continue;
            }
            let t = self.tok();
            match t.kind {
                T::Eof => return,
                T::CurlyR if in_block => return,
                T::CurlyR => {
                    self.error(t.start, t.end, "at-rule or selector expected");
                    self.bump();
                }
                T::Semicolon => self.bump(),
                T::AtKeyword if self.syntax == Syntax::Less && self.next_is_colon() => self.variable(parent),
                T::AtKeyword => self.at_rule(parent, declarations),
                T::Variable => self.variable(parent),
                _ if declarations && self.declaration_ahead() => self.declaration(parent),
                _ => self.rule_set(parent),
            }
        }
    }

    /// The next non-trivia token after the current one is a colon.
    fn next_is_colon(&self) -> bool {
        self.toks[self.pos + 1..].iter().find(|t| !matches!(t.kind, T::Whitespace | T::Comment)).is_some_and(|t| t.kind == T::Colon)
    }

    /// Whether the statement here is a declaration: it ends (`;`, `}`) before any `{`.
    fn declaration_ahead(&self) -> bool {
        let mut depth = 0i32;
        for t in &self.toks[self.pos..] {
            match t.kind {
                T::ParenL | T::Function | T::BracketL => depth += 1,
                T::ParenR | T::BracketR => depth -= 1,
                T::CurlyL if depth <= 0 => return false,
                T::Semicolon | T::CurlyR | T::Eof if depth <= 0 => return true,
                _ => {}
            }
        }
        true
    }

    fn rule_set(&mut self, parent: usize) {
        let start = self.tok().start;
        let id = self.push(Kind::RuleSet, start, Some(parent));
        let mut depth = 0i32;
        loop {
            match self.kind() {
                T::ParenL | T::Function | T::BracketL => depth += 1,
                T::ParenR | T::BracketR => depth -= 1,
                T::CurlyL if depth <= 0 => break,
                T::Semicolon | T::CurlyR | T::Eof if depth <= 0 => break,
                _ => {}
            }
            self.bump();
        }
        let selector_end = self.last_end().max(start);
        self.nodes[id].name = (start, selector_end);
        if self.kind() != T::CurlyL {
            // A Less mixin call (`.mixin();`) is a statement of its own.
            let mixin = self.syntax == Syntax::Less && self.kind() == T::Semicolon && self.text[start..selector_end].ends_with(')');
            if !mixin {
                self.error(selector_end, selector_end, "{ expected");
            }
            if self.kind() == T::Semicolon {
                self.bump();
            }
            self.nodes[id].end = self.last_end().max(selector_end);
            return;
        }
        if selector_end == start {
            self.error(self.tok().start, self.tok().end, "at-rule or selector expected");
        }
        self.block(id, true);
    }

    /// A `{ ... }` block for node `id`.
    fn block(&mut self, id: usize, declarations: bool) {
        let open = self.tok().start;
        self.bump();
        self.nodes[id].block = Some((open, None));
        self.statements(id, true, declarations);
        if self.kind() == T::CurlyR {
            let close = self.tok().start;
            self.nodes[id].block = Some((open, Some(close)));
            self.bump();
            self.nodes[id].end = close + 1;
        } else {
            self.error(self.tok().start, self.tok().start, "} expected");
            self.nodes[id].end = self.text.len();
        }
    }

    fn at_rule(&mut self, parent: usize, in_declarations: bool) {
        let t = self.tok();
        let id = self.push(Kind::AtRule, t.start, Some(parent));
        self.nodes[id].name = (t.start, t.end);
        self.bump();
        self.skip_trivia();
        let prelude_start = self.tok().start;
        let mut depth = 0i32;
        loop {
            match self.kind() {
                T::ParenL | T::Function | T::BracketL => depth += 1,
                T::ParenR | T::BracketR => depth -= 1,
                T::CurlyL | T::Semicolon | T::CurlyR | T::Eof if depth <= 0 => break,
                _ => {}
            }
            self.bump();
        }
        let prelude_end = self.last_end().max(prelude_start);
        self.nodes[id].value = (prelude_start.min(prelude_end), prelude_end);
        self.nodes[id].end = prelude_end;
        match self.kind() {
            T::CurlyL => {
                // Nested inside a rule, any at-rule's block holds declarations.
                let declarations = in_declarations || !holds_rules(&self.text[t.start..t.end]);
                self.block(id, declarations);
            }
            T::Semicolon => {
                self.bump();
                self.nodes[id].end = self.last_end();
            }
            _ => {}
        }
    }

    fn variable(&mut self, parent: usize) {
        let t = self.tok();
        let id = self.push(Kind::Variable, t.start, Some(parent));
        self.nodes[id].name = (t.start, t.end);
        self.bump();
        self.skip_trivia();
        if self.kind() == T::Colon {
            self.nodes[id].colon = Some(self.tok().start);
            self.bump();
        } else {
            self.error(self.last_end(), self.last_end(), "colon expected");
        }
        self.value(id);
    }

    fn declaration(&mut self, parent: usize) {
        let start = self.tok().start;
        let id = self.push(Kind::Declaration, start, Some(parent));
        // The property: an identifier, maybe with an IE hack (`*zoom`, `_height`) or
        // interpolation.
        while !matches!(self.kind(), T::Colon | T::Semicolon | T::CurlyR | T::Eof | T::Whitespace | T::Comment) {
            self.bump();
        }
        let name_end = self.last_end().max(start);
        self.nodes[id].name = (start, name_end);
        self.skip_trivia();
        if self.kind() != T::Colon {
            self.error(name_end, name_end, "colon expected");
            while !matches!(self.kind(), T::Semicolon | T::CurlyR | T::Eof) {
                self.bump();
            }
            if self.kind() == T::Semicolon {
                self.bump();
            }
            self.nodes[id].end = self.last_end().max(name_end);
            return;
        }
        self.nodes[id].colon = Some(self.tok().start);
        self.bump();
        self.value(id);
        let custom = self.text[start..name_end].starts_with("--");
        let (vs, ve) = self.nodes[id].value;
        if vs == ve && !custom {
            let colon = self.nodes[id].colon.unwrap();
            self.error(colon + 1, colon + 1, "property value expected");
        }
    }

    /// A declaration's or variable's value, to `;` or `}`. A colon at the top level means the
    /// `;` before the next declaration is missing: the value stops at the line before it.
    fn value(&mut self, id: usize) {
        self.skip_trivia();
        let start = self.tok().start;
        let mut depth = 0i32;
        let custom = self.text[self.nodes[id].name.0..self.nodes[id].name.1].starts_with("--");
        let mut last_ident: Option<usize> = None;
        loop {
            let t = self.tok();
            match t.kind {
                T::ParenL | T::Function | T::BracketL => depth += 1,
                T::ParenR | T::BracketR => depth -= 1,
                T::Semicolon | T::CurlyR | T::Eof if depth <= 0 => break,
                // SCSS nested properties (`font: { family: x }`) and blocks after values.
                T::CurlyL if depth <= 0 && !custom => break,
                T::Colon if depth <= 0 && !custom && self.nodes[id].kind == Kind::Declaration => {
                    if let Some(ident) = last_ident {
                        // `color: red\n  margin: 0`: rewind to `margin`.
                        let ident_start = self.toks[ident].start;
                        let had_newline = self.text[start..ident_start].contains('\n');
                        if had_newline {
                            self.pos = ident;
                            let end = self.last_end();
                            self.nodes[id].value = (start, end.max(start));
                            self.nodes[id].end = end;
                            self.error(end, end, "semi-colon expected");
                            return;
                        }
                    }
                }
                T::Ident => last_ident = Some(self.pos),
                T::Whitespace | T::Comment => {}
                _ => last_ident = None,
            }
            self.bump();
        }
        let end = self.last_end().max(start);
        self.nodes[id].value = (start.min(end), end);
        self.nodes[id].end = end;
        match self.kind() {
            T::Semicolon => {
                self.nodes[id].end = self.tok().end;
                self.bump();
            }
            T::CurlyL => self.block(id, true),
            _ => {}
        }
    }
}

impl Stylesheet {
    pub fn parse(text: &str, syntax: Syntax) -> Stylesheet {
        let tokens = tokenize(text, syntax);
        let mut p = Parser { text, syntax, toks: &tokens, pos: 0, nodes: Vec::new(), errors: Vec::new() };
        let root = p.push(Kind::Stylesheet, 0, None);
        p.nodes[root].end = text.len();
        p.statements(root, false, false);
        let Parser { nodes, errors, .. } = p;
        Stylesheet { text: text.to_string(), syntax, tokens, nodes, errors }
    }

    /// Parses the contents of a `style` attribute: declarations only.
    pub fn parse_declarations(text: &str, syntax: Syntax) -> Stylesheet {
        let tokens = tokenize(text, syntax);
        let mut p = Parser { text, syntax, toks: &tokens, pos: 0, nodes: Vec::new(), errors: Vec::new() };
        let root = p.push(Kind::Stylesheet, 0, None);
        p.nodes[root].end = text.len();
        p.nodes[root].block = Some((0, None));
        p.statements(root, true, true);
        let Parser { nodes, errors, .. } = p;
        Stylesheet { text: text.to_string(), syntax, tokens, nodes, errors }
    }

    pub fn node(&self, id: usize) -> &Node {
        &self.nodes[id]
    }

    pub fn slice(&self, (a, b): (usize, usize)) -> &str {
        &self.text[a..b]
    }

    /// The innermost node containing `offset` (a node's end counts as inside).
    pub fn node_at(&self, offset: usize) -> usize {
        let mut id = 0;
        'down: loop {
            for &c in &self.nodes[id].children {
                let n = &self.nodes[c];
                if n.start <= offset && offset <= n.end {
                    // Past a closed block's `}` is outside.
                    if let Some((_, Some(close))) = n.block {
                        if offset > close {
                            continue;
                        }
                    }
                    id = c;
                    continue 'down;
                }
            }
            return id;
        }
    }

    /// The tokens within a byte range.
    pub fn tokens_in(&self, (a, b): (usize, usize)) -> impl Iterator<Item = &Token> {
        self.tokens.iter().filter(move |t| t.start >= a && t.end <= b && t.kind != T::Eof)
    }

    /// Whether the node is inside a rule (so nested at-rules hold declarations).
    pub fn in_rule(&self, mut id: usize) -> bool {
        while let Some(p) = self.nodes[id].parent {
            if self.nodes[p].kind == Kind::RuleSet {
                return true;
            }
            id = p;
        }
        false
    }

    /// Whether `id`'s block holds declarations (a rule, `@font-face`, an at-rule nested in a
    /// rule) rather than rules.
    pub fn holds_declarations(&self, id: usize) -> bool {
        let n = &self.nodes[id];
        match n.kind {
            Kind::RuleSet | Kind::Declaration => true,
            Kind::AtRule => {
                let name = self.slice(n.name);
                !(holds_rules(name) && !self.in_rule(id)) && !name.to_ascii_lowercase().ends_with("keyframes")
            }
            Kind::Stylesheet => n.block.is_some(),
            Kind::Variable => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(text: &str) -> Vec<(&str, usize)> {
        Stylesheet::parse(text, Syntax::Css).errors.iter().map(|e| (e.message, e.start)).collect()
    }

    #[test]
    fn parses_rules_and_declarations() {
        let text = "@media screen { a:hover, .b > c { color: red !important; margin: 0 } }\n@import url(x.css);";
        let s = Stylesheet::parse(text, Syntax::Css);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        let media = s.node(s.node(0).children[0]);
        assert_eq!(media.kind, Kind::AtRule);
        assert_eq!(s.slice(media.value), "screen");
        let rule = s.node(media.children[0]);
        assert_eq!(s.slice(rule.name), "a:hover, .b > c");
        let decls: Vec<(&str, &str)> = rule.children.iter().map(|&d| (s.slice(s.node(d).name), s.slice(s.node(d).value))).collect();
        assert_eq!(decls, [("color", "red !important"), ("margin", "0")]);
        assert_eq!(s.node(s.node(0).children[1]).kind, Kind::AtRule);
    }

    #[test]
    fn reports_and_recovers() {
        assert_eq!(errors("a { color red; }"), [("colon expected", 9)]);
        assert_eq!(errors("a { color: ; }"), [("property value expected", 10)]);
        assert_eq!(errors("a { color: red\n  margin: 0 }"), [("semi-colon expected", 14)]);
        assert_eq!(errors("a { color: red;"), [("} expected", 15)]);
        assert_eq!(errors("a b"), [("{ expected", 3)]);
        // The missing semicolon doesn't swallow the next declaration.
        let s = Stylesheet::parse("a { color: red\n  margin: 0 }", Syntax::Css);
        assert_eq!(s.node(1).children.len(), 2);
    }

    #[test]
    fn scss_nesting_and_variables() {
        let text = "$size: 2px;\n.a { &:hover { b: c } @include m; .d { e: f } // note\n}";
        let s = Stylesheet::parse(text, Syntax::Scss);
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.node(1).kind, Kind::Variable);
        let rule = s.node(2);
        let kinds: Vec<Kind> = rule.children.iter().map(|&c| s.node(c).kind).collect();
        assert_eq!(kinds, [Kind::RuleSet, Kind::AtRule, Kind::RuleSet]);
    }

    #[test]
    fn finds_nodes_at_offsets() {
        let text = "a { color: red; }";
        let s = Stylesheet::parse(text, Syntax::Css);
        assert_eq!(s.node(s.node_at(6)).kind, Kind::Declaration);
        assert_eq!(s.node(s.node_at(16)).kind, Kind::RuleSet);
        assert_eq!(s.node(s.node_at(17)).kind, Kind::Stylesheet);
    }
}
