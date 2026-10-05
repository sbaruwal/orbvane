//! The JSON language server, run in-process (`lsp::Client::in_process`). Schemas come with
//! `initialize` (`initializationOptions.schemas`: `{ fileMatch, uri, schema }`, like the
//! `json.schemas` setting) or from a document's own `$schema` (a local file).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use lsp::server::{Service, TextDocument};
use serde_json::{json, Value};

use crate::features::{self, Analysis};
use crate::parse::{Doc, ErrorKind};
use crate::schema::{self, Severity, Validator};

struct Association {
    patterns: Vec<String>,
    uri: String,
    schema: Value,
}

#[derive(Default)]
struct JsonService {
    associations: Vec<Association>,
    /// Schemas loaded from files named by `$schema`.
    loaded: HashMap<PathBuf, Option<Value>>,
}

/// Runs the server until the client goes away or sends `exit`.
pub fn serve(rx: Receiver<Value>, tx: Sender<Value>) {
    lsp::server::serve(rx, tx, JsonService::default());
}

impl Service for JsonService {
    fn initialize(&mut self, params: &Value) -> Value {
        for entry in params["initializationOptions"]["schemas"].as_array().into_iter().flatten() {
            let patterns = entry["fileMatch"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
            let uri = entry["uri"].as_str().unwrap_or_default().to_string();
            self.associations.push(Association { patterns, uri, schema: entry["schema"].clone() });
        }
        json!({
            "capabilities": {
                "completionProvider": { "triggerCharacters": ["\""] },
                "hoverProvider": true,
                "documentSymbolProvider": true,
                "foldingRangeProvider": true,
                "documentFormattingProvider": true,
            },
            "serverInfo": { "name": "orbvane-json" },
        })
    }

    fn request(&mut self, method: &str, params: &Value, text_doc: &TextDocument) -> Option<Value> {
        let doc = Doc::parse(&text_doc.text);
        let schema = self.schema_for(&text_doc.uri, &doc);
        let schema = schema.as_ref().and_then(|s| self.schema_value(s));
        let range = |(a, b): (usize, usize)| text_doc.range(a, b);
        Some(match method {
            "textDocument/completion" => {
                let offset = text_doc.offset_at(&params["position"]);
                let items: Vec<Value> = Analysis::new(&doc, schema)
                    .complete(offset)
                    .into_iter()
                    .map(|i| {
                        let mut item = json!({
                            "label": i.label,
                            "kind": i.kind,
                            "filterText": i.filter,
                            "insertTextFormat": 2,
                            "textEdit": { "range": range(i.range), "newText": i.insert },
                        });
                        if let Some(d) = i.documentation {
                            item["documentation"] = json!({ "kind": "markdown", "value": d });
                        }
                        item
                    })
                    .collect();
                json!({ "isIncomplete": false, "items": items })
            }
            "textDocument/hover" => {
                let offset = text_doc.offset_at(&params["position"]);
                match Analysis::new(&doc, schema).hover(offset) {
                    Some((text, r)) => json!({ "contents": { "kind": "markdown", "value": text }, "range": range(r) }),
                    None => Value::Null,
                }
            }
            "textDocument/documentSymbol" => {
                fn to_lsp(doc: &TextDocument, sym: features::Symbol) -> Value {
                    json!({
                        "name": sym.name,
                        "kind": sym.kind,
                        "range": doc.range(sym.range.0, sym.range.1),
                        "selectionRange": doc.range(sym.selection.0, sym.selection.1),
                        "children": sym.children.into_iter().map(|c| to_lsp(doc, c)).collect::<Vec<_>>(),
                    })
                }
                Value::Array(features::symbols(&doc).into_iter().map(|sym| to_lsp(text_doc, sym)).collect())
            }
            "textDocument/foldingRange" => {
                let ranges: Vec<Value> = features::folding(&doc)
                    .into_iter()
                    .filter_map(|(a, b)| {
                        let (start, end) = (text_doc.line_of(a), text_doc.line_of(b));
                        // Brackets keep their closing line visible; comments fold whole.
                        let closer = matches!(doc.text.as_bytes().get(b.wrapping_sub(1)), Some(b'}' | b']'));
                        let end = if closer { end.checked_sub(1)? } else { end };
                        (end > start).then(|| json!({ "startLine": start, "endLine": end }))
                    })
                    .collect();
                Value::Array(ranges)
            }
            "textDocument/formatting" => {
                let options = &params["options"];
                let size = options["tabSize"].as_u64().unwrap_or(4) as usize;
                let tab = if options["insertSpaces"].as_bool().unwrap_or(true) { " ".repeat(size) } else { "\t".into() };
                let eol = if doc.text.contains("\r\n") { "\r\n" } else { "\n" };
                Value::Array(features::format(&doc.text, &tab, eol).into_iter().map(|(r, new)| json!({ "range": range(r), "newText": new })).collect())
            }
            _ => return None,
        })
    }

    fn diagnose(&mut self, text_doc: &TextDocument) -> Vec<Value> {
        let jsonc = text_doc.language_id != "json";
        let doc = Doc::parse(&text_doc.text);
        let schema = self.schema_for(&text_doc.uri, &doc);
        let schema = schema.as_ref().and_then(|s| self.schema_value(s));
        let allow = |key: &str| schema.and_then(|s| s.get(key)).and_then(Value::as_bool);
        let mut diagnostics = Vec::new();
        let mut push = |start: usize, end: usize, message: &str, severity: Severity| {
            let severity = match severity {
                Severity::Error => 1,
                Severity::Warning => 2,
            };
            diagnostics.push(json!({ "range": text_doc.range(start, end), "message": message, "severity": severity, "source": "json" }));
        };
        for e in &doc.errors {
            let severity = match e.kind {
                ErrorKind::Syntax => Severity::Error,
                ErrorKind::Comment if jsonc || allow("allowComments") == Some(true) => continue,
                ErrorKind::Comment => Severity::Error,
                ErrorKind::TrailingComma if allow("allowTrailingCommas") == Some(true) => continue,
                ErrorKind::TrailingComma if jsonc => Severity::Warning,
                ErrorKind::TrailingComma => Severity::Error,
            };
            push(e.start, e.end, e.message, severity);
        }
        for p in schema::duplicate_keys(&doc) {
            push(p.start, p.end, &p.message, p.severity);
        }
        if let Some(schema) = schema {
            for p in Validator::new(&doc, schema).run().0 {
                push(p.start, p.end, &p.message, p.severity);
            }
        }
        diagnostics
    }
}

impl JsonService {
    /// Which schema applies: the document's `$schema`, else the first association whose
    /// pattern matches the file.
    fn schema_for(&mut self, uri: &str, doc: &Doc) -> Option<SchemaRef> {
        let declared = doc.root.and_then(|r| doc.get(r, "$schema")).map(|n| doc.node(n).text.clone());
        if let Some(declared) = declared {
            if let Some(i) = self.associations.iter().position(|a| a.uri == declared) {
                return Some(SchemaRef::Association(i));
            }
            let path = match declared.strip_prefix("file://") {
                Some(p) => Some(PathBuf::from(decode(p))),
                None if declared.starts_with('/') => Some(PathBuf::from(&declared)),
                None if !declared.contains("://") => uri_to_path(uri).and_then(|p| p.parent().map(|d| d.join(&declared))),
                None => None,
            };
            if let Some(path) = path {
                self.loaded.entry(path.clone()).or_insert_with(|| {
                    let text = std::fs::read_to_string(&path).ok()?;
                    let doc = Doc::parse(&text);
                    doc.root.map(|r| doc.value(r))
                });
                return Some(SchemaRef::File(path));
            }
            return None;
        }
        let path = uri_to_path(uri)?;
        let path = path.to_string_lossy();
        self.associations.iter().position(|a| a.patterns.iter().any(|p| file_matches(p, &path))).map(SchemaRef::Association)
    }

