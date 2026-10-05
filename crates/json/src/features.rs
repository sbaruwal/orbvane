//! Editor features over a parsed document and its schema: completion, hovers, the outline,
//! folding and formatting. Positions are byte offsets; `server.rs` converts them to LSP's.

use serde_json::Value;

use crate::parse::{Doc, Kind, Scanner, Tok};
use crate::schema::{self, Match, Regexes, Validator};

/// LSP completion item kinds.
const KIND_PROPERTY: u32 = 10;
const KIND_VALUE: u32 = 12;
const KIND_SNIPPET: u32 = 15;

#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    pub kind: u32,
    pub documentation: Option<String>,
    /// Snippet text.
    pub insert: String,
    pub filter: String,
    /// The byte range the item replaces.
    pub range: (usize, usize),
}

/// What the schema-driven features know about a document.
pub struct Analysis<'d, 's> {
    pub doc: &'d Doc,
    root: Option<&'s Value>,
    matches: Vec<Match<'s>>,
    regexes: Regexes,
}

/// Who a value belongs to.
#[derive(Clone)]
enum Owner {
    Property { object: usize, key: String },
    Item { array: usize, index: usize },
    Root,
}

impl<'d, 's> Analysis<'d, 's> {
    pub fn new(doc: &'d Doc, root: Option<&'s Value>) -> Self {
        let (matches, regexes) = match root {
            Some(r) => {
                let mut v = Validator::new(doc, r);
                let (_, matches) = v.run();
                (matches, v.regexes)
            }
            None => (Vec::new(), Regexes::default()),
        };
        Self { doc, root, matches, regexes }
    }

