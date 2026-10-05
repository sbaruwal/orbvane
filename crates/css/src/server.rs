//! The CSS language server, run in-process, and the conversions to LSP results it shares with
//! the HTML server (which hands its embedded CSS to the same functions).

use std::sync::mpsc::{Receiver, Sender};

use lsp::server::{Service, TextDocument};
use serde_json::{json, Value};

use crate::features::{self, Item};
use crate::lint::{self, Severity};
use crate::parse::{Stylesheet, Syntax};
use crate::format;

/// Converts a byte range to an LSP range.
pub type ToRange<'a> = &'a dyn Fn(usize, usize) -> Value;

pub fn completion_items(items: Vec<Item>, range: ToRange) -> Vec<Value> {
    items
        .into_iter()
        .map(|i| {
            let mut item = json!({
                "label": i.label,
                "kind": i.kind,
                "sortText": i.sort,
                "textEdit": { "range": range(i.range.0, i.range.1), "newText": i.insert },
            });
            if i.snippet {
                item["insertTextFormat"] = json!(2);
            }
            if let Some(d) = i.documentation {
                item["documentation"] = json!({ "kind": "markdown", "value": d });
            }
            if i.retrigger {
                item["command"] = json!({ "title": "Suggest", "command": "editor.action.triggerSuggest" });
            }
            item
        })
        .collect()
}

pub fn diagnostics(s: &Stylesheet, range: ToRange) -> Vec<Value> {
    lint::lint(s)
        .into_iter()
        .map(|p| {
            let severity = if p.severity == Severity::Error { 1 } else { 2 };
            json!({ "range": range(p.start, p.end), "message": p.message, "severity": severity, "source": "css" })
        })
        .collect()
}

pub fn hover(s: &Stylesheet, offset: usize, range: ToRange) -> Value {
    match features::hover(s, offset) {
        Some((text, (a, b))) => json!({ "contents": { "kind": "markdown", "value": text }, "range": range(a, b) }),
        None => Value::Null,
    }
}

pub fn symbols(s: &Stylesheet, range: ToRange) -> Vec<Value> {
    fn to_lsp(sym: features::Symbol, range: ToRange) -> Value {
        json!({
            "name": sym.name,
            "kind": sym.kind,
            "range": range(sym.range.0, sym.range.1),
            "selectionRange": range(sym.selection.0, sym.selection.1),
            "children": sym.children.into_iter().map(|c| to_lsp(c, range)).collect::<Vec<_>>(),
        })
    }
    features::symbols(s).into_iter().map(|sym| to_lsp(sym, range)).collect()
}

/// Folding ranges as (start line, end line); a block's closing line stays visible.
pub fn folding(s: &Stylesheet, line_of: &dyn Fn(usize) -> usize) -> Vec<Value> {
    features::folding(s)
        .into_iter()
        .filter_map(|(a, b)| {
            let block = s.text.as_bytes().get(b - 1) == Some(&b'}');
            let end = if block { line_of(b - 1).checked_sub(1)? } else { line_of(b) };
            let start = line_of(a);
            (end > start).then(|| json!({ "startLine": start, "endLine": end }))
        })
        .collect()
}

pub fn document_colors(s: &Stylesheet, range: ToRange) -> Vec<Value> {
    features::document_colors(s)
        .into_iter()
        .map(|(a, b, [r, g, bl, al])| json!({ "range": range(a, b), "color": { "red": r, "green": g, "blue": bl, "alpha": al } }))
        .collect()
}

/// `textDocument/colorPresentation`: the color as rgb(), hex and hsl().
pub fn color_presentations(color: &Value, range: Value) -> Vec<Value> {
    let get = |k: &str| color[k].as_f64().unwrap_or(0.0);
    let (r, g, b, a) = (get("red"), get("green"), get("blue"), get("alpha"));
    let byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let alpha = (a * 100.0).round() / 100.0;
    let (h, s, l) = hsl(r, g, b);
    let labels = if a >= 1.0 {
        [
            format!("rgb({}, {}, {})", byte(r), byte(g), byte(b)),
            format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b)),
            format!("hsl({h}, {s}%, {l}%)"),
        ]
    } else {
        [
            format!("rgba({}, {}, {}, {alpha})", byte(r), byte(g), byte(b)),
            format!("#{:02x}{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b), byte(a)),
            format!("hsla({h}, {s}%, {l}%, {alpha})"),
        ]
    };
    labels.into_iter().map(|label| json!({ "label": label, "textEdit": { "range": range, "newText": label } })).collect()
}

