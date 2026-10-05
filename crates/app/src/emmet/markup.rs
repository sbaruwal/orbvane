//! Markup abbreviations (`ul>li.item$*3`) expanded to HTML, XML or JSX, ported from Emmet
//! (emmetio/emmet, MIT): the abbreviation parser, snippet resolution, implicit tag names,
//! lorem ipsum and the HTML formatter with its rules for when elements go on their own line.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::{Out, Syntax};

/// A piece of a name, value or text before repeats are expanded.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Text(String),
    Field(u32, String),
    /// `$`, `$$`, `$@-`, `$@3`: the repeat number, `size` digits wide.
    Num { size: usize, reverse: bool, base: i64 },
}

/// A piece of a value after expansion.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Val {
    Text(String),
    Field(u32, String),
}

#[derive(Clone, Debug, Default)]
struct PAttr {
    name: Vec<Tok>,
    value: Option<Vec<Tok>>,
    boolean: bool,
    /// A JSX expression (`[onClick={go}]`).
    expr: bool,
}

#[derive(Debug, Default)]
struct Elem {
    name: Vec<Tok>,
    attrs: Option<Vec<PAttr>>,
    value: Option<Vec<Tok>>,
    self_closing: bool,
}

#[derive(Debug)]
enum Kind {
    Elem(Elem),
    Group(Vec<Item>),
}

/// A parsed element or group. `repeat` is `*N` (`Some(None)` for a bare `*`).
#[derive(Debug)]
struct Item {
    kind: Kind,
    repeat: Option<Option<usize>>,
    children: Vec<Item>,
}

#[derive(Clone, Debug)]
struct Attr {
    name: String,
    value: Option<Vec<Val>>,
    boolean: bool,
    expr: bool,
}

/// An element (or a text snippet when it has neither name nor attributes).
#[derive(Clone, Debug, Default)]
struct Node {
    name: Option<String>,
    attrs: Option<Vec<Attr>>,
    value: Option<Vec<Val>>,
    children: Vec<Node>,
    self_closing: bool,
    /// (count, index) when repeated.
    repeat: Option<(usize, usize)>,
}

const INLINE: &[&str] = &[
    "a", "abbr", "acronym", "applet", "b", "basefont", "bdo", "big", "br", "button", "cite", "code", "del", "dfn",
    "em", "font", "i", "iframe", "img", "input", "ins", "kbd", "label", "map", "object", "q", "s", "samp",
    "select", "small", "span", "strike", "strong", "sub", "sup", "textarea", "tt", "u", "var",
];

const BOOLEAN_ATTRS: &[&str] = &[
    "contenteditable", "seamless", "async", "autofocus", "autoplay", "checked", "controls", "defer", "disabled",
    "formnovalidate", "hidden", "ismap", "loop", "multiple", "muted", "novalidate", "readonly", "required",
    "reversed", "selected", "typemustmatch",
];

fn variable(name: &str) -> &'static str {
    match name {
        "lang" => "en",
        "locale" => "en-US",
        "charset" => "UTF-8",
        "indentation" => "\t",
        "newline" => "\n",
        _ => "",
    }
}

fn snippets() -> &'static HashMap<String, String> {
    static S: OnceLock<HashMap<String, String>> = OnceLock::new();
    S.get_or_init(|| super::load_snippets(include_str!("data/html.json")))
}

// ------------------------------------------------------------------ parsing

/// Splits `s` into text, fields (`${1:x}`), variables (`${lang}`) and numbering (`$$`).
fn tokens(s: &[char]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == '\\' && i + 1 < s.len() {
            buf.push(s[i + 1]);
            i += 2;
            continue;
        }
        if c != '$' {
            buf.push(c);
            i += 1;
            continue;
        }
        if s.get(i + 1) == Some(&'{') {
            if let Some((tok, end)) = field(s, i + 2) {
                match tok {
                    Ok(f) => {
                        if !buf.is_empty() {
                            out.push(Tok::Text(std::mem::take(&mut buf)));
                        }
                        out.push(f);
                    }
                    Err(var) => buf.push_str(variable(&var)),
                }
                i = end;
                continue;
            }
            buf.push(c);
            i += 1;
            continue;
        }
        let mut size = 0;
        while s.get(i) == Some(&'$') {
            size += 1;
            i += 1;
        }
        let (mut reverse, mut base) = (false, 1);
        if s.get(i) == Some(&'@') {
            i += 1;
            if s.get(i) == Some(&'-') {
                reverse = true;
                i += 1;
            }
            let start = i;
            while s.get(i).is_some_and(char::is_ascii_digit) {
                i += 1;
            }
            if i > start {
                base = s[start..i].iter().collect::<String>().parse().unwrap_or(1);
            }
        }
        if !buf.is_empty() {
            out.push(Tok::Text(std::mem::take(&mut buf)));
        }
        out.push(Tok::Num { size, reverse, base });
    }
    if !buf.is_empty() {
        out.push(Tok::Text(buf));
    }
    out
}