    /// The schemas that apply to `node` (flattened alternatives).
    fn node_schemas(&self, node: usize) -> Vec<&'s Value> {
        let Some(root) = self.root else { return Vec::new() };
        let mut out = Vec::new();
        for m in self.matches.iter().filter(|m| m.node == node && !m.inverted) {
            schema::alternatives(root, m.schema, &mut out);
        }
        out
    }

    /// The schemas for a value of `owner`, whether or not it's there yet.
    fn owner_schemas(&mut self, owner: &Owner) -> Vec<&'s Value> {
        let Some(root) = self.root else { return Vec::new() };
        let parents = match owner {
            Owner::Property { object, .. } => self.node_schemas(*object),
            Owner::Item { array, .. } => self.node_schemas(*array),
            Owner::Root => vec![root],
        };
        let mut out = Vec::new();
        for parent in parents {
            let child = match owner {
                Owner::Property { key, .. } => schema::property_schema(root, parent, key, &mut self.regexes),
                Owner::Item { index, .. } => schema::item_schema(root, parent, *index),
                Owner::Root => Some(parent),
            };
            if let Some(child) = child {
                schema::alternatives(root, child, &mut out);
            }
        }
        out
    }

    fn owner_of(&self, node: usize) -> Owner {
        let doc = self.doc;
        match doc.node(node).parent {
            Some(p) if doc.node(p).kind == Kind::Property => {
                Owner::Property { object: doc.node(p).parent.unwrap_or(p), key: doc.key(p).to_string() }
            }
            Some(p) if doc.node(p).kind == Kind::Array => {
                Owner::Item { array: p, index: doc.node(p).children.iter().position(|&c| c == node).unwrap_or(0) }
            }
            _ => Owner::Root,
        }
    }

    /// Whether `offset` is between a container's brackets.
    fn inside(&self, node: usize, offset: usize) -> bool {
        let n = self.doc.node(node);
        let closed = matches!(self.doc.text.as_bytes().get(n.end.wrapping_sub(1)), Some(b'}' | b']')) && n.end > n.start + 1;
        n.start < offset && (offset < n.end || !closed)
    }

    /// The word at `offset` (letters, digits, `_-.$`, quotes), as a byte range.
    fn word(&self, offset: usize) -> (usize, usize) {
        let b = self.doc.text.as_bytes();
        let is_word = |c: u8| c.is_ascii_alphanumeric() || b"_-.$\"".contains(&c);
        let mut start = offset;
        while start > 0 && is_word(b[start - 1]) {
            start -= 1;
        }
        let mut end = offset;
        while end < b.len() && is_word(b[end]) && b[end] != b'"' {
            end += 1;
        }
        (start, end)
    }

    /// The range a string node covers, without the rest of the line an unterminated one
    /// swallowed.
    fn string_range(&self, node: usize, offset: usize) -> (usize, usize) {
        let n = self.doc.node(node);
        let terminated = n.end > n.start + 1 && self.doc.text.as_bytes()[n.end - 1] == b'"';
        (n.start, if terminated { n.end } else { offset.max(n.start) })
    }

    /// "," when another property or item follows `offset`.
    fn separator_after(&self, offset: usize) -> &'static str {
        let mut s = Scanner::new(&self.doc.text);
        s.pos = offset;
        match s.next() {
            Tok::Str | Tok::Unknown | Tok::Num | Tok::True | Tok::False | Tok::Null | Tok::OpenBrace | Tok::OpenBracket => ",",
            _ => "",
        }
    }

    pub fn complete(&mut self, offset: usize) -> Vec<Item> {
        let doc = self.doc;
        let Some(node) = doc.node_at(offset, true) else {
            return match doc.root {
                None => self.complete_value(Owner::Root, self.word(offset), None),
                Some(_) => Vec::new(),
            };
        };
        let n = doc.node(node);
        match n.kind {
            Kind::String => match n.parent {
                Some(p) if doc.node(p).kind == Kind::Property && doc.node(p).children[0] == node => {
                    let object = doc.node(p).parent.unwrap_or(p);
                    self.complete_key(object, self.string_range(node, offset), Some(p))
                }
                _ => {
                    let owner = self.owner_of(node);
                    self.complete_value(owner, self.string_range(node, offset), Some(node))
                }
            },
            Kind::Number | Kind::Bool | Kind::Null => {
                let owner = self.owner_of(node);
                self.complete_value(owner, (n.start, n.end), Some(node))
            }
            Kind::Property => match n.colon {
                Some(colon) if offset > colon => {
                    let owner = Owner::Property { object: n.parent.unwrap_or(node), key: doc.key(node).to_string() };
                    self.complete_value(owner, self.word(offset), None)
                }
                _ => {
                    let key = n.children[0];
                    let range = self.string_range(key, offset);
                    self.complete_key(n.parent.unwrap_or(node), range, Some(node))
                }
            },
            Kind::Object if self.inside(node, offset) => {
                let range = self.word(offset);
                let before = doc.text[..range.0].trim_end();
                if before.ends_with(':') {
                    // After `"key":` with no value yet.
                    let colon = before.len() - 1;
                    if let Some(&p) = n.children.iter().find(|&&p| doc.node(p).colon == Some(colon)) {
                        let owner = Owner::Property { object: node, key: doc.key(p).to_string() };
                        return self.complete_value(owner, range, None);
                    }
                }
                self.complete_key(node, range, None)
            }
            Kind::Array if self.inside(node, offset) => {
                let index = n.children.iter().filter(|&&c| doc.node(c).end <= offset).count();
                self.complete_value(Owner::Item { array: node, index }, self.word(offset), None)
            }
            _ => Vec::new(),
        }
    }

    fn complete_key(&mut self, object: usize, range: (usize, usize), current: Option<usize>) -> Vec<Item> {
        let Some(root) = self.root else { return Vec::new() };
        let doc = self.doc;
        let existing: Vec<&str> = doc.node(object).children.iter().filter(|&&p| Some(p) != current).map(|&p| doc.key(p)).collect();
        let separator = self.separator_after(range.1);
        let mut items: Vec<Item> = Vec::new();
        for s in self.node_schemas(object) {
            let Some(properties) = s.get("properties").and_then(Value::as_object) else { continue };
            for (key, prop) in properties {
                if existing.contains(&key.as_str()) || items.iter().any(|i| i.label == *key) {
                    continue;
                }
                let prop = schema::resolve(root, prop);
                let hidden = |p: &Value| p.get("doNotSuggest") == Some(&Value::Bool(true)) || p.get("deprecationMessage").is_some();
                if hidden(prop) {
                    continue;
                }
                let quoted = Value::String(key.clone()).to_string();
                items.push(Item {
                    label: key.clone(),
                    kind: KIND_PROPERTY,
                    documentation: description(prop),
                    insert: format!("{}: {}{separator}", escape_snippet(&quoted), value_snippet(root, prop)),
                    filter: quoted,
                    range,
                });
            }
        }
        items
    }

    fn complete_value(&mut self, owner: Owner, range: (usize, usize), _existing: Option<usize>) -> Vec<Item> {
        let separator = match owner {
            Owner::Root => "",
            _ => self.separator_after(range.1),
        };
        let mut items: Vec<Item> = Vec::new();
        let add = |items: &mut Vec<Item>, label: String, insert: String, kind: u32, documentation: Option<String>| {
            if !items.iter().any(|i| i.label == label) {
                items.push(Item { filter: label.clone(), label, kind, documentation, insert: format!("{insert}{separator}"), range });
            }
        };
        for s in self.owner_schemas(&owner) {
            let docs = |i: usize| {
                let pick = |key: &str| s.get(key).and_then(|d| d.get(i)).and_then(Value::as_str).filter(|d| !d.is_empty()).map(String::from);
                pick("markdownEnumDescriptions").or_else(|| pick("enumDescriptions"))
            };
            for (i, v) in s.get("enum").and_then(Value::as_array).into_iter().flatten().enumerate() {
                add(&mut items, v.to_string(), escape_snippet(&v.to_string()), KIND_VALUE, docs(i).or_else(|| description(s)));
            }
            if let Some(c) = s.get("const") {
                add(&mut items, c.to_string(), escape_snippet(&c.to_string()), KIND_VALUE, description(s));
            }
            for snippet in s.get("defaultSnippets").and_then(Value::as_array).into_iter().flatten() {
                let body = match (snippet.get("bodyText").and_then(Value::as_str), snippet.get("body")) {
                    (Some(text), _) => text.to_string(),
                    (None, Some(body)) => snippet_body(body, ""),
                    _ => continue,
                };
                let label = snippet.get("label").and_then(Value::as_str).map_or_else(|| body.clone(), String::from);
                let doc = snippet.get("markdownDescription").or_else(|| snippet.get("description")).and_then(Value::as_str).map(String::from);
                add(&mut items, label, body, KIND_SNIPPET, doc);
            }
            let types: Vec<&str> = match s.get("type") {
                Some(Value::String(t)) => vec![t.as_str()],
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            if types.contains(&"boolean") {
                add(&mut items, "true".into(), "true".into(), KIND_VALUE, None);
                add(&mut items, "false".into(), "false".into(), KIND_VALUE, None);
            }
            if types.contains(&"null") {
                add(&mut items, "null".into(), "null".into(), KIND_VALUE, None);
            }
            if let Some(d) = s.get("default") {
                add(&mut items, d.to_string(), snippet_body(d, ""), KIND_VALUE, Some("Default value".into()));
            }
            for e in s.get("examples").and_then(Value::as_array).into_iter().flatten() {
                add(&mut items, e.to_string(), snippet_body(e, ""), KIND_VALUE, None);
            }
            if s.get("defaultSnippets").is_none() && s.get("enum").is_none() {
                if types.contains(&"object") {
                    add(&mut items, "{}".into(), "{$1}".into(), KIND_VALUE, None);
                }
                if types.contains(&"array") {
                    add(&mut items, "[]".into(), "[$1]".into(), KIND_VALUE, None);
                }
            }
        }
        items
    }

    /// The hover for `offset`: the schema's description of the value there, and the
    /// description of the enum value it has.
    pub fn hover(&mut self, offset: usize) -> Option<(String, (usize, usize))> {
        let doc = self.doc;
        let node = doc.node_at(offset, false)?;
        let n = doc.node(node);
        // A key stands for its property's value.
        let (owner, value, range) = match (n.kind, n.parent) {
            (Kind::String, Some(p)) if doc.node(p).kind == Kind::Property && doc.node(p).children[0] == node => {
                let object = doc.node(p).parent?;
                (Owner::Property { object, key: doc.key(p).to_string() }, doc.property_value(p), (n.start, n.end))
            }
            (Kind::Object | Kind::Array | Kind::Property, _) => return None,
            _ => (self.owner_of(node), Some(node), (n.start, n.end)),
        };
        let schemas = self.owner_schemas(&owner);
        let mut parts: Vec<String> = Vec::new();
        if let Some(text) = schemas.iter().find_map(|s| description(s)) {
            parts.push(text);
        }
        if let Some(v) = value.map(|v| doc.value(v)) {
            for s in &schemas {
                let Some(i) = s.get("enum").and_then(Value::as_array).and_then(|e| e.iter().position(|x| *x == v)) else { continue };
                let d = s.get("markdownEnumDescriptions").or_else(|| s.get("enumDescriptions")).and_then(|d| d.get(i)).and_then(Value::as_str);
                if let Some(d) = d.filter(|d| !d.is_empty()) {
                    parts.push(format!("`{v}`: {d}"));
                    break;
                }
            }
        }
        (!parts.is_empty()).then(|| (parts.join("\n\n"), range))
    }
}

