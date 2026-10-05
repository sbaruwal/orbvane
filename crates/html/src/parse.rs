//! HTML scanning and parsing, after HTML language service: a state machine that
//! tokenizes tags, attributes, comments and raw `<script>`/`<style>` text, and a tolerant parser
//! that builds the element tree (void elements, optional end tags, unclosed elements).

use crate::data::{closed_by, is_void};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H {
    StartCommentTag,
    Comment,
    EndCommentTag,
    StartDoctypeTag,
    Doctype,
    EndDoctypeTag,
    /// `<`
    StartTagOpen,
    StartTag,
    /// `>` (zero length when the tag ends at a `<`).
    StartTagClose,
    StartTagSelfClose,
    /// `</`
    EndTagOpen,
    EndTag,
    EndTagClose,
    AttributeName,
    DelimiterAssign,
    AttributeValue,
    Content,
    Script,
    Styles,
    Whitespace,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: H,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    WithinContent,
    AfterOpeningStartTag,
    WithinTag,
    AfterAttributeName,
    BeforeAttributeValue,
    AfterOpeningEndTag,
    WithinEndTag,
    WithinComment,
    WithinDoctype,
    WithinScript,
    WithinStyle,
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    (from..hay.len().saturating_sub(needle.len() - 1)).find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

pub fn tokenize(text: &str) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut state = State::WithinContent;
    let mut last_tag = String::new();
    let tag_name_end = |mut j: usize| {
        while j < b.len() && !is_space(b[j]) && !matches!(b[j], b'>' | b'/' | b'<' | b'"' | b'\'' | b'=') {
            j += 1;
        }
        j
    };
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match state {
            State::WithinContent => {
                if b[i..].starts_with(b"<!--") {
                    i += 4;
                    state = State::WithinComment;
                    H::StartCommentTag
                } else if b[i..].starts_with(b"<!") {
                    i += 2;
                    state = State::WithinDoctype;
                    H::StartDoctypeTag
                } else if b[i..].starts_with(b"</") {
                    i += 2;
                    state = State::AfterOpeningEndTag;
                    H::EndTagOpen
                } else if c == b'<' {
                    i += 1;
                    state = State::AfterOpeningStartTag;
                    H::StartTagOpen
                } else {
                    i = b[i + 1..].iter().position(|&x| x == b'<').map_or(b.len(), |k| i + 1 + k);
                    H::Content
                }
            }
            State::WithinComment => match text[i..].find("-->") {
                Some(0) => {
                    i += 3;
                    state = State::WithinContent;
                    H::EndCommentTag
                }
                Some(k) => {
                    i += k;
                    H::Comment
                }
                None => {
                    i = b.len();
                    H::Comment
                }
            },
            State::WithinDoctype => match b[i..].iter().position(|&x| x == b'>') {
                Some(0) => {
                    i += 1;
                    state = State::WithinContent;
                    H::EndDoctypeTag
                }
                Some(k) => {
                    i += k;
                    H::Doctype
                }
                None => {
                    i = b.len();
                    H::Doctype
                }
            },
            State::AfterOpeningEndTag => {
                if is_space(c) {
                    while i < b.len() && is_space(b[i]) {
                        i += 1;
                    }
                    H::Whitespace
                } else if c == b'>' {
                    i += 1;
                    state = State::WithinContent;
                    H::EndTagClose
                } else if tag_name_end(i) > i {
                    i = tag_name_end(i);
                    state = State::WithinEndTag;
                    H::EndTag
                } else {
                    state = State::WithinContent;
                    continue;
                }
            }
            State::WithinEndTag => {
                if is_space(c) {
                    while i < b.len() && is_space(b[i]) {
                        i += 1;
                    }
                    H::Whitespace
                } else if c == b'>' {
                    i += 1;
                    state = State::WithinContent;
                    H::EndTagClose
                } else if c == b'<' {
                    state = State::WithinContent;
                    out.push(Token { kind: H::EndTagClose, start: i, end: i });
                    continue;
                } else {
                    i += 1;
                    H::Unknown
                }
            }
            State::AfterOpeningStartTag => {
                if tag_name_end(i) > i && !is_space(c) {
                    i = tag_name_end(i);
                    last_tag = text[start..i].to_ascii_lowercase();
                    state = State::WithinTag;
                    H::StartTag
                } else if is_space(c) {
                    while i < b.len() && is_space(b[i]) {
                        i += 1;
                    }
                    H::Whitespace
                } else {
                    state = State::WithinTag;
                    last_tag.clear();
                    continue;
                }
            }
            State::WithinTag => {
                if is_space(c) {
                    while i < b.len() && is_space(b[i]) {
                        i += 1;
                    }
                    H::Whitespace
                } else if b[i..].starts_with(b"/>") {
                    i += 2;
                    state = State::WithinContent;
                    H::StartTagSelfClose
                } else if c == b'>' {
                    i += 1;
                    state = match last_tag.as_str() {
                        "script" => State::WithinScript,
                        "style" => State::WithinStyle,
                        _ => State::WithinContent,
                    };
                    H::StartTagClose
                } else if c == b'<' {
                    state = State::WithinContent;
                    out.push(Token { kind: H::StartTagClose, start: i, end: i });
                    continue;
                } else if !matches!(c, b'"' | b'\'' | b'=' | b'/') {
                    while i < b.len() && !is_space(b[i]) && !matches!(b[i], b'"' | b'\'' | b'=' | b'<' | b'>') && !b[i..].starts_with(b"/>") {
                        i += 1;
                    }
                    state = State::AfterAttributeName;
                    H::AttributeName
                } else {
                    i += 1;
                    H::Unknown
                }
            }
            State::AfterAttributeName => {
                if is_space(c) {
                    while i < b.len() && is_space(b[i]) {
                        i += 1;
                    }
                    H::Whitespace
                } else if c == b'=' {
                    i += 1;
                    state = State::BeforeAttributeValue;
                    H::DelimiterAssign
                } else {
                    state = State::WithinTag;
                    continue;
                }
            }
            State::BeforeAttributeValue => {
                if is_space(c) {
                    while i < b.len() && is_space(b[i]) {
                        i += 1;
                    }
                    H::Whitespace
                } else if c == b'"' || c == b'\'' {
                    i = b[i + 1..].iter().position(|&x| x == c).map_or(b.len(), |k| i + 1 + k + 1);
                    state = State::WithinTag;
                    H::AttributeValue
                } else if c == b'>' || c == b'<' {
                    state = State::WithinTag;
                    continue;
                } else {
                    while i < b.len() && !is_space(b[i]) && !matches!(b[i], b'>' | b'<' | b'"' | b'\'' | b'`' | b'=') && !b[i..].starts_with(b"/>") {
                        i += 1;
                    }
                    state = State::WithinTag;
                    H::AttributeValue
                }
            }
            State::WithinScript | State::WithinStyle => {
                let (needle, kind): (&[u8], H) = if state == State::WithinScript { (b"</script", H::Script) } else { (b"</style", H::Styles) };
                state = State::WithinContent;
                match find_ci(b, i, needle) {
                    Some(k) if k == i => continue,
                    Some(k) => {
                        i = k;
                        kind
                    }
                    None => {
                        i = b.len();
                        kind
                    }
                }
            }
        };
        out.push(Token { kind, start, end: i });
    }
    out
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub name: String,
    pub name_range: (usize, usize),
    /// The value as written (quotes included) and its range.
    pub value: Option<(String, (usize, usize))>,
}

