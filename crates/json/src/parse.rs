//! An error-tolerant parser for JSON with comments that keeps every value's byte range, for
//! completion, hovers and diagnostics while the text is being typed.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Object,
    Array,
    /// A key and its value: `children` is `[key]` or `[key, value]`.
    Property,
    String,
    Number,
    Bool,
    Null,
}

#[derive(Debug)]
pub struct Node {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    pub parent: Option<usize>,
    /// Object: its properties. Array: its items. Property: key, then value if there is one.
    pub children: Vec<usize>,
    /// Property: where its colon is.
    pub colon: Option<usize>,
    /// String: the decoded text. Number and Bool: the source text.
    pub text: String,
}

/// What kind of problem a syntax error is; comments and trailing commas are fine in JSONC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Comment,
    TrailingComma,
}

#[derive(Debug)]
pub struct SyntaxError {
    pub start: usize,
    pub end: usize,
    pub message: &'static str,
    pub kind: ErrorKind,
}

pub struct Doc {
    pub text: String,
    pub nodes: Vec<Node>,
    pub root: Option<usize>,
    pub errors: Vec<SyntaxError>,
    /// Comment byte ranges.
    pub comments: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    Colon,
    Comma,
    Str,
    Num,
    True,
    False,
    Null,
    /// A word or character that isn't JSON (an unquoted key while typing).
    Unknown,
    Eof,
}

pub(crate) struct Scanner<'a> {
    b: &'a [u8],
    pub pos: usize,
    pub tok: Tok,
    pub start: usize,
    /// The decoded string of a `Str` token.
    pub value: String,
    pub errors: Vec<SyntaxError>,
    pub comments: Vec<(usize, usize)>,
}

impl<'a> Scanner<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { b: text.as_bytes(), pos: 0, tok: Tok::Eof, start: 0, value: String::new(), errors: Vec::new(), comments: Vec::new() }
    }

    fn error(&mut self, start: usize, end: usize, message: &'static str) {
        self.errors.push(SyntaxError { start, end, message, kind: ErrorKind::Syntax });
    }

    /// Moves to the next token, skipping whitespace and comments.
    pub fn next(&mut self) -> Tok {
        let b = self.b;
        loop {
            while self.pos < b.len() && matches!(b[self.pos], b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b) {
                self.pos += 1;
            }
            // A BOM or non-breaking space counts as whitespace too.
            if b[self.pos..].starts_with("\u{feff}".as_bytes()) {
                self.pos += 3;
                continue;
            }
            if b[self.pos..].starts_with(b"//") {
                let start = self.pos;
                while self.pos < b.len() && b[self.pos] != b'\n' && b[self.pos] != b'\r' {
                    self.pos += 1;
                }
                self.comment(start);
                continue;
            }
            if b[self.pos..].starts_with(b"/*") {
                let start = self.pos;
                match b[self.pos + 2..].windows(2).position(|w| w == b"*/") {
                    Some(i) => self.pos += 2 + i + 2,
                    None => {
                        self.pos = b.len();
                        self.error(start, self.pos, "Unexpected end of comment.");
                    }
                }
                self.comment(start);
                continue;
            }
            break;
        }
        self.start = self.pos;
        let Some(&c) = b.get(self.pos) else {
            self.tok = Tok::Eof;
            return self.tok;
        };
        self.pos += 1;
        self.tok = match c {
            b'{' => Tok::OpenBrace,
            b'}' => Tok::CloseBrace,
            b'[' => Tok::OpenBracket,
            b']' => Tok::CloseBracket,
            b':' => Tok::Colon,
            b',' => Tok::Comma,
            b'"' => {
                self.string();
                Tok::Str
            }
            b'-' | b'0'..=b'9' => {
                self.number();
                Tok::Num
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                while self.pos < b.len() && (b[self.pos].is_ascii_alphanumeric() || matches!(b[self.pos], b'_' | b'$' | b'-' | b'.')) {
                    self.pos += 1;
                }
                match &b[self.start..self.pos] {
                    b"true" => Tok::True,
                    b"false" => Tok::False,
                    b"null" => Tok::Null,
                    _ => Tok::Unknown,
                }
            }
            _ => {
                // One whole (UTF-8) character.
                while self.pos < b.len() && (b[self.pos] & 0xC0) == 0x80 {
                    self.pos += 1;
                }
                Tok::Unknown
            }
        };
        self.tok
    }

    fn comment(&mut self, start: usize) {
        self.comments.push((start, self.pos));
        self.errors.push(SyntaxError { start, end: self.pos, message: "Comments are not permitted in JSON.", kind: ErrorKind::Comment });
    }

    fn string(&mut self) {
        let b = self.b;
        let mut out = Vec::new();
        loop {
            let Some(&c) = b.get(self.pos) else {
                self.error(self.start, self.pos, "Unexpected end of string.");
                break;
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\n' | b'\r' => {
                    self.error(self.start, self.pos, "Unexpected end of string.");
                    break;
                }
                b'\\' => {
                    let esc = self.pos;
                    self.pos += 1;
                    let Some(&e) = b.get(self.pos) else { continue };
                    self.pos += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => match self.hex4() {
                            Some(mut code) => {
                                // A surrogate pair is two escapes.
                                if (0xD800..0xDC00).contains(&code) && b[self.pos..].starts_with(b"\\u") {
                                    let save = self.pos;
                                    self.pos += 2;
                                    match self.hex4() {
                                        Some(low) if (0xDC00..0xE000).contains(&low) => code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00),
                                        _ => self.pos = save,
                                    }
                                }
                                let ch = char::from_u32(code).unwrap_or('\u{fffd}');
                                out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                            }
                            None => self.error(esc, self.pos, "Invalid unicode sequence in string."),
                        },
                        _ => self.error(esc, self.pos, "Invalid escape character in string."),
                    }
                }
                0..=0x1f => {
                    self.error(self.pos, self.pos + 1, "Invalid characters in string. Control characters must be escaped.");
                    out.push(c);
                    self.pos += 1;
                }
                _ => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        self.value = String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.b.get(self.pos..self.pos + 4)?;
        let code = u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
        self.pos += 4;
        Some(code)
    }

    fn number(&mut self) {
        let b = self.b;
        let digits = |pos: &mut usize| {
            let from = *pos;
            while *pos < b.len() && b[*pos].is_ascii_digit() {
                *pos += 1;
            }
            *pos > from
        };
        let mut ok = true;
        if b[self.start] == b'-' {
            ok = digits(&mut self.pos);
        } else {
            digits(&mut self.pos);
        }
        if ok && self.pos < b.len() && b[self.pos] == b'.' {
            self.pos += 1;
            ok = digits(&mut self.pos);
        }
        if ok && self.pos < b.len() && (b[self.pos] == b'e' || b[self.pos] == b'E') {
            self.pos += 1;
            if self.pos < b.len() && (b[self.pos] == b'+' || b[self.pos] == b'-') {
                self.pos += 1;
            }
            ok = digits(&mut self.pos);
        }
        if !ok {
            self.error(self.start, self.pos, "Invalid number format.");
        }
    }
}

