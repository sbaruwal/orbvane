//! The HTML language server, run in-process. CSS in `<style>` and `style=""` is handed to the
//! CSS server's functions: `<style>` contents are parsed as one stylesheet over a copy of the
//! document with everything else blanked out (so offsets match), each `style` attribute as
//! declarations of its own.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use css::parse::{Stylesheet, Syntax};
use lsp::server::{Service, TextDocument};
use serde_json::{json, Value};

use crate::features;
use crate::parse::{Document, H};

/// `<style>` contents as a stylesheet of the document's length.
fn style_sheet(doc: &Document) -> Option<Stylesheet> {
    let mut masked: Vec<u8> = doc.text.bytes().map(|c| if c == b'\n' { b'\n' } else { b' ' }).collect();
    let mut any = false;
    for t in doc.tokens.iter().filter(|t| t.kind == H::Styles) {
        masked[t.start..t.end].copy_from_slice(&doc.text.as_bytes()[t.start..t.end]);
        any = true;
    }
    any.then(|| Stylesheet::parse(&String::from_utf8(masked).unwrap_or_default(), Syntax::Css))
}

/// The `style` attributes: (offset of the value, its declarations).
fn style_attributes(doc: &Document) -> Vec<(usize, Stylesheet)> {
    doc.elements
        .iter()
        .flat_map(|e| &e.attributes)
        .filter(|a| a.name == "style")
        .filter_map(|a| a.inner_value())
        .map(|(v, (start, _))| (start, Stylesheet::parse_declarations(v, Syntax::Css)))
        .collect()
}

fn in_style(doc: &Document, offset: usize) -> bool {
    doc.tokens.iter().any(|t| t.kind == H::Styles && t.start <= offset && offset <= t.end)
}

/// The `style` attribute whose value holds `offset`.
fn style_attribute_at(doc: &Document, offset: usize) -> Option<(usize, Stylesheet)> {
    let a = doc.elements.iter().flat_map(|e| &e.attributes).find(|a| {
        a.name == "style" && a.inner_value().is_some_and(|(_, (s, e))| s <= offset && offset <= e)
    })?;
    let (v, (start, _)) = a.inner_value()?;
    Some((start, Stylesheet::parse_declarations(v, Syntax::Css)))
}

fn file_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    // Undo percent-encoding.
    let mut out = Vec::new();
    let b = path.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = path.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

#[derive(Default)]
struct HtmlService;

/// Runs the server until the client goes away or sends `exit`.
pub fn serve(rx: Receiver<Value>, tx: Sender<Value>) {
    lsp::server::serve(rx, tx, HtmlService);
}

impl Service for HtmlService {
    fn initialize(&mut self, _params: &Value) -> Value {
        json!({
            "capabilities": {
                "completionProvider": { "triggerCharacters": [".", ":", "<", "\"", "=", "/"] },
                "hoverProvider": true,
                "documentHighlightProvider": true,
                "linkedEditingRangeProvider": true,
                "documentSymbolProvider": true,
                "foldingRangeProvider": true,
                "colorProvider": true,
            },
            "serverInfo": { "name": "orbvane-html" },
        })
    }

    fn diagnose(&mut self, doc: &TextDocument) -> Vec<Value> {
        let html = Document::parse(&doc.text);
        let mut out = style_sheet(&html).map(|s| css::server::diagnostics(&s, &|a, b| doc.range(a, b))).unwrap_or_default();
        for (base, s) in style_attributes(&html) {
            out.extend(css::server::diagnostics(&s, &|a, b| doc.range(base + a, base + b)));
        }
        out
    }