/// A field after `${` at `i`: `Ok(field)` or `Err(variable name)`, and the index after `}`.
fn field(s: &[char], mut i: usize) -> Option<(Result<Tok, String>, usize)> {
    let start = i;
    while s.get(i).is_some_and(char::is_ascii_digit) {
        i += 1;
    }
    if i > start {
        let index: u32 = s[start..i].iter().collect::<String>().parse().ok()?;
        let mut name = String::new();
        if s.get(i) == Some(&':') {
            i += 1;
            let mut depth = 0;
            while let Some(&c) = s.get(i) {
                match c {
                    '{' => depth += 1,
                    '}' if depth == 0 => break,
                    '}' => depth -= 1,
                    _ => {}
                }
                name.push(c);
                i += 1;
            }
        }
        return (s.get(i) == Some(&'}')).then_some((Ok(Tok::Field(index, name)), i + 1));
    }
    let mut name = String::new();
    while let Some(&c) = s.get(i).filter(|c| c.is_alphanumeric() || **c == '_') {
        name.push(c);
        i += 1;
    }
    (!name.is_empty() && s.get(i) == Some(&'}')).then_some((Err(name), i + 1))
}

struct Parser {
    s: Vec<char>,
    i: usize,
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '!' | '$' | '@' | '%')
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    /// Siblings up to the end, a `)` in a group, or a climb (`^`) out of this level. Returns the
    /// items and how many levels are left to climb.
    fn list(&mut self, root: bool, in_group: bool) -> Option<(Vec<Item>, usize)> {
        let mut items = Vec::new();
        loop {
            let mut item = self.item()?;
            match self.peek() {
                Some('>') => {
                    self.i += 1;
                    let (children, climb) = self.list(false, in_group)?;
                    item.children = children;
                    items.push(item);
                    match climb {
                        0 => return Some((items, 0)),
                        1 => {}
                        _ if root => {}
                        n => return Some((items, n - 1)),
                    }
                }
                Some('+') => {
                    self.i += 1;
                    items.push(item);
                }
                Some('^') => {
                    let mut n = 0;
                    while self.peek() == Some('^') {
                        n += 1;
                        self.i += 1;
                    }
                    items.push(item);
                    if !root {
                        return Some((items, n));
                    }
                }
                Some(')') if in_group => {
                    items.push(item);
                    return Some((items, 0));
                }
                None => {
                    items.push(item);
                    return Some((items, 0));
                }
                Some(_) => return None,
            }
        }
    }

    fn item(&mut self) -> Option<Item> {
        if self.peek() == Some('(') {
            self.i += 1;
            let (items, _) = self.list(true, true)?;
            if self.peek() != Some(')') {
                return None;
            }
            self.i += 1;
            let repeat = self.repeat();
            return Some(Item { kind: Kind::Group(items), repeat, children: Vec::new() });
        }
        let start = self.i;
        let mut e = Elem::default();
        let mut name = Vec::new();
        while let Some(c) = self.peek() {
            let dotted = c == '.'
                && name.first().is_some_and(|c: &char| c.is_uppercase())
                && self.s.get(self.i + 1).is_some_and(|c| c.is_uppercase());
            if !is_name_char(c) && !dotted {
                break;
            }
            name.push(c);
            self.i += 1;
        }
        e.name = tokens(&name);
        loop {
            match self.peek() {
                Some(c @ ('#' | '.')) => {
                    self.i += 1;
                    let v = self.word();
                    let name = if c == '#' { "id" } else { "class" };
                    e.attrs.get_or_insert_with(Vec::new).push(PAttr {
                        name: vec![Tok::Text(name.into())],
                        value: Some(tokens(&v)),
                        ..Default::default()
                    });
                }
                Some('[') => {
                    self.i += 1;
                    let attrs = self.attributes()?;
                    e.attrs.get_or_insert_with(Vec::new).extend(attrs);
                }
                Some('{') => {
                    let text = self.braced()?;
                    e.value = Some(tokens(&text));
                }
                _ => break,
            }
        }
        let repeat = self.repeat();
        if self.peek() == Some('/') {
            self.i += 1;
            e.self_closing = true;
        }
        if self.i == start {
            return None;
        }
        Some(Item { kind: Kind::Elem(e), repeat, children: Vec::new() })
    }

    fn repeat(&mut self) -> Option<Option<usize>> {
        if self.peek() != Some('*') {
            return None;
        }
        self.i += 1;
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        Some(self.s[start..self.i].iter().collect::<String>().parse().ok())
    }

    /// An id or class name: up to the next operator.
    fn word(&mut self) -> Vec<char> {
        let mut out = Vec::new();
        while let Some(c) = self.peek() {
            if c == '$' && self.s.get(self.i + 1) == Some(&'{') {
                let Some(b) = self.braced_from(self.i + 1) else { break };
                out.push('$');
                out.push('{');
                out.extend(b);
                out.push('}');
                continue;
            }
            if c.is_whitespace() || "#.[]{}*>+^()/".contains(c) {
                break;
            }
            out.push(c);
            self.i += 1;
        }
        out
    }

    /// `{...}` at the cursor (nested braces allowed, `\` escapes kept): its contents.
    fn braced(&mut self) -> Option<Vec<char>> {
        self.braced_from(self.i)
    }

    fn braced_from(&mut self, open: usize) -> Option<Vec<char>> {
        let mut i = open + 1;
        let mut depth = 0;
        let mut out = Vec::new();
        while let Some(&c) = self.s.get(i) {
            match c {
                '\\' if i + 1 < self.s.len() => {
                    out.push(c);
                    out.push(self.s[i + 1]);
                    i += 2;
                    continue;
                }
                '{' => depth += 1,
                '}' if depth == 0 => {
                    self.i = i + 1;
                    return Some(out);
                }
                '}' => depth -= 1,
                _ => {}
            }
            out.push(c);
            i += 1;
        }
        None
    }

    /// The attributes inside `[...]` (after the `[`).
    fn attributes(&mut self) -> Option<Vec<PAttr>> {
        let mut out = Vec::new();
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.i += 1;
            }
            match self.peek()? {
                ']' => {
                    self.i += 1;
                    return Some(out);
                }
                q @ ('"' | '\'') => {
                    // A value without a name: nothing to output.
                    self.quoted(q)?;
                    continue;
                }
                _ => {}
            }
            let mut attr = PAttr::default();
            let mut name = Vec::new();
            while let Some(c) = self.peek() {
                if c.is_whitespace() || c == '=' || c == ']' {
                    break;
                }
                if c == '.' && self.s.get(self.i + 1).is_none_or(|n| n.is_whitespace() || *n == ']') {
                    attr.boolean = true;
                    self.i += 1;
                    break;
                }
                name.push(c);
                self.i += 1;
            }
            attr.name = tokens(&name);
            if self.peek() == Some('=') {
                self.i += 1;
                let value = match self.peek()? {
                    q @ ('"' | '\'') => self.quoted(q)?,
                    '{' => {
                        attr.expr = true;
                        self.braced()?
                    }
                    _ => {
                        let mut v = Vec::new();
                        while let Some(c) = self.peek() {
                            if c.is_whitespace() || c == ']' {
                                break;
                            }
                            if c == '$' && self.s.get(self.i + 1) == Some(&'{') {
                                let b = self.braced_from(self.i + 1)?;
                                v.push('$');
                                v.push('{');
                                v.extend(b);
                                v.push('}');
                                continue;
                            }
                            v.push(c);
                            self.i += 1;
                        }
                        v
                    }
                };
                attr.value = Some(if attr.expr { vec![Tok::Text(value.iter().collect())] } else { tokens(&value) });
            }
            if !name.is_empty() {
                out.push(attr);
            }
        }
    }

    fn quoted(&mut self, q: char) -> Option<Vec<char>> {
        let mut i = self.i + 1;
        let mut out = Vec::new();
        while let Some(&c) = self.s.get(i) {
            if c == '\\' && i + 1 < self.s.len() {
                out.push(c);
                out.push(self.s[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                self.i = i + 1;
                return Some(out);
            }
            out.push(c);
            i += 1;
        }
        None
    }
}

fn parse(src: &str) -> Option<Vec<Item>> {
    let mut p = Parser { s: src.chars().collect(), i: 0 };
    if p.s.is_empty() {
        return None;
    }
    let (items, _) = p.list(true, false)?;
    (p.i == p.s.len()).then_some(items)
}

// ------------------------------------------------------------------ repeats

fn number(tok: &Tok, repeats: &[(usize, usize)]) -> Val {
    match tok {
        Tok::Text(s) => Val::Text(s.clone()),
        Tok::Field(i, s) => Val::Field(*i, s.clone()),
        Tok::Num { size, reverse, base } => match repeats.last() {
            Some(&(count, index)) => {
                let n = if *reverse { base + count as i64 - 1 - index as i64 } else { base + index as i64 };
                Val::Text(format!("{n:0size$}"))
            }
            None => Val::Text("$".repeat(*size)),
        },
    }
}

fn number_all(toks: &[Tok], repeats: &[(usize, usize)]) -> Vec<Val> {
    let mut out: Vec<Val> = Vec::new();
    for t in toks {
        match (number(t, repeats), out.last_mut()) {
            (Val::Text(s), Some(Val::Text(prev))) => prev.push_str(&s),
            (v, _) => out.push(v),
        }
    }
    out
}

fn plain(vals: &[Val]) -> String {
    vals.iter().map(|v| if let Val::Text(s) = v { s.as_str() } else { "" }).collect()
}

/// Expands repeats and numbering into nodes.
fn convert(items: &[Item], repeats: &mut Vec<(usize, usize)>, out: &mut Vec<Node>) {
    for item in items {
        let count = item.repeat.flatten().unwrap_or(1);
        for index in 0..count {
            if item.repeat.is_some() {
                repeats.push((count, index));
            }
            match &item.kind {
                Kind::Elem(e) => {
                    let name = plain(&number_all(&e.name, repeats));
                    let mut node = Node {
                        name: (!name.is_empty()).then_some(name),
                        attrs: e.attrs.as_ref().map(|attrs| {
                            attrs
                                .iter()
                                .map(|a| Attr {
                                    name: plain(&number_all(&a.name, repeats)),
                                    value: a.value.as_ref().map(|v| number_all(v, repeats)),
                                    boolean: a.boolean,
                                    expr: a.expr,
                                })
                                .collect()
                        }),
                        value: e.value.as_ref().map(|v| number_all(v, repeats)),
                        self_closing: e.self_closing,
                        repeat: item.repeat.map(|_| (count, index)),
                        children: Vec::new(),
                    };
                    convert(&item.children, repeats, &mut node.children);
                    out.push(node);
                }
                Kind::Group(items) => {
                    let before = out.len();
                    convert(items, repeats, out);
                    if !item.children.is_empty() && out.len() > before {
                        let mut children = Vec::new();
                        convert(&item.children, repeats, &mut children);
                        deepest(out).children.extend(children);
                    }
                }
            }
            if item.repeat.is_some() {
                repeats.pop();
            }
        }
    }
}

/// The last node, descending through last children.
fn deepest(nodes: &mut [Node]) -> &mut Node {
    let last = nodes.last_mut().unwrap();
    if last.children.is_empty() {
        last
    } else {
        deepest(&mut last.children)
    }
}

// ------------------------------------------------------------------ snippets and transforms

fn parse_nodes(src: &str) -> Option<Vec<Node>> {
    let items = parse(src)?;
    let mut out = Vec::new();
    convert(&items, &mut Vec::new(), &mut out);
    Some(out)
}

/// Replaces elements named like a snippet with the snippet's elements.
fn resolve_snippets(nodes: Vec<Node>, stack: &mut Vec<&'static str>) -> Vec<Node> {
    let mut out = Vec::new();
    for mut child in nodes {
        let snippet = child.name.as_ref().and_then(|n| snippets().get(n)).map(String::as_str).filter(|s| !stack.contains(s));
        if let Some((snippet, parsed)) = snippet.and_then(|s| Some((s, parse_nodes(s)?))) {
            stack.push(snippet);
            let mut resolved = resolve_snippets(parsed, stack);
            stack.pop();
            for top in &mut resolved {
                if let Some(to) = &child.attrs {
                    top.attrs.get_or_insert_with(Vec::new).extend(to.iter().cloned());
                }
                if child.self_closing {
                    top.self_closing = true;
                }
                if child.value.is_some() {
                    top.value = child.value.clone();
                }
                if child.repeat.is_some() {
                    top.repeat = child.repeat;
                }
            }
            let children = resolve_snippets(std::mem::take(&mut child.children), stack);
            if !resolved.is_empty() {
                deepest(&mut resolved).children.extend(children);
            }
            out.extend(resolved);
            continue;
        }
        child.children = resolve_snippets(std::mem::take(&mut child.children), stack);
        out.push(child);
    }
    out
}

fn is_inline_name(name: &str) -> bool {
    INLINE.contains(&name.to_ascii_lowercase().as_str())
}

fn implicit_tag(parent: Option<&str>) -> String {
    let parent = parent.unwrap_or("").to_ascii_lowercase();
    let name = match parent.as_str() {
        "p" => "span",
        "ul" | "ol" => "li",
        "table" | "tbody" | "thead" | "tfoot" => "tr",
        "tr" => "td",
        "colgroup" => "col",
        "select" | "optgroup" => "option",
        "audio" | "video" => "source",
        "object" => "param",
        "map" => "area",
        p if is_inline_name(p) => "span",
        _ => "div",
    };
    name.into()
}

/// Merges repeated attributes: classes join with a space, others take the later value.
fn merge_attributes(node: &mut Node) {
    let Some(attrs) = node.attrs.take() else { return };
    let mut out: Vec<Attr> = Vec::new();
    for a in attrs {
        if a.name.is_empty() {
            continue;
        }
        let Some(prev) = out.iter_mut().find(|p| p.name == a.name) else {
            out.push(a);
            continue;
        };
        if a.name == "class" {
            prev.value = match (prev.value.take(), a.value) {
                (Some(mut p), Some(n)) => {
                    if !p.is_empty() {
                        append(&mut p, Val::Text(" ".into()));
                    }
                    for v in n {
                        append(&mut p, v);
                    }
                    Some(p)
                }
                (p, n) => p.or(n),
            };
        } else {
            prev.value = a.value;
            prev.boolean |= a.boolean;
            if !prev.expr {
                prev.expr = a.expr;
            }
        }
    }
    node.attrs = Some(out);
}

fn append(vals: &mut Vec<Val>, v: Val) {
    match (vals.last_mut(), v) {
        (Some(Val::Text(prev)), Val::Text(s)) => prev.push_str(&s),
        (_, v) => vals.push(v),
    }
}

fn is_empty_attr(a: &Attr) -> bool {
    match a.value.as_deref() {
        None => true,
        Some([Val::Field(_, name)]) => name.is_empty(),
        _ => false,
    }
}

fn find_input(nodes: &[Node]) -> Option<usize> {
    nodes.iter().position(|n| matches!(n.name.as_deref(), Some("input" | "textarea"))).or_else(|| {
        nodes.iter().position(|n| find_input(&n.children).is_some())
    })
}

fn input_mut(nodes: &mut [Node]) -> Option<&mut Node> {
    let i = find_input(nodes)?;
    if matches!(nodes[i].name.as_deref(), Some("input" | "textarea")) {
        Some(&mut nodes[i])
    } else {
        input_mut(&mut nodes[i].children)
    }
}

/// Implicit tags, attribute merging, lorem ipsum and `label>input` ids, top-down.
fn transform(nodes: &mut [Node], ancestors: &mut Vec<(Option<String>, Option<(usize, usize)>)>, rng: &mut super::Rng) {
    for node in nodes.iter_mut() {
        let parent = ancestors.last().and_then(|a| a.0.as_deref());
        if node.name.is_none() && node.attrs.is_some() {
            node.name = Some(implicit_tag(parent));
        }
        merge_attributes(node);
        lorem(node, ancestors, rng);
        if node.name.as_deref() == Some("label") {
            if let Some(input) = input_mut(&mut node.children) {
                if let Some(attrs) = &mut input.attrs {
                    attrs.retain(|a| !(a.name == "id" && is_empty_attr(a)));
                }
                if let Some(attrs) = &mut node.attrs {
                    attrs.retain(|a| !(a.name == "for" && is_empty_attr(a)));
                }
            }
        }
        ancestors.push((node.name.clone(), node.repeat));
        transform(&mut node.children, ancestors, rng);
        ancestors.pop();
    }
}

fn lorem(node: &mut Node, ancestors: &[(Option<String>, Option<(usize, usize)>)], rng: &mut super::Rng) {
    let Some(name) = node.name.as_deref() else { return };
    let lower = name.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("lorem").or_else(|| lower.strip_prefix("lipsum")) else { return };
    // lorem[lang][count][-max]; only Latin is bundled.
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_lowercase());
    let (min, max) = match rest.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    if !min.chars().all(|c| c.is_ascii_digit()) || max.is_some_and(|m| !m.chars().all(|c| c.is_ascii_digit())) {
        return;
    }
    let min = if min.is_empty() { 30 } else { min.parse::<usize>().unwrap_or(30).max(1) };
    let max = max.and_then(|m| m.parse::<usize>().ok()).map_or(min, |m| m.max(min));
    let count = if max > min { rng.range(min, max) } else { min };
    let repeat = node.repeat.or_else(|| ancestors.iter().rev().find_map(|a| a.1));
    node.name = None;
    node.attrs = None;
    node.value = Some(vec![Val::Text(super::lorem::paragraph(count, repeat.is_none_or(|r| r.1 == 0), rng))]);
    if node.repeat.is_some() && !ancestors.is_empty() {
        node.name = Some(implicit_tag(ancestors.last().and_then(|a| a.0.as_deref())));
    }
}

