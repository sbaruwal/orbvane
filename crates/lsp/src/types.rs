//! The subset of LSP types the editor uses, parsed from JSON by hand.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::uri_to_path;

/// How the server counts columns (negotiated in `initialize`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

impl Encoding {
    /// Converts a char column in `line` to the server's column units.
    pub fn to_lsp(self, line: &str, char_col: usize) -> u32 {
        let prefix = line.chars().take(char_col);
        match self {
            Encoding::Utf8 => prefix.map(char::len_utf8).sum::<usize>() as u32,
            Encoding::Utf16 => prefix.map(char::len_utf16).sum::<usize>() as u32,
        }
    }

    /// Converts a server column to a char column in `line` (clamped to the line).
    pub fn from_lsp(self, line: &str, col: u32) -> usize {
        let mut units = 0u32;
        for (i, c) in line.chars().enumerate() {
            if units >= col {
                return i;
            }
            units += match self {
                Encoding::Utf8 => c.len_utf8() as u32,
                Encoding::Utf16 => c.len_utf16() as u32,
            };
        }
        line.chars().count()
    }
}

/// A position in server units: (line, column).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character })
    }

    fn parse(v: &Value) -> Option<Self> {
        Some(Self { line: v["line"].as_u64()? as u32, character: v["character"].as_u64()? as u32 })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn parse(v: &Value) -> Option<Self> {
        Some(Self { start: Position::parse(&v["start"])?, end: Position::parse(&v["end"])? })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
    /// The diagnostic as the server sent it (code action requests send it back).
    pub raw: Value,
}

/// Parses `textDocument/publishDiagnostics` params.
pub fn parse_diagnostics(params: &Value) -> Option<(PathBuf, Vec<Diagnostic>)> {
    let path = uri_to_path(params["uri"].as_str()?)?;
    let diags = params["diagnostics"]
        .as_array()?
        .iter()
        .filter_map(|d| {
            Some(Diagnostic {
                range: Range::parse(&d["range"])?,
                severity: match d["severity"].as_u64() {
                    Some(2) => Severity::Warning,
                    Some(3) => Severity::Information,
                    Some(4) => Severity::Hint,
                    _ => Severity::Error,
                },
                message: d["message"].as_str()?.to_string(),
                source: d["source"].as_str().map(String::from),
                code: match &d["code"] {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                },
                raw: d.clone(),
            })
        })
        .collect();
    Some((path, diags))
}

/// Extracts hover contents as markdown (from MarkupContent, MarkedString or an array of them).
pub fn parse_hover(result: &Value) -> Option<String> {
    fn marked(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => {
                let value = o.get("value")?.as_str()?;
                match o.get("language").and_then(Value::as_str) {
                    Some(lang) => Some(format!("```{lang}\n{value}\n```")),
                    None => Some(value.to_string()),
                }
            }
            _ => None,
        }
    }
    let contents = &result["contents"];
    let text = match contents {
        Value::Array(items) => items.iter().filter_map(marked).collect::<Vec<_>>().join("\n\n"),
        other => marked(other)?,
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[derive(Clone, Debug)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

/// Parses a definition result: Location, Location[] or LocationLink[].
pub fn parse_locations(result: &Value) -> Vec<Location> {
    fn one(v: &Value) -> Option<Location> {
        if let Some(uri) = v["targetUri"].as_str() {
            let range = Range::parse(&v["targetSelectionRange"]).or_else(|| Range::parse(&v["targetRange"]))?;
            return Some(Location { path: uri_to_path(uri)?, range });
        }
        Some(Location { path: uri_to_path(v["uri"].as_str()?)?, range: Range::parse(&v["range"])? })
    }
    match result {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Null => Vec::new(),
        other => one(other).into_iter().collect(),
    }
}

#[derive(Clone, Debug)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl TextEdit {
    fn parse(v: &Value) -> Option<Self> {
        // InsertReplaceEdit has `insert`/`replace` instead of `range`; prefer the replace range.
        let range = Range::parse(&v["range"]).or_else(|| Range::parse(&v["replace"]))?;
        Some(Self { range, new_text: v["newText"].as_str()?.to_string() })
    }
}

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    /// Extra text after the label (e.g. a function signature).
    pub label_detail: Option<String>,
    pub detail: Option<String>,
    pub kind: u32,
    pub filter_text: String,
    pub sort_text: String,
    pub insert_text: String,
    pub edit: Option<TextEdit>,
    pub additional_edits: Vec<TextEdit>,
    /// `insert_text` / the edit's text is a snippet (`${1:x}`), not plain text.
    pub snippet: bool,
    /// Documentation, as markdown (plain text is shown as is).
    pub documentation: Option<String>,
    /// A command to run after inserting (`editor.action.triggerSuggest`).
    pub command: Option<String>,
}

/// Parses a completion result (an array or a CompletionList). Returns items and `isIncomplete`.
pub fn parse_completions(result: &Value) -> (Vec<CompletionItem>, bool) {
    let (items, incomplete) = match result {
        Value::Array(items) => (items.as_slice(), false),
        Value::Object(o) => (
            o.get("items").and_then(Value::as_array).map_or(&[][..], Vec::as_slice),
            o.get("isIncomplete").and_then(Value::as_bool).unwrap_or(false),
        ),
        _ => (&[][..], false),
    };
    let items = items
        .iter()
        .filter_map(|v| {
            let label = v["label"].as_str()?.to_string();
            let edit = TextEdit::parse(&v["textEdit"]);
            Some(CompletionItem {
                label_detail: v["labelDetails"]["detail"].as_str().map(String::from),
                detail: v["detail"].as_str().map(String::from),
                kind: v["kind"].as_u64().unwrap_or(1) as u32,
                filter_text: v["filterText"].as_str().unwrap_or(&label).to_string(),
                sort_text: v["sortText"].as_str().unwrap_or(&label).to_string(),
                insert_text: v["insertText"].as_str().unwrap_or(&label).to_string(),
                edit,
                snippet: v["insertTextFormat"].as_u64() == Some(2),
                documentation: match &v["documentation"] {
                    Value::String(s) => Some(s.clone()),
                    d => d["value"].as_str().map(String::from),
                }
                .filter(|d| !d.is_empty()),
                command: v["command"]["command"].as_str().map(String::from),
                additional_edits: v["additionalTextEdits"]
                    .as_array()
                    .map(|a| a.iter().filter_map(TextEdit::parse).collect())
                    .unwrap_or_default(),
                label,
            })
        })
        .collect();
    (items, incomplete)
}


/// Short label for a completion item kind, used for the icon in the suggest widget.
pub fn completion_kind_name(kind: u32) -> &'static str {
    match kind {
        2 => "method",
        3 => "function",
        4 => "constructor",
        5 => "field",
        6 => "variable",
        7 => "class",
        8 => "interface",
        9 => "module",
        10 => "property",
        12 => "value",
        13 => "enum",
        14 => "keyword",
        15 => "snippet",
        20 => "enum member",
        21 => "constant",
        22 => "struct",
        25 => "type parameter",
        _ => "text",
    }
}