/// How deep values may nest before the parser gives up on them.
const MAX_DEPTH: usize = 400;

struct Parser<'a> {
    s: Scanner<'a>,
    nodes: Vec<Node>,
    /// Where the last token ended (for errors placed after a value).
    last_end: usize,
}

impl Parser<'_> {
    fn advance(&mut self) -> Tok {
        self.last_end = self.s.pos;
        self.s.next()
    }

    fn error(&mut self, start: usize, end: usize, message: &'static str) {
        // One error per spot is enough.
        if self.s.errors.last().is_some_and(|e| e.start == start && e.kind == ErrorKind::Syntax) {
            return;
        }
        self.s.errors.push(SyntaxError { start, end, message, kind: ErrorKind::Syntax });
    }

    fn token_error(&mut self, message: &'static str) {
        let (start, end) = (self.s.start, self.s.pos);
        self.error(start, end, message);
    }

    fn push(&mut self, kind: Kind, start: usize, end: usize, parent: Option<usize>, text: String) -> usize {
        self.nodes.push(Node { kind, start, end, parent, children: Vec::new(), colon: None, text });
        if let Some(p) = parent {
            let id = self.nodes.len() - 1;
            self.nodes[p].children.push(id);
        }
        self.nodes.len() - 1
    }

    /// Parses the value at the current token, if it starts one.
    fn value(&mut self, parent: Option<usize>, depth: usize) -> Option<usize> {
        let (start, end) = (self.s.start, self.s.pos);
        let leaf = |kind| Some((kind, start, end));
        let (kind, start, end) = match self.s.tok {
            Tok::OpenBrace if depth < MAX_DEPTH => return Some(self.object(parent, depth + 1)),
            Tok::OpenBracket if depth < MAX_DEPTH => return Some(self.array(parent, depth + 1)),
            Tok::Str => leaf(Kind::String)?,
            Tok::Num => leaf(Kind::Number)?,
            Tok::True | Tok::False => leaf(Kind::Bool)?,
            Tok::Null => leaf(Kind::Null)?,
            _ => return None,
        };
        let text = match kind {
            Kind::String => std::mem::take(&mut self.s.value),
            Kind::Null => String::new(),
            _ => self.s_text(start, end),
        };
        let id = self.push(kind, start, end, parent, text);
        self.advance();
        Some(id)
    }

    fn s_text(&self, start: usize, end: usize) -> String {
        String::from_utf8_lossy(&self.s.b[start..end]).into_owned()
    }

    fn object(&mut self, parent: Option<usize>, depth: usize) -> usize {
        let id = self.push(Kind::Object, self.s.start, self.s.pos, parent, String::new());
        self.advance();
        let mut first = true;
        loop {
            match self.s.tok {
                Tok::CloseBrace => {
                    self.nodes[id].end = self.s.pos;
                    self.advance();
                    return id;
                }
                Tok::Eof => {
                    self.error(self.s.start, self.s.start, "Expected comma or closing brace");
                    self.nodes[id].end = self.last_end;
                    return id;
                }
                _ => {}
            }
            if !first {
                // `a: 1 "b": 2`: a missing comma before another property.
                if self.s.tok == Tok::Comma {
                    let comma = (self.s.start, self.s.pos);
                    self.advance();
                    if self.s.tok == Tok::CloseBrace {
                        self.s.errors.push(SyntaxError { start: comma.0, end: comma.1, message: "Trailing comma", kind: ErrorKind::TrailingComma });
                        continue;
                    }
                } else if matches!(self.s.tok, Tok::Str | Tok::Unknown) {
                    self.error(self.last_end, self.last_end, "Expected comma");
                } else {
                    self.token_error("Expected comma or closing brace");
                    self.advance();
                    continue;
                }
            }
            first = false;
            self.property(id, depth);
        }
    }

    fn property(&mut self, object: usize, depth: usize) {
        let (start, end) = (self.s.start, self.s.pos);
        let key = match self.s.tok {
            Tok::Str => std::mem::take(&mut self.s.value),
            Tok::Unknown if self.s.b[start].is_ascii_alphabetic() => {
                self.token_error("Property keys must be doublequoted");
                self.s_text(start, end)
            }
            Tok::Comma | Tok::CloseBrace => {
                self.token_error("Property expected");
                return;
            }
            _ => {
                self.token_error("Property expected");
                self.advance();
                return;
            }
        };
        let prop = self.push(Kind::Property, start, end, Some(object), String::new());
        self.push(Kind::String, start, end, Some(prop), key);
        self.advance();
        if self.s.tok != Tok::Colon {
            self.error(self.last_end, self.last_end, "Colon expected");
            return;
        }
        self.nodes[prop].colon = Some(self.s.start);
        self.nodes[prop].end = self.s.pos;
        self.advance();
        match self.value(Some(prop), depth) {
            Some(v) => self.nodes[prop].end = self.nodes[v].end,
            None => {
                let (start, end) = (self.s.start, self.s.pos);
                self.error(start, end, "Value expected");
                if !matches!(self.s.tok, Tok::Comma | Tok::CloseBrace | Tok::CloseBracket | Tok::Eof) {
                    self.advance();
                }
            }
        }
    }

    fn array(&mut self, parent: Option<usize>, depth: usize) -> usize {
        let id = self.push(Kind::Array, self.s.start, self.s.pos, parent, String::new());
        self.advance();
        let mut first = true;
        loop {
            match self.s.tok {
                Tok::CloseBracket => {
                    self.nodes[id].end = self.s.pos;
                    self.advance();
                    return id;
                }
                Tok::Eof => {
                    self.error(self.s.start, self.s.start, "Expected comma or closing bracket");
                    self.nodes[id].end = self.last_end;
                    return id;
                }
                _ => {}
            }
            if !first {
                if self.s.tok == Tok::Comma {
                    let comma = (self.s.start, self.s.pos);
                    self.advance();
                    if self.s.tok == Tok::CloseBracket {
                        self.s.errors.push(SyntaxError { start: comma.0, end: comma.1, message: "Trailing comma", kind: ErrorKind::TrailingComma });
                        continue;
                    }
                } else if matches!(self.s.tok, Tok::Str | Tok::Num | Tok::True | Tok::False | Tok::Null | Tok::OpenBrace | Tok::OpenBracket) {
                    self.error(self.last_end, self.last_end, "Expected comma");
                } else {
                    self.token_error("Expected comma or closing bracket");
                    self.advance();
                    continue;
                }
            }
            first = false;
            if self.value(Some(id), depth).is_none() {
                self.token_error("Value expected");
                if !matches!(self.s.tok, Tok::Comma | Tok::CloseBracket | Tok::CloseBrace) {
                    self.advance();
                }
            }
        }
    }
}