// ------------------------------------------------------------------ output

const CARET: &[Val] = &[Val::Field(0, String::new())];

struct Html<'a> {
    out: &'a mut Out,
    syntax: Syntax,
    field: u32,
}

fn is_snippet(n: &Node) -> bool {
    n.name.is_none() && n.attrs.is_none()
}

fn is_inline(n: &Node) -> bool {
    match &n.name {
        Some(name) => is_inline_name(name),
        None => n.value.is_some() && n.attrs.is_none(),
    }
}

fn has_newline(v: &Val) -> bool {
    matches!(v, Val::Text(s) if s.contains(['\r', '\n']))
}

fn starts_with_block_tag(value: &[Val]) -> bool {
    let Some(Val::Text(s)) = value.first() else { return false };
    let Some(rest) = s.strip_prefix('<') else { return false };
    let end = rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | ':'))).unwrap_or(rest.len());
    end > 0 && rest[end..].starts_with(|c: char| c.is_whitespace() || c == '>') && !is_inline_name(&rest[..end])
}

impl Html<'_> {
    fn tokens(&mut self, vals: &[Val]) {
        let mut largest = None;
        for v in vals {
            match v {
                Val::Text(s) => self.out.text(s),
                Val::Field(i, name) => {
                    self.out.field(self.field + i, name);
                    largest = largest.max(Some(*i));
                }
            }
        }
        if let Some(l) = largest {
            self.field += l + 1;
        }
    }

    fn should_format(&self, node: &Node, index: usize, items: &[Node], parent: Option<&Node>) -> bool {
        if index == 0 && parent.is_none() {
            return false;
        }
        if parent.is_some_and(is_snippet) && items.len() == 1 {
            return false;
        }
        if is_snippet(node) {
            let v = node.value.as_deref().unwrap_or_default();
            if (index > 0 && is_snippet(&items[index - 1]))
                || items.get(index + 1).is_some_and(is_snippet)
                || v.iter().any(has_newline)
                || (v.iter().any(|v| matches!(v, Val::Field(..))) && !node.children.is_empty())
            {
                return true;
            }
        }
        if !is_inline(node) {
            return true;
        }
        if index == 0 {
            if items.iter().any(|n| !is_inline(n)) {
                return true;
            }
        } else if !is_inline(&items[index - 1]) {
            return true;
        }
        let before = items[..index].iter().rev().take_while(|n| is_inline(n)).count();
        let after = items[index + 1..].iter().take_while(|n| is_inline(n)).count();
        if 1 + before + after >= 3 {
            return true;
        }
        node.children.iter().enumerate().any(|(i, c)| self.should_format(c, i, &node.children, parent))
    }

    fn element(&mut self, node: &Node, index: usize, items: &[Node], parent: Option<&Node>) {
        let format = self.should_format(node, index, items, parent);
        let indent = match parent {
            None => 0,
            Some(p) if is_snippet(p) || p.name.as_deref() == Some("html") => 0,
            Some(_) => 1,
        };
        self.out.level += indent;
        if format {
            self.out.newline(self.out.level);
        }
        if let Some(name) = &node.name {
            self.out.text(&format!("<{name}"));
            for a in node.attrs.iter().flatten() {
                self.attribute(a);
            }
            if node.self_closing && node.children.is_empty() && node.value.is_none() {
                let close = match self.syntax {
                    Syntax::Xml => "/",
                    Syntax::Jsx => " /",
                    _ => "",
                };
                self.out.text(&format!("{close}>"));
            } else {
                self.out.text(">");
                if !self.snippet(node) {
                    if let Some(v) = &node.value {
                        let inner = v.iter().any(has_newline) || starts_with_block_tag(v);
                        if inner {
                            self.out.level += 1;
                            self.out.newline(self.out.level);
                        }
                        self.tokens(v);
                        if inner {
                            self.out.level -= 1;
                            self.out.newline(self.out.level);
                        }
                    }
                    for (i, c) in node.children.iter().enumerate() {
                        self.element(c, i, &node.children, Some(node));
                    }
                    if node.value.is_none() && node.children.is_empty() {
                        let inner = name == "body";
                        if inner {
                            self.out.level += 1;
                            self.out.newline(self.out.level);
                        }
                        self.tokens(CARET);
                        if inner {
                            self.out.level -= 1;
                            self.out.newline(self.out.level);
                        }
                    }
                }
                self.out.text(&format!("</{name}>"));
            }
        } else if !self.snippet(node) {
            if let Some(v) = &node.value {
                self.tokens(v);
                for (i, c) in node.children.iter().enumerate() {
                    self.element(c, i, &node.children, Some(node));
                }
            }
        }
        if let Some(p) = parent.filter(|_| format && index + 1 == items.len()) {
            let offset = if is_snippet(p) { 0 } else { 1 };
            self.out.newline(self.out.level - offset);
        }
        self.out.level -= indent;
    }

    fn attribute(&mut self, a: &Attr) {
        let name = match (self.syntax, a.name.as_str()) {
            (Syntax::Jsx, "class") => "className",
            (Syntax::Jsx, "for") => "htmlFor",
            (_, n) => n,
        };
        let boolean = a.boolean || BOOLEAN_ATTRS.contains(&a.name.to_ascii_lowercase().as_str());
        let value = match &a.value {
            Some(v) => v.clone(),
            None if boolean => vec![Val::Text(name.into())],
            None => CARET.to_vec(),
        };
        let (open, close) = if a.expr { ("{", "}") } else { ("\"", "\"") };
        self.out.text(&format!(" {name}={open}"));
        self.tokens(&value);
        self.out.text(close);
    }

    /// A text snippet with a field and children: the children go in at the field.
    fn snippet(&mut self, node: &Node) -> bool {
        let Some(v) = node.value.as_ref().filter(|_| !node.children.is_empty()) else { return false };
        let Some(at) = v.iter().position(|v| matches!(v, Val::Field(..))) else { return false };
        self.tokens(&v[..at]);
        let line = self.out.line;
        let mut pos = at + 1;
        for (i, c) in node.children.iter().enumerate() {
            self.element(c, i, &node.children, Some(node));
        }
        if self.out.line != line {
            if let Some(Val::Text(s)) = v.get(pos) {
                self.out.text(s.trim_start());
                pos += 1;
            }
        }
        self.tokens(&v[pos.min(v.len())..]);
        true
    }
}