/// Edits to several files (rename, code actions), grouped by file.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceEdit {
    pub changes: Vec<(PathBuf, Vec<TextEdit>)>,
}

impl WorkspaceEdit {
    pub fn is_empty(&self) -> bool {
        self.changes.iter().all(|(_, e)| e.is_empty())
    }
}

/// Parses a WorkspaceEdit (`changes` or `documentChanges`; file create/rename/delete
/// operations aren't supported and are skipped).
pub fn parse_workspace_edit(v: &Value) -> Option<WorkspaceEdit> {
    let mut out: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
    let mut add = |uri: &str, edits: &Value| {
        let Some(path) = uri_to_path(uri) else { return };
        let edits: Vec<TextEdit> = edits.as_array().into_iter().flatten().filter_map(TextEdit::parse).collect();
        match out.iter_mut().find(|(p, _)| *p == path) {
            Some((_, e)) => e.extend(edits),
            None => out.push((path, edits)),
        }
    };
    if let Some(docs) = v["documentChanges"].as_array() {
        for d in docs {
            if let Some(uri) = d["textDocument"]["uri"].as_str() {
                add(uri, &d["edits"]);
            }
        }
    } else if let Some(map) = v["changes"].as_object() {
        for (uri, edits) in map {
            add(uri, edits);
        }
    } else {
        return None;
    }
    Some(WorkspaceEdit { changes: out })
}