impl Doc {
    pub fn parse(text: &str) -> Doc {
        let mut p = Parser { s: Scanner::new(text), nodes: Vec::new(), last_end: 0 };
        p.advance();
        let mut root = None;
        if p.s.tok != Tok::Eof {
            root = p.value(None, 0);
            if root.is_none() {
                p.token_error("Expected a JSON object, array or literal.");
                // Skip to something that starts a value.
                while !matches!(p.s.tok, Tok::Eof | Tok::OpenBrace | Tok::OpenBracket) {
                    p.advance();
                }
                if p.s.tok != Tok::Eof {
                    root = p.value(None, 0);
                }
            }
            if p.s.tok != Tok::Eof {
                p.token_error("End of file expected.");
            }
        }
        let Parser { s, nodes, .. } = p;
        Doc { text: text.to_string(), nodes, root, errors: s.errors, comments: s.comments }
    }

    pub fn node(&self, id: usize) -> &Node {
        &self.nodes[id]
    }

    /// The deepest node containing `offset` (`at_end`: also one ending right at it).
    pub fn node_at(&self, offset: usize, at_end: bool) -> Option<usize> {
        let contains = |n: &Node| n.start <= offset && (offset < n.end || (at_end && offset == n.end));
        let mut id = self.root.filter(|&r| contains(&self.nodes[r]))?;
        'down: loop {
            for &c in &self.nodes[id].children {
                if contains(&self.nodes[c]) {
                    id = c;
                    continue 'down;
                }
                if self.nodes[c].start > offset {
                    break;
                }
            }
            return Some(id);
        }
    }

    /// A property's key.
    pub fn key(&self, property: usize) -> &str {
        &self.nodes[self.nodes[property].children[0]].text
    }

    /// A property's value, if it has one.
    pub fn property_value(&self, property: usize) -> Option<usize> {
        self.nodes[property].children.get(1).copied()
    }

    /// An object's property named `key`.
    pub fn get(&self, object: usize, key: &str) -> Option<usize> {
        self.nodes[object].children.iter().copied().find(|&p| self.key(p) == key).and_then(|p| self.property_value(p))
    }

    /// The node as a JSON value (Null for nodes that aren't values).
    pub fn value(&self, id: usize) -> Value {
        let n = &self.nodes[id];
        match n.kind {
            Kind::Object => Value::Object(
                n.children.iter().filter_map(|&p| Some((self.key(p).to_string(), self.value(self.property_value(p)?)))).collect(),
            ),
            Kind::Array => Value::Array(n.children.iter().map(|&c| self.value(c)).collect()),
            Kind::String => Value::String(n.text.clone()),
            Kind::Number => serde_json::from_str(&n.text).unwrap_or(Value::Null),
            Kind::Bool => Value::Bool(n.text == "true"),
            Kind::Null | Kind::Property => Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(text: &str) -> Vec<&'static str> {
        Doc::parse(text).errors.iter().filter(|e| e.kind == ErrorKind::Syntax).map(|e| e.message).collect()
    }

    #[test]
    fn parses_values_with_ranges() {
        let text = r#"{ "a": [12, -3.5e2, true, null], "b": { "c": "d\u0041\n" } }"#;
        let doc = Doc::parse(text);
        assert!(doc.errors.is_empty(), "{:?}", doc.errors);
        let root = doc.root.unwrap();
        assert_eq!(doc.value(root), serde_json::json!({ "a": [12, -350.0, true, null], "b": { "c": "dA\n" } }));
        let c = doc.get(doc.get(root, "b").unwrap(), "c").unwrap();
        assert_eq!(&text[doc.node(c).start..doc.node(c).end], r#""d\u0041\n""#);
        let at = doc.node_at(text.find("true").unwrap() + 1, false).unwrap();
        assert_eq!(doc.node(at).kind, Kind::Bool);
    }

    #[test]
    fn recovers_from_errors() {
        assert_eq!(messages(r#"{ "a": 1 "b": 2 }"#), ["Expected comma"]);
        assert_eq!(messages(r#"{ "a" }"#), ["Colon expected"]);
        assert_eq!(messages(r#"{ "a": }"#), ["Value expected"]);
        assert_eq!(messages(r#"{ a: 1 }"#), ["Property keys must be doublequoted"]);
        assert_eq!(messages(r#"{ "a": 1"#), ["Expected comma or closing brace"]);
        assert_eq!(messages(r#"[1 2]"#), ["Expected comma"]);
        assert_eq!(messages(r#"{} x"#), ["End of file expected."]);
        assert_eq!(messages("\"abc"), ["Unexpected end of string."]);
        assert_eq!(messages(""), Vec::<&str>::new());
        // Still a tree to complete in.
        let doc = Doc::parse(r#"{ "a": 1, "b" "#);
        assert_eq!(doc.node(doc.root.unwrap()).children.len(), 2);
    }

    #[test]
    fn comments_and_trailing_commas_are_their_own_kind() {
        let doc = Doc::parse("// hi\n{ \"a\": [1,], /* x */ }");
        let kinds: Vec<ErrorKind> = doc.errors.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [ErrorKind::Comment, ErrorKind::TrailingComma, ErrorKind::Comment, ErrorKind::TrailingComma]);
        assert_eq!(doc.comments.len(), 2);
    }
}