fn description(s: &Value) -> Option<String> {
    let text = s.get("markdownDescription").or_else(|| s.get("description")).and_then(Value::as_str)?;
    Some(match s.get("title").and_then(Value::as_str) {
        Some(title) => format!("**{title}**\n\n{text}"),
        None => text.to_string(),
    })
}

/// Escapes text for a snippet.
fn escape_snippet(s: &str) -> String {
    s.replace('\\', "\\\\").replace('$', "\\$").replace('}', "\\}")
}

/// What follows `"key": ` when a property is completed.
fn value_snippet(root: &Value, prop: &Value) -> String {
    if let Some(d) = prop.get("default") {
        return match d {
            Value::String(s) => {
                let quoted = Value::String(s.clone()).to_string();
                format!("\"${{1:{}}}\"", escape_snippet(&quoted[1..quoted.len() - 1]))
            }
            Value::Object(_) | Value::Array(_) => snippet_body(d, ""),
            _ => format!("${{1:{}}}", d),
        };
    }
    let mut alts = Vec::new();
    schema::alternatives(root, prop, &mut alts);
    for s in &alts {
        if let Some([only]) = s.get("enum").and_then(Value::as_array).map(Vec::as_slice) {
            return format!("${{1:{}}}", escape_snippet(&only.to_string()));
        }
        if let Some(snippet) = s.get("defaultSnippets").and_then(|d| d.get(0)) {
            if let Some(body) = snippet.get("body") {
                return snippet_body(body, "");
            }
        }
    }
    let ty = alts.iter().find_map(|s| match s.get("type") {
        Some(Value::String(t)) => Some(t.as_str()),
        Some(Value::Array(a)) => a.first().and_then(Value::as_str),
        _ => None,
    });
    match ty {
        Some("string") => "\"$1\"".into(),
        Some("object") => "{$1}".into(),
        Some("array") => "[$1]".into(),
        Some("number" | "integer") => "${1:0}".into(),
        Some("boolean") => "${1:false}".into(),
        Some("null") => "${1:null}".into(),
        _ => "$1".into(),
    }
}