impl Attr {
    /// The value without its quotes, and that range.
    pub fn inner_value(&self) -> Option<(&str, (usize, usize))> {
        let (v, (a, b)) = self.value.as_ref()?;
        let quoted = v.len() >= 1 && (v.starts_with('"') || v.starts_with('\''));
        if !quoted {
            return Some((v, (*a, *b)));
        }
        let closed = v.len() >= 2 && v.ends_with(&v[..1]);
        let inner = if closed { &v[1..v.len() - 1] } else { &v[1..] };
        Some((inner, (a + 1, a + 1 + inner.len())))
    }
}

#[derive(Debug)]
pub struct Element {
    /// Lowercase tag name (None for the root).
    pub tag: Option<String>,
    pub start: usize,
    pub end: usize,
    /// Where the start tag's name is.
    pub name_range: (usize, usize),
    pub start_tag_end: Option<usize>,
    /// The `</` of the end tag, and the end tag's name.
    pub end_tag: Option<(usize, (usize, usize))>,
    /// Self-closed, void, or has its end tag.
    pub closed: bool,
    pub attributes: Vec<Attr>,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

pub struct Document {
    pub text: String,
    pub tokens: Vec<Token>,
    /// Element 0 is the root.
    pub elements: Vec<Element>,
}

impl Document {
    pub fn parse(text: &str) -> Document {
        let tokens = tokenize(text);
        let root = Element { tag: None, start: 0, end: text.len(), name_range: (0, 0), start_tag_end: None, end_tag: None, closed: true, attributes: Vec::new(), parent: None, children: Vec::new() };
        let mut elements = vec![root];
        let mut curr = 0;
        let mut end_tag_open = 0;
        let mut pending_end: Option<usize> = None;
        for t in &tokens {
            let word = &text[t.start..t.end];
            match t.kind {
                H::StartTagOpen => {
                    let child = Element {
                        tag: None,
                        start: t.start,
                        end: text.len(),
                        name_range: (t.end, t.end),
                        start_tag_end: None,
                        end_tag: None,
                        closed: false,
                        attributes: Vec::new(),
                        parent: Some(curr),
                        children: Vec::new(),
                    };
                    elements.push(child);
                    let id = elements.len() - 1;
                    elements[curr].children.push(id);
                    curr = id;
                }
                H::StartTag => {
                    let tag = word.to_ascii_lowercase();
                    // `<li>` after an open `<li>` ends it.
                    let parent = elements[curr].parent.unwrap_or(0);
                    if elements[parent].tag.as_deref().is_some_and(|o| !elements[parent].closed && closed_by(o, &tag)) {
                        let me = elements[parent].children.pop().unwrap();
                        elements[parent].end = elements[me].start;
                        let grand = elements[parent].parent.unwrap_or(0);
                        elements[me].parent = Some(grand);
                        elements[grand].children.push(me);
                    }
                    elements[curr].tag = Some(tag);
                    elements[curr].name_range = (t.start, t.end);
                }
                H::StartTagClose | H::StartTagSelfClose => {
                    if curr != 0 {
                        let e = &mut elements[curr];
                        e.start_tag_end = Some(t.end);
                        if t.kind == H::StartTagSelfClose || e.tag.as_deref().is_some_and(is_void) || e.tag.is_none() {
                            e.closed = true;
                            e.end = t.end;
                            curr = e.parent.unwrap_or(0);
                        }
                    }
                }
                H::AttributeName => {
                    if curr != 0 {
                        elements[curr].attributes.push(Attr { name: word.to_ascii_lowercase(), name_range: (t.start, t.end), value: None });
                    }
                }
                H::AttributeValue => {
                    if let Some(a) = elements[curr].attributes.last_mut() {
                        a.value = Some((word.to_string(), (t.start, t.end)));
                    }
                }
                H::EndTagOpen => {
                    end_tag_open = t.start;
                    // A start tag that never got its `>`.
                    if curr != 0 && elements[curr].start_tag_end.is_none() {
                        elements[curr].end = t.start;
                        curr = elements[curr].parent.unwrap_or(0);
                    }
                }
                H::EndTag => {
                    let name = word.to_ascii_lowercase();
                    // The nearest open element with this name; the ones inside it end here.
                    let mut e = curr;
                    let mut found = None;
                    while e != 0 {
                        if elements[e].tag.as_deref() == Some(name.as_str()) && !elements[e].closed {
                            found = Some(e);
                            break;
                        }
                        e = elements[e].parent.unwrap_or(0);
                    }
                    if let Some(f) = found {
                        let mut e = curr;
                        while e != f {
                            elements[e].end = end_tag_open;
                            e = elements[e].parent.unwrap_or(0);
                        }
                        elements[f].closed = true;
                        elements[f].end_tag = Some((end_tag_open, (t.start, t.end)));
                        curr = f;
                        pending_end = Some(f);
                    }
                }
                H::EndTagClose => {
                    if let Some(f) = pending_end.take() {
                        elements[f].end = t.end;
                        curr = elements[f].parent.unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
        Document { text: text.to_string(), tokens, elements }
    }

    /// The innermost element containing `offset` (between its `<` and the end of its end tag).
    pub fn element_at(&self, offset: usize) -> usize {
        let mut id = 0;
        'down: loop {
            for &c in &self.elements[id].children {
                let e = &self.elements[c];
                if e.start < offset && offset < e.end.max(e.start + 1) || (e.start < offset && offset == e.end && !e.closed) {
                    id = c;
                    continue 'down;
                }
            }
            return id;
        }
    }

    /// The token containing `offset` (or ending at it).
    pub fn token_at(&self, offset: usize) -> Option<usize> {
        let i = self.tokens.partition_point(|t| t.end < offset);
        (i < self.tokens.len()).then_some(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes() {
        let text = "<!DOCTYPE html><div class=\"a\" hidden><!-- c --><style>p{}</style></div>";
        let kinds: Vec<H> = tokenize(text).iter().map(|t| t.kind).collect();
        use H::*;
        assert_eq!(kinds, [
            StartDoctypeTag, Doctype, EndDoctypeTag, StartTagOpen, StartTag, Whitespace, AttributeName, DelimiterAssign, AttributeValue,
            Whitespace, AttributeName, StartTagClose, StartCommentTag, Comment, EndCommentTag, StartTagOpen, StartTag, StartTagClose, Styles,
            EndTagOpen, EndTag, EndTagClose, EndTagOpen, EndTag, EndTagClose
        ]);
    }

    fn tree(d: &Document, id: usize) -> String {
        let e = &d.elements[id];
        let inner: Vec<String> = e.children.iter().map(|&c| tree(d, c)).collect();
        match &e.tag {
            Some(t) => format!("{t}[{}]", inner.join(",")),
            None => inner.join(","),
        }
    }

    #[test]
    fn builds_the_tree() {
        let d = Document::parse("<ul><li>a<li>b</ul><p>x<div>y</div><br><img src=x>");
        assert_eq!(tree(&d, 0), "ul[li[],li[]],p[],div[],br[],img[]");
        let ul = &d.elements[d.elements[0].children[0]];
        assert!(ul.closed && ul.end_tag.is_some());
        let d = Document::parse("<div><span>open</div>");
        assert_eq!(tree(&d, 0), "div[span[]]");
        assert!(!d.elements[2].closed);
        let at = d.element_at(d.text.find("open").unwrap());
        assert_eq!(d.elements[at].tag.as_deref(), Some("span"));
    }
}
