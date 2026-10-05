//! Editor features over a parsed HTML document, after HTML language service:
//! completion of tags, end tags, attributes and their values (and paths in `src`/`href`),
//! hovers, linked editing of tag names, highlights, folding, the outline and auto-insertion of
//! end tags and quotes. Positions are byte offsets.

use std::path::Path;

use css::features::Item;

use crate::data::{data, is_void};
use crate::parse::{Document, H};

/// LSP completion item kinds, like the standard HTML service uses them.
const KIND_TAG: u32 = 10;
const KIND_ATTRIBUTE: u32 = 12;
const KIND_VALUE: u32 = 11;
const KIND_FILE: u32 = 17;
const KIND_FOLDER: u32 = 19;

fn is_name(c: u8) -> bool {
    !c.is_ascii_whitespace() && !matches!(c, b'<' | b'>' | b'"' | b'\'' | b'=' | b'/')
}

/// The attribute-name-like word around `offset`.
fn name_around(text: &str, offset: usize) -> (usize, usize) {
    let b = text.as_bytes();
    let mut a = offset;
    while a > 0 && is_name(b[a - 1]) {
        a -= 1;
    }
    let mut e = offset;
    while e < b.len() && is_name(b[e]) {
        e += 1;
    }
    (a, e)
}

/// The element whose start tag `offset` is inside (after its `<`, up to its `>`).
fn start_tag_at(doc: &Document, offset: usize) -> Option<usize> {
    (1..doc.elements.len()).rev().find(|&i| {
        let e = &doc.elements[i];
        e.start < offset && e.start_tag_end.map_or(offset <= e.end, |end| offset < end)
    })
}

/// The innermost element still open at `offset` (whose end tag, if any, comes later).
fn open_element(doc: &Document, offset: usize) -> Option<usize> {
    let mut id = doc.element_at(offset);
    while id != 0 {
        let e = &doc.elements[id];
        let open_here = e.start_tag_end.is_some_and(|s| s <= offset) && e.end_tag.is_none_or(|(s, _)| s >= offset) && !is_void(e.tag.as_deref().unwrap_or("br"));
        if open_here && e.tag.is_some() {
            return Some(id);
        }
        id = e.parent.unwrap_or(0);
    }
    None
}

pub fn complete(doc: &Document, offset: usize, path: Option<&Path>) -> Vec<Item> {
    let text = &doc.text;
    let Some(ti) = doc.tokens.iter().rposition(|t| t.start < offset) else { return Vec::new() };
    let t = doc.tokens[ti];
    match t.kind {
        H::EndTagOpen | H::EndTag if offset <= t.end || t.kind == H::EndTagOpen => {
            let (a, b) = if t.kind == H::EndTag { (t.start, t.end) } else { (offset, name_around(text, offset).1) };
            return close_tags(doc, t.start.min(a), (a, b));
        }
        _ => {}
    }
    if let Some(id) = start_tag_at(doc, offset) {
        let e = &doc.elements[id];
        // The tag name, or just after `<`.
        if offset <= e.name_range.1 && offset > e.start {
            let range = if e.tag.is_some() { e.name_range } else { (offset, offset) };
            return tags(doc, e.start, range);
        }
        // An attribute's value.
        for a in &e.attributes {
            if let Some((_, (vs, ve))) = &a.value {
                if *vs <= offset && offset <= *ve && !(offset == *ve && is_quoted_closed(&text[*vs..*ve])) {
                    return values(doc, e.tag.as_deref().unwrap_or(""), &a.name, a, offset, path);
                }
            }
        }
        // Just after `=` with no value yet.
        if let Some(a) = e.attributes.iter().find(|a| a.value.is_none() && text[a.name_range.1..offset].trim() == "=") {
            return values(doc, e.tag.as_deref().unwrap_or(""), &a.name, a, offset, path);
        }
        let range = name_around(text, offset);
        let seen: Vec<&str> = e.attributes.iter().filter(|a| a.name_range != range).map(|a| a.name.as_str()).collect();
        return attributes(e.tag.as_deref().unwrap_or(""), range, &seen, text.as_bytes().get(range.1) == Some(&b'='));
    }
    Vec::new()
}

fn is_quoted_closed(v: &str) -> bool {
    v.len() >= 2 && (v.starts_with('"') || v.starts_with('\'')) && v.ends_with(&v[..1])
}