/// The range `prepareRename` answered with, and the name to start from if it gave one.
pub fn parse_prepare_rename(v: &Value) -> Option<(Range, Option<String>)> {
    if let Some(r) = Range::parse(v) {
        return Some((r, None));
    }
    let r = Range::parse(&v["range"])?;
    Some((r, v["placeholder"].as_str().map(String::from)))
}

/// A code action (quick fix, refactoring) or a bare command.
#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title: String,
    /// "quickfix", "refactor.extract", ... (empty if the server didn't say).
    pub kind: String,
    pub preferred: bool,
    /// Why it can't be applied now, if the server offers it disabled.
    pub disabled: Option<String>,
    pub edit: Option<WorkspaceEdit>,
    /// A command to run on the server after the edit (`workspace/executeCommand`).
    pub command: Option<Value>,
    /// The raw action, for `codeAction/resolve` when it has neither edit nor command.
    pub raw: Value,
}

pub fn parse_code_actions(result: &Value) -> Vec<CodeAction> {
    result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let title = v["title"].as_str()?.to_string();
            // A bare Command has a string `command`; a CodeAction has an object `command`.
            if v["command"].is_string() {
                return Some(CodeAction { title, kind: String::new(), preferred: false, disabled: None, edit: None, command: Some(v.clone()), raw: v.clone() });
            }
            Some(CodeAction {
                title,
                kind: v["kind"].as_str().unwrap_or_default().to_string(),
                preferred: v["isPreferred"].as_bool().unwrap_or(false),
                disabled: v["disabled"]["reason"].as_str().map(String::from),
                edit: parse_workspace_edit(&v["edit"]),
                command: v.get("command").filter(|c| c.is_object()).cloned(),
                raw: v.clone(),
            })
        })
        .collect()
}

/// Parses `textDocument/formatting` (and range formatting) edits.
pub fn parse_text_edits(result: &Value) -> Vec<TextEdit> {
    result.as_array().into_iter().flatten().filter_map(TextEdit::parse).collect()
}

impl Range {
    pub fn to_json(self) -> Value {
        json!({ "start": self.start.to_json(), "end": self.end.to_json() })
    }
}


/// A symbol in a document (`textDocument/documentSymbol`), for the Outline view.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentSymbol {
    pub name: String,
    /// Extra text shown after the name (a signature or type), if any.
    pub detail: String,
    /// LSP `SymbolKind` (1 = File ... 26 = TypeParameter).
    pub kind: u32,
    /// The whole symbol, including its body.
    pub range: Range,
    /// The part to reveal when jumping to it (usually the name).
    pub selection_range: Range,
    pub children: Vec<DocumentSymbol>,
}


/// One signature of a `textDocument/signatureHelp` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Signature {
    pub label: String,
    pub documentation: String,
    /// Each parameter's byte range in `label`, and its documentation.
    pub parameters: Vec<(std::ops::Range<usize>, String)>,
    /// Overrides the answer's active parameter for this signature.
    pub active_parameter: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignatureHelp {
    pub signatures: Vec<Signature>,
    pub active_signature: usize,
    pub active_parameter: Option<usize>,
}