/// A `defaultSnippets` body as snippet text: JSON indented with tabs, where strings are
/// snippet syntax and a string starting with `^` is inserted without quotes.
fn snippet_body(v: &Value, indent: &str) -> String {
    let inner = format!("{indent}\t");
    match v {
        Value::String(s) => match s.strip_prefix('^') {
            Some(raw) => raw.to_string(),
            // The rest is snippet syntax already.
            None => Value::String(s.clone()).to_string(),
        },
        Value::Object(o) if o.is_empty() => "{}".into(),
        Value::Array(a) if a.is_empty() => "[]".into(),
        Value::Object(o) => {
            let fields: Vec<String> = o.iter().map(|(k, v)| format!("{inner}{}: {}", Value::String(k.clone()), snippet_body(v, &inner))).collect();
            format!("{{\n{}\n{indent}}}", fields.join(",\n"))
        }
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(|v| format!("{inner}{}", snippet_body(v, &inner))).collect();
            format!("[\n{}\n{indent}]", items.join(",\n"))
        }
        other => other.to_string(),
    }
}

/// An outline entry.
#[derive(Debug)]
pub struct Symbol {
    pub name: String,
    /// LSP SymbolKind.
    pub kind: u32,
    pub range: (usize, usize),
    pub selection: (usize, usize),
    pub children: Vec<Symbol>,
}