fn tags(doc: &Document, lt: usize, range: (usize, usize)) -> Vec<Item> {
    let mut out: Vec<Item> = data()
        .tags
        .iter()
        .map(|t| Item::new(t.name.clone(), KIND_TAG, range).docs(Some(t.markdown())))
        .collect();
    // `</div>` for the element still open here.
    if let Some(open) = open_element(doc, lt) {
        let tag = doc.elements[open].tag.clone().unwrap_or_default();
        let has_gt = doc.text.as_bytes().get(range.1) == Some(&b'>');
        let insert = if has_gt { format!("/{tag}") } else { format!("/{tag}>") };
        out.push(Item::new(format!("/{tag}"), KIND_TAG, range).snippet(insert).sort(" "));
    }
    out
}

fn close_tags(doc: &Document, lt: usize, range: (usize, usize)) -> Vec<Item> {
    let Some(open) = open_element(doc, lt) else { return Vec::new() };
    let tag = doc.elements[open].tag.clone().unwrap_or_default();
    let has_gt = doc.text[range.1..].trim_start().starts_with('>');
    let insert = if has_gt { tag.clone() } else { format!("{tag}>") };
    let docs = data().tag(&tag).map(|t| t.markdown());
    vec![Item::new(format!("/{tag}"), KIND_TAG, range).snippet(insert).docs(docs).sort(" ")]
}

fn attributes(tag: &str, range: (usize, usize), seen: &[&str], has_eq: bool) -> Vec<Item> {
    let d = data();
    let mut out = Vec::new();
    for (a, own) in d.attributes(tag) {
        if seen.iter().any(|s| s.eq_ignore_ascii_case(&a.name)) {
            continue;
        }
        let no_value = a.value_set.as_deref() == Some("v");
        let mut item = Item::new(a.name.clone(), KIND_ATTRIBUTE, range).docs(Some(a.markdown()));
        if !no_value && !has_eq {
            item = item.snippet(format!("{}=\"$1\"", a.name));
            item.retrigger = !d.values(a).is_empty();
        }
        // The element's own first, then global ones, then event handlers.
        let rank = if own { "0" } else if a.name.starts_with("on") { "2" } else { "1" };
        out.push(item.sort(format!("{rank}{}", a.name)));
    }
    out
}

const PATH_ATTRIBUTES: &[&str] = &["src", "href", "action", "poster", "data", "srcset", "cite", "formaction", "manifest"];

fn values(doc: &Document, tag: &str, name: &str, attr: &crate::parse::Attr, offset: usize, path: Option<&Path>) -> Vec<Item> {
    let (quoted, (a, b)) = match attr.inner_value() {
        Some((_, r)) => (attr.value.as_ref().is_some_and(|(v, _)| v.starts_with(['"', '\''])), r),
        None => (false, (offset, offset)),
    };
    let wrap = |v: &str| if quoted { v.to_string() } else { format!("\"{v}\"") };
    if PATH_ATTRIBUTES.contains(&name) {
        let typed = &doc.text[a..offset.max(a)];
        return paths(typed, (a + typed.rfind('/').map_or(0, |i| i + 1), b), path, quoted);
    }
    let d = data();
    let Some(attribute) = d.attribute(tag, name) else { return Vec::new() };
    d.values(attribute)
        .iter()
        .map(|v| Item::new(v.name.clone(), KIND_VALUE, (a, b)).snippet(wrap(&v.name)).docs(v.description.clone()))
        .collect()
}

/// Files and folders next to the document for a path value typed so far.
fn paths(typed: &str, range: (usize, usize), doc_path: Option<&Path>, quoted: bool) -> Vec<Item> {
    let Some(dir) = doc_path.and_then(Path::parent) else { return Vec::new() };
    if typed.contains("://") || typed.starts_with("//") || typed.starts_with('#') {
        return Vec::new();
    }
    let folder = typed.rfind('/').map_or("", |i| &typed[..=i]);
    let base = if folder.starts_with('/') { return Vec::new() } else { dir.join(folder) };
    let Ok(entries) = std::fs::read_dir(base) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let insert = if is_dir { format!("{name}/") } else { name.clone() };
        let insert = if quoted || !folder.is_empty() { insert } else { format!("\"{insert}\"") };
        let mut item = Item::new(if is_dir { format!("{name}/") } else { name.clone() }, if is_dir { KIND_FOLDER } else { KIND_FILE }, range)
            .snippet(insert.replace('$', "\\$"))
            .sort(format!("{}{name}", if is_dir { "0" } else { "1" }));
        item.retrigger = is_dir;
        out.push(item);
    }
    out
}