/// Documentation: a string or MarkupContent.
fn doc_text(v: &Value) -> String {
    v.as_str().or_else(|| v["value"].as_str()).unwrap_or_default().to_string()
}

/// Parses a signatureHelp result. None when there's nothing to show (null or no signatures).
pub fn parse_signature_help(result: &Value) -> Option<SignatureHelp> {
    let signatures: Vec<Signature> = result["signatures"]
        .as_array()?
        .iter()
        .filter_map(|s| {
            let label = s["label"].as_str()?.to_string();
            let mut from = 0;
            let parameters = s["parameters"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            let range = match &p["label"] {
                                // [start, end] in UTF-16 code units of the signature label.
                                Value::Array(a) => {
                                    let unit = |i: usize| a.get(i).and_then(Value::as_u64).map(|n| n as u32);
                                    let byte = |u: u32| {
                                        let ch = Encoding::Utf16.from_lsp(&label, u);
                                        label.char_indices().nth(ch).map_or(label.len(), |(i, _)| i)
                                    };
                                    byte(unit(0)?)..byte(unit(1)?)
                                }
                                // A substring of the label: the first one after the previous parameter.
                                Value::String(sub) => {
                                    let at = label[from..].find(sub.as_str())? + from;
                                    at..at + sub.len()
                                }
                                _ => return None,
                            };
                            from = range.end;
                            Some((range, doc_text(&p["documentation"])))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Signature {
                documentation: doc_text(&s["documentation"]),
                parameters,
                active_parameter: s["activeParameter"].as_u64().map(|n| n as usize),
                label,
            })
        })
        .collect();
    if signatures.is_empty() {
        return None;
    }
    let active_signature = (result["activeSignature"].as_u64().unwrap_or(0) as usize).min(signatures.len() - 1);
    Some(SignatureHelp { signatures, active_signature, active_parameter: result["activeParameter"].as_u64().map(|n| n as usize) })
}

/// An inlay hint (`textDocument/inlayHint`).
#[derive(Clone, Debug, PartialEq)]
pub struct InlayHint {
    pub position: Position,
    pub label: String,
    /// 1: a type, 2: a parameter name.
    pub kind: u32,
    pub padding_left: bool,
    pub padding_right: bool,
}

pub fn parse_inlay_hints(result: &Value) -> Vec<InlayHint> {
    let Some(items) = result.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            // A string, or label parts (each may link somewhere; we show the text).
            let label = match &v["label"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["value"].as_str()).collect(),
                _ => return None,
            };
            Some(InlayHint {
                position: Position::parse(&v["position"])?,
                label,
                kind: v["kind"].as_u64().unwrap_or(0) as u32,
                padding_left: v["paddingLeft"].as_bool().unwrap_or(false),
                padding_right: v["paddingRight"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// A match of `workspace/symbol`.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub kind: u32,
    /// The symbol it's in (a module, a type), if the server says.
    pub container: String,
    pub path: PathBuf,
    /// Where it is; None when the server only gave the file (resolved later in LSP).
    pub range: Option<Range>,
}

pub fn parse_workspace_symbols(result: &Value) -> Vec<WorkspaceSymbol> {
    let Some(items) = result.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            let location = &v["location"];
            Some(WorkspaceSymbol {
                name: v["name"].as_str()?.to_string(),
                kind: v["kind"].as_u64().unwrap_or(0) as u32,
                container: v["containerName"].as_str().unwrap_or_default().to_string(),
                path: uri_to_path(location["uri"].as_str()?)?,
                range: Range::parse(&location["range"]),
            })
        })
        .collect()
}

