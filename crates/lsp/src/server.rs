//! The server side, for language servers that run in-process (`Client::in_process`): the
//! message loop, the open documents, and position conversion. Positions use UTF-8 columns
//! (byte offsets within the line), which the server announces in `initialize`.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

use serde_json::{json, Value};

/// An open document.
pub struct TextDocument {
    pub uri: String,
    pub language_id: String,
    pub text: String,
    line_starts: Vec<usize>,
}

impl TextDocument {
    pub fn new(uri: &str, language_id: &str, text: String) -> Self {
        let mut doc = Self { uri: uri.to_string(), language_id: language_id.to_string(), text: String::new(), line_starts: Vec::new() };
        doc.set_text(text);
        doc
    }

    pub fn set_text(&mut self, text: String) {
        self.line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        self.text = text;
    }

    /// The line `offset` is on.
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset) - 1
    }

    /// A byte offset for an LSP position (clamped to the line).
    pub fn offset_at(&self, position: &Value) -> usize {
        let line = position["line"].as_u64().unwrap_or(0) as usize;
        let col = position["character"].as_u64().unwrap_or(0) as usize;
        let Some(&start) = self.line_starts.get(line) else { return self.text.len() };
        let end = self.line_starts.get(line + 1).map_or(self.text.len(), |&s| s - 1);
        let mut offset = (start + col).min(end);
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    pub fn position_at(&self, offset: usize) -> Value {
        let offset = offset.min(self.text.len());
        let line = self.line_of(offset);
        json!({ "line": line, "character": offset - self.line_starts[line] })
    }

    pub fn range(&self, start: usize, end: usize) -> Value {
        json!({ "start": self.position_at(start), "end": self.position_at(end) })
    }
}

/// A language service behind `serve`.
pub trait Service {
    /// Answers `initialize` (its `params`) with the server's capabilities.
    fn initialize(&mut self, params: &Value) -> Value;

    /// Diagnostics for a document that opened or changed.
    fn diagnose(&mut self, _doc: &TextDocument) -> Vec<Value> {
        Vec::new()
    }

    /// Answers a `textDocument/*` request about `doc`; None if the method isn't handled.
    fn request(&mut self, method: &str, params: &Value, doc: &TextDocument) -> Option<Value>;
}

/// Runs `service` until the client goes away or sends `exit`. Changes that arrive together
/// are diagnosed once.
pub fn serve(rx: Receiver<Value>, tx: Sender<Value>, mut service: impl Service) {
    let mut docs: HashMap<String, TextDocument> = HashMap::new();
    let send = |msg: Value| {
        let _ = tx.send(msg);
    };
    while let Ok(first) = rx.recv() {
        let mut changed: Vec<String> = Vec::new();
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            let method = msg["method"].as_str().unwrap_or_default();
            let params = &msg["params"];
            let id = msg.get("id").filter(|id| !id.is_null());
            let uri = params["textDocument"]["uri"].as_str().unwrap_or_default().to_string();
            let result = match method {
                "initialize" => {
                    let mut result = service.initialize(params);
                    result["capabilities"]["positionEncoding"] = json!("utf-8");
                    result["capabilities"]["textDocumentSync"] = json!({ "openClose": true, "change": 1 });
                    Some(result)
                }
                "textDocument/didOpen" => {
                    let d = &params["textDocument"];
                    let text = d["text"].as_str().unwrap_or_default().to_string();
                    docs.insert(uri.clone(), TextDocument::new(&uri, d["languageId"].as_str().unwrap_or_default(), text));
                    changed.push(uri);
                    continue;
                }
                "textDocument/didChange" => {
                    let text = params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str());
                    if let (Some(doc), Some(text)) = (docs.get_mut(&uri), text) {
                        doc.set_text(text.to_string());
                        if !changed.contains(&uri) {
                            changed.push(uri);
                        }
                    }
                    continue;
                }
                "textDocument/didClose" => {
                    docs.remove(&uri);
                    changed.retain(|u| *u != uri);
                    send(json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": [] } }));
                    continue;
                }
                "shutdown" => Some(Value::Null),
                "exit" => return,
                _ => docs.get(&uri).and_then(|doc| service.request(method, params, doc)),
            };
            let Some(id) = id else { continue };
            send(match result {
                Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                None if docs.contains_key(&uri) || uri.is_empty() => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Unhandled method {method}") } })
                }
                // A document that isn't open: nothing to say about it.
                None => json!({ "jsonrpc": "2.0", "id": id, "result": null }),
            });
        }
        for uri in changed {
            if let Some(doc) = docs.get(&uri) {
                let diagnostics = service.diagnose(doc);
                send(json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": diagnostics } }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_positions() {
        let doc = TextDocument::new("file:///x", "css", "a {\n  é: 1;\n}".into());
        let offset = doc.text.find('1').unwrap();
        assert_eq!(doc.position_at(offset), json!({ "line": 1, "character": 6 }));
        assert_eq!(doc.offset_at(&doc.position_at(offset)), offset);
        assert_eq!(doc.offset_at(&json!({ "line": 0, "character": 99 })), 3);
        assert_eq!(doc.line_of(doc.text.len()), 2);
    }
}