/// Hover text and the range it's about.
pub fn hover(doc: &Document, offset: usize) -> Option<(String, (usize, usize))> {
    let ti = doc.tokens.iter().position(|t| t.start <= offset && offset < t.end.max(t.start + 1))?;
    let t = doc.tokens[ti];
    let word = &doc.text[t.start..t.end];
    let d = data();
    match t.kind {
        H::StartTag | H::EndTag => {
            let tag = d.tag(word)?;
            Some((tag.markdown(), (t.start, t.end)))
        }
        H::AttributeName => {
            let id = start_tag_at(doc, offset)?;
            let a = d.attribute(doc.elements[id].tag.as_deref().unwrap_or(""), word)?;
            Some((a.markdown(), (t.start, t.end)))
        }
        H::AttributeValue => {
            let id = start_tag_at(doc, offset)?;
            let e = &doc.elements[id];
            let attr = e.attributes.iter().find(|a| a.value.as_ref().is_some_and(|(_, r)| r.0 == t.start))?;
            let a = d.attribute(e.tag.as_deref().unwrap_or(""), &attr.name)?;
            let (inner, range) = attr.inner_value()?;
            let v = d.values(a).iter().find(|v| v.name == inner)?;
            Some((crate::data::markdown(v.description.as_deref(), None), range)).filter(|(m, _)| !m.is_empty())
        }
        _ => None,
    }
}

/// The start and end tag names of the element whose tag name `offset` touches.
pub fn tag_pair(doc: &Document, offset: usize) -> Option<[(usize, usize); 2]> {
    doc.elements.iter().skip(1).find_map(|e| {
        let (_, end_name) = e.end_tag?;
        let touches = |(a, b): (usize, usize)| a <= offset && offset <= b;
        (e.tag.is_some() && (touches(e.name_range) || touches(end_name))).then_some([e.name_range, end_name])
    })
}