/// Parses a documentSymbol result: either a `DocumentSymbol[]` tree or a flat
/// `SymbolInformation[]` list, which is nested by range containment.
pub fn parse_document_symbols(result: &Value) -> Vec<DocumentSymbol> {
    fn tree(v: &Value) -> Option<DocumentSymbol> {
        let range = Range::parse(&v["range"])?;
        let mut children: Vec<DocumentSymbol> = v["children"].as_array().map(|a| a.iter().filter_map(tree).collect()).unwrap_or_default();
        children.sort_by_key(|c| c.range.start);
        Some(DocumentSymbol {
            name: v["name"].as_str()?.to_string(),
            detail: v["detail"].as_str().unwrap_or_default().to_string(),
            kind: v["kind"].as_u64().unwrap_or(0) as u32,
            selection_range: Range::parse(&v["selectionRange"]).unwrap_or(range),
            range,
            children,
        })
    }
    let Some(items) = result.as_array() else { return Vec::new() };
    if items.iter().all(|v| v.get("location").is_none()) {
        let mut out: Vec<DocumentSymbol> = items.iter().filter_map(tree).collect();
        out.sort_by_key(|s| s.range.start);
        return out;
    }
    // SymbolInformation: sort by start (outer first at equal starts), then place each one
    // inside the innermost earlier symbol whose range contains it.
    let mut flat: Vec<DocumentSymbol> = items
        .iter()
        .filter_map(|v| {
            let range = Range::parse(&v["location"]["range"])?;
            Some(DocumentSymbol {
                name: v["name"].as_str()?.to_string(),
                detail: String::new(),
                kind: v["kind"].as_u64().unwrap_or(0) as u32,
                range,
                selection_range: range,
                children: Vec::new(),
            })
        })
        .collect();
    flat.sort_by(|a, b| a.range.start.cmp(&b.range.start).then(b.range.end.cmp(&a.range.end)));
    fn insert(into: &mut Vec<DocumentSymbol>, s: DocumentSymbol) {
        match into.last_mut() {
            Some(last) if last.range.start <= s.range.start && s.range.end <= last.range.end => insert(&mut last.children, s),
            _ => into.push(s),
        }
    }
    let mut out = Vec::new();
    for s in flat {
        insert(&mut out, s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        let line = "aé😀b";
        assert_eq!(Encoding::Utf8.to_lsp(line, 3), 1 + 2 + 4);
        assert_eq!(Encoding::Utf16.to_lsp(line, 3), 1 + 1 + 2);
        assert_eq!(Encoding::Utf16.from_lsp(line, 4), 3);
        assert_eq!(Encoding::Utf8.from_lsp(line, 7), 3);
        assert_eq!(Encoding::Utf8.from_lsp(line, 99), 4);
    }

    #[test]
    fn snippets_are_stripped() {
    }

    #[test]
    fn parses_hover_and_locations() {
        let hover = json!({ "contents": { "kind": "markdown", "value": "```rust\nfn f()\n```" } });
        assert_eq!(parse_hover(&hover).unwrap(), "```rust\nfn f()\n```");
        let links = json!([{
            "targetUri": "file:///a.rs",
            "targetRange": { "start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 1} },
            "targetSelectionRange": { "start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 4} }
        }]);
        let locs = parse_locations(&links);
        assert_eq!(locs[0].path, PathBuf::from("/a.rs"));
        assert_eq!(locs[0].range.start, Position { line: 1, character: 3 });
    }

    #[test]
    fn parses_workspace_edits_and_code_actions() {
        let range = json!({ "start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6} });
        let changes = json!({ "changes": { "file:///a.rs": [ { "range": range, "newText": "bar" } ] } });
        let e = parse_workspace_edit(&changes).unwrap();
        assert_eq!(e.changes[0].0, PathBuf::from("/a.rs"));
        assert_eq!(e.changes[0].1[0].new_text, "bar");
        let docs = json!({ "documentChanges": [
            { "textDocument": { "uri": "file:///a.rs", "version": 3 }, "edits": [ { "range": range, "newText": "x" } ] },
            { "kind": "create", "uri": "file:///new.rs" },
            { "textDocument": { "uri": "file:///a.rs", "version": 3 }, "edits": [ { "range": range, "newText": "y" } ] }
        ]});
        let e = parse_workspace_edit(&docs).unwrap();
        assert_eq!(e.changes.len(), 1);
        assert_eq!(e.changes[0].1.len(), 2);

        let actions = json!([
            { "title": "Import `HashMap`", "kind": "quickfix", "isPreferred": true, "edit": changes },
            { "title": "Extract into function", "kind": "refactor.extract", "data": { "id": 1 } },
            { "title": "Run test", "command": "rust-analyzer.runSingle", "arguments": [] }
        ]);
        let a = parse_code_actions(&actions);
        assert_eq!(a.len(), 3);
        assert!(a[0].preferred && a[0].edit.is_some());
        assert!(a[1].edit.is_none() && a[1].command.is_none());
        assert!(a[2].command.is_some());
        assert_eq!(parse_prepare_rename(&json!({ "range": range, "placeholder": "foo" })).unwrap().1.as_deref(), Some("foo"));
    }

    #[test]
    fn parses_completion_list() {
        let list = json!({ "isIncomplete": true, "items": [
            { "label": "push", "kind": 2, "detail": "fn(&mut self, T)",
              "insertTextFormat": 2,
              "textEdit": { "range": { "start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 4} }, "newText": "push(${1:value})" } }
        ]});
        let (items, incomplete) = parse_completions(&list);
        assert!(incomplete);
        // Snippets are kept as they are (the editor expands them), and flagged.
        assert_eq!(items[0].edit.as_ref().unwrap().new_text, "push(${1:value})");
        assert!(items[0].snippet);
        assert_eq!(completion_kind_name(items[0].kind), "method");
    }

    #[test]
    fn document_symbols_both_shapes() {
        let r = |l0: u32, l1: u32| json!({ "start": { "line": l0, "character": 0 }, "end": { "line": l1, "character": 1 } });
        let tree = json!([
            { "name": "b", "kind": 12, "range": r(5, 6), "selectionRange": r(5, 5) },
            { "name": "S", "kind": 23, "detail": "struct", "range": r(0, 3), "selectionRange": r(0, 0),
              "children": [{ "name": "x", "kind": 8, "range": r(1, 1), "selectionRange": r(1, 1) }] },
        ]);
        let t = parse_document_symbols(&tree);
        assert_eq!(t.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["S", "b"]);
        assert_eq!(t[0].children[0].name, "x");
        assert_eq!(t[0].detail, "struct");

        let loc = |l0: u32, l1: u32| json!({ "uri": "file:///a.rs", "range": r(l0, l1) });
        let flat = json!([
            { "name": "x", "kind": 8, "location": loc(1, 1) },
            { "name": "S", "kind": 23, "location": loc(0, 3) },
            { "name": "b", "kind": 12, "location": loc(5, 6) },
        ]);
        let f = parse_document_symbols(&flat);
        assert_eq!(f.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["S", "b"]);
        assert_eq!(f[0].children[0].name, "x");
    }

    #[test]
    fn signature_help_labels() {
        let v = json!({
            "signatures": [{
                "label": "fn sum_of(a: i32, b: i32) -> i32",
                "documentation": { "kind": "markdown", "value": "Adds." },
                "parameters": [{ "label": [10, 16] }, { "label": "b: i32" }],
            }],
            "activeSignature": 0,
            "activeParameter": 1,
        });
        let h = parse_signature_help(&v).unwrap();
        let s = &h.signatures[0];
        assert_eq!(&s.label[s.parameters[0].0.clone()], "a: i32");
        assert_eq!(&s.label[s.parameters[1].0.clone()], "b: i32");
        assert_eq!(s.documentation, "Adds.");
        assert_eq!(h.active_parameter, Some(1));
        assert!(parse_signature_help(&Value::Null).is_none());
        assert!(parse_signature_help(&json!({ "signatures": [] })).is_none());
    }
}