/// The parsed, resolved and transformed abbreviation, or None if it doesn't parse.
fn build(abbr: &str) -> Option<Vec<Node>> {
    let nodes = parse_nodes(abbr)?;
    let mut nodes = resolve_snippets(nodes, &mut Vec::new());
    transform(&mut nodes, &mut Vec::new(), &mut super::Rng::new());
    Some(nodes)
}

/// Expands a markup abbreviation to snippet text (snippet syntax, tab indents).
pub(super) fn expand(abbr: &str, syntax: Syntax) -> Option<String> {
    let nodes = build(abbr)?;
    let mut out = Out::default();
    let mut html = Html { out: &mut out, syntax, field: 1 };
    for (i, n) in nodes.iter().enumerate() {
        html.element(n, i, &nodes, None);
    }
    Some(out.text_value())
}

/// Wraps `text` with the abbreviation: into the element with a bare `*` repeat (once per
/// line) or else into the deepest element.
pub(super) fn wrap(abbr: &str, text: &str, syntax: Syntax) -> Option<String> {
    let items = parse(abbr)?;
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut nodes = Vec::new();
    convert_wrapped(&items, &lines, &mut Vec::new(), &mut nodes);
    if !has_implicit_repeat(&items) && !nodes.is_empty() {
        let content: Vec<Val> = vec![Val::Text(super::dedent(text))];
        let d = deepest(&mut nodes);
        match &mut d.value {
            Some(v) => v.extend(content),
            None => d.value = Some(content),
        }
    }
    let mut nodes = resolve_snippets(nodes, &mut Vec::new());
    transform(&mut nodes, &mut Vec::new(), &mut super::Rng::new());
    let mut out = Out::default();
    let mut html = Html { out: &mut out, syntax, field: 1 };
    for (i, n) in nodes.iter().enumerate() {
        html.element(n, i, &nodes, None);
    }
    Some(out.text_value())
}