/// The outline: properties by key, array items by index.
pub fn symbols(doc: &Doc) -> Vec<Symbol> {
    fn kind_of(doc: &Doc, node: Option<usize>) -> u32 {
        match node.map(|n| doc.node(n).kind) {
            Some(Kind::Object) => 2,
            Some(Kind::Array) => 18,
            Some(Kind::String) => 15,
            Some(Kind::Number) => 16,
            Some(Kind::Bool) => 17,
            _ => 13,
        }
    }
    fn children(doc: &Doc, node: usize, budget: &mut usize) -> Vec<Symbol> {
        let n = doc.node(node);
        let mut out = Vec::new();
        for (i, &c) in n.children.iter().enumerate() {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let (name, value, selection) = match n.kind {
                Kind::Object => {
                    let key = doc.node(doc.node(c).children[0]);
                    (doc.key(c).to_string(), doc.property_value(c), (key.start, key.end))
                }
                _ => (i.to_string(), Some(c), (doc.node(c).start, doc.node(c).end)),
            };
            let nested = match value {
                Some(v) if matches!(doc.node(v).kind, Kind::Object | Kind::Array) => children(doc, v, budget),
                _ => Vec::new(),
            };
            let name = if name.is_empty() { "\"\"".to_string() } else { name };
            out.push(Symbol { name, kind: kind_of(doc, value), range: (doc.node(c).start, doc.node(c).end), selection, children: nested });
        }
        out
    }
    let mut budget = 5000;
    match doc.root {
        Some(root) if matches!(doc.node(root).kind, Kind::Object | Kind::Array) => children(doc, root, &mut budget),
        _ => Vec::new(),
    }
}

/// Foldable regions as byte ranges: multi-line objects, arrays and block comments, and
/// `// #region` ... `// #endregion`.
pub fn folding(doc: &Doc) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = doc.nodes.iter().filter(|n| matches!(n.kind, Kind::Object | Kind::Array)).map(|n| (n.start, n.end)).collect();
    let mut regions = Vec::new();
    for &(s, e) in &doc.comments {
        let text = &doc.text[s..e];
        let body = text.trim_start_matches(['/', '*']).trim_start();
        if body.starts_with("#region") {
            regions.push(s);
        } else if body.starts_with("#endregion") {
            if let Some(start) = regions.pop() {
                out.push((start, e));
            }
        } else if text.starts_with("/*") {
            out.push((s, e));
        }
    }
    out
}

