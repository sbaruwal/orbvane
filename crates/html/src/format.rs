//! Format Document for HTML: one block element per line, indented by nesting; an element whose
//! content is only text and inline elements stays on one line when it fits (`WRAP`); text has
//! its whitespace collapsed; one blank line between elements is kept where there was one.
//! `<pre>` and `<textarea>` stay exactly as written, scripts keep their lines (re-indented),
//! and `<style>` is formatted as CSS. Attributes keep their values as written.

use crate::parse::{Document, Element};

/// Lines longer than this are laid out as blocks.
const WRAP: usize = 120;

/// Elements that flow with text.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "br", "button", "cite", "code", "data", "dfn", "em", "i", "img", "input", "kbd", "label", "mark", "q", "s", "samp",
    "select", "small", "span", "strong", "sub", "sup", "time", "u", "var", "wbr",
];

/// What's inside an element, in order.
enum Item<'a> {
    Text(&'a str),
    /// A comment or doctype, kept as written.
    Raw(&'a str),
    Element(usize),
}

struct Formatter<'a> {
    doc: &'a Document,
    tab: String,
    eol: String,
    out: String,
}

/// The whole document formatted (`tab` is one indent).
pub fn format(text: &str, tab: &str, eol: &str) -> String {
    let doc = Document::parse(text);
    let mut f = Formatter { doc: &doc, tab: tab.to_string(), eol: eol.to_string(), out: String::new() };
    f.block_items(0, 0);
    let mut out = f.out.trim_end().to_string();
    out.push_str(eol);
    out
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl<'a> Formatter<'a> {
    fn text(&self) -> &'a str {
        &self.doc.text
    }

    fn el(&self, i: usize) -> &'a Element {
        &self.doc.elements[i]
    }

    fn tag(&self, i: usize) -> &'a str {
        self.el(i).tag.as_deref().unwrap_or("")
    }

    /// Where element `i`'s content starts and ends.
    fn content(&self, i: usize) -> (usize, usize) {
        let e = self.el(i);
        if i == 0 {
            return (0, self.text().len());
        }
        let start = e.start_tag_end.unwrap_or(e.end);
        let end = e.end_tag.map_or(e.end, |(lt, _)| lt).max(start);
        (start, end)
    }

    /// The text, comments and child elements inside element `i`.
    fn items(&self, i: usize) -> Vec<Item<'a>> {
        let (start, end) = self.content(i);
        let mut items = Vec::new();
        let mut at = start;
        for &c in &self.el(i).children {
            let child = self.el(c);
            self.gap(at, child.start.max(at), &mut items);
            items.push(Item::Element(c));
            at = child.end.max(at);
        }
        self.gap(at, end.max(at), &mut items);
        items
    }

    /// Text between elements: comments and doctypes apart.
    fn gap(&self, mut a: usize, b: usize, items: &mut Vec<Item<'a>>) {
        let text = self.text();
        while a < b {
            let rest = &text[a..b];
            let Some(open) = rest.find("<!") else {
                items.push(Item::Text(rest));
                return;
            };
            if open > 0 {
                items.push(Item::Text(&rest[..open]));
            }
            let close = if rest[open..].starts_with("<!--") { rest[open..].find("-->").map(|p| p + 3) } else { rest[open..].find('>').map(|p| p + 1) };
            let len = close.unwrap_or(rest.len() - open);
            items.push(Item::Raw(&rest[open..open + len]));
            a += open + len;
        }
    }

    fn is_inline(&self, i: usize) -> bool {
        INLINE.contains(&self.tag(i))
    }

    /// Element `i`'s start tag, attributes normalized.
    fn start_tag(&self, i: usize) -> String {
        let e = self.el(i);
        let text = self.text();
        let mut s = format!("<{}", &text[e.name_range.0..e.name_range.1]);
        for a in &e.attributes {
            s.push(' ');
            s.push_str(&text[a.name_range.0..a.name_range.1]);
            if let Some((v, _)) = &a.value {
                s.push('=');
                s.push_str(v);
            }
        }
        let self_closed = e.start_tag_end.is_some_and(|end| text[..end].ends_with("/>"));
        s.push_str(if self_closed { " />" } else { ">" });
        s
    }

    fn end_tag(&self, i: usize) -> Option<String> {
        let (_, (a, b)) = self.el(i).end_tag?;
        Some(format!("</{}>", &self.text()[a..b]))
    }

    /// Element `i` on one line, if its content is only text and inline elements and the line
    /// fits after `indent` columns.
    fn one_line(&self, i: usize, indent: usize) -> Option<String> {
        if matches!(self.tag(i), "pre" | "textarea" | "script" | "style") {
            return None;
        }
        let mut s = self.start_tag(i);
        let items = self.items(i);
        let mut inner = String::new();
        for item in &items {
            match item {
                Item::Text(t) => {
                    // Whitespace between words and inline elements counts once.
                    if t.starts_with(char::is_whitespace) && !inner.is_empty() && !inner.ends_with(' ') {
                        inner.push(' ');
                    }
                    inner.push_str(&collapse(t));
                    if t.ends_with(char::is_whitespace) && !t.trim().is_empty() {
                        inner.push(' ');
                    }
                }
                Item::Raw(r) if !r.contains('\n') => inner.push_str(r),
                Item::Element(c) if self.is_inline(*c) => inner.push_str(&self.one_line(*c, 0)?),
                _ => return None,
            }
        }
        s.push_str(inner.trim());
        if let Some(end) = self.end_tag(i) {
            s.push_str(&end);
        }
        (indent + s.chars().count() <= WRAP).then_some(s)
    }

    fn line(&mut self, depth: usize, text: &str) {
        self.out.push_str(&self.tab.repeat(depth));
        self.out.push_str(text);
        self.out.push_str(&self.eol);
    }

    /// The items of element `i` as lines at `depth`.
    fn block_items(&mut self, i: usize, depth: usize) {
        let items = self.items(i);
        let mut first = true;
        for item in items {
            match item {
                Item::Text(t) => {
                    // A blank line between elements stays (one).
                    if !first && t.trim().is_empty() && t.matches('\n').count() >= 2 && !self.out.ends_with(&format!("{0}{0}", self.eol)) {
                        self.out.push_str(&self.eol);
                    }
                    let t = collapse(t);
                    if !t.is_empty() {
                        self.line(depth, &t);
                        first = false;
                    }
                }
                Item::Raw(r) => {
                    for (k, l) in r.lines().enumerate() {
                        self.line(depth, if k == 0 { l.trim() } else { l.trim_end() });
                    }
                    first = false;
                }
                Item::Element(c) => {
                    self.element(c, depth);
                    first = false;
                }
            }
        }
    }

    fn element(&mut self, i: usize, depth: usize) {
        let text = self.text();
        let e = self.el(i);
        let indent = depth * self.tab.chars().count().max(1);
        match self.tag(i) {
            // Exactly as written.
            "pre" | "textarea" => {
                self.out.push_str(&self.tab.repeat(depth));
                self.out.push_str(&text[e.start..e.end]);
                self.out.push_str(&self.eol);
                return;
            }
            "script" | "style" => return self.raw_text(i, depth),
            _ => {}
        }
        if let Some(line) = self.one_line(i, indent) {
            return self.line(depth, &line);
        }
        self.line(depth, &self.start_tag(i));
        self.block_items(i, depth + 1);
        if let Some(end) = self.end_tag(i) {
            self.line(depth, &end);
        }
    }

    /// A script (its lines re-indented) or style sheet (formatted as CSS).
    fn raw_text(&mut self, i: usize, depth: usize) {
        let (a, b) = self.content(i);
        let body = &self.text()[a..b];
        let start = self.start_tag(i);
        let end = self.end_tag(i).unwrap_or_default();
        if body.trim().is_empty() {
            return self.line(depth, &format!("{start}{end}"));
        }
        let body = if self.tag(i) == "style" {
            let body = body.trim();
            let sheet = css::parse::Stylesheet::parse(body, css::parse::Syntax::Css);
            let mut out = body.to_string();
            let mut edits = css::format::format(&sheet, &self.tab, &self.eol);
            edits.sort_by(|x, y| y.0.0.cmp(&x.0.0));
            for ((s, e), new) in edits {
                out.replace_range(s..e, &new);
            }
            out
        } else {
            body.to_string()
        };
        // The lines, without the indentation they share.
        let mut lines: Vec<&str> = body.lines().collect();
        while lines.first().is_some_and(|l| l.trim().is_empty()) {
            lines.remove(0);
        }
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let shared = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        self.line(depth, &start);
        for l in lines {
            if l.trim().is_empty() {
                self.out.push_str(&self.eol);
            } else {
                let l = l.get(shared..).unwrap_or(l.trim_start());
                self.line(depth + 1, l.trim_end());
            }
        }
        self.line(depth, &end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_documents() {
        let html = "<!DOCTYPE html>\n<html><head><title>Hi</title>\n<style>body{color:red}</style></head>\n<body>\n<div class=\"a\"   id=x><p>Some   <b>bold</b>\n text.</p>\n\n\n<ul><li>One<li>Two</ul>\n<pre>  keep\n    this</pre><script>\n    let a = 1;\n      if (a) { go(); }\n</script><br/></div></body></html>\n";
        let want = "<!DOCTYPE html>
<html>
  <head>
    <title>Hi</title>
    <style>
      body {
        color: red
      }
    </style>
  </head>
  <body>
    <div class=\"a\" id=x>
      <p>Some <b>bold</b> text.</p>

      <ul>
        <li>One
        <li>Two
      </ul>
      <pre>  keep
    this</pre>
      <script>
        let a = 1;
          if (a) { go(); }
      </script>
      <br />
    </div>
  </body>
</html>
";
        let got = format(html, "  ", "\n");
        assert_eq!(got, want, "\n{got}");
        // Formatting again changes nothing.
        assert_eq!(format(&got, "  ", "\n"), got);
    }

    #[test]
    fn long_inline_content_becomes_a_block() {
        let words = "word ".repeat(30);
        let got = format(&format!("<p>{words}<a href=\"x\">link</a></p>"), "  ", "\n");
        assert_eq!(got, format!("<p>\n  {}\n  <a href=\"x\">link</a>\n</p>\n", words.trim()));
    }
}