/// Foldable line ranges: elements spanning lines (up to the line before the end tag),
/// multi-line comments, and `<!-- #region -->` ... `<!-- #endregion -->`.
pub fn folding(doc: &Document, line_of: &dyn Fn(usize) -> usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for e in doc.elements.iter().skip(1) {
        if e.tag.is_none() || is_void(e.tag.as_deref().unwrap_or("")) {
            continue;
        }
        let start = line_of(e.start);
        let end = match e.end_tag {
            Some((s, _)) => line_of(s).saturating_sub(1),
            None => line_of(e.end.saturating_sub(1)),
        };
        if end > start {
            out.push((start, end));
        }
    }
    let mut regions = Vec::new();
    for (i, t) in doc.tokens.iter().enumerate() {
        if t.kind != H::Comment {
            continue;
        }
        let c = doc.text[t.start..t.end].trim();
        if c.starts_with("#region") {
            regions.push(line_of(t.start));
        } else if c.starts_with("#endregion") {
            if let Some(s) = regions.pop() {
                let end = line_of(t.start);
                if end > s {
                    out.push((s, end));
                }
            }
        } else {
            let start = doc.tokens.get(i.wrapping_sub(1)).map_or(t.start, |p| p.start);
            let (s, e) = (line_of(start), line_of(t.end));
            if e > s {
                out.push((s, e));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// An outline entry: `div#id.class`.
#[derive(Debug)]
pub struct Symbol {
    pub name: String,
    pub range: (usize, usize),
    pub selection: (usize, usize),
    pub children: Vec<Symbol>,
}

pub fn symbols(doc: &Document) -> Vec<Symbol> {
    fn walk(doc: &Document, id: usize) -> Vec<Symbol> {
        let mut out = Vec::new();
        for &c in &doc.elements[id].children {
            let e = &doc.elements[c];
            let Some(tag) = &e.tag else { continue };
            let mut name = tag.clone();
            for a in &e.attributes {
                let Some((v, _)) = a.inner_value() else { continue };
                match a.name.as_str() {
                    "id" if !v.is_empty() => name += &format!("#{}", v.split_whitespace().collect::<String>()),
                    "class" => name.extend(v.split_whitespace().map(|c| format!(".{c}"))),
                    _ => {}
                }
            }
            out.push(Symbol { name, range: (e.start, e.end), selection: e.name_range, children: walk(doc, c) });
        }
        out
    }
    walk(doc, 0)
}

/// `html/autoInsert`: after typing `>` or `</` the end tag (`autoClose`), after `=` quotes
/// (`autoQuote`). A snippet to insert at `offset`.
pub fn auto_insert(doc: &Document, offset: usize, kind: &str) -> Option<String> {
    let b = doc.text.as_bytes();
    let before = *b.get(offset.checked_sub(1)?)?;
    let token_ending = |kind: H| doc.tokens.iter().any(|t| t.kind == kind && t.end == offset);
    match (kind, before) {
        ("autoClose", b'>') if token_ending(H::StartTagClose) => {
            let e = doc.elements.iter().skip(1).find(|e| e.start_tag_end == Some(offset))?;
            let tag = e.tag.as_deref()?;
            let open = e.end_tag.is_none_or(|(s, _)| s > offset);
            (!is_void(tag) && open && !e.end_tag.is_some_and(|(s, _)| s == offset)).then(|| format!("$0</{tag}>"))
        }
        ("autoClose", b'/') if token_ending(H::EndTagOpen) => {
            let open = open_element(doc, offset - 2)?;
            // Not when the end tag already has its name.
            if doc.elements[open].end_tag.is_some_and(|(s, _)| s == offset - 2) {
                return None;
            }
            Some(format!("{}>", doc.elements[open].tag.as_deref()?))
        }
        ("autoQuote", b'=') if token_ending(H::DelimiterAssign) => {
            let next = b.get(offset).copied();
            (!matches!(next, Some(b'"' | b'\''))).then(|| "\"$1\"".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> (Document, usize) {
        let offset = text.find('|').unwrap();
        (Document::parse(&text.replace('|', "")), offset)
    }

    fn labels(text: &str) -> Vec<String> {
        let (doc, offset) = at(text);
        complete(&doc, offset, None).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn completes() {
        assert!(labels("<di|").contains(&"div".to_string()));
        assert!(labels("<div><|").contains(&"/div".to_string()));
        assert_eq!(labels("<div></|"), ["/div"]);
        let attrs = labels("<input ty|>");
        assert!(attrs.contains(&"type".to_string()) && attrs.contains(&"class".to_string()));
        assert!(!labels("<input type=\"text\" |>").contains(&"type".to_string()));
        let values = labels("<input type=\"|\">");
        assert!(values.contains(&"checkbox".to_string()));
        let (doc, offset) = at("<input |>");
        let item = complete(&doc, offset, None).into_iter().find(|i| i.label == "type").unwrap();
        assert_eq!(item.insert, "type=\"$1\"");
        assert!(item.retrigger);
    }

    #[test]
    fn hovers_and_pairs() {
        let (doc, offset) = at("<d|iv class=\"a\"></div>");
        assert!(hover(&doc, offset).unwrap().0.contains("MDN"));
        let [s, e] = tag_pair(&doc, offset).unwrap();
        assert_eq!((&doc.text[s.0..s.1], &doc.text[e.0..e.1]), ("div", "div"));
    }

    #[test]
    fn auto_inserts() {
        let (doc, offset) = at("<div>|");
        assert_eq!(auto_insert(&doc, offset, "autoClose").as_deref(), Some("$0</div>"));
        let (doc, offset) = at("<br>|");
        assert_eq!(auto_insert(&doc, offset, "autoClose"), None);
        let (doc, offset) = at("<ul><li>a</|");
        assert_eq!(auto_insert(&doc, offset, "autoClose").as_deref(), Some("li>"));
        let (doc, offset) = at("<a href=|>");
        assert_eq!(auto_insert(&doc, offset, "autoQuote").as_deref(), Some("\"$1\""));
    }

    #[test]
    fn outlines_and_folds() {
        let doc = Document::parse("<body>\n  <div id=\"x\" class=\"a b\">\n    <p>t</p>\n  </div>\n</body>\n<!-- #region -->\n<!-- #endregion -->");
        let s = symbols(&doc);
        assert_eq!(s[0].name, "body");
        assert_eq!(s[0].children[0].name, "div#x.a.b");
        let line_of = |o: usize| doc.text[..o].matches('\n').count();
        assert_eq!(folding(&doc, &line_of), [(0, 3), (1, 2), (5, 6)]);
    }
}
