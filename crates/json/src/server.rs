//! The JSON language server, run in-process (`lsp::Client::in_process`). Schemas come with
//! `initialize` (`initializationOptions.schemas`: `{ fileMatch, uri, schema }`, like the
//! `json.schemas` setting; an entry without `schema` is loaded from its `uri`) or from a
//! document's own `$schema`. Schemas named by URL are downloaded with `/usr/bin/curl` (unless
//! `download` is false) and kept in the `cache` folder for a week.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use std::sync::mpsc::{Receiver, Sender};

use lsp::server::{Service, TextDocument};
use serde_json::{json, Value};

use crate::features::{self, Analysis};
use crate::parse::{Doc, ErrorKind};
use crate::schema::{self, Severity, Validator};

struct Association {
    patterns: Vec<String>,
    uri: String,
    /// None: loaded from `uri`.
    schema: Option<Value>,
}

#[derive(Default)]
struct JsonService {
    associations: Vec<Association>,
    /// Schemas loaded from files and URLs, by path or URL (None: couldn't be).
    loaded: HashMap<String, Option<Value>>,
    download: bool,
    cache: Option<PathBuf>,
}

/// How long a downloaded schema is used before it's downloaded again.
const CACHE_FOR: Duration = Duration::from_secs(7 * 24 * 3600);

/// A file name for `url` in the cache (FNV-1a).
fn cache_name(url: &str) -> String {
    let hash = url.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{hash:016x}.json")
}

fn parse_schema(text: &str) -> Option<Value> {
    let doc = Doc::parse(text);
    doc.root.map(|r| doc.value(r)).filter(Value::is_object)
}

/// Downloads `url`, or takes it from `cache` while fresh (or when the download fails).
fn download(url: &str, cache: Option<&Path>) -> Option<String> {
    let cached = cache.map(|c| c.join(cache_name(url)));
    let fresh = cached.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok()).is_some_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < CACHE_FOR);
    if fresh {
        if let Some(text) = cached.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            return Some(text);
        }
    }
    let out = std::process::Command::new("/usr/bin/curl").args(["-sSfL", "--max-time", "15", "--", url]).output().ok();
    match out.filter(|o| o.status.success()).and_then(|o| String::from_utf8(o.stdout).ok()) {
        Some(text) => {
            if let Some(path) = &cached {
                if parse_schema(&text).is_some() && path.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) {
                    let _ = std::fs::write(path, &text);
                }
            }
            Some(text)
        }
        None => cached.and_then(|p| std::fs::read_to_string(p).ok()),
    }
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
            let schema = Some(entry["schema"].clone()).filter(|s| !s.is_null());
            self.associations.push(Association { patterns, uri, schema });
        }
        let options = &params["initializationOptions"];
        self.download = options["download"].as_bool().unwrap_or(true);
        self.cache = options["cache"].as_str().map(PathBuf::from);
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
                return self.association(i);
            }
            let location = match declared.strip_prefix("file://") {
                Some(p) => decode(p),
                None if declared.starts_with('/') || declared.starts_with("http://") || declared.starts_with("https://") => declared,
                None if !declared.contains("://") => uri_to_path(uri)?.parent()?.join(&declared).to_string_lossy().into_owned(),
                None => return None,
            };
            return self.load(&location);
        }
        let path = uri_to_path(uri)?;
        let path = path.to_string_lossy();
        let i = self.associations.iter().position(|a| a.patterns.iter().any(|p| file_matches(p, &path)))?;
        self.association(i)
    }

    fn association(&mut self, i: usize) -> Option<SchemaRef> {
        if self.associations[i].schema.is_some() {
            return Some(SchemaRef::Association(i));
        }
        let uri = self.associations[i].uri.clone();
        let location = match uri.strip_prefix("file://") {
            Some(p) => decode(p),
            None => uri,
        };
        self.load(&location)
    }

    /// The schema in a file or at a URL, loaded once.
    fn load(&mut self, location: &str) -> Option<SchemaRef> {
        if !self.loaded.contains_key(location) {
            let remote = location.starts_with("http://") || location.starts_with("https://");
            let text = match remote {
                true if self.download => download(location, self.cache.as_deref()),
                true => None,
                false => std::fs::read_to_string(location).ok(),
            };
            self.loaded.insert(location.to_string(), text.as_deref().and_then(parse_schema));
        }
        Some(SchemaRef::Loaded(location.to_string()))
    }

    fn schema_value(&self, r: &SchemaRef) -> Option<&Value> {
        match r {
            SchemaRef::Association(i) => self.associations[*i].schema.as_ref(),
            SchemaRef::Loaded(l) => self.loaded.get(l)?.as_ref(),
        }
    }
}

enum SchemaRef {
    Association(usize),
    Loaded(String),
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

    /// Serves `body` over HTTP on a local port for `n` requests; its URL.
    fn serve_once(body: &'static str, n: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/schema.json", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(n).flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        url
    }

    /// Diagnostics for a document, with these initialization options.
    fn messages(options: Value, uri: &str, text: &str) -> Vec<String> {
        let (to_server, rx) = channel();
        let (tx, from_server) = channel();
        let thread = std::thread::spawn(move || serve(rx, tx));
        to_server.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "initializationOptions": options } })).unwrap();
        from_server.recv().unwrap();
        to_server.send(json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": { "textDocument": {
            "uri": uri, "languageId": "json", "version": 1, "text": text } } })).unwrap();
        let diags = from_server.recv().unwrap();
        to_server.send(json!({ "jsonrpc": "2.0", "method": "exit" })).unwrap();
        thread.join().unwrap();
        diags["params"]["diagnostics"].as_array().unwrap().iter().map(|d| d["message"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn schemas_from_urls_and_files() {
        let dir = std::env::temp_dir().join(format!("orbvane-json-schemas-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("cache");
        let url = serve_once(r#"{ "properties": { "port": { "type": "number" } } }"#, 1);
        let text = format!(r#"{{ "$schema": "{url}", "port": "80" }}"#);
        let options = json!({ "cache": cache });
        assert_eq!(messages(options.clone(), "file:///w/a.json", &text), ["Incorrect type. Expected \"number\"."]);
        // The second time it comes from the cache (the server is gone).
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        assert_eq!(messages(options, "file:///w/a.json", &text), ["Incorrect type. Expected \"number\"."]);
        // Downloads off: no schema.
        assert!(messages(json!({ "download": false }), "file:///w/a.json", &text).is_empty());
        // A `json.schemas` entry naming a file.
        let file = dir.join("s.json");
        std::fs::write(&file, r#"{ "required": ["name"] }"#).unwrap();
        let options = json!({ "schemas": [{ "fileMatch": ["*.conf.json"], "uri": format!("file://{}", file.display()) }] });
        assert_eq!(messages(options, "file:///w/x.conf.json", "{}"), ["Missing property \"name\"."]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