fn has_implicit_repeat(items: &[Item]) -> bool {
    items.iter().any(|i| {
        i.repeat == Some(None)
            || has_implicit_repeat(&i.children)
            || matches!(&i.kind, Kind::Group(g) if has_implicit_repeat(g))
    })
}

/// Like `convert`, but a bare `*` repeats once per line of `lines`, each copy holding its line.
fn convert_wrapped(items: &[Item], lines: &[&str], repeats: &mut Vec<(usize, usize)>, out: &mut Vec<Node>) {
    for item in items {
        if item.repeat != Some(None) {
            let before = out.len();
            // Convert this item normally but recurse into its children with wrapping.
            let single = Item { kind: shallow(&item.kind), repeat: item.repeat, children: Vec::new() };
            convert(std::slice::from_ref(&single), repeats, out);
            if !item.children.is_empty() {
                for n in &mut out[before..] {
                    if let Some(r) = n.repeat {
                        repeats.push(r);
                    }
                    let mut kids = Vec::new();
                    convert_wrapped(&item.children, lines, repeats, &mut kids);
                    if n.repeat.is_some() {
                        repeats.pop();
                    }
                    n.children.extend(kids);
                }
            }
            continue;
        }
        let count = lines.len().max(1);
        for (index, line) in lines.iter().enumerate() {
            let single = Item { kind: shallow(&item.kind), repeat: None, children: Vec::new() };
            repeats.push((count, index));
            let mut nodes = Vec::new();
            convert(std::slice::from_ref(&single), repeats, &mut nodes);
            for n in &mut nodes {
                n.repeat = Some((count, index));
                let mut kids = Vec::new();
                convert_wrapped(&item.children, &[], repeats, &mut kids);
                n.children.extend(kids);
            }
            if let Some(d) = (!nodes.is_empty()).then(|| deepest(&mut nodes)) {
                d.value.get_or_insert_with(Vec::new).push(Val::Text((*line).to_string()));
            }
            repeats.pop();
            out.extend(nodes);
        }
    }
}

