//! The editor's own tools for the agent, as a Model Context Protocol server.
//!
//! Agents start MCP servers themselves (the editor lists them in `session/new`), so the server
//! is the editor's executable in a helper mode (`serve_stdio`): it speaks MCP on stdio
//! (newline-delimited JSON-RPC) and forwards `tools/list` and `tools/call` over a Unix socket to
//! the `Bridge` in the running editor, which answers them (from its language servers, its
//! diagnostics...).
//!
//! Socket protocol: the helper writes one JSON object `{"method", "params"}` and closes its
//! write half; the editor replies with `{"result"}` or `{"error"}` and closes.

use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use serde_json::{json, Value};

use crate::Waker;

/// The environment variable naming the editor's socket (set for the helper).
pub const SOCKET_VAR: &str = "ORBVANE_TOOLS_SOCKET";
/// The argument that starts the editor's executable as the helper.
pub const HELPER_ARG: &str = "--editor-tools";
/// The MCP version we speak (we answer with the client's if it asks for an older one).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A `tools/list` or `tools/call` from the agent, waiting for the editor's answer.
pub struct Request {
    pub method: String,
    pub params: Value,
    stream: UnixStream,
}

impl Request {
    pub fn answer(mut self, result: Result<Value, String>) {
        let reply = match result {
            Ok(r) => json!({ "result": r }),
            Err(e) => json!({ "error": e }),
        };
        let _ = self.stream.write_all(reply.to_string().as_bytes());
    }

    /// Answers a `tools/call` with text (`error`: the tool failed, the agent sees why).
    pub fn answer_text(self, text: &str, error: bool) {
        self.answer(Ok(tool_result(text, error)));
    }
}

/// A `tools/call` result holding `text`.
pub fn tool_result(text: &str, error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": error })
}

/// The editor's end: listens on a socket of its own for the helper's requests.
pub struct Bridge {
    socket: PathBuf,
    requests: Receiver<Request>,
}

impl Bridge {
    pub fn start(waker: Waker) -> io::Result<Bridge> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let socket = std::env::temp_dir().join(format!("orbvane-tools-{}-{nanos:x}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let (tx, requests) = mpsc::channel();
        thread::Builder::new().name("editor-tools".into()).spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut text = String::new();
                if stream.read_to_string(&mut text).is_err() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(&text) else { continue };
                let method = msg["method"].as_str().unwrap_or("").to_string();
                if tx.send(Request { method, params: msg["params"].clone(), stream }).is_err() {
                    break;
                }
                waker();
            }
        })?;
        Ok(Bridge { socket, requests })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn poll(&self) -> Option<Request> {
        self.requests.try_recv().ok()
    }

    /// The `mcpServers` entry for `session/new`: `program` (the editor's executable) as the helper.
    pub fn server_config(&self, name: &str, program: &Path) -> Value {
        json!({
            "name": name,
            "command": program,
            "args": [HELPER_ARG],
            "env": [{ "name": SOCKET_VAR, "value": self.socket }],
        })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Asks the editor listening on `socket`.
pub fn ask(socket: &Path, method: &str, params: Value) -> Result<Value, String> {
    let exchange = || -> io::Result<String> {
        let mut stream = UnixStream::connect(socket)?;
        stream.write_all(json!({ "method": method, "params": params }).to_string().as_bytes())?;
        stream.shutdown(std::net::Shutdown::Write)?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        Ok(reply)
    };
    let reply = exchange().map_err(|e| format!("the editor isn't answering ({e})"))?;
    let reply: Value = serde_json::from_str(&reply).map_err(|_| "the editor closed the connection".to_string())?;
    match reply.get("error") {
        Some(e) => Err(e.as_str().unwrap_or("failed").to_string()),
        None => Ok(reply["result"].clone()),
    }
}

/// Answers one MCP message (None for notifications). `forward` asks the editor.
pub fn handle(msg: &Value, version: &str, forward: &mut dyn FnMut(&str, Value) -> Result<Value, String>) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            // Their version if it's one we know (older), else ours.
            let asked = msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION);
            let version_ok = asked <= PROTOCOL_VERSION;
            Ok(json!({
                "protocolVersion": if version_ok { asked } else { PROTOCOL_VERSION },
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "orbvane", "title": "Orbvane editor tools", "version": version },
                "instructions": "Tools that ask the code editor the user has open: its language servers (definitions, references, hover, symbols) and its current diagnostics. Paths may be absolute or relative to the workspace folder; lines are 1-based.",
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" | "tools/call" => forward(method, msg["params"].clone()),
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        _ => return Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{method} isn't supported") } })),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        // A call the editor couldn't take: still a tool result, so the agent reads why.
        Err(e) if method == "tools/call" => json!({ "jsonrpc": "2.0", "id": id, "result": tool_result(&e, true) }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": e } }),
    })
}

/// The helper: an MCP server on stdio forwarding to the editor at `socket`. Returns when stdin
/// closes.
pub fn serve_stdio(socket: &Path, version: &str) -> i32 {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut forward = |method: &str, params: Value| ask(socket, method, params);
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let err = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } });
            let _ = writeln!(stdout, "{err}");
            continue;
        };
        if let Some(reply) = handle(&msg, version, &mut forward) {
            if writeln!(stdout, "{reply}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn calls_reach_the_editor() {
        let bridge = Bridge::start(Arc::new(|| {})).unwrap();
        let socket = bridge.socket().to_path_buf();
        let helper = thread::spawn(move || {
            let mut forward = |method: &str, params: Value| ask(&socket, method, params);
            let init = handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } }), "1.0", &mut forward).unwrap();
            assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
            assert!(handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), "1.0", &mut forward).is_none());
            let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } });
            let ok = handle(&call, "1.0", &mut forward).unwrap();
            let failed = handle(&call, "1.0", &mut forward).unwrap();
            (ok, failed)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut answered = 0;
        while answered < 2 {
            assert!(Instant::now() < deadline);
            match bridge.poll() {
                Some(req) => {
                    assert_eq!(req.method, "tools/call");
                    let text = req.params["arguments"]["text"].as_str().unwrap().to_string();
                    if answered == 0 {
                        req.answer_text(&text, false);
                    } else {
                        req.answer(Err("no such tool".into()));
                    }
                    answered += 1;
                }
                None => thread::sleep(Duration::from_millis(5)),
            }
        }
        let (ok, failed) = helper.join().unwrap();
        assert_eq!(ok["result"]["content"][0]["text"], "hi");
        assert_eq!(ok["result"]["isError"], false);
        assert_eq!(failed["result"]["isError"], true);
        assert_eq!(failed["result"]["content"][0]["text"], "no such tool");
    }
}
