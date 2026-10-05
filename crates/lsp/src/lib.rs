//! A small Language Server Protocol client: JSON-RPC over a child process's stdio (or with a
//! server running in-process).
//!
//! Reading and writing happen on background threads so a slow server never blocks the UI.
//! Incoming messages are queued and the `waker` callback is invoked so the UI can `poll`.

pub mod server;
mod types;

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

pub use types::*;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug)]
pub enum Incoming {
    Response { id: i64, result: Result<Value, String> },
    Notification { method: String, params: Value },
    /// A request from the server the editor must answer (`workspace/applyEdit`) with
    /// `Client::respond`. Other requests are answered inside `poll`.
    Request { id: Value, method: String, params: Value },
    /// A line the server wrote to stderr (shown in the Output panel).
    Log(String),
    Exited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Initializing,
    Running,
    Exited,
}

pub struct Client {
    pub name: String,
    pub state: State,
    pub encoding: Encoding,
    /// Characters that should trigger completion, from the server's capabilities.
    pub completion_triggers: Vec<String>,
    /// The server's capabilities from `initialize` (Null until then).
    pub capabilities: Value,
    /// The server process (None for a server running in-process on a thread).
    child: Option<Child>,
    out: Sender<Value>,
    rx: Receiver<Incoming>,
    next_id: i64,
    /// Messages sent before the server finished initializing.
    queued: VecDeque<Value>,
    /// Answers to `workspace/configuration`, by section.
    settings: Value,
}

const INITIALIZE_ID: i64 = 0;

/// The value of a dotted configuration `section` ("yaml.format") in `settings`; all of it
/// without a section.
fn configuration(settings: &Value, section: Option<&str>) -> Value {
    let Some(section) = section.filter(|s| !s.is_empty()) else { return settings.clone() };
    section.split('.').try_fold(settings, |v, key| v.get(key)).cloned().unwrap_or(Value::Null)
}

