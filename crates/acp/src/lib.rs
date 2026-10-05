//! Our client for the Agent Client Protocol: an editor talking to a coding agent that runs as
//! a child process, JSON-RPC 2.0 messages one per line on its stdin and stdout.
//!
//! The editor asks (`initialize`, `session/new`, `session/prompt`...) with `request`; the agent
//! streams `session/update` notifications and asks the editor things (`session/request_permission`,
//! `fs/read_text_file`, `fs/write_text_file`), answered with `respond`. `poll` hands over what
//! arrived; the waker runs whenever something does, so the UI can poll on its next frame.
//! `update::parse` turns a `session/update` into an `Update`.
//!
//! An agent can also run in-process (`Client::in_process`): bridges that put this protocol in
//! front of an agent's own (`codex`) talk to the client over channels instead of pipes.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

mod bridge;
pub mod claude;
pub mod codex;
pub use bridge::Options;
pub mod mcp;
pub mod update;

/// The protocol version we speak.
pub const PROTOCOL_VERSION: u64 = 1;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Something the agent sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// The answer to request `id`: its result, or the error's message (and code).
    Response { id: i64, result: Result<Value, (i64, String)> },
    Notification { method: String, params: Value },
    /// A request the editor must answer with `respond` (or `respond_error`).
    Request { id: Value, method: String, params: Value },
    /// A line it printed on stderr.
    Log(String),
    /// The process ended (or closed its output).
    Exited,
}

/// Where messages to the agent go.
enum Out {
    Pipe(ChildStdin),
    Channel(Sender<Value>),
}

pub struct Client {
    child: Option<Child>,
    out: Option<Out>,
    rx: Receiver<Incoming>,
    next_id: i64,
    queued: VecDeque<Incoming>,
}

/// The message for a line that isn't JSON-RPC we understand (None: ignore it).
fn classify(line: &str) -> Option<Incoming> {
    classify_value(serde_json::from_str(line.trim()).ok()?)
}

