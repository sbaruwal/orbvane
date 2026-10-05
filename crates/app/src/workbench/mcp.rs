//! The editor's own tools for the agent (`assistant.editorTools`): an MCP server the agent
//! starts (our executable as `acp::mcp`'s helper), bridged to this window over a socket. The
//! tools answer from the language servers (definitions, references, hover, symbols) and the
//! Problems the editor already has, so the agent sees what the user sees.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use acp::mcp::{Bridge, Request};
use language::Lang;
use serde_json::{json, Value};
use text::{Buffer, Pos, Selection};

use super::Workbench;
use crate::servers::ServerKey;

/// How long a tool may wait for its language server (they answer nothing while loading).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_EVERY: Duration = Duration::from_millis(800);
/// At most this many lines per answer.
const MAX_LINES: usize = 200;

#[derive(Default)]
pub(super) struct EditorTools {
    bridge: Option<Bridge>,
    calls: Vec<Call>,
}

/// A tool call waiting for language servers.
struct Call {
    req: Option<Request>,
    tool: String,
    args: Value,
    /// The requests out, and the answers so far.
    waiting: Vec<(ServerKey, i64)>,
    answers: Vec<Value>,
    errors: Vec<String>,
    /// Whether a server said it's busy (loading): asked again at `retry_at`.
    busy: bool,
    retry_at: Option<Instant>,
    started: Instant,
    /// A file opened with the server just for this call (closed after it).
    opened: Option<PathBuf>,
}

/// The tools, as `tools/list` describes them.
fn tools() -> Value {
    let at = |what: &str| {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file (absolute, or relative to the workspace folder)" },
                "line": { "type": "integer", "description": "1-based line number" },
                "symbol": { "type": "string", "description": format!("The identifier on that line to {what}") },
            },
            "required": ["path", "line", "symbol"],
        })
    };
    json!({ "tools": [
        {
            "name": "diagnostics",
            "title": "Problems",
            "description": "The errors and warnings the editor currently shows (from its language servers and linters), for one file or the whole workspace.",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "Only this file (absolute, or relative to the workspace folder)" } } },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "definition",
            "title": "Go to Definition",
            "description": "Where a symbol is defined, from the language server: file, line and that line's text.",
            "inputSchema": at("look up"),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "references",
            "title": "Find All References",
            "description": "Every place a symbol is used (and declared), from the language server.",
            "inputSchema": at("find"),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "hover",
            "title": "Hover",
            "description": "What the language server says about a symbol: its type or signature and documentation.",
            "inputSchema": at("describe"),
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "document_symbols",
            "title": "Outline",
            "description": "A file's outline from the language server: its functions, types, fields... with line ranges, nested.",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string", "description": "The file (absolute, or relative to the workspace folder)" } }, "required": ["path"] },
            "annotations": { "readOnlyHint": true },
        },
        {
            "name": "workspace_symbols",
            "title": "Go to Symbol in Workspace",
            "description": "Finds functions, types and other symbols by name across the workspace, from the running language servers.",
            "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A name or part of one" } }, "required": ["query"] },
            "annotations": { "readOnlyHint": true },
        },
    ]})
}

fn symbol_kind(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "file", "module", "namespace", "package", "class", "method", "property", "field", "constructor", "enum", "interface", "function", "variable", "constant",
        "string", "number", "boolean", "array", "object", "key", "null", "enum member", "struct", "event", "operator", "type parameter",
    ];
    KINDS.get((kind as usize).wrapping_sub(1)).copied().unwrap_or("symbol")
}

/// The char column of `symbol` on `line`: a whole-word match first, else any.
fn find_symbol(line: &str, symbol: &str) -> Option<usize> {
    if symbol.is_empty() {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let at_char = |byte: usize| line[..byte].chars().count();
    let mut whole = None;
    let mut any = None;
    for (byte, _) in line.match_indices(symbol) {
        any.get_or_insert(byte);
        let before = line[..byte].chars().next_back().is_none_or(|c| !is_word(c));
        let after = line[byte + symbol.len()..].chars().next().is_none_or(|c| !is_word(c));
        if before && after {
            whole = Some(byte);
            break;
        }
    }
    whole.or(any).map(at_char)
}

/// Text of a hover's `contents` (MarkupContent, MarkedString or a list of them).
fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(hover_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n"),
        Value::Object(o) => {
            let value = o.get("value").and_then(Value::as_str).unwrap_or("");
            match o.get("language").and_then(Value::as_str) {
                Some(lang) => format!("```{lang}\n{value}\n```"),
                None => value.to_string(),
            }
        }
        _ => String::new(),
    }
}