impl Client {
    /// Starts `command` as a language server rooted at `root` and sends `initialize`.
    /// `init_options` goes in `initializationOptions`; `settings` answers `workspace/configuration`
    /// and is sent with `workspace/didChangeConfiguration` once the server runs (Null: neither).
    pub fn spawn(name: &str, command: &Path, args: &[&str], root: &Path, init_options: Value, settings: Value, waker: Waker) -> io::Result<Self> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let (child, out, rx) = spawn_framed(name, command, &args, root, &[], waker)?;
        Ok(Self::start(name, Some(child), out, rx, root, init_options, settings))
    }

    /// Runs a language server written in Rust on a thread of this process. `serve` gets the
    /// client's messages and sends its own back, as JSON-RPC values (no framing); it returns
    /// when the receiver closes.
    pub fn in_process(
        name: &str,
        root: &Path,
        init_options: Value,
        settings: Value,
        waker: Waker,
        serve: impl FnOnce(Receiver<Value>, Sender<Value>) + Send + 'static,
    ) -> io::Result<Self> {
        let (out, server_rx) = mpsc::channel::<Value>();
        let (server_tx, server_out) = mpsc::channel::<Value>();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name(format!("{name} server")).spawn(move || serve(server_rx, server_tx))?;
        // Forwards the server's messages, as the reader thread does for a process.
        thread::Builder::new().name(format!("{name} reader")).spawn(move || {
            for msg in server_out {
                if let Some(incoming) = classify(msg) {
                    if tx.send(incoming).is_err() {
                        return;
                    }
                    waker();
                }
            }
            let _ = tx.send(Incoming::Exited);
            waker();
        })?;
        Ok(Self::start(name, None, out, rx, root, init_options, settings))
    }

    /// Sends `initialize` over a new connection.
    fn start(name: &str, child: Option<Child>, out: Sender<Value>, rx: Receiver<Incoming>, root: &Path, init_options: Value, settings: Value) -> Self {
        let client = Self {
            name: name.to_string(),
            state: State::Initializing,
            encoding: Encoding::Utf16,
            completion_triggers: Vec::new(),
            capabilities: Value::Null,
            child,
            out,
            rx,
            next_id: INITIALIZE_ID + 1,
            queued: VecDeque::new(),
            settings,
        };
        let uri = path_to_uri(root);
        let folder = root.file_name().map_or_else(|| "root".into(), |n| n.to_string_lossy().to_string());
        client.send(json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_ID,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "orbvane", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": folder }],
                "capabilities": client_capabilities(),
                "initializationOptions": init_options,
            }
        }));
        client
    }

    fn send(&self, msg: Value) {
        let _ = self.out.send(msg);
    }

    fn send_or_queue(&mut self, msg: Value) {
        match self.state {
            State::Running => self.send(msg),
            State::Initializing => self.queued.push_back(msg),
            State::Exited => {}
        }
    }

    /// Sends a request and returns its id; the response arrives later via `poll`.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send_or_queue(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    /// Answers a request the server sent (see `Incoming::Request`).
    pub fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send_or_queue(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Drains incoming messages. Handshake traffic and server-to-client requests are
    /// handled here; everything else is returned.
    pub fn poll(&mut self) -> Vec<Incoming> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Incoming::Response { id: INITIALIZE_ID, result } => match result {
                    Ok(result) => self.on_initialized(&result),
                    Err(e) => {
                        out.push(Incoming::Log(format!("initialize failed: {e}")));
                        self.state = State::Exited;
                    }
                },
                Incoming::Request { id, method, params } if method == "workspace/applyEdit" => {
                    out.push(Incoming::Request { id, method, params });
                }
                Incoming::Request { id, method, params } => {
                    // Answer so the server doesn't wait on us. We have no settings to offer, so
                    // configuration requests get nulls (the server keeps its defaults).
                    let result = match method.as_str() {
                        "workspace/configuration" => {
                            let items = params["items"].as_array().map_or(&[][..], Vec::as_slice);
                            Value::Array(items.iter().map(|i| configuration(&self.settings, i["section"].as_str())).collect())
                        }
                        _ => Value::Null,
                    };
                    self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                    // "Ask me again" requests (workspace/inlayHint/refresh) matter to the editor.
                    if method.ends_with("/refresh") {
                        out.push(Incoming::Notification { method, params });
                    }
                }
                Incoming::Exited => {
                    self.state = State::Exited;
                    out.push(Incoming::Exited);
                }
                other => out.push(other),
            }
        }
        out
    }

    fn on_initialized(&mut self, result: &Value) {
        self.capabilities = result["capabilities"].clone();
        let caps = &result["capabilities"];
        self.encoding = match caps["positionEncoding"].as_str() {
            Some("utf-8") => Encoding::Utf8,
            _ => Encoding::Utf16,
        };
        self.completion_triggers = caps["completionProvider"]["triggerCharacters"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        self.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
        if !self.settings.is_null() {
            self.send(json!({ "jsonrpc": "2.0", "method": "workspace/didChangeConfiguration", "params": { "settings": self.settings } }));
        }
        self.state = State::Running;
        while let Some(msg) = self.queued.pop_front() {
            self.send(msg);
        }
    }

    /// Asks the server to shut down and exit, then stops waiting for it.
    pub fn shutdown(&mut self) {
        if self.state == State::Running {
            let id = self.next_id;
            self.next_id += 1;
            self.send(json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null }));
            self.send(json!({ "jsonrpc": "2.0", "method": "exit", "params": null }));
        }
        self.state = State::Exited;
        let Some(child) = &mut self.child else { return };
        // Give the server a moment to exit on its own, then make sure it's gone.
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = child.kill();
    }

    /// Like `shutdown`, but waits for the process to exit on a thread of its own, so the
    /// caller isn't held up (stopping a server while the editor runs).
    pub fn shutdown_in_background(mut self) {
        let child = self.child.take();
        self.shutdown();
        let Some(mut child) = child else { return };
        let _ = thread::Builder::new().name(format!("{} shutdown", self.name)).spawn(move || {
            for _ in 0..40 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.state != State::Exited {
            self.shutdown();
        }
    }
}

/// Starts `command` with JSON-RPC (`Content-Length` framing) over its stdin and stdout; stderr
/// lines arrive as `Incoming::Log`.
fn spawn_framed(name: &str, command: &Path, args: &[String], root: &Path, env: &[(String, String)], waker: Waker) -> io::Result<(Child, Sender<Value>, Receiver<Incoming>)> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();

    // Writer: frames and writes outgoing messages.
    let (out, out_rx) = mpsc::channel::<Value>();
    thread::Builder::new().name(format!("{name} writer")).spawn(move || {
        let mut stdin = io::BufWriter::new(stdin);
        for msg in out_rx {
            let body = msg.to_string();
            let ok = write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).and_then(|_| stdin.flush());
            if ok.is_err() {
                break;
            }
        }
    })?;

    // Reader: parses framed messages from stdout.
    let (reader_tx, reader_waker) = (tx.clone(), waker.clone());
    thread::Builder::new().name(format!("{name} reader")).spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_message(&mut reader) {
            if let Some(incoming) = classify(msg) {
                if reader_tx.send(incoming).is_err() {
                    return;
                }
                reader_waker();
            }
        }
        let _ = reader_tx.send(Incoming::Exited);
        reader_waker();
    })?;

    // stderr: forwarded as log lines.
    thread::Builder::new().name(format!("{name} stderr")).spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(Incoming::Log(line)).is_err() {
                return;
            }
            waker();
        }
    })?;

    Ok((child, out, rx))
}

/// A JSON-RPC connection to a program, without LSP's handshake: for other protocols over the
/// same transport (extensions). Requests the program sends arrive as `Incoming::Request`.
pub struct Connection {
    child: Option<Child>,
    out: Sender<Value>,
    rx: Receiver<Incoming>,
}

impl Connection {
    pub fn spawn(name: &str, command: &Path, args: &[String], dir: &Path, env: &[(String, String)], waker: Waker) -> io::Result<Self> {
        let (child, out, rx) = spawn_framed(name, command, args, dir, env, waker)?;
        Ok(Connection { child: Some(child), out, rx })
    }