fn classify_value(msg: Value) -> Option<Incoming> {
    let method = msg.get("method").and_then(Value::as_str);
    match (msg.get("id"), method) {
        (Some(id), Some(method)) => Some(Incoming::Request { id: id.clone(), method: method.to_string(), params: msg.get("params").cloned().unwrap_or(Value::Null) }),
        (None, Some(method)) => Some(Incoming::Notification { method: method.to_string(), params: msg.get("params").cloned().unwrap_or(Value::Null) }),
        (Some(id), None) => {
            let id = id.as_i64()?;
            let result = match msg.get("error") {
                Some(e) => Err((e["code"].as_i64().unwrap_or(0), e["message"].as_str().unwrap_or("error").to_string())),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}

impl Client {
    /// Starts `program` with `args` in `cwd` (with `env` added to ours).
    pub fn spawn(program: &str, args: &[String], cwd: &std::path::Path, env: &[(String, String)], waker: Waker) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdin = child.stdin.take();
        let (tx, rx) = mpsc::channel();
        {
            let (tx, waker) = (tx.clone(), waker.clone());
            thread::Builder::new().name("agent reader".into()).spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(msg) = classify(&line) {
                        if tx.send(msg).is_err() {
                            return;
                        }
                        waker();
                    }
                }
                let _ = tx.send(Incoming::Exited);
                waker();
            })?;
        }
        thread::Builder::new().name("agent stderr".into()).spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(Incoming::Log(line)).is_err() {
                    return;
                }
                waker();
            }
        })?;
        Ok(Self { child: Some(child), out: stdin.map(Out::Pipe), rx, next_id: 0, queued: VecDeque::new() })
    }

    /// Runs an agent on a thread of this process. `serve` gets the client's messages, sends its
    /// own back (JSON-RPC values, no framing) and lines for the log; it returns when the
    /// receiver closes, which counts as the agent exiting.
    pub fn in_process(name: &str, waker: Waker, serve: impl FnOnce(Receiver<Value>, Sender<Value>, Sender<String>) + Send + 'static) -> std::io::Result<Self> {
        let (out, agent_rx) = mpsc::channel::<Value>();
        let (agent_tx, agent_out) = mpsc::channel::<Value>();
        let (log_tx, log_rx) = mpsc::channel::<String>();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name(format!("{name} agent")).spawn(move || serve(agent_rx, agent_tx, log_tx))?;
        {
            let (tx, waker) = (tx.clone(), waker.clone());
            thread::Builder::new().name(format!("{name} log")).spawn(move || {
                for line in log_rx {
                    if tx.send(Incoming::Log(line)).is_err() {
                        return;
                    }
                    waker();
                }
            })?;
        }
        thread::Builder::new().name(format!("{name} reader")).spawn(move || {
            for msg in agent_out {
                if let Some(incoming) = classify_value(msg) {
                    if tx.send(incoming).is_err() {
                        return;
                    }
                    waker();
                }
            }
            let _ = tx.send(Incoming::Exited);
            waker();
        })?;
        Ok(Self { child: None, out: Some(Out::Channel(out)), rx, next_id: 0, queued: VecDeque::new() })
    }

    fn send(&mut self, msg: Value) {
        let ok = match &mut self.out {
            None => return,
            Some(Out::Channel(tx)) => tx.send(msg).is_ok(),
            Some(Out::Pipe(stdin)) => {
                let mut line = msg.to_string();
                line.push('\n');
                stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_ok()
            }
        };
        if !ok {
            self.out = None;
        }
    }

    /// Sends request `method`; its answer arrives as `Incoming::Response` with the returned id.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn respond_error(&mut self, id: Value, code: i64, message: &str) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
    }

    /// `initialize`, offering file reads and writes through the editor.
    pub fn initialize(&mut self, client_name: &str, version: &str) -> i64 {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": false },
                "clientInfo": { "name": client_name, "version": version },
            }),
        )
    }

    /// Everything that arrived since the last call.
    pub fn poll(&mut self) -> Vec<Incoming> {
        let mut out: Vec<Incoming> = self.queued.drain(..).collect();
        out.extend(self.rx.try_iter());
        out
    }

    /// Waits up to `timeout` for the next message (tests and scripted use).
    pub fn wait(&mut self, timeout: std::time::Duration) -> Option<Incoming> {
        if let Some(m) = self.queued.pop_front() {
            return Some(m);
        }
        self.rx.recv_timeout(timeout).ok()
    }

    /// Ends the agent: closes its input, then kills it if it doesn't exit at once.
    pub fn shutdown(&mut self) {
        self.out = None;
        if let Some(mut child) = self.child.take() {
            thread::spawn(move || {
                for _ in 0..20 {
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
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s"}}"#),
            Some(Incoming::Response { id: 3, result: Ok(json!({ "sessionId": "s" })) })
        );
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"Authentication required"}}"#),
            Some(Incoming::Response { id: 4, result: Err((-32000, "Authentication required".into())) })
        );
        assert_eq!(
            classify(r#"{"jsonrpc":"2.0","id":"p1","method":"fs/read_text_file","params":{"path":"/a"}}"#),
            Some(Incoming::Request { id: json!("p1"), method: "fs/read_text_file".into(), params: json!({ "path": "/a" }) })
        );
        assert!(matches!(classify(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#), Some(Incoming::Notification { .. })));
        assert_eq!(classify("not json"), None);
    }

    #[test]
    fn talks_to_a_process() {
        // `cat` echoes our request back, which reads as a request from the agent.
        let Ok(mut c) = Client::spawn("/bin/cat", &[], std::path::Path::new("/"), &[], Arc::new(|| {})) else { return };
        let id = c.request("ping", json!({ "x": 1 }));
        match c.wait(std::time::Duration::from_secs(5)) {
            Some(Incoming::Request { id: got, method, params }) => {
                assert_eq!((got, method.as_str(), params), (json!(id), "ping", json!({ "x": 1 })));
            }
            other => panic!("{other:?}"),
        }
        c.shutdown();
    }

    #[test]
    fn talks_to_an_agent_in_process() {
        let mut c = Client::in_process("echo", Arc::new(|| {}), |rx, tx, log| {
            for msg in rx {
                let _ = log.send("got one".into());
                let _ = tx.send(json!({ "jsonrpc": "2.0", "id": msg["id"], "result": msg["params"] }));
            }
        })
        .unwrap();
        let id = c.request("echo", json!({ "x": 2 }));
        let mut got = Vec::new();
        while got.len() < 2 {
            got.push(c.wait(std::time::Duration::from_secs(5)).expect("an answer"));
        }
        assert!(got.contains(&Incoming::Response { id, result: Ok(json!({ "x": 2 })) }));
        assert!(got.contains(&Incoming::Log("got one".into())));
        // Closing our end ends the agent.
        c.shutdown();
        assert_eq!(c.wait(std::time::Duration::from_secs(5)), Some(Incoming::Exited));
    }
}