    fn schema_value(&self, r: &SchemaRef) -> Option<&Value> {
        match r {
            SchemaRef::Association(i) => Some(&self.associations[*i].schema),
            SchemaRef::File(p) => self.loaded.get(p)?.as_ref(),
        }
    }
}

enum SchemaRef {
    Association(usize),
    File(PathBuf),
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(|p| PathBuf::from(decode(p)))
}

/// `fileMatch`: a pattern with a `/` matches the end of the path (whole segments),
/// one without matches the file name; `*` matches within a segment.
pub fn file_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("**/");
    if !pattern.contains('/') {
        let name = path.rsplit('/').next().unwrap_or(path);
        return glob(pattern, name);
    }
    let pattern = pattern.trim_start_matches('/');
    let segments = pattern.split('/').count();
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() >= segments && glob(pattern, &parts[parts.len() - segments..].join("/"))
}

fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, rest)) => {
            let Some(tail) = text.strip_prefix(head) else { return false };
            (0..=tail.len()).filter(|&i| tail.is_char_boundary(i)).any(|i| !tail[..i].contains('/') && glob(rest, &tail[i..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn matches_files() {
        assert!(file_matches("*.code-workspace", "/a/b/x.code-workspace"));
        assert!(file_matches("/.orbvane/settings.json", "/w/.orbvane/settings.json"));
        assert!(!file_matches("/.orbvane/settings.json", "/w/settings.json"));
        assert!(file_matches("/User/settings.json", "/Users/me/Library/Application Support/Orbvane/User/settings.json"));
        assert!(!file_matches("/User/settings.json", "/Users/me/xUser/settings.json"));
    }

    #[test]
    fn serves_diagnostics_and_completion() {
        let (to_server, rx) = channel();
        let (tx, from_server) = channel();
        let thread = std::thread::spawn(move || serve(rx, tx));
        let schema = json!({ "properties": { "a": { "type": "number", "description": "An a." } }, "additionalProperties": false });
        to_server.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "initializationOptions": { "schemas": [{ "fileMatch": ["/conf/x.json"], "uri": "test://x", "schema": schema }] } } })).unwrap();
        assert!(from_server.recv().unwrap()["result"]["capabilities"]["hoverProvider"].as_bool().unwrap());
        let uri = "file:///w/conf/x.json";
        to_server.send(json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": { "textDocument": {
            "uri": uri, "languageId": "jsonc", "version": 1, "text": "{ \"a\": \"s\", \"b\": 1, }" } } })).unwrap();
        let diags = from_server.recv().unwrap();
        let messages: Vec<&str> = diags["params"]["diagnostics"].as_array().unwrap().iter().map(|d| d["message"].as_str().unwrap()).collect();
        assert_eq!(messages, ["Trailing comma", "Incorrect type. Expected \"number\".", "Property b is not allowed."]);
        to_server.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "textDocument/completion", "params": {
            "textDocument": { "uri": uri }, "position": { "line": 0, "character": 1 } } })).unwrap();
        let items = from_server.recv().unwrap()["result"]["items"].clone();
        assert_eq!(items.as_array().unwrap().len(), 0); // `a` is already there
        to_server.send(json!({ "jsonrpc": "2.0", "method": "exit" })).unwrap();
        thread.join().unwrap();
    }
}