    pub fn send(&self, msg: Value) {
        let _ = self.out.send(msg);
    }

    pub fn request(&self, id: i64, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond(&self, id: Value, result: Result<Value, String>) {
        self.send(match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": e } }),
        });
    }

    pub fn poll(&mut self) -> Vec<Incoming> {
        self.rx.try_iter().collect()
    }

    /// Gives the program a moment to exit (after `exit`), then kills it.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        thread::spawn(move || {
            for _ in 0..40 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop();
    }
}

fn client_capabilities() -> Value {
    json!({
        "general": { "positionEncodings": ["utf-8", "utf-16"] },
        "window": { "workDoneProgress": true },
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "symbol": {},
            "applyEdit": true,
            "workspaceEdit": { "documentChanges": true },
            "inlayHint": { "refreshSupport": true },
            "codeLens": { "refreshSupport": true },
            "executeCommand": {}
        },
        // Client-side commands code lenses may use (rust-analyzer only sends lenses for these).
        "experimental": {
            // rust-analyzer's test explorer: it reports discovered tests as files change.
            "testExplorer": true,
            "commands": { "commands": ["rust-analyzer.runSingle", "rust-analyzer.debugSingle", "rust-analyzer.showReferences", "rust-analyzer.gotoLocation"] }
        },
        "textDocument": {
            "synchronization": { "didSave": true, "dynamicRegistration": false },
            "publishDiagnostics": { "relatedInformation": false },
            "hover": { "contentFormat": ["markdown", "plaintext"] },
            "definition": { "linkSupport": true },
            "completion": {
                "completionItem": { "snippetSupport": true, "labelDetailsSupport": true },
                "contextSupport": true
            },
            "rename": { "prepareSupport": true },
            "references": {},
            "inlayHint": {},
            "codeLens": {},
            "callHierarchy": {},
            "typeHierarchy": {},
            "linkedEditingRange": {},
            "colorProvider": {},
            "foldingRange": { "lineFoldingOnly": true },
            "semanticTokens": {
                "requests": { "full": true },
                "tokenTypes": [
                    "namespace", "type", "class", "enum", "interface", "struct", "typeParameter", "parameter",
                    "variable", "property", "enumMember", "event", "function", "method", "macro", "keyword",
                    "modifier", "comment", "string", "number", "regexp", "operator", "decorator"
                ],
                "tokenModifiers": [
                    "declaration", "definition", "readonly", "static", "deprecated", "abstract", "async",
                    "modification", "documentation", "defaultLibrary"
                ],
                "formats": ["relative"],
                "overlappingTokenSupport": false,
                "multilineTokenSupport": false
            },
            "signatureHelp": {
                "signatureInformation": {
                    "documentationFormat": ["markdown", "plaintext"],
                    "parameterInformation": { "labelOffsetSupport": true },
                    "activeParameterSupport": true
                },
                "contextSupport": true
            },
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "codeAction": {
                "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [
                    "", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite",
                    "source", "source.organizeImports", "source.fixAll"
                ] } },
                "isPreferredSupport": true,
                "disabledSupport": true,
                "dataSupport": true,
                "resolveSupport": { "properties": ["edit"] }
            },
            "formatting": {},
            "onTypeFormatting": {},
            "rangeFormatting": {}
        }
    })
}

/// Reads one `Content-Length`-framed JSON message. Returns None at EOF or on a broken stream.
fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(v) = header.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn classify(msg: Value) -> Option<Incoming> {
    let method = msg["method"].as_str().map(String::from);
    let has_id = !msg["id"].is_null();
    match (method, has_id) {
        (Some(m), true) => Some(Incoming::Request { id: msg["id"].clone(), method: m, params: msg["params"].clone() }),
        (Some(m), false) => Some(Incoming::Notification { method: m, params: msg["params"].clone() }),
        (None, true) => {
            let id = msg["id"].as_i64()?;
            let result = match msg.get("error") {
                Some(err) => Err(err["message"].as_str().unwrap_or("unknown error").to_string()),
                None => Ok(msg["result"].clone()),
            };
            Some(Incoming::Response { id, result })
        }
        (None, false) => None,
    }
}

/// Converts a path to a `file://` URI, percent-encoding unsafe bytes.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trip() {
        let p = Path::new("/Users/me/My Project/src/main.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///Users/me/My%20Project/src/main.rs");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
        assert_eq!(uri_to_path("file:///a/b%2Bc").unwrap(), Path::new("/a/b+c"));
    }

    #[test]
    fn reads_framed_messages() {
        let body = r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        let raw = format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n{}", body.len(), body);
        let mut reader = BufReader::new(raw.as_bytes());
        let msg = read_message(&mut reader).unwrap();
        match classify(msg).unwrap() {
            Incoming::Response { id, result } => {
                assert_eq!(id, 3);
                assert_eq!(result.unwrap()["ok"], true);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(read_message(&mut reader).is_none());
    }
}