    fn request(&mut self, method: &str, params: &Value, doc: &TextDocument) -> Option<Value> {
        let html = Document::parse(&doc.text);
        let offset = || doc.offset_at(&params["position"]);
        let range = |a, b| doc.range(a, b);
        Some(match method {
            "textDocument/completion" => {
                let offset = offset();
                let items = if in_style(&html, offset) {
                    let s = style_sheet(&html)?;
                    css::server::completion_items(css::features::complete(&s, offset), &range)
                } else if let Some((base, s)) = style_attribute_at(&html, offset) {
                    css::server::completion_items(css::features::complete(&s, offset - base), &|a, b| doc.range(base + a, base + b))
                } else {
                    let path = file_path(&doc.uri);
                    css::server::completion_items(features::complete(&html, offset, path.as_deref()), &range)
                };
                json!({ "isIncomplete": false, "items": items })
            }
            "textDocument/hover" => {
                let offset = offset();
                if in_style(&html, offset) {
                    css::server::hover(&style_sheet(&html)?, offset, &range)
                } else if let Some((base, s)) = style_attribute_at(&html, offset) {
                    css::server::hover(&s, offset - base, &|a, b| doc.range(base + a, base + b))
                } else {
                    match features::hover(&html, offset) {
                        Some((text, (a, b))) => json!({ "contents": { "kind": "markdown", "value": text }, "range": range(a, b) }),
                        None => Value::Null,
                    }
                }
            }
            "textDocument/documentHighlight" => match features::tag_pair(&html, offset()) {
                Some(pair) => Value::Array(pair.iter().map(|&(a, b)| json!({ "range": range(a, b), "kind": 1 })).collect()),
                None => Value::Null,
            },
            "textDocument/linkedEditingRange" => match features::tag_pair(&html, offset()) {
                Some(pair) => json!({ "ranges": pair.iter().map(|&(a, b)| range(a, b)).collect::<Vec<_>>(), "wordPattern": "[-_\\.:a-zA-Z0-9]+" }),
                None => Value::Null,
            },
            "textDocument/documentSymbol" => {
                fn to_lsp(sym: features::Symbol, range: &dyn Fn(usize, usize) -> Value) -> Value {
                    json!({
                        "name": sym.name,
                        "kind": 8,
                        "range": range(sym.range.0, sym.range.1),
                        "selectionRange": range(sym.selection.0, sym.selection.1),
                        "children": sym.children.into_iter().map(|c| to_lsp(c, range)).collect::<Vec<_>>(),
                    })
                }
                Value::Array(features::symbols(&html).into_iter().map(|s| to_lsp(s, &range)).collect())
            }
            "textDocument/foldingRange" => {
                let line_of = |o| doc.line_of(o);
                let mut out: Vec<Value> = features::folding(&html, &line_of).into_iter().map(|(s, e)| json!({ "startLine": s, "endLine": e })).collect();
                if let Some(s) = style_sheet(&html) {
                    out.extend(css::server::folding(&s, &line_of));
                }
                Value::Array(out)
            }
            "textDocument/documentColor" => {
                let mut out = style_sheet(&html).map(|s| css::server::document_colors(&s, &range)).unwrap_or_default();
                for (base, s) in style_attributes(&html) {
                    out.extend(css::server::document_colors(&s, &|a, b| doc.range(base + a, base + b)));
                }
                Value::Array(out)
            }
            "textDocument/colorPresentation" => Value::Array(css::server::color_presentations(&params["color"], params["range"].clone())),
            "html/autoInsert" => {
                let kind = params["kind"].as_str().unwrap_or_default();
                features::auto_insert(&html, offset(), kind).map_or(Value::Null, Value::String)
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(text: &str, method: &str) -> Value {
        let offset = text.find('|').unwrap();
        let doc = TextDocument::new("file:///x.html", "html", text.replace('|', ""));
        let params = json!({ "position": doc.position_at(offset), "kind": "autoClose" });
        HtmlService.request(method, &params, &doc).unwrap()
    }

    #[test]
    fn embeds_css() {
        let items = request("<style>\n  a { col| }\n</style>", "textDocument/completion");
        assert!(items["items"].as_array().unwrap().iter().any(|i| i["label"] == "color"));
        let items = request("<p style=\"col|\">", "textDocument/completion");
        let color = items["items"].as_array().unwrap().iter().find(|i| i["label"] == "color").unwrap().clone();
        assert_eq!(color["textEdit"]["range"]["start"], json!({ "line": 0, "character": 10 }));
        let colors = request("<p style=\"color: red\">|<style>b { color: #fff }</style>", "textDocument/documentColor");
        assert_eq!(colors.as_array().unwrap().len(), 2);
        let doc = TextDocument::new("file:///x.html", "html", "<style>a { colr: red }</style><p style=\"colr: 1\">".into());
        let d = HtmlService.diagnose(&doc);
        assert_eq!(d.len(), 2, "{d:?}");
    }

    #[test]
    fn auto_closes() {
        assert_eq!(request("<div>|", "html/autoInsert"), json!("$0</div>"));
    }

    #[test]
    fn decodes_paths() {
        assert_eq!(file_path("file:///a%20b/c.html").unwrap(), PathBuf::from("/a b/c.html"));
    }
}