/// The (file, 0-based line) of each location in a definition/references answer (Location,
/// LocationLink, or a list of either).
fn locations(v: &Value) -> Vec<(PathBuf, usize)> {
    let one = |l: &Value| {
        let uri = l["uri"].as_str().or(l["targetUri"].as_str())?;
        let range = if l["targetSelectionRange"].is_object() { &l["targetSelectionRange"] } else if l["range"].is_object() { &l["range"] } else { &l["targetRange"] };
        Some((lsp::uri_to_path(uri)?, range["start"]["line"].as_u64().unwrap_or(0) as usize))
    };
    match v {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Lines of a document outline (DocumentSymbol trees or flat SymbolInformation).
fn outline_lines(symbols: &[Value], depth: usize, out: &mut Vec<String>) {
    for s in symbols {
        let name = s["name"].as_str().unwrap_or("?");
        let kind = symbol_kind(s["kind"].as_u64().unwrap_or(0));
        let range = if s["range"].is_object() { &s["range"] } else { &s["location"]["range"] };
        let (a, b) = (range["start"]["line"].as_u64().unwrap_or(0) + 1, range["end"]["line"].as_u64().unwrap_or(0) + 1);
        let detail = s["detail"].as_str().filter(|d| !d.is_empty()).map(|d| format!(" — {d}")).unwrap_or_default();
        let lines = if a == b { format!("line {a}") } else { format!("lines {a}-{b}") };
        out.push(format!("{}{kind} {name}{detail} ({lines})", "  ".repeat(depth)));
        if let Some(children) = s["children"].as_array() {
            outline_lines(children, depth + 1, out);
        }
    }
}

/// The answer's lines, cut at `MAX_LINES`.
fn limit(mut lines: Vec<String>) -> String {
    if lines.len() > MAX_LINES {
        let more = lines.len() - MAX_LINES;
        lines.truncate(MAX_LINES);
        lines.push(format!("... and {more} more"));
    }
    lines.join("\n")
}

impl Workbench {
    /// The MCP servers the agent gets with each session: ours, when `assistant.editorTools` is on.
    pub(super) fn mcp_servers(&mut self) -> Vec<Value> {
        if !self.settings.bool("assistant.editorTools") {
            return Vec::new();
        }
        if self.tools.bridge.is_none() {
            match Bridge::start(self.waker.clone()) {
                Ok(b) => self.tools.bridge = Some(b),
                Err(e) => {
                    self.output.append("Assistant", &format!("The editor's tools aren't available: {e}\n"));
                    return Vec::new();
                }
            }
        }
        let Ok(program) = std::env::current_exe() else { return Vec::new() };
        self.tools.bridge.as_ref().map(|b| b.server_config("orbvane", &program)).into_iter().collect()
    }

    /// Environment for the agent's process.
    pub(super) fn mcp_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Takes the agent's tool requests, asks again where a server was busy, gives up on slow
    /// ones. Called every frame.
    pub(super) fn mcp_tick(&mut self) {
        while let Some(req) = self.tools.bridge.as_ref().and_then(Bridge::poll) {
            match req.method.clone().as_str() {
                "tools/list" => req.answer(Ok(tools())),
                "tools/call" => self.tool_call(req),
                other => req.answer(Err(format!("{other} isn't supported"))),
            }
        }
        let now = Instant::now();
        let mut i = 0;
        while i < self.tools.calls.len() {
            let call = &self.tools.calls[i];
            if call.started.elapsed() > CALL_TIMEOUT {
                let mut call = self.tools.calls.remove(i);
                self.finish_call(&mut call, Err("The language server didn't answer in time (it may still be loading the project).".into()));
                continue;
            }
            if call.retry_at.is_some_and(|t| t <= now) {
                let mut call = self.tools.calls.remove(i);
                call.retry_at = None;
                call.busy = false;
                call.answers.clear();
                call.errors.clear();
                match self.issue(&mut call) {
                    Ok(()) => self.tools.calls.insert(i, call),
                    Err(e) => self.finish_call(&mut call, Err(e)),
                }
            }
            i += 1;
        }
    }

    pub(super) fn mcp_deadline(&self) -> Option<Instant> {
        let retry = self.tools.calls.iter().filter_map(|c| c.retry_at).min();
        let timeout = self.tools.calls.iter().map(|c| c.started + CALL_TIMEOUT).min();
        retry.into_iter().chain(timeout).min()
    }

    /// A language server answered: true if it was one of ours.
    pub(super) fn mcp_response(&mut self, key: &ServerKey, id: i64, result: &Result<Value, String>) -> bool {
        let Some(i) = self.tools.calls.iter().position(|c| c.waiting.iter().any(|(k, n)| k == key && *n == id)) else { return false };
        let call = &mut self.tools.calls[i];
        call.waiting.retain(|(k, n)| !(k == key && *n == id));
        let loading = self.lsp.ready(key) == Some(false);
        match result {
            Ok(v) => call.answers.push(v.clone()),
            // Busy while loading ("content modified" and the like): ask again.
            Err(e) if loading || e.contains("modified") || e.contains("-32801") => call.busy = true,
            Err(e) => call.errors.push(e.clone()),
        }
        if !call.waiting.is_empty() {
            return true;
        }
        let empty = call.answers.iter().all(|a| a.is_null() || a.as_array().is_some_and(Vec::is_empty));
        if call.busy || (empty && loading) {
            call.retry_at = Some(Instant::now() + RETRY_EVERY);
            return true;
        }
        let mut call = self.tools.calls.remove(i);
        if call.answers.is_empty() && !call.errors.is_empty() {
            let e = format!("The language server couldn't answer: {}", call.errors.join("; "));
            self.finish_call(&mut call, Err(e));
            return true;
        }
        let text = self.format_answer(&call);
        self.finish_call(&mut call, Ok(text));
        true
    }

    fn finish_call(&mut self, call: &mut Call, result: Result<String, String>) {
        if let Some(path) = call.opened.take() {
            if !self.docs.iter().flatten().any(|d| d.buffer.path() == Some(path.as_path())) {
                self.lsp.close(&path);
            }
        }
        if let Some(req) = call.req.take() {
            match result {
                Ok(text) => req.answer_text(&text, false),
                Err(e) => req.answer_text(&e, true),
            }
        }
    }

    /// A file the agent named: absolute, relative to a workspace folder, or a `display_path`.
    fn tool_path(&self, arg: &str) -> PathBuf {
        let p = Path::new(arg);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        if let Some(found) = self.folders().into_iter().map(|f| f.join(p)).find(|f| f.exists()) {
            return found;
        }
        self.resolve_display_path(arg).filter(|p| p.exists()).unwrap_or_else(|| self.folder().unwrap_or_default().join(p))
    }

    /// The text of `path`: the open document's (unsaved changes and all), else the file's.
    fn tool_text(&self, path: &Path) -> Option<String> {
        match self.docs.iter().flatten().find(|d| d.buffer.path() == Some(path)) {
            Some(d) => Some(d.buffer.text()),
            None => std::fs::read_to_string(path).ok(),
        }
    }

    fn tool_call(&mut self, req: Request) {
        let tool = req.params["name"].as_str().unwrap_or("").to_string();
        let args = req.params["arguments"].clone();
        if tool == "diagnostics" {
            let path = args["path"].as_str().map(|p| self.tool_path(p));
            let text = self.diagnostics_text(path.as_deref());
            return req.answer_text(&text, false);
        }
        let mut call = Call { req: Some(req), tool, args, waiting: Vec::new(), answers: Vec::new(), errors: Vec::new(), busy: false, retry_at: None, started: Instant::now(), opened: None };
        match self.issue(&mut call) {
            Ok(()) => self.tools.calls.push(call),
            Err(e) => self.finish_call(&mut call, Err(e)),
        }
    }

    /// Sends the call's requests to the language servers.
    fn issue(&mut self, call: &mut Call) -> Result<(), String> {
        let args = call.args.clone();
        if call.tool == "workspace_symbols" {
            let query = args["query"].as_str().unwrap_or("");
            for key in self.lsp.running() {
                if let Some(id) = self.lsp.ext_request(&key, "workspace/symbol", json!({ "query": query })) {
                    call.waiting.push((key, id));
                }
            }
            if call.waiting.is_empty() {
                return Err("No language server is running yet. Open a file of the project's language first.".into());
            }
            return Ok(());
        }
        let (method, needs_pos) = match call.tool.as_str() {
            "definition" => ("textDocument/definition", true),
            "references" => ("textDocument/references", true),
            "hover" => ("textDocument/hover", true),
            "document_symbols" => ("textDocument/documentSymbol", false),
            other => return Err(format!("There is no tool called {other}.")),
        };
        let path_arg = args["path"].as_str().ok_or("Give the file's path.")?;
        let path = self.tool_path(path_arg);
        let text = self.tool_text(&path).ok_or_else(|| format!("Can't read {}.", path.display()))?;
        let mut buffer = Buffer::new();
        buffer.insert(Selection::default(), &text);
        let pos = if needs_pos {
            let line = args["line"].as_u64().ok_or("Give the 1-based line number.")? as usize;
            let symbol = args["symbol"].as_str().unwrap_or("");
            if line == 0 || line > buffer.len_lines() {
                return Err(format!("{} has {} lines.", self.display_path(&path), buffer.len_lines()));
            }
            let line_text = buffer.line(line - 1);
            let col = find_symbol(&line_text, symbol).ok_or_else(|| format!("\"{symbol}\" isn't on line {line} of {}. That line is: {}", self.display_path(&path), line_text.trim_end()))?;
            Some(Pos::new(line - 1, col))
        } else {
            None
        };
        let lang = Lang::detect(Some(&path));
        // The server knows open documents; others are opened with it for this call.
        let open = self.docs.iter().flatten().any(|d| d.buffer.path() == Some(path.as_path()));
        if !open && !self.lsp.is_open(&path) {
            let root = self.lsp_root(&path, lang);
            self.lsp.sync(&path, lang, &buffer, &root);
            if self.lsp.is_open(&path) {
                call.opened = Some(path.clone());
            }
        }
        let params = if method == "textDocument/references" { json!({ "context": { "includeDeclaration": true } }) } else { json!({}) };
        let sent = self.lsp.doc_request(&path, &buffer, method, params, pos);
        let (key, id) = sent.ok_or_else(|| format!("There's no language server for {} files.", lang.name()))?;
        call.waiting.push((key, id));
        Ok(())
    }

    /// Text for an answered call.
    fn format_answer(&self, call: &Call) -> String {
        let mut cache: std::collections::HashMap<PathBuf, Vec<String>> = Default::default();
        let mut line_of = |path: &Path, line: usize| -> String {
            let lines = cache.entry(path.to_path_buf()).or_insert_with(|| self.tool_text(path).map(|t| t.lines().map(String::from).collect()).unwrap_or_default());
            lines.get(line).map(|l| l.trim().to_string()).unwrap_or_default()
        };
        let answer = call.answers.first().cloned().unwrap_or(Value::Null);
        match call.tool.as_str() {
            "definition" | "references" => {
                let mut locs: Vec<(PathBuf, usize)> = call.answers.iter().flat_map(locations).collect();
                locs.dedup();
                if locs.is_empty() {
                    return format!("The language server found no {}.", if call.tool == "definition" { "definition" } else { "references" });
                }
                let mut lines = Vec::new();
                if call.tool == "references" {
                    lines.push(format!("{} references:", locs.len()));
                }
                for (path, line) in locs {
                    let text = line_of(&path, line);
                    lines.push(format!("{}:{}: {text}", self.display_path(&path), line + 1));
                }
                limit(lines)
            }
            "hover" => {
                let text = hover_text(&answer["contents"]);
                if text.trim().is_empty() { "The language server has nothing to say about it.".into() } else { text }
            }
            "document_symbols" => {
                let mut lines = Vec::new();
                outline_lines(answer.as_array().map_or(&[][..], Vec::as_slice), 0, &mut lines);
                if lines.is_empty() { "The language server found no symbols in this file.".into() } else { limit(lines) }
            }
            "workspace_symbols" => {
                let mut lines = Vec::new();
                for s in call.answers.iter().filter_map(Value::as_array).flatten() {
                    let name = s["name"].as_str().unwrap_or("?");
                    let kind = symbol_kind(s["kind"].as_u64().unwrap_or(0));
                    let container = s["containerName"].as_str().filter(|c| !c.is_empty()).map(|c| format!(" in {c}")).unwrap_or_default();
                    let at = s["location"]["uri"].as_str().and_then(lsp::uri_to_path).map(|p| {
                        let line = s["location"]["range"]["start"]["line"].as_u64();
                        match line {
                            Some(l) => format!("{}:{}", self.display_path(&p), l + 1),
                            None => self.display_path(&p),
                        }
                    });
                    lines.push(format!("{kind} {name}{container}{}", at.map(|a| format!(" — {a}")).unwrap_or_default()));
                }
                if lines.is_empty() { "No symbols match.".into() } else { limit(lines) }
            }
            _ => String::new(),
        }
    }

    /// The Problems for `path` (or all), one per line.
    fn diagnostics_text(&self, path: Option<&Path>) -> String {
        let mut lines = Vec::new();
        for (file, (_, diags)) in &self.lsp.diagnostics {
            if path.is_some_and(|p| p != file) {
                continue;
            }
            for d in diags {
                let severity = match d.severity {
                    lsp::Severity::Error => "error",
                    lsp::Severity::Warning => "warning",
                    lsp::Severity::Information => "info",
                    lsp::Severity::Hint => "hint",
                };
                let source = d.source.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default();
                let message = d.message.lines().next().unwrap_or("");
                lines.push(format!("{}:{}:{}: {severity}: {message}{source}", self.display_path(file), d.range.start.line + 1, d.range.start.character + 1));
            }
        }
        if lines.is_empty() {
            return match path {
                Some(p) => format!("No problems in {}.", self.display_path(p)),
                None => "No problems in the workspace.".into(),
            };
        }
        limit(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_symbols_on_a_line() {
        assert_eq!(find_symbol("let total = sum(total_items);", "total"), Some(4));
        assert_eq!(find_symbol("total_items", "total"), Some(0)); // no whole word: any
        assert_eq!(find_symbol("é = fn_é()", "fn_é"), Some(4));
        assert_eq!(find_symbol("abc", "x"), None);
    }

    #[test]
    fn formats_server_answers() {
        assert_eq!(hover_text(&json!({ "kind": "markdown", "value": "**x**" })), "**x**");
        assert_eq!(hover_text(&json!([{ "language": "rust", "value": "fn f()" }, "doc"])), "```rust\nfn f()\n```\n\ndoc");
        let loc = json!([{ "uri": "file:///a/b.rs", "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 1 } } },
                         { "targetUri": "file:///a/c.rs", "targetRange": {}, "targetSelectionRange": { "start": { "line": 9, "character": 2 } } }]);
        assert_eq!(locations(&loc), vec![(PathBuf::from("/a/b.rs"), 3), (PathBuf::from("/a/c.rs"), 9)]);
        let mut out = Vec::new();
        let tree = json!([{ "name": "Point", "kind": 23, "range": { "start": { "line": 0 }, "end": { "line": 3 } },
                            "children": [{ "name": "x", "kind": 8, "detail": "f32", "range": { "start": { "line": 1 }, "end": { "line": 1 } } }] }]);
        outline_lines(tree.as_array().unwrap(), 0, &mut out);
        assert_eq!(out, vec!["struct Point (lines 1-4)", "  field x — f32 (line 2)"]);
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    /// The tools as the agent's MCP server asks for them, answered by the built-in JSON server.
    #[test]
    fn tools_answer_from_the_language_server() {
        let dir = std::env::temp_dir().join(format!("orbvane-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, "{\n  \"name\": \"demo\",\n  \"build\": { \"release\": true },\n  \"broken\": \n}\n").unwrap();
        // SAFETY: every test that reads this wants the same scratch user data folder.
        unsafe { std::env::set_var("ORBVANE_USER_DATA", std::env::temp_dir().join("orbvane-test-user")) };
        let mut wb = Workbench::new(Some(dir.clone()), &[file.clone()], std::sync::Arc::new(|| {}));
        let servers = wb.mcp_servers();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["args"][0], acp::mcp::HELPER_ARG);
        let socket = PathBuf::from(servers[0]["env"][0]["value"].as_str().unwrap());

        // The agent's side, on another thread: list the tools, then call them.
        let agent = std::thread::spawn(move || {
            let ask = |name: &str, args: Value| {
                let r = acp::mcp::ask(&socket, "tools/call", json!({ "name": name, "arguments": args })).unwrap();
                (r["content"][0]["text"].as_str().unwrap().to_string(), r["isError"].as_bool().unwrap())
            };
            let list = acp::mcp::ask(&socket, "tools/list", json!({})).unwrap();
            let names: Vec<String> = list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
            // Problems arrive once the server has looked at the file.
            let deadline = Instant::now() + Duration::from_secs(20);
            let problems = loop {
                let (text, _) = ask("diagnostics", json!({}));
                if text.contains("config.json") || Instant::now() > deadline {
                    break text;
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let outline = ask("document_symbols", json!({ "path": "config.json" }));
            let missing = ask("hover", json!({ "path": "config.json", "line": 2, "symbol": "nothere" }));
            let unknown = ask("rename_everything", json!({}));
            (names, problems, outline, missing, unknown)
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        while !agent.is_finished() {
            assert!(Instant::now() < deadline, "the tools didn't answer");
            wb.lsp_tick();
            wb.mcp_tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let (names, problems, outline, missing, unknown) = agent.join().unwrap();
        assert!(names.contains(&"definition".to_string()) && names.contains(&"workspace_symbols".to_string()), "{names:?}");
        assert!(problems.starts_with("config.json:5:") && problems.contains("error"), "{problems}");
        assert!(!outline.1 && outline.0.contains("name") && outline.0.contains("  ") && outline.0.contains("release"), "{}", outline.0);
        assert!(missing.1 && missing.0.contains("isn't on line 2"), "{}", missing.0);
        assert!(unknown.1 && unknown.0.contains("no tool"), "{}", unknown.0);
        wb.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