/// Formatting: the whitespace between tokens, the way (one property or item per line,
/// `"key": value`, comments kept where they were). Returns edits (byte range, new text) that
/// only touch whitespace.
pub fn format(text: &str, tab: &str, eol: &str) -> Vec<((usize, usize), String)> {
    // Tokens with comments, which the scanner skips: walk the gaps ourselves.
    let mut s = Scanner::new(text);
    let mut tokens: Vec<(Tok, usize, usize)> = Vec::new();
    let mut seen_comments = 0;
    loop {
        let tok = s.next();
        while seen_comments < s.comments.len() {
            let (a, b) = s.comments[seen_comments];
            tokens.push((Tok::Unknown, a, b));
            seen_comments += 1;
        }
        if tok == Tok::Eof {
            break;
        }
        tokens.push((tok, s.start, s.pos));
    }
    let is_comment = |(_, a, _): &(Tok, usize, usize)| text[*a..].starts_with("//") || text[*a..].starts_with("/*");
    let mut edits = Vec::new();
    let mut depth: usize = 0;
    let newline = |depth: usize| format!("{eol}{}", tab.repeat(depth));
    for i in 0..tokens.len() {
        let (tok, start, _) = tokens[i];
        let comment = is_comment(&tokens[i]);
        let prev = i.checked_sub(1).map(|j| tokens[j]);
        // Depth changes: closers apply before their own whitespace.
        if !comment && matches!(tok, Tok::CloseBrace | Tok::CloseBracket) {
            depth = depth.saturating_sub(1);
        }
        let Some(prev) = prev else {
            if !comment && matches!(tok, Tok::OpenBrace | Tok::OpenBracket) {
                depth += 1;
            }
            continue;
        };
        let gap = (prev.2, start);
        let had_newline = text[gap.0..gap.1].contains('\n');
        let prev_comment = is_comment(&prev);
        let prev_line_comment = prev_comment && text[prev.1..].starts_with("//");
        let want = if comment {
            if had_newline { newline(depth) } else { " ".to_string() }
        } else if prev_line_comment || (prev_comment && had_newline) {
            newline(depth)
        } else {
            match (prev.0, tok) {
                (Tok::OpenBrace, Tok::CloseBrace) | (Tok::OpenBracket, Tok::CloseBracket) if !prev_comment => String::new(),
                (Tok::OpenBrace | Tok::OpenBracket | Tok::Comma, _) if !prev_comment => newline(depth),
                (_, Tok::CloseBrace | Tok::CloseBracket) => newline(depth),
                (Tok::Colon, _) => " ".to_string(),
                (_, Tok::Colon | Tok::Comma) => String::new(),
                _ if prev_comment => " ".to_string(),
                _ => " ".to_string(),
            }
        };
        if text[gap.0..gap.1] != want {
            edits.push((gap, want));
        }
        if !comment && matches!(tok, Tok::OpenBrace | Tok::OpenBracket) {
            depth += 1;
        }
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete(text_with_caret: &str, schema: &Value) -> Vec<Item> {
        let offset = text_with_caret.find('|').unwrap();
        let text = text_with_caret.replacen('|', "", 1);
        let doc = Doc::parse(&text);
        Analysis::new(&doc, Some(schema)).complete(offset)
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "editor.fontSize": { "type": "number", "default": 14, "description": "Controls the font size." },
                "editor.wordWrap": { "enum": ["off", "on"], "enumDescriptions": ["Lines never wrap.", "Lines wrap."], "default": "off" },
                "editor.minimap": { "type": "boolean", "default": true },
                "list": { "type": "array", "items": { "defaultSnippets": [{ "label": "New item", "body": { "name": "${1:x}" } }] } }
            }
        })
    }

    #[test]
    fn completes_keys_and_values() {
        let s = schema();
        let items = complete("{ \"editor.fontSize\": 12, | }", &s);
        let mut got = labels(&items);
        got.sort();
        assert_eq!(got, ["editor.minimap", "editor.wordWrap", "list"]);
        let wrap = items.iter().find(|i| i.label == "editor.wordWrap").unwrap();
        assert_eq!(wrap.insert, "\"editor.wordWrap\": \"${1:off}\"");
        // A key being typed in quotes is replaced whole, and a comma added before the next one.
        let items = complete("{ \"edi|\"\n \"list\": [] }", &s);
        let size = items.iter().find(|i| i.label == "editor.fontSize").unwrap();
        assert_eq!(size.range, (2, 7));
        assert_eq!(size.insert, "\"editor.fontSize\": ${1:14},");
        // Values: enums with their descriptions, booleans, defaults.
        let items = complete("{ \"editor.wordWrap\": | }", &s);
        assert_eq!(labels(&items), ["\"off\"", "\"on\""]);
        assert_eq!(items[1].documentation.as_deref(), Some("Lines wrap."));
        let items = complete("{ \"editor.wordWrap\": \"o|\" }", &s);
        assert_eq!(items[0].range, (21, 24));
        let items = complete("{ \"editor.minimap\": t| }", &s);
        assert_eq!(labels(&items), ["true", "false"]);
        // Snippets for array items.
        let items = complete("{ \"list\": [ | ] }", &s);
        assert_eq!(labels(&items), ["New item"]);
        assert_eq!(items[0].insert, "{\n\t\"name\": \"${1:x}\"\n}");
    }

    #[test]
    fn hovers_describe_keys_and_values() {
        let s = schema();
        let text = r#"{ "editor.fontSize": 12, "editor.wordWrap": "on" }"#;
        let doc = Doc::parse(text);
        let mut a = Analysis::new(&doc, Some(&s));
        assert_eq!(a.hover(text.find("fontSize").unwrap()).unwrap().0, "Controls the font size.");
        assert_eq!(a.hover(text.find("\"on\"").unwrap() + 1).unwrap().0, "`\"on\"`: Lines wrap.");
    }

    #[test]
    fn outline_and_folding() {
        let text = "{\n  \"a\": { \"b\": [1, 2] },\n  \"c\": true\n}";
        let doc = Doc::parse(text);
        let syms = symbols(&doc);
        assert_eq!(syms.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(syms[0].children[0].name, "b");
        assert_eq!(syms[0].children[0].children.len(), 2);
        assert_eq!(folding(&doc).len(), 3);
    }

    fn formatted(text: &str) -> String {
        let mut out = text.to_string();
        for ((a, b), new) in format(text, "\t", "\n").into_iter().rev() {
            out.replace_range(a..b, &new);
        }
        out
    }

    #[test]
    fn formats_like_the_reference() {
        assert_eq!(formatted(r#"{"a":1,"b":[1,2],"c":{},"d":[]}"#), "{\n\t\"a\": 1,\n\t\"b\": [\n\t\t1,\n\t\t2\n\t],\n\t\"c\": {},\n\t\"d\": []\n}");
        // Comments stay on their line.
        assert_eq!(formatted("{\n// note\n\"a\":1, // why\n\"b\":2}"), "{\n\t// note\n\t\"a\": 1, // why\n\t\"b\": 2\n}");
    }
}