fn hsl(r: f64, g: f64, b: f64) -> (i64, i64, i64) {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    let s = if d == 0.0 { 0.0 } else { d / (1.0 - (2.0 * l - 1.0).abs()) };
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.round() as i64, (s * 100.0).round() as i64, (l * 100.0).round() as i64)
}

#[derive(Default)]
struct CssService;

/// Runs the server until the client goes away or sends `exit`.
pub fn serve(rx: Receiver<Value>, tx: Sender<Value>) {
    lsp::server::serve(rx, tx, CssService);
}

/// The capabilities both the CSS and HTML servers announce for CSS.
pub fn capabilities() -> Value {
    json!({
        "completionProvider": { "triggerCharacters": ["/", "-", ":"] },
        "hoverProvider": true,
        "documentSymbolProvider": true,
        "foldingRangeProvider": true,
        "colorProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "renameProvider": { "prepareProvider": true },
        "documentFormattingProvider": true,
    })
}

impl Service for CssService {
    fn initialize(&mut self, _params: &Value) -> Value {
        json!({ "capabilities": capabilities(), "serverInfo": { "name": "orbvane-css" } })
    }

    fn diagnose(&mut self, doc: &TextDocument) -> Vec<Value> {
        let s = Stylesheet::parse(&doc.text, Syntax::of(&doc.language_id));
        diagnostics(&s, &|a, b| doc.range(a, b))
    }

    fn request(&mut self, method: &str, params: &Value, doc: &TextDocument) -> Option<Value> {
        let s = Stylesheet::parse(&doc.text, Syntax::of(&doc.language_id));
        let offset = || doc.offset_at(&params["position"]);
        let range = |a, b| doc.range(a, b);
        let location = |(a, b): (usize, usize)| json!({ "uri": doc.uri, "range": doc.range(a, b) });
        Some(match method {
            "textDocument/completion" => json!({ "isIncomplete": false, "items": completion_items(features::complete(&s, offset()), &range) }),
            "textDocument/hover" => hover(&s, offset(), &range),
            "textDocument/documentSymbol" => Value::Array(symbols(&s, &range)),
            "textDocument/foldingRange" => Value::Array(folding(&s, &|o| doc.line_of(o))),
            "textDocument/documentColor" => Value::Array(document_colors(&s, &range)),
            "textDocument/colorPresentation" => Value::Array(color_presentations(&params["color"], params["range"].clone())),
            "textDocument/definition" => match features::references(&s, offset()) {
                Some((_, defs)) if !defs.is_empty() => location(defs[0]),
                _ => Value::Null,
            },
            "textDocument/references" => match features::references(&s, offset()) {
                Some((refs, _)) => Value::Array(refs.into_iter().map(location).collect()),
                None => Value::Array(Vec::new()),
            },
            "textDocument/prepareRename" => match features::rename_range(&s, offset()) {
                Some((a, b)) => json!({ "range": doc.range(a, b), "placeholder": &doc.text[a..b] }),
                None => Value::Null,
            },
            "textDocument/rename" => {
                let new_name = params["newName"].as_str().unwrap_or_default();
                let Some((refs, _)) = features::references(&s, offset()) else { return Some(Value::Null) };
                let edits: Vec<Value> = refs.into_iter().map(|(a, b)| json!({ "range": doc.range(a, b), "newText": new_name })).collect();
                json!({ "changes": { doc.uri.clone(): edits } })
            }
            "textDocument/formatting" => {
                let options = &params["options"];
                let size = options["tabSize"].as_u64().unwrap_or(4) as usize;
                let tab = if options["insertSpaces"].as_bool().unwrap_or(true) { " ".repeat(size) } else { "\t".into() };
                let eol = if doc.text.contains("\r\n") { "\r\n" } else { "\n" };
                Value::Array(format::format(&s, &tab, eol).into_iter().map(|((a, b), new)| json!({ "range": doc.range(a, b), "newText": new })).collect())
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presents_colors() {
        let labels: Vec<String> = color_presentations(&json!({ "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 }), Value::Null)
            .iter()
            .map(|p| p["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(labels, ["rgb(255, 0, 0)", "#ff0000", "hsl(0, 100%, 50%)"]);
    }

    #[test]
    fn colors_parse() {
        assert!(crate::colors::hex("#abc").is_some());
    }
}