fn shallow(kind: &Kind) -> Kind {
    match kind {
        Kind::Elem(e) => Kind::Elem(Elem { name: e.name.clone(), attrs: e.attrs.clone(), value: e.value.clone(), self_closing: e.self_closing }),
        Kind::Group(items) => Kind::Group(items.iter().map(|i| Item { kind: shallow(&i.kind), repeat: i.repeat, children: i.children.iter().map(clone_item).collect() }).collect()),
    }
}

fn clone_item(i: &Item) -> Item {
    Item { kind: shallow(&i.kind), repeat: i.repeat, children: i.children.iter().map(clone_item).collect() }
}

/// Whether a plain word expands to an element named like it (`foo` → `<foo></foo>`), which
/// we treat as noise unless it's a known tag.
pub(super) fn is_known_tag(word: &str) -> bool {
    HTML_TAGS.contains(&word) || snippets().contains_key(word) || word == "lorem"
}

const HTML_TAGS: &[&str] = &[
    "html", "head", "title", "base", "link", "meta", "style", "body", "article", "section", "nav", "aside", "h1",
    "h2", "h3", "h4", "h5", "h6", "header", "footer", "address", "p", "hr", "pre", "blockquote", "ol", "ul", "li",
    "dl", "dt", "dd", "figure", "figcaption", "main", "div", "a", "em", "strong", "small", "s", "cite", "q", "dfn",
    "abbr", "ruby", "rb", "rt", "rp", "time", "code", "var", "samp", "kbd", "sub", "sup", "i", "b", "u", "mark",
    "bdi", "bdo", "span", "br", "wbr", "ins", "del", "picture", "img", "iframe", "embed", "object", "param",
    "video", "audio", "source", "track", "map", "area", "table", "caption", "colgroup", "col", "tbody", "thead",
    "tfoot", "tr", "td", "th", "form", "label", "input", "button", "select", "datalist", "optgroup", "option",
    "textarea", "output", "progress", "meter", "fieldset", "legend", "details", "summary", "dialog", "script",
    "noscript", "template", "canvas", "slot", "data", "hgroup", "menu", "search", "svg", "math",
];
